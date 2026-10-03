//! `RethExecutor`: the `rome_zk_executor_api::Executor` implementation. See this crate's module doc
//! (`src/lib.rs`) for the source citations and the design mapping.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy_consensus::transaction::SignerRecoverable;
use alloy_consensus::{BlockHeader, Header, Transaction};
use alloy_eips::eip2718::Decodable2718;
use alloy_eips::eip4895::Withdrawals;
use alloy_primitives::{Address, Bytes, TxHash, B256};
use reth_chain_state::{ExecutedBlock, MemoryOverlayStateProviderRef};
use reth_chainspec::ChainSpec;
use reth_ethereum_primitives::{Receipt, TransactionSigned};
use reth_evm::execute::BlockBuilder;
use reth_evm::{ConfigureEvm, Evm, NextBlockEnvAttributes};
use reth_evm_ethereum::EthEvmConfig;
use reth_execution_types::{BlockExecutionOutput, ExecutionOutcome};
use reth_primitives_traits::{Recovered, SealedHeader};
use reth_provider::providers::BlockchainProvider;
use reth_provider::{BlockNumReader, HeaderProvider, ProviderFactory, StateProviderBox};
use reth_revm::database::StateProviderDatabase;
use reth_storage_api::{BlockWriter, StateProvider};
use reth_trie_common::{ComputedTrieData, HashedPostState};
use revm::database::{CacheState, State};

use rome_zk_executor_api::{
    canonical_header_rule_with_withdrawals, BlockEnv, BlockOutcome, BlockSealInputs,
    Executor as ZkExecutor, ExecutorError, Head, Reason, RejectedTx, SubBlockLimits,
    SubBlockOutcome,
};

use crate::chain::{genesis_from_path, open_provider_factory, RethTypes};
use crate::static_heal;

/// `[reth]` config section (see `config.example.toml`): datadir for the genesis-only MDBX store,
/// and the genesis JSON this chain was bred from (shaped like the devnet genesis template).
#[derive(Debug, Clone)]
pub struct RethConfig {
    pub datadir: PathBuf,
    pub genesis_path: PathBuf,
    /// The profile's block gas limit; `RethExecutor::new` refuses a genesis whose gas limit differs.
    pub block_gas_limit: u64,
}

/// One block's in-progress preview execution state, live for exactly the 20 sub-blocks between two
/// `seal_block` calls (module doc: "sub-block N executes against ... persists across the block's 20
/// sub-blocks"). Thrown away at `seal_block` — the canonical trajectory (`RethExecutor::
/// canonical_parent`, and now the real MDBX behind `self.factory`) is advanced independently, by
/// re-executing the same ordered tx list once more (module doc's sanctioned fallback).
struct PendingBlock {
    state: State<StateProviderDatabase<reth_provider::StateProviderBox>>,
    /// Ordered, included txs accumulated across this block's sub-blocks so far — exactly the forced
    /// list `seal_block` re-executes (no pool, ever). Stored already-`decode_and_recover`ed (measured:
    /// signature recovery alone costs ~113ms at the 5k-tx design load point — `Recovered`
    /// is what `execute_sub_block` already computed per tx to run the preview in the first place, so
    /// carrying it forward here means `seal_block` never redundantly re-recovers the same senders a
    /// second time).
    included: Vec<Recovered<TransactionSigned>>,
    attrs: NextBlockEnvAttributes,
    /// The `withdrawals_root` the shared header rule fixes for this block
    /// ([`canonical_header_rule_with_withdrawals`] over the env's withdrawals). `seal_block` checks the header reth
    /// built against it.
    withdrawals_root: B256,
}

/// One sealed-but-not-yet-CONFIRMED-durable block, kept in the shape reth's own
/// in-memory chain uses for exactly this situation. `executed` (an `ExecutedBlock` — one recovered
/// block plus its own execution outcome plus its own trie data) is fed to
/// `MemoryOverlayStateProviderRef` (see [`RethExecutor::overlay_state_provider`]) — reth's own
/// machinery for "state built on top of parents that have not yet been persisted", the same thing
/// its in-memory chain / pending-block RPC support uses. That is what gives every LATER
/// not-yet-confirmed block's preview/re-execution `State` correct account/storage values AND (via
/// `BlockBuilder::finish`'s `root_state_provider` argument) a correct state ROOT, without this
/// block's own write having landed — with no need for a later block's own bundle to be cumulative
/// over this one (see `RethExecutor::write_in_flight`'s doc for why that distinction is load-bearing:
/// it is what makes batching several of these into one write always safe). `hashed_state` duplicates
/// `executed`'s own (pre-sorted, for the overlay) hashed diff in UNSORTED form, so several of these
/// can be combined via `HashedPostState::extend` (sorting only once, at write time) when
/// [`RethExecutor::spawn_write_batch`] batches more than one into a single
/// `BlockWriter::append_blocks_with_state` call.
struct NotYetConfirmedBlock {
    block_number: u64,
    block_hash: B256,
    header_for_head: SealedHeader<Header>,
    executed: ExecutedBlock,
    hashed_state: HashedPostState,
}

/// In-process reth executor. See `src/lib.rs` module doc for the full design mapping
/// and what is deliberately out of scope.
pub struct RethExecutor {
    factory: ProviderFactory<RethTypes>,
    evm_config: EthEvmConfig,
    chain_spec: Arc<ChainSpec>,
    /// The real canonical parent header — genesis (reth block 0) on a fresh datadir, or the real
    /// last-persisted header read back from MDBX on `new()` when resuming an existing one (on start:
    /// open the datadir, read the canonical head). Every block this executor seals
    /// is built by re-executing from here, which advances by exactly one reth block number per
    /// `seal_block` call. Reth's own auto-incrementing header
    /// number (parent + 1, genesis = 0) **coincides** with the sequencer's own
    /// `BlockSealInputs::block` numbering (which `Head::block` echoes verbatim, matching
    /// `MockExecutor`'s convention) — the sequencer numbers its first sealed block 1, so reth block N
    /// is sequencer block N for every N ≥ 1; only reth block 0 (genesis) carries no sequencer block
    /// number at all — see this field's use in `seal_block` and [`Self::last_persisted_block`].
    canonical_parent: SealedHeader<Header>,
    /// The ONE `BlockchainProvider` this executor's own MDBX ever gets wrapped
    /// in — `RethExecutor::rpc_provider` hands out clones of THIS instance (a `BlockchainProvider`
    /// clones cheaply: `database: ProviderFactory` is an `Arc<DatabaseEnv>` clone, and
    /// `canonical_in_memory_state` is itself `Arc<CanonicalInMemoryStateInner>` — reth's
    /// `crates/chain-state/src/in_memory.rs`) so every clone shares the identical
    /// `CanonicalInMemoryState`. An earlier attempt failed because `node::serve` built its OWN
    /// `BlockchainProvider::new(factory)` from a bare `ProviderFactory` handed across — a fresh,
    /// independent `CanonicalInMemoryState` that this executor's own `set_canonical_head` calls (below,
    /// in `seal_block`'s persist completion) never touch. Constructing the shared instance HERE, once,
    /// and requiring every consumer to clone FROM it (never rebuild one from a bare factory), is what
    /// makes `eth_blockNumber`/`eth_getBlockByNumber("latest", ..)`/every other "latest"-tag read
    /// actually advance.
    blockchain_provider: BlockchainProvider<RethTypes>,
    /// The sequencer block number (`Head::block` numbering) durably reflected in this
    /// executor's own MDBX at the moment [`Self::new`] opened it — captured once, never mutated
    /// afterward (see [`rome_zk_executor_api::Executor::last_persisted_block`]'s doc: replay treats
    /// this as a fixed fact about startup, not something later `seal_block` calls should move).
    /// `None` on a fresh datadir (nothing beyond genesis persisted yet).
    persisted_at_open: Option<u64>,
    head: Head,
    pending: Option<PendingBlock>,
    /// (Root cause of a residual p99 latency: `open_block` used to `.await` the previous
    /// block's persist before building the new block's preview state — see this crate's module doc's
    /// "overlay/persist pipeline" section for the full mechanism.) Every block sealed but not yet
    /// CONFIRMED durable — whether its own write has not even been dispatched yet or is mid-flight —
    /// oldest first. `open_block` NEVER awaits anything to update this — entries are removed only
    /// once [`Self::reap_finished_write`]/[`Self::join_all_pending_persists`] confirms, via the write
    /// task's own result, that block (and every older one) is durable.
    not_yet_confirmed: VecDeque<NotYetConfirmedBlock>,
    /// How many of `not_yet_confirmed`'s FRONT-most entries are already covered by
    /// `write_in_flight` (0 when it is `None`). A new write, once dispatched, covers every entry from
    /// this index to the end (see [`Self::spawn_write_batch`]); once that write is confirmed, exactly
    /// this many entries are popped from the front and this resets to 0.
    dispatched_count: usize,
    /// The SINGLE background write task persisting a batch of one or more
    /// consecutive blocks, if any. Only ever one at a time: `seal_block` and `reap_finished_write`
    /// both check this is `None` before calling `spawn_write_batch` — there is never a second,
    /// concurrently-outstanding write.
    ///
    /// An earlier attempt instead made each new block's own `State` cumulative over every
    /// not-yet-persisted parent via `StateBuilder::with_bundle_prestate`, so that whichever single
    /// block ended up dispatched would carry correct values/roots on its own — but that meant its
    /// `BundleState` secretly carried OTHER blocks' revert batches too, and `BlockWriter::
    /// append_blocks_with_state` derives `StorageChangeSets`' own per-write block count from that
    /// same revert-batch count: a write declaring one block while its bundle spanned two silently
    /// wrote a phantom extra one (red before the real fix: "trying to append data to
    /// StorageChangeSets as block #3 but expected block #4"). `MemoryOverlayStateProviderRef` (see
    /// [`NotYetConfirmedBlock`]'s doc) gives the same value/root correctness without that coupling —
    /// each block's own bundle stays genuinely its own — so batching is simply "however many entries
    /// queued up while a write was busy" (see [`Self::spawn_write_batch`]), never a source of
    /// mismatched bookkeeping.
    write_in_flight: Option<tokio::task::JoinHandle<Result<(), ExecutorError>>>,
    /// Test hook: an artificial delay injected into every background write task,
    /// before it does any real MDBX work — proves `open_block` never waits on one, however slow.
    /// Zero (no delay) in production; never exposed via `RethConfig` or any other production knob —
    /// see the `#[cfg(test)]`-only setter.
    persist_delay_for_test: Duration,
}

impl RethExecutor {
    pub fn new(config: RethConfig) -> Result<Self, ExecutorError> {
        let genesis =
            genesis_from_path(&config.genesis_path).map_err(|e| backend(e.to_string()))?;
        let chain_spec = Arc::new(ChainSpec::from_genesis(genesis));
        // Every block this executor seals carries the profile's gas limit; a stock reth verifier
        // rejects a gas limit that moved from its parent by more than 1/1024, so block 1 can only be
        // valid if genesis already carries the same figure. Refuse before anything is opened.
        let genesis_gas_limit = chain_spec.genesis().gas_limit;
        if genesis_gas_limit != config.block_gas_limit {
            return Err(ExecutorError::GenesisGasLimitMismatch {
                genesis: genesis_gas_limit,
                configured: config.block_gas_limit,
            });
        }
        let factory = open_provider_factory(&config.datadir, chain_spec.clone())
            .map_err(|e| backend(e.to_string()))?;
        let evm_config = EthEvmConfig::new(chain_spec.clone());

        // Reth keeps headers/transactions/receipts in static files
        // (NippyJar) and indices/checkpoints in MDBX; `DatabaseProvider::commit` runs static `finalize()`
        // (data+offsets fsync, `.conf` tmp+rename+dir fsync) → RocksDB → MDBX `tx.commit()`. A kill
        // between the static fsync and the `.conf` rename leaves one header row durable that MDBX never
        // committed (the shape behind a past Tiber
        // devnet incident); NippyJar's cursor then reads the last configured row's
        // hash column up to the file's end and `sealed_header(n)` decodes to None — the old "canonical
        // head header missing" refusal. `static_heal` reads any dangling row BEFORE
        // `factory.check_consistency()` truncates it: a row MDBX did commit (a `.conf` lagging a committed
        // MDBX — not crash-producible, kept as defence in depth) is verified four ways and re-committed
        // forward; the crash shape's row is refused by name, reth truncates it, and the ordered log
        // replays the block. See `static_heal`'s module doc for the source citations.
        let forward_heal = static_heal::detect_and_verify_forward_heal(&factory)?;
        match &forward_heal {
            static_heal::ForwardHealOutcome::Consistent
            | static_heal::ForwardHealOutcome::Verified(_) => {}
            static_heal::ForwardHealOutcome::UnexpectedExtraRowCount { extra } => {
                tracing::warn!(
                    target: "rome_zk_executor_reth::open",
                    extra,
                    "Headers static file holds an unexpected number of rows beyond its committed \
                     config — the forward heal applies only to exactly one extra \
                     row; falling through to reth's own backward heal"
                );
            }
            static_heal::ForwardHealOutcome::Refused(reason) => {
                tracing::warn!(
                    target: "rome_zk_executor_reth::open",
                    %reason,
                    "Headers static file's one extra durable row failed forward-heal verification; \
                     falling through to reth's own backward heal"
                );
            }
        }

        // `ProviderFactory::check_consistency` is reth's own node-launch healer
        // (`reth_node_builder::LaunchContext::create_provider_factory` calls it unconditionally,
        // before ever reading a canonical head, in the exact same no-static-file-writer-held window
        // this constructor is in) — it returns `(rocksdb_unwind, static_file_unwind)`, each `Some(n)`
        // naming the block a storage layer must roll back to, and it self-heals the STATIC FILE side
        // of that divergence (`check_file_consistency`, called internally, first — the same pass that
        // would have destroyed the row `forward_heal` above already captured) so `sealed_header(n)`
        // below is reliable.
        //
        // KNOWN GAP (found while building the heal, not assumed from the design): trusting `n` here only fixes THIS
        // crate's OWN bookkeeping (`canonical_parent`/`head`/`persisted_at_open`) — it does not run reth's MDBX/RocksDB
        // table-level removal for blocks above `n` (account/storage state, changesets, the `StageId::Finish` checkpoint
        // itself all stay at the UNHEALED, too-high number; `ProviderFactory::check_consistency`'s own doc: it "may
        // result in writes to the static files", never to MDBX/RocksDB). Reth's own launcher performs that second half
        // with its full stage-unwind PIPELINE (`reth_stages::Pipeline` + `DefaultStages` + Noop consensus/downloaders,
        // `reth_node_builder::LaunchContext::create_provider_factory`) — async, heavier machinery this crate does not
        // yet wire in (a genuine follow-on, not a footnote). This gap only applies to the backward-heal path above
        // (`forward_heal` being anything other than `Verified`): a verified forward heal re-commits the exact row MDBX
        // already reflects, so there is no table-level removal to run in the first place.
        let (rocksdb_unwind, static_file_unwind) = factory
            .check_consistency()
            .map_err(|e| backend(e.to_string()))?;
        let healed_to = rocksdb_unwind.into_iter().chain(static_file_unwind).min();
        if let Some(n) = healed_to {
            tracing::info!(
                target: "rome_zk_executor_reth::open",
                healed_to = n,
                ?rocksdb_unwind,
                ?static_file_unwind,
                "reth's storage layers disagreed on the persisted head at open; trusting the lower, \
                 mutually consistent block — the ordered log replays the rest"
            );
        }

        // A verified forward heal re-commits the captured row onto
        // the Headers segment (now truncated back to `healed_to` by `check_consistency` above) and
        // requires the datadir be fully consistent again afterward — see `static_heal::
        // commit_forward_heal`'s own doc.
        let mut effective_head = healed_to;
        if let static_heal::ForwardHealOutcome::Verified(heal) = &forward_heal {
            static_heal::commit_forward_heal(&factory, heal)?;
            tracing::info!(
                target: "rome_zk_executor_reth::open",
                healed_forward_from = heal.block_number - 1,
                healed_forward_to = heal.block_number,
                hash = %heal.hash,
                "the Headers static file's lagging `.conf` was healed FORWARD onto the row reth's \
                 own check_consistency would otherwise have discarded — no block lost, no replay \
                 needed for it"
            );
            effective_head = Some(heal.block_number);
        }

        // On start: open the datadir, read the canonical head. `best_block_number`
        // (not `last_block_number`, which only tracks the static-file frontier — never advanced by
        // this crate's `append_blocks_with_state` writes below) reads the `StageId::Finish` MDBX
        // checkpoint, which `append_blocks_with_state` DOES advance
        // (reth_provider::providers::database::provider::DatabaseProvider::append_blocks_with_state,
        // "Update pipeline progress" / `update_pipeline_stages`) — so this is the real persisted
        // canonical head across a restart, not just genesis; `effective_head` above overrides it
        // exactly when `check_consistency` found the two storage layers disagree (or, having been
        // healed forward, agree at one block higher than `check_consistency` alone would trust).
        let provider = factory.provider().map_err(|e| backend(e.to_string()))?;
        let best_block_number = provider
            .best_block_number()
            .map_err(|e| backend(e.to_string()))?;
        // Neither storage layer confirms a head above
        // MDBX's own checkpoint — a healed value beyond it would be trusting a number nothing durable
        // backs. Named refusal, not a panic: this never fires on any path above (`check_consistency`'s
        // own targets are bounded by the checkpoint it read; a verified forward heal re-commits
        // exactly the row that checkpoint already reflects), so a failure here is this constructor's
        // own bug, not an operator-facing datadir shape.
        assert_healed_to_within_best(effective_head, best_block_number)?;
        let reth_head_number = effective_head.unwrap_or(best_block_number);
        let canonical_parent = provider
            .sealed_header(reth_head_number)
            .map_err(|e| backend(e.to_string()))?
            .ok_or_else(|| {
                backend(format!(
                    "canonical head header missing for persisted reth block {reth_head_number}"
                ))
            })?;
        drop(provider);

        // Reth block 0 is genesis and carries no sequencer block
        // number; reth block N (N >= 1) IS sequencer block N — the sequencer numbers its first
        // sealed block 1, coinciding at the source with reth's own auto-incrementing header number
        // (this crate's `seal_block` advances `canonical_parent` by exactly one reth block per
        // sequencer block sealed — see `chain.rs`'s genesis-is-reth-block-0 setup).
        let persisted_at_open = (reth_head_number > 0).then_some(reth_head_number);

        let head = Head {
            block: persisted_at_open.unwrap_or(0),
            sub_block_index: 0,
            state_root: canonical_parent.state_root(),
            block_hash: canonical_parent.hash(),
        };

        // The ONE `BlockchainProvider` (see this field's doc) —
        // `with_latest` (not `BlockchainProvider::new`) since `canonical_parent` above is already the
        // real persisted head read off MDBX; this skips a second, redundant `provider()`/
        // `header_by_number` round trip `new` would otherwise do to rediscover the exact same header.
        let blockchain_provider =
            BlockchainProvider::with_latest(factory.clone(), canonical_parent.clone())
                .map_err(|e| backend(e.to_string()))?;

        Ok(Self {
            factory,
            evm_config,
            chain_spec,
            canonical_parent,
            blockchain_provider,
            persisted_at_open,
            head,
            pending: None,
            not_yet_confirmed: VecDeque::new(),
            dispatched_count: 0,
            write_in_flight: None,
            persist_delay_for_test: Duration::ZERO,
        })
    }

    /// Test hook: see [`Self::persist_delay_for_test`]'s doc. Never called outside
    /// this crate's own tests.
    #[cfg(test)]
    fn set_persist_delay_for_test(&mut self, delay: Duration) {
        self.persist_delay_for_test = delay;
    }

    /// `factory.latest()` (real, on-disk state) OVERLAID, via reth's own
    /// `MemoryOverlayStateProviderRef` (the SAME machinery its in-memory chain / pending-block RPC
    /// support uses), with every not-yet-confirmed block's own execution outcome — newest first, per
    /// that type's own contract (see [`NotYetConfirmedBlock`]'s doc). Gives both correct
    /// account/storage reads AND, when handed to `BlockBuilder::finish` as its `root_state_provider`
    /// argument, a correct state root for a block built on top of parents that have not yet landed on
    /// disk. Falls back to `factory.latest()` alone when nothing is outstanding.
    ///
    /// Used ONLY by [`Self::seal_block`]'s short-lived, entirely synchronous re-execution pass (built
    /// and consumed with no `.await` in between — see that method) — never for
    /// [`Self::open_block`]'s long-lived preview `State` (held in `RethExecutor::pending` across many
    /// `execute_sub_block` calls), because `MemoryOverlayStateProviderRef` is not `Send` (one of its
    /// fields is a bare `Box<dyn StateProvider + 'a>`, no `+Send` bound) and `RethExecutor` — like
    /// every `Executor` — must be. See [`Self::preview_overlay_cache`] for `open_block`'s own,
    /// `Send`-safe equivalent.
    fn overlay_state_provider(&self) -> Result<Box<dyn StateProvider>, ExecutorError> {
        let historical: StateProviderBox =
            self.factory.latest().map_err(|e| backend(e.to_string()))?;
        if self.not_yet_confirmed.is_empty() {
            return Ok(historical);
        }
        let in_memory: Vec<ExecutedBlock> = self
            .not_yet_confirmed
            .iter()
            .rev()
            .map(|b| b.executed.clone())
            .collect();
        Ok(MemoryOverlayStateProviderRef::new(historical, in_memory).boxed())
    }

    /// The `Send`-safe counterpart to [`Self::overlay_state_provider`], for
    /// [`Self::open_block`]'s long-lived preview `State` specifically. Folds every not-yet-confirmed
    /// block's own per-block `BundleState` into ONE combined `BundleState` (`BundleState::extend`,
    /// oldest first — a proper per-account, per-storage-slot merge, not a wholesale replace, so an
    /// account touched by different slots in different not-yet-confirmed blocks is combined
    /// correctly), then converts ONLY its `state`/`contracts` — never `reverts`, which `CacheState`
    /// has no field for and does not need, since this is a read-only VALUE overlay seeded into the
    /// preview `State`'s CACHE via `StateBuilder::with_cached_prestate`, never its `bundle_state`.
    /// Preview execution's own accounting is discarded wholesale at `seal_block` regardless (module
    /// doc: "thrown away at seal_block"), so there is nothing here for a later block's bundle to
    /// accidentally inherit.
    fn preview_overlay_cache(&self) -> Option<CacheState> {
        let mut iter = self.not_yet_confirmed.iter();
        let mut combined = iter.next()?.executed.execution_outcome().state.clone();
        for entry in iter {
            combined.extend(entry.executed.execution_outcome().state.clone());
        }
        let mut cache = CacheState::new();
        for (address, bundle_account) in &combined.state {
            cache.accounts.insert(*address, bundle_account.into());
        }
        cache.contracts = combined.contracts;
        Some(cache)
    }

    /// `addr`'s nonce from the not-yet-confirmed overlay alone (the caller supplies
    /// the on-disk fallback) — newest block first, so a later block's view wins over an earlier
    /// one's for the same address. `None` if no not-yet-confirmed block ever touched it.
    fn overlay_nonce(&self, addr: Address) -> Option<u64> {
        for entry in self.not_yet_confirmed.iter().rev() {
            if let Some(account) = entry.executed.execution_outcome().account(&addr) {
                return Some(account.map(|a| a.nonce).unwrap_or(0));
            }
        }
        None
    }

    /// Checks whether the single in-flight write has finished — NEVER awaits one
    /// still running (that is the whole point of the design). If it has, joins it (immediate,
    /// since finished), pops the `dispatched_count` now-confirmed entries off the front of
    /// `not_yet_confirmed`, and — if more entries queued up while it ran — immediately dispatches the
    /// next batch. Called from [`Self::open_block`], once per block, which is why checking (not
    /// waiting) here is enough: a write that finishes between one `open_block` call and the next is
    /// reaped on that next call, at the latest.
    async fn reap_finished_write(&mut self) -> Result<(), ExecutorError> {
        let Some(handle) = self.write_in_flight.as_ref() else {
            return Ok(());
        };
        if !handle.is_finished() {
            return Ok(());
        }
        let handle = self.write_in_flight.take().expect("checked Some above");
        handle
            .await
            .map_err(|e| backend(format!("write task panicked or was cancelled: {e}")))??;
        for _ in 0..self.dispatched_count {
            self.not_yet_confirmed.pop_front();
        }
        self.dispatched_count = 0;
        self.spawn_write_batch();
        Ok(())
    }

    /// A graceful shutdown's last chance to make everything durable — repeatedly
    /// joins whatever write is in flight and dispatches the next queued batch, until
    /// `not_yet_confirmed` is empty. A hard kill skips this entirely, which is fine by design: tail
    /// replay off the ordered log repairs whatever the background writes didn't finish (see
    /// `rome_zk_sequencer::recovery::replay_into_executor`).
    async fn join_all_pending_persists(&mut self) -> Result<(), ExecutorError> {
        loop {
            if let Some(handle) = self.write_in_flight.take() {
                handle
                    .await
                    .map_err(|e| backend(format!("write task panicked or was cancelled: {e}")))??;
                for _ in 0..self.dispatched_count {
                    self.not_yet_confirmed.pop_front();
                }
                self.dispatched_count = 0;
            }
            if self.not_yet_confirmed.is_empty() {
                return Ok(());
            }
            self.spawn_write_batch();
        }
    }

    /// Dispatches ONE `BlockWriter::append_blocks_with_state` write, in the
    /// background, covering every `not_yet_confirmed` entry from `dispatched_count` to the end (i.e.
    /// every block sealed since the last write was dispatched) — combining their individually
    /// per-block `BundleState`s (`BundleState::extend`) and hashed diffs (`HashedPostState::extend`,
    /// sorted once here) into the ONE bundle/hashed-state the call needs. Because each entry's own
    /// bundle is genuinely just its own block's diff (see [`NotYetConfirmedBlock`]'s doc — never
    /// cumulative), combining any N of them always yields exactly N revert batches, matching the N
    /// blocks/receipts this same call declares — the mismatch the first attempt hit
    /// (see [`Self::write_in_flight`]'s doc) is structurally impossible here.
    ///
    /// No-op if there is nothing new to dispatch. Never called while `write_in_flight` is already
    /// `Some` — both call sites ([`Self::seal_block`], [`Self::reap_finished_write`]/
    /// [`Self::join_all_pending_persists`]) check that first (or have just cleared it).
    fn spawn_write_batch(&mut self) {
        debug_assert!(
            self.write_in_flight.is_none(),
            "spawn_write_batch called while a write is already in flight"
        );
        let undispatched = &self.not_yet_confirmed.make_contiguous()[self.dispatched_count..];
        if undispatched.is_empty() {
            return;
        }
        let batch_len = undispatched.len();
        let first_block = undispatched[0].block_number;
        let last = undispatched.last().expect("checked non-empty above");
        let last_block_number = last.block_number;
        let last_block_hash = last.block_hash;
        let header_for_head = last.header_for_head.clone();

        let mut bundle = undispatched[0].executed.execution_outcome().state.clone();
        let mut hashed_state = undispatched[0].hashed_state.clone();
        let mut blocks = Vec::with_capacity(batch_len);
        let mut receipts = Vec::with_capacity(batch_len);
        let mut requests = Vec::with_capacity(batch_len);
        for (i, entry) in undispatched.iter().enumerate() {
            if i > 0 {
                bundle.extend(entry.executed.execution_outcome().state.clone());
                hashed_state.extend(entry.hashed_state.clone());
            }
            blocks.push(entry.executed.recovered_block().clone());
            receipts.push(entry.executed.execution_outcome().result.receipts.clone());
            requests.push(entry.executed.execution_outcome().result.requests.clone());
        }
        let hashed_state = hashed_state.into_sorted();
        self.dispatched_count = self.not_yet_confirmed.len();

        let factory = self.factory.clone();
        // Cloned in (cheap — see this crate's `blockchain_provider` field
        // doc) so the SAME `CanonicalInMemoryState` `RethExecutor::rpc_provider` hands out clones of
        // gets its canonical/safe/finalized head advanced right here, once this batch's write is
        // verified durable — a reader can never observe `canonical_in_memory_state` ahead of what is
        // actually durable on disk.
        let blockchain_provider = self.blockchain_provider.clone();
        let persist_delay_for_test = self.persist_delay_for_test;

        self.write_in_flight = Some(tokio::task::spawn_blocking(
            move || -> Result<(), ExecutorError> {
                // Test hook — see `persist_delay_for_test`'s doc. Zero in production.
                if !persist_delay_for_test.is_zero() {
                    std::thread::sleep(persist_delay_for_test);
                }

                let persist_started = Instant::now();
                let execution_outcome = ExecutionOutcome {
                    bundle,
                    receipts,
                    first_block,
                    requests,
                };
                let provider_rw = factory.provider_rw().map_err(|e| backend(e.to_string()))?;
                provider_rw
                    .append_blocks_with_state(blocks, &execution_outcome, hashed_state)
                    .map_err(|e| backend(e.to_string()))?;
                provider_rw.commit().map_err(|e| backend(e.to_string()))?;

                // Assert after each seal that the node's canonical head hash ==
                // BlockOutcome.block_hash (fatal otherwise) — read the just-committed head back off
                // MDBX independently (not the header this closure was handed) so this is a real check of
                // what got persisted, not a tautology against a value already assumed correct. Checked
                // against the BATCH's last block — the highest one this call claims to have written.
                let post_commit_provider =
                    factory.provider().map_err(|e| backend(e.to_string()))?;
                let persisted_number = post_commit_provider
                    .best_block_number()
                    .map_err(|e| backend(e.to_string()))?;
                let persisted_hash = post_commit_provider
                    .sealed_header(persisted_number)
                    .map_err(|e| backend(e.to_string()))?
                    .ok_or_else(|| {
                        backend("no header at the just-committed canonical head".into())
                    })?
                    .hash();
                drop(post_commit_provider);
                if persisted_number != last_block_number || persisted_hash != last_block_hash {
                    return Err(backend(format!(
                        "canonical head mismatch after commit: expected block {last_block_number} \
                     hash {last_block_hash:#x}, MDBX reports block {persisted_number} hash \
                     {persisted_hash:#x}"
                    )));
                }
                // Advance the shared `CanonicalInMemoryState` now that the batch
                // is verified durable on disk (the consistency check above already passed) — this is
                // what makes `eth_blockNumber`/`eth_getBlockByNumber("latest", ..)`/every other
                // "latest"-tag read on `RethExecutor::rpc_provider`'s clones actually move. `set_safe`/
                // `set_finalized` both track the same head for now (single sequencer, no reorgs — the
                // settlement watcher (`rome-zk-settlement-watcher`) is the intended source for "finalized" but is not
                // wired in here yet; see this crate's module doc).
                let canonical_in_memory_state = blockchain_provider.canonical_in_memory_state();
                canonical_in_memory_state.set_canonical_head(header_for_head.clone());
                canonical_in_memory_state.set_safe(header_for_head.clone());
                canonical_in_memory_state.set_finalized(header_for_head);

                let persist_elapsed = persist_started.elapsed();
                tracing::debug!(
                    target: "rome_zk_executor_reth::seal_block",
                    first_block,
                    last_block = last_block_number,
                    batch_len,
                    persist_ms = persist_elapsed.as_secs_f64() * 1000.0,
                    "batched persist phase timings"
                );
                Ok(())
            },
        ));
    }

    /// `NextBlockEnvAttributes` from the sealer's published `BlockEnv` —
    /// exactly the env every sub-block of this block, and the block's own `seal_block`, run under.
    /// Prague-valid (parent_beacon_block_root + withdrawals populated — this chain is Prague-at-0, see
    /// the devnet genesis template).
    ///
    /// Every rule-fixed field (`prev_randao`, `beneficiary`,
    /// `parent_beacon_block_root`, `extra_data`) is read from the ONE shared
    /// [`rome_zk_executor_api::canonical_header_rule`] — never an independently-pinned literal — so this
    /// executor, `rome-zk-derive`'s `PayloadAttributes` construction, and the guest's per-block
    /// assertion can never silently drift apart on what the rule actually says. `withdrawals: Some(empty)`
    /// is reth's own input for computing the withdrawals-trie root; the guest asserts the RESULT equals
    /// `rule.withdrawals_root` directly rather than this executor passing a root in (reth computes that
    /// root itself from the withdrawals list, it does not accept one).
    ///
    /// The block's own withdrawals (the deposits it credits, empty for a block without any) go to reth unchanged, and
    /// reth credits them after the block's transactions. The rule is the one that carries them
    /// ([`canonical_header_rule_with_withdrawals`]); with an empty list it is exactly the rule every block had before.
    fn attrs_from_env(env: &BlockEnv, chain_id: u64) -> NextBlockEnvAttributes {
        let rule = Self::rule_from_env(env, chain_id);
        NextBlockEnvAttributes {
            timestamp: env.timestamp_secs,
            suggested_fee_recipient: rule.beneficiary,
            prev_randao: rule.prev_randao,
            gas_limit: env.gas_limit,
            parent_beacon_block_root: Some(rule.parent_beacon_block_root),
            withdrawals: Some(Withdrawals::new(env.withdrawals.clone())),
            extra_data: rule.extra_data,
            slot_number: None,
        }
    }

    /// The shared header rule for the block this env opens, carrying the env's withdrawals.
    fn rule_from_env(env: &BlockEnv, chain_id: u64) -> rome_zk_executor_api::HeaderRule {
        canonical_header_rule_with_withdrawals(chain_id, env.number, env.coinbase, &env.withdrawals)
    }
}

fn backend(msg: String) -> ExecutorError {
    ExecutorError::Backend(msg)
}

/// `healed_to <= best_block_number()` in every path — a healed head
/// above MDBX's own checkpoint is trusting a number neither storage layer's own commit backs.
/// Extracted as a pure function so a mutation test can force the violation directly (see
/// `executor.rs`'s own test module) rather than having to fabricate a real torn datadir shape that
/// triggers it — it never does, by construction, on any path `RethExecutor::new` takes.
fn assert_healed_to_within_best(
    healed_to: Option<u64>,
    best_block_number: u64,
) -> Result<(), ExecutorError> {
    if let Some(n) = healed_to {
        if n > best_block_number {
            return Err(backend(format!(
                "healed_to {n} exceeds MDBX's own best_block_number {best_block_number} — refusing \
                 to trust a head neither storage layer confirms"
            )));
        }
    }
    Ok(())
}

fn decode_and_recover(raw: &Bytes) -> Result<Recovered<TransactionSigned>, String> {
    let mut slice = raw.as_ref();
    let tx = TransactionSigned::decode_2718(&mut slice).map_err(|e| e.to_string())?;
    let signer = tx.recover_signer().map_err(|e| e.to_string())?;
    Ok(Recovered::new_unchecked(tx, signer))
}

/// Ordered trie root of one sub-block's own receipts (not cumulative across
/// sub-blocks — each sub-block is rooted like a tiny standalone block would be). Each receipt is
/// encoded EIP-2718-typed (type-prefix byte + RLP body for anything but a legacy tx) — the same
/// shape a real block's receipts trie uses — via `ReceiptWithBloom`'s `Encodable2718` impl, not
/// bare RLP (`Receipt` alone has no type-prefix byte to key the trie's typed-tx leaves on).
fn receipts_root(receipts: &[Receipt]) -> B256 {
    use alloy_consensus::proofs::calculate_receipt_root;
    use alloy_consensus::ReceiptWithBloom;
    let with_bloom: Vec<ReceiptWithBloom<&Receipt>> =
        receipts.iter().map(ReceiptWithBloom::from).collect();
    calculate_receipt_root(&with_bloom)
}

/// Map a revm execution-time failure to a typed `Reason`: admission
/// needs to act on WHICH rejection this is (park a future nonce, reset a stale one, otherwise just
/// report it), not parse a Debug string to find out. Bounding `E::InvalidTransaction` to revm's own
/// concrete enum (rather than the generic `InvalidTxError` trait) lets this match its variants
/// directly, with no downcasting.
fn classify_rejection<E>(err: E) -> Reason
where
    E: alloy_evm::EvmError<
        InvalidTransaction = revm::context_interface::result::InvalidTransaction,
    >,
{
    use revm::context_interface::result::InvalidTransaction;

    match err.as_invalid_tx_err() {
        Some(InvalidTransaction::NonceTooLow { state, .. }) => {
            Reason::NonceTooLow { expected: *state }
        }
        Some(InvalidTransaction::NonceTooHigh { state, .. }) => {
            Reason::NonceTooHigh { expected: *state }
        }
        Some(InvalidTransaction::LackOfFundForMaxFee { .. }) => Reason::InsufficientFunds,
        Some(
            InvalidTransaction::CallGasCostMoreThanGasLimit { .. }
            | InvalidTransaction::GasFloorMoreThanGasLimit { .. },
        ) => Reason::IntrinsicGas,
        Some(
            InvalidTransaction::CallerGasLimitMoreThanBlock
            | InvalidTransaction::TxGasLimitGreaterThanCap { .. },
        ) => Reason::GasLimitExceeded,
        _ => Reason::Other(format!("{err:?}")),
    }
}

impl ZkExecutor for RethExecutor {
    /// Commits this block's environment, before its first sub-block
    /// executes. Builds the pending preview `State` from that env's `NextBlockEnvAttributes` — the
    /// SAME attrs every sub-block of this block, and `seal_block` itself, run under; no placeholder,
    /// no override at seal time.
    async fn open_block(&mut self, env: BlockEnv) -> Result<(), ExecutorError> {
        if self.pending.is_some() {
            return Err(backend(
                "open_block called with a block already open".into(),
            ));
        }
        // The premise "BlockEnv.number == the resulting header.number" used to be arranged by callers, never enforced
        // here — this is the source-of-truth boundary where a numbering drift (a caller replaying an
        // old 0-based log, or any bug upstream) must become unconstructable rather than silently
        // producing a header whose real height disagrees with what the sequencer/derive/batcher think
        // this block is. `canonical_parent.number()` is the real, on-disk reth height; the next block
        // this executor can ever seal is exactly one past it.
        let expected = self.canonical_parent.number() + 1;
        if env.number != expected {
            return Err(ExecutorError::EnvMismatch(format!(
                "open_block: BlockEnv.number {} but the next real reth height is {expected}",
                env.number
            )));
        }
        // open_block must NEVER wait for a persist. It used to
        // `join_pending_persist().await` here — the previous block's full MDBX-commit latency
        // (~100ms measured), paid by the FIRST sub-block of every block, was the real cause of the
        // p99 tail. Reap only what has ALREADY finished (never blocks); anything still running (or not
        // yet even dispatched) stays layered into this block's overlay below instead of waited on.
        self.reap_finished_write().await?;
        // Build this block's preview `State` over `factory.latest()` (real, on-disk
        // state — every block whose write has already landed) LAYERED with a read-only, `Send`-safe
        // VALUE overlay of every block that has NOT (normally at most one — see
        // `not_yet_confirmed`'s doc) — see `preview_overlay_cache`'s doc for why this uses a `CacheState`
        // (via `with_cached_prestate`), not `MemoryOverlayStateProviderRef` (that one is for
        // `seal_block`'s own, short-lived re-execution only — see `overlay_state_provider`'s doc).
        // This is what makes `open_block` correct without ever waiting: sub-block N+1 executes while the
        // sealer finalizes N-1.
        let state_provider = self.factory.latest().map_err(|e| backend(e.to_string()))?;
        let mut state_builder = State::builder()
            .with_database(StateProviderDatabase::new(state_provider))
            .with_bundle_update();
        if let Some(cache) = self.preview_overlay_cache() {
            state_builder = state_builder.with_cached_prestate(cache);
        }
        let state = state_builder.build();
        self.pending = Some(PendingBlock {
            state,
            included: Vec::new(),
            attrs: Self::attrs_from_env(&env, self.chain_spec.chain.id()),
            withdrawals_root: Self::rule_from_env(&env, self.chain_spec.chain.id())
                .withdrawals_root,
        });
        Ok(())
    }

    async fn execute_sub_block(
        &mut self,
        txs: &[Bytes],
        limits: SubBlockLimits,
    ) -> Result<SubBlockOutcome, ExecutorError> {
        // A block must be opened (via `open_block`) before its first
        // sub-block executes — no more lazily creating one under a placeholder env.
        if self.pending.is_none() {
            return Err(backend(
                "execute_sub_block called with no block open — call open_block first".into(),
            ));
        }
        // Disjoint field borrows (not one long-lived `&mut self` through `pending`) — `evm_config`
        // and `canonical_parent` are read while `pending` is mutated in the same loop.
        let Self {
            evm_config,
            canonical_parent,
            pending,
            ..
        } = self;
        let pending = pending.as_mut().expect("checked is_some above");

        let evm_env = evm_config
            .next_evm_env(canonical_parent, &pending.attrs)
            .map_err(|e| backend(e.to_string()))?;
        let mut evm = evm_config.evm_with_env(&mut pending.state, evm_env);

        let mut included = Vec::new();
        let mut rejected = Vec::new();
        let mut receipts: Vec<Receipt> = Vec::new();
        let mut gas_used = 0u64;
        let mut not_executed: Vec<Bytes> = Vec::new();

        for (i, raw) in txs.iter().enumerate() {
            let recovered = match decode_and_recover(raw) {
                Ok(r) => r,
                Err(e) => return Err(ExecutorError::Malformed(e)),
            };
            let declared_gas_limit = recovered.gas_limit();
            if Instant::now() >= limits.deadline || gas_used + declared_gas_limit > limits.gas_limit
            {
                not_executed = txs[i..].to_vec();
                break;
            }

            let tx_hash: TxHash = *recovered.tx_hash();
            let sender = recovered.signer();
            let tx_type = recovered.tx_type();
            let tx_env = evm_config.tx_env(recovered.clone());

            match evm.transact_commit(tx_env) {
                Ok(result) => {
                    gas_used += result.tx_gas_used();
                    included.push(tx_hash);
                    receipts.push(Receipt {
                        tx_type,
                        success: result.is_success(),
                        cumulative_gas_used: gas_used,
                        logs: result.into_logs(),
                    });
                    pending.included.push(recovered.clone());
                }
                Err(err) => {
                    rejected.push(RejectedTx {
                        tx_hash,
                        sender,
                        reason: classify_rejection(err),
                    });
                }
            }
        }
        drop(evm);

        Ok(SubBlockOutcome {
            included,
            rejected,
            receipts_root: receipts_root(&receipts),
            gas_used,
            not_executed,
        })
    }

    async fn seal_block(&mut self, inputs: BlockSealInputs) -> Result<BlockOutcome, ExecutorError> {
        // Brackets this whole function — see the `foreground_ms` tracing line
        // near the end.
        let seal_block_started = Instant::now();
        let pending = self
            .pending
            .take()
            .ok_or_else(|| backend("seal_block called with no open block".into()))?;

        // `inputs.timestamp_secs` is the sealer's OWN re-derivation of
        // the exact same block timestamp it resolved (and published via `open_block`) at this block's
        // index 0 — `resolve_block_timestamp_secs`'s inputs never change mid-block, so the two
        // computations are guaranteed to agree (see `sealer::seal_sub_block`'s comment). If they ever
        // diverge, that is a genuine integrity failure in the caller, not something to paper over by
        // silently overriding the env this block was actually opened and previewed under.
        if pending.attrs.timestamp != inputs.timestamp_secs {
            return Err(ExecutorError::EnvMismatch(format!(
                "block {}: opened with timestamp {} but seal_block was given {}",
                inputs.block, pending.attrs.timestamp, inputs.timestamp_secs
            )));
        }
        let attrs = pending.attrs;
        let expected_withdrawals_root = pending.withdrawals_root;

        // The sanctioned re-execution at seal time (module doc): a fresh preview-shaped State over
        // the same canonical parent (now read straight off MDBX — see `open_block`'s comment),
        // executed once more through reth's own block-building path so the result is a real,
        // assembled, rooted `RecoveredBlock` — not reimplemented. This fresh State's
        // database is `overlay_state_provider()` — reth's own `MemoryOverlayStateProviderRef`,
        // layering every not-yet-CONFIRMED block's own execution outcome over `factory.latest()` —
        // rather than `factory.latest()` alone, which would silently run this re-execution against
        // stale pre-overlay state whenever a prior block's write has not yet landed (the exact bug
        // a test caught: a block N+1 whose sender nonce only advanced via block N's
        // overlay would fail seal-time re-execution with a spurious nonce mismatch). Safe to use here
        // (unlike in `open_block`, see `overlay_state_provider`'s doc) because everything from here
        // through `finish()` below is synchronous, with no `.await` in between.
        let mut state = State::builder()
            .with_database(StateProviderDatabase::new(self.overlay_state_provider()?))
            .with_bundle_update()
            .build();

        // `pending.included` is already `Recovered<TransactionSigned>` — computed
        // once, by `execute_sub_block`'s own `decode_and_recover`, when each tx was first previewed.
        // Re-decoding/re-recovering the same senders here would be pure redundant work (measured:
        // ~113ms for 5,000 txs, i.e. as expensive as this crate's whole re-execution pass) for a
        // result identical to what preview execution already computed and, crucially, is NOT ordered
        // by the deadline/gas cutoff those sub-blocks applied — `pending.included` is exactly the
        // forced list to re-execute, in order, with nothing left to redo.
        let recovered_txs = pending.included;

        let mut builder = self
            .evm_config
            .builder_for_next_block(&mut state, &self.canonical_parent, attrs)
            .map_err(|e| backend(e.to_string()))?;
        builder
            .apply_pre_execution_changes()
            .map_err(|e| backend(e.to_string()))?;
        // Timer 1 of 3 — the re-execution loop alone (excludes state-root, excludes
        // persistence — see the two timers below).
        let reexecution_started = Instant::now();
        for tx in &recovered_txs {
            builder
                .execute_transaction(tx.clone())
                .map_err(|e| backend(e.to_string()))?;
        }
        let reexecution_elapsed = reexecution_started.elapsed();

        // Timer 2 of 3 — `BlockBuilder::finish` (state-root computation; see this
        // crate's module doc for what trie state it does/doesn't have persisted to accelerate it).
        // A FRESH `overlay_state_provider()` call (not a clone/reuse of the one that
        // built `state`'s own database above — `MemoryOverlayStateProviderRef` is not `Clone`, and a
        // second, independent MDBX read snapshot here is cheap) — `finish()`'s own internal
        // `state_root_with_updates` call reads THROUGH this provider, so it correctly aggregates
        // every not-yet-confirmed block's own trie data with this block's own (per-block-only) hashed
        // diff, giving a CORRECT root even though `state.bundle_state`/`take_bundle()` below is never
        // seeded/cumulative (see `NotYetConfirmedBlock`'s doc).
        let state_root_started = Instant::now();
        let root_state_provider = self.overlay_state_provider()?;
        let outcome = builder
            .finish(root_state_provider, None)
            .map_err(|e| backend(e.to_string()))?;
        let state_root_elapsed = state_root_started.elapsed();

        let header = outcome.block.sealed_header().clone();
        let state_root = header.state_root();
        let block_hash = header.hash();
        let receipts_root = header.receipts_root();
        let block_number = header.number();
        // The header reth built must carry the withdrawals root the shared rule fixes for this block: the same root
        // derive and the guest compute from the same list, so a block the executor seals can never disagree with
        // them about it.
        if header.withdrawals_root() != Some(expected_withdrawals_root) {
            return Err(ExecutorError::EnvMismatch(format!(
                "seal_block: block {block_number} header withdrawals_root {:?} but the header rule says {expected_withdrawals_root:#x}",
                header.withdrawals_root()
            )));
        }
        // The block this executor just
        // built (from the env `open_block` committed) must carry the exact number the caller says it
        // sealed (`BlockSealInputs::block`) — `open_block`'s own check above already ties `env.number`
        // to the real reth height, but binding it again here, at the point the number is actually
        // committed to the log/prev_randao boundary, is what makes that equality unconstructable
        // rather than merely arranged by two callers agreeing with each other.
        if block_number != inputs.block {
            return Err(ExecutorError::EnvMismatch(format!(
                "seal_block: sealed header number {block_number} but BlockSealInputs.block was {}",
                inputs.block
            )));
        }

        tracing::debug!(
            target: "rome_zk_executor_reth::seal_block",
            block = block_number,
            reexecution_ms = reexecution_elapsed.as_secs_f64() * 1000.0,
            state_root_ms = state_root_elapsed.as_secs_f64() * 1000.0,
            "seal_block phase timings"
        );

        // The MDBX write moves OFF this function's return
        // path. `seal_block` returns `BlockOutcome` as soon as the header above is known (the actor's
        // tick is unblocked immediately — `seal_block`'s own foreground cost is just re-execution +
        // state-root, ~3-7ms measured at the 5k-tx design load point, not the ~110-260ms this same
        // call took with the write inline). An earlier version joined the write exactly once, at the
        // START of the NEXT block's `open_block` — which turned out to be the ACTUAL cause of the
        // residual p99 (paying the previous block's ~100ms commit latency on the first sub-block of
        // every block). Now `open_block` never awaits it at all — it only reaps a write once
        // finished (see `reap_finished_write`) and dispatches the next queued batch, if any; a crash
        // at any point before a write completes is safe BY DESIGN: the ordered log is the durable
        // source of truth and `recovery::replay_into_executor`'s tail-only replay (driven by
        // `last_persisted_block`) re-derives whatever the database is missing.
        //
        // `BlockBuilder::finish` already merged this block's transitions into `state`'s own bundle
        // (see `crates/evm/evm/src/execute.rs`'s `BasicBlockBuilder::finish`, "merge all transitions
        // into bundle state" — called on the SAME `State` this function holds by value). Since
        // `state` was built PLAIN above (no prestate — value/root correctness for not-yet-confirmed
        // parents is `overlay_state_provider`'s job now, not a cumulative bundle's, see
        // `NotYetConfirmedBlock`'s doc), `take_bundle()` returns exactly THIS block's own diff —
        // never any earlier not-yet-confirmed block's.
        let bundle = state.take_bundle();
        // Kept in UNSORTED form alongside the `ExecutedBlock`'s own (pre-sorted, for the overlay)
        // copy below — `spawn_write_batch` combines several such diffs via `HashedPostState::extend`
        // when it batches more than one block into one write, then sorts once, at write time.
        let hashed_state_for_write_batching = outcome.hashed_state.clone();
        let executed = ExecutedBlock::new(
            Arc::new(outcome.block),
            Arc::new(BlockExecutionOutput {
                result: outcome.execution_result,
                state: bundle.clone(),
            }),
            ComputedTrieData::new(
                Arc::new(outcome.hashed_state.into_sorted()),
                Arc::new(outcome.trie_updates.into_sorted()),
            ),
        );
        self.not_yet_confirmed.push_back(NotYetConfirmedBlock {
            block_number,
            block_hash,
            header_for_head: header.clone(),
            executed,
            hashed_state: hashed_state_for_write_batching,
        });
        // Dispatch a write immediately if none is currently in flight — the common
        // case, since a full block period (~1s) ordinarily lets the previous write land well before
        // this point. If one IS still running, this block's data simply stays queued;
        // `reap_finished_write` picks up the growing backlog, batched, the moment that write
        // finishes.
        if self.write_in_flight.is_none() {
            self.spawn_write_batch();
        }

        self.canonical_parent = header;
        self.head = Head {
            block: inputs.block,
            sub_block_index: 0,
            state_root,
            block_hash,
        };
        // The number that actually matters — how long `seal_block` itself
        // blocks the actor's tick, now that persistence is backgrounded (compare against the ~110-260ms
        // this used to take with the write inline; budget is the 50ms sub-block cadence).
        tracing::debug!(
            target: "rome_zk_executor_reth::seal_block",
            block = block_number,
            foreground_ms = seal_block_started.elapsed().as_secs_f64() * 1000.0,
            "seal_block foreground (blocking) latency"
        );
        Ok(BlockOutcome {
            state_root,
            block_hash,
            receipts_root,
        })
    }

    fn head(&self) -> Head {
        self.head
    }

    /// Reads from the pending block's own execution state first
    /// (`pending.state.cache` — every account this block's sub-blocks have touched so far, revm's own
    /// cache, no side bookkeeping to keep in sync), falling back to the canonical provider (the last
    /// sealed block) for an account this block hasn't touched yet. This is correct for a
    /// contract-creating tx's sender (its nonce lives in the SAME cache entry `CREATE` reads to derive
    /// the new contract's address — there is no separate path to fall out of sync) and for replay
    /// (which drives this same method against a freshly rebuilt `RethExecutor`, never a stale map
    /// left over from a previous run). A missing/never-touched account is nonce 0 (standard EVM
    /// semantics), matching every existing caller's expectation (admission's nonce-cache seed on
    /// first sight of a sender).
    fn nonce(&self, addr: Address) -> u64 {
        if let Some(pending) = &self.pending {
            if let Some(cache_account) = pending.state.cache.accounts.get(&addr) {
                return cache_account
                    .account
                    .as_ref()
                    .map(|plain| plain.info.nonce)
                    .unwrap_or(0);
            }
        }
        // An address the in-progress block's own execution hasn't touched yet may
        // still have a newer nonce than `factory.latest()` reflects, if the block that touched it
        // has sealed but not yet been confirmed durable — check the not-yet-confirmed overlay before
        // falling to disk (see `overlay_nonce`'s doc: newest block wins).
        if let Some(nonce) = self.overlay_nonce(addr) {
            return nonce;
        }
        self.factory
            .latest()
            .ok()
            .and_then(|provider| provider.account_nonce(&addr).ok().flatten())
            .unwrap_or(0)
    }

    /// Captured once in `new()` from the real MDBX head — see [`Self::persisted_at_open`].
    fn last_persisted_block(&self) -> Option<u64> {
        self.persisted_at_open
    }

    /// Joins EVERY one of this executor's own still-outstanding
    /// background persists, in order — a graceful shutdown's last chance to make the full tail
    /// durable before the process exits normally (see [`Self::join_all_pending_persists`]'s doc; a
    /// hard kill skips this entirely, which is fine by design — tail replay repairs it).
    async fn flush(&mut self) -> Result<(), ExecutorError> {
        self.join_all_pending_persists().await
    }
}

impl RethExecutor {
    /// A cheap-to-clone read handle onto the SAME on-disk MDBX
    /// this executor writes to AND the SAME `CanonicalInMemoryState` this executor's own `seal_block`
    /// advances (`self.blockchain_provider`'s doc explains why it must be THIS instance's clone, not a
    /// fresh `BlockchainProvider::new(factory)` built from a bare `ProviderFactory` elsewhere — an
    /// earlier "latest" bug was exactly that mistake). `src/node.rs`'s RPC server holds its own clone
    /// rather than a reference to `self`: the sequencer actor owns `RethExecutor` exclusively via
    /// `&mut self` (it is not `Sync`-shareable across the RPC server's connection tasks), but every
    /// read through a clone opens its own MDBX read transaction and always sees whatever the
    /// executor's own writer transactions have most recently committed, plus the same canonical head
    /// pointer — exactly the "reth's own eth_* JSON-RPC, reading the node's canonical state" the
    /// contract asks for, with no shared-mutable-state plumbing needed between the two.
    pub fn rpc_provider(&self) -> BlockchainProvider<RethTypes> {
        self.blockchain_provider.clone()
    }

    /// The chain spec this executor was built from — `src/node.rs` needs its own `Arc` for the RPC
    /// node's `EthEvmConfig`/`EthBeaconConsensus`.
    pub fn chain_spec(&self) -> Arc<ChainSpec> {
        self.chain_spec.clone()
    }
}

#[cfg(test)]
mod tests {
    /// Every test opens its own MDBX environment; reth's default map size is large
    /// enough that a dozen environments opened concurrently exhaust the process's address space on
    /// macOS ("Cannot allocate memory (12)"). Tests that construct a RethExecutor hold this lock so
    /// they run one at a time regardless of --test-threads. Async tests await it (a std MutexGuard
    /// across an await point is a clippy error and a deadlock risk on a multi-thread runtime).
    /// All executor tests are async, so there is no blocking variant.
    static MDBX_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    async fn serial_mdbx() -> tokio::sync::MutexGuard<'static, ()> {
        MDBX_SERIAL.lock().await
    }

    use super::*;
    use alloy_consensus::{SignableTransaction, TxEip1559};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{TxKind, U256};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use reth_provider::{StaticFileProviderFactory, StaticFileSegment};
    use tempfile::tempdir;

    const CHAIN_ID: u64 = 200_101;
    /// Matches the devnet genesis template's `gasLimit` field.
    const GAS_LIMIT_HEX: &str = "0x2540be400";
    const GAS_LIMIT: u64 = 0x2540be400;

    /// `attrs_from_env`'s rule-fixed fields must equal the real, independently
    /// known values (pinned as literals here, NOT re-derived by calling `canonical_header_rule` a second
    /// time in the test itself — a test that re-derives the same function it is checking would stay green
    /// even if that function's own constants changed, proving nothing: see
    /// `rome-zk-executor-api::canonical_header_rule_matches_the_real_tiber_header_fixture` for where these
    /// literals come from). Mutation: change
    /// `canonical_header_rule`'s `parent_beacon_block_root` from `B256::ZERO` to anything else and this
    /// test goes red, because `attrs_from_env` would then emit that changed value while this test still
    /// expects the pinned literal.
    #[test]
    fn attrs_from_env_reads_every_rule_fixed_field_from_the_shared_rule() {
        let env = BlockEnv {
            number: 11,
            timestamp_secs: 1_757_000_011,
            gas_limit: GAS_LIMIT,
            coinbase: Address::ZERO,
            prev_randao: rome_zk_executor_api::prev_randao(CHAIN_ID, 11),
            base_fee: None,
            withdrawals: vec![],
        };
        let attrs = RethExecutor::attrs_from_env(&env, CHAIN_ID);
        assert_eq!(
            attrs.prev_randao,
            rome_zk_executor_api::prev_randao(CHAIN_ID, 11),
            "prev_randao must be the shared formula's output for this exact (chain_id, number)"
        );
        assert_eq!(
            attrs.suggested_fee_recipient,
            Address::ZERO,
            "beneficiary must be the chain's fee recipient (env.coinbase), ZERO on Tiber"
        );
        assert_eq!(
            attrs.parent_beacon_block_root,
            Some(B256::ZERO),
            "parent_beacon_block_root must be the rule's fixed ZERO constant"
        );
        assert_eq!(
            attrs.extra_data,
            Bytes::new(),
            "extra_data must be the rule's fixed empty constant"
        );
        assert_eq!(attrs.gas_limit, env.gas_limit);
        assert_eq!(attrs.timestamp, env.timestamp_secs);
    }

    /// A different chain-config fee recipient (`env.coinbase`) must change
    /// `attrs_from_env`'s `suggested_fee_recipient` identically — the rule's `beneficiary` field is
    /// threaded from the caller, never a hardcoded `Address::ZERO`.
    #[test]
    fn attrs_from_env_beneficiary_follows_env_coinbase() {
        let fee_recipient = Address::repeat_byte(0xCD);
        let env = BlockEnv {
            number: 11,
            timestamp_secs: 1_757_000_011,
            gas_limit: GAS_LIMIT,
            coinbase: fee_recipient,
            prev_randao: rome_zk_executor_api::prev_randao(CHAIN_ID, 11),
            base_fee: None,
            withdrawals: vec![],
        };
        let attrs = RethExecutor::attrs_from_env(&env, CHAIN_ID);
        assert_eq!(attrs.suggested_fee_recipient, fee_recipient);
    }

    /// Writes a genesis.json shaped exactly like the devnet genesis template (Prague at
    /// 0, chain 200101) with `funded` accounts substituted into `alloc`, and returns a `RethConfig`
    /// pointed at it plus a fresh datadir under `dir`.
    fn test_config(dir: &std::path::Path, funded: &[(Address, U256)]) -> RethConfig {
        let alloc: serde_json::Map<String, serde_json::Value> = funded
            .iter()
            .map(|(addr, balance)| {
                (
                    format!("{addr:#x}"),
                    serde_json::json!({ "balance": format!("{balance:#x}") }),
                )
            })
            .collect();
        let genesis = serde_json::json!({
            "config": {
                "chainId": CHAIN_ID,
                "homesteadBlock": 0, "eip150Block": 0, "eip155Block": 0, "eip158Block": 0,
                "byzantiumBlock": 0, "constantinopleBlock": 0, "petersburgBlock": 0,
                "istanbulBlock": 0, "berlinBlock": 0, "londonBlock": 0,
                "terminalTotalDifficulty": 0, "terminalTotalDifficultyPassed": true,
                "shanghaiTime": 0, "cancunTime": 0, "pragueTime": 0
            },
            "nonce": "0x0",
            "timestamp": "0x0",
            "extraData": "0x",
            "gasLimit": GAS_LIMIT_HEX,
            "difficulty": "0x0",
            "mixHash": format!("{:#x}", B256::ZERO),
            "coinbase": "0x0000000000000000000000000000000000000000",
            "alloc": alloc,
            "number": "0x0",
            "gasUsed": "0x0",
            "parentHash": format!("{:#x}", B256::ZERO),
            "baseFeePerGas": "0x3b9aca00"
        });
        let genesis_path = dir.join("genesis.json");
        std::fs::write(&genesis_path, genesis.to_string()).unwrap();
        RethConfig {
            datadir: dir.join("db"),
            genesis_path,
            block_gas_limit: GAS_LIMIT,
        }
    }

    /// Like [`test_config`], but also pre-deploys `code` at `contract_addr` in genesis `alloc` — used
    /// by the pre-confirmation-integrity test, which needs a contract
    /// whose behavior depends on `block.timestamp`/`block.prevrandao` already present at block 0
    /// without spending a sub-block on a CREATE tx.
    fn test_config_with_contract(
        dir: &std::path::Path,
        funded: &[(Address, U256)],
        contract_addr: Address,
        code: &Bytes,
    ) -> RethConfig {
        let mut alloc: serde_json::Map<String, serde_json::Value> = funded
            .iter()
            .map(|(addr, balance)| {
                (
                    format!("{addr:#x}"),
                    serde_json::json!({ "balance": format!("{balance:#x}") }),
                )
            })
            .collect();
        alloc.insert(
            format!("{contract_addr:#x}"),
            serde_json::json!({ "balance": "0x0", "code": format!("0x{}", hex::encode(code)) }),
        );
        let genesis = serde_json::json!({
            "config": {
                "chainId": CHAIN_ID,
                "homesteadBlock": 0, "eip150Block": 0, "eip155Block": 0, "eip158Block": 0,
                "byzantiumBlock": 0, "constantinopleBlock": 0, "petersburgBlock": 0,
                "istanbulBlock": 0, "berlinBlock": 0, "londonBlock": 0,
                "terminalTotalDifficulty": 0, "terminalTotalDifficultyPassed": true,
                "shanghaiTime": 0, "cancunTime": 0, "pragueTime": 0
            },
            "nonce": "0x0",
            "timestamp": "0x0",
            "extraData": "0x",
            "gasLimit": GAS_LIMIT_HEX,
            "difficulty": "0x0",
            "mixHash": format!("{:#x}", B256::ZERO),
            "coinbase": "0x0000000000000000000000000000000000000000",
            "alloc": alloc,
            "number": "0x0",
            "gasUsed": "0x0",
            "parentHash": format!("{:#x}", B256::ZERO),
            "baseFeePerGas": "0x3b9aca00"
        });
        let genesis_path = dir.join("genesis.json");
        std::fs::write(&genesis_path, genesis.to_string()).unwrap();
        RethConfig {
            datadir: dir.join("db"),
            genesis_path,
            block_gas_limit: GAS_LIMIT,
        }
    }

    /// A signed EIP-1559 transfer to an ARBITRARY `to` (unlike
    /// [`signed_transfer`]'s fixed `Address::ZERO`) — used to grow the touched-account set with
    /// fresh recipients, one per tx, for the trie-persistence measurement below.
    fn signed_transfer_to(signer: &PrivateKeySigner, nonce: u64, to: Address) -> Bytes {
        let tx = TxEip1559 {
            chain_id: CHAIN_ID,
            nonce,
            gas_limit: 21_000,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: TxKind::Call(to),
            value: U256::ZERO,
            access_list: Default::default(),
            input: Bytes::new(),
        };
        let sig_hash = tx.signature_hash();
        let signature = signer.sign_hash_sync(&sig_hash).expect("sign fixture tx");
        let signed = tx.into_signed(signature);
        Bytes::from(alloy_consensus::TxEnvelope::from(signed).encoded_2718())
    }

    /// Like [`signed_transfer_to`], but with an explicit `value` — needed by the
    /// state-visible-through-the-overlay test, which must move a real balance from `signer` to a
    /// fresh recipient so the recipient's OWN next block can spend it.
    fn signed_transfer_value(
        signer: &PrivateKeySigner,
        nonce: u64,
        to: Address,
        value: U256,
    ) -> Bytes {
        let tx = TxEip1559 {
            chain_id: CHAIN_ID,
            nonce,
            gas_limit: 21_000,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: TxKind::Call(to),
            value,
            access_list: Default::default(),
            input: Bytes::new(),
        };
        let sig_hash = tx.signature_hash();
        let signature = signer.sign_hash_sync(&sig_hash).expect("sign fixture tx");
        let signed = tx.into_signed(signature);
        Bytes::from(alloy_consensus::TxEnvelope::from(signed).encoded_2718())
    }

    /// A signed EIP-1559 transfer, matching `rome_zk_sequencer::testutil::signed_raw_tx`'s exact
    /// shape (flat 21_000 gas, `to: Address::ZERO`) so this crate's tests exercise the same fixture
    /// the sequencer's own e2e/sealer tests do.
    fn signed_transfer(signer: &PrivateKeySigner, nonce: u64) -> Bytes {
        let tx = TxEip1559 {
            chain_id: CHAIN_ID,
            nonce,
            gas_limit: 21_000,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            access_list: Default::default(),
            input: Bytes::new(),
        };
        let sig_hash = tx.signature_hash();
        let signature = signer.sign_hash_sync(&sig_hash).expect("sign fixture tx");
        let signed = tx.into_signed(signature);
        Bytes::from(alloy_consensus::TxEnvelope::from(signed).encoded_2718())
    }

    /// A signed call to `to` with no calldata — used by the pre-confirmation-integrity test to
    /// invoke a pre-deployed contract that reads `block.timestamp`/`block.prevrandao`.
    fn signed_call(signer: &PrivateKeySigner, nonce: u64, to: Address, gas_limit: u64) -> Bytes {
        let tx = TxEip1559 {
            chain_id: CHAIN_ID,
            nonce,
            gas_limit,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: TxKind::Call(to),
            value: U256::ZERO,
            access_list: Default::default(),
            input: Bytes::new(),
        };
        let sig_hash = tx.signature_hash();
        let signature = signer.sign_hash_sync(&sig_hash).expect("sign fixture tx");
        let signed = tx.into_signed(signature);
        Bytes::from(alloy_consensus::TxEnvelope::from(signed).encoded_2718())
    }

    /// Surfaces `seal_block`'s own per-phase `tracing::debug!` timings (the three
    /// timers) when a measurement test runs with `--nocapture` — no-op after
    /// the first call (`try_init` — several tests in this module may call it).
    fn init_tracing_for_measurement() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("rome_zk_executor_reth=debug")
            .with_test_writer()
            .try_init();
    }

    fn unbounded_limits() -> SubBlockLimits {
        SubBlockLimits {
            gas_limit: u64::MAX,
            deadline: Instant::now() + Duration::from_secs(3600),
        }
    }

    /// A `BlockEnv` for the block that follows `ex`'s current head —
    /// timestamp `parent + 1` (monotonic), gas_limit the genesis constant (mirrors this chain's real
    /// `gasLimit`, matching this crate's tests' prior behaviour before `BlockEnv` existed), coinbase
    /// zero (v1 default), a real (not opaque) `prev_randao`.
    fn block_env(ex: &RethExecutor, number: u64) -> BlockEnv {
        BlockEnv {
            number,
            timestamp_secs: ex.canonical_parent.timestamp() + 1,
            gas_limit: ex.canonical_parent.gas_limit(),
            coinbase: Address::ZERO,
            prev_randao: rome_zk_executor_api::prev_randao(CHAIN_ID, number),
            base_fee: None,
            withdrawals: vec![],
        }
    }

    /// A transfer sub-block on the 200101 genesis — included, gas 21_000,
    /// receipts_root matches an independently-computed ordered receipts root, `nonce()` advances.
    #[tokio::test]
    async fn transfer_sub_block_is_included_with_flat_gas_and_advances_nonce() {
        let _serial = serial_mdbx().await;
        let dir = tempdir().unwrap();
        let signer = PrivateKeySigner::random();
        let config = test_config(
            dir.path(),
            &[(signer.address(), U256::from(10u128).pow(U256::from(20u8)))],
        );
        let mut ex = RethExecutor::new(config).unwrap();
        assert_eq!(ex.nonce(signer.address()), 0);
        ex.open_block(block_env(&ex, 1)).await.unwrap();

        let tx = signed_transfer(&signer, 0);
        let outcome = ex
            .execute_sub_block(&[tx], unbounded_limits())
            .await
            .unwrap();

        assert_eq!(outcome.included.len(), 1, "the transfer must be included");
        assert!(outcome.rejected.is_empty());
        assert!(outcome.not_executed.is_empty());
        assert_eq!(
            outcome.gas_used, 21_000,
            "a plain transfer costs exactly the intrinsic 21_000"
        );
        assert_eq!(
            ex.nonce(signer.address()),
            1,
            "an included tx must advance the sender's nonce"
        );

        let expected_root = receipts_root(&[Receipt {
            tx_type: alloy_consensus::TxType::Eip1559,
            success: true,
            cumulative_gas_used: 21_000,
            logs: vec![],
        }]);
        assert_eq!(
            outcome.receipts_root, expected_root,
            "receipts_root must be the ordered trie root alloy's own receipt encoding produces"
        );
    }

    /// A sender with no genesis balance can't cover `gas_limit * max_fee` —
    /// rejected with a `Reason`, no gas consumed.
    #[tokio::test]
    async fn insufficient_balance_tx_is_rejected_with_a_reason_and_no_gas() {
        let _serial = serial_mdbx().await;
        let dir = tempdir().unwrap();
        let signer = PrivateKeySigner::random(); // never funded
        let config = test_config(dir.path(), &[]);
        let mut ex = RethExecutor::new(config).unwrap();
        ex.open_block(block_env(&ex, 1)).await.unwrap();

        let tx = signed_transfer(&signer, 0);
        let outcome = ex
            .execute_sub_block(&[tx], unbounded_limits())
            .await
            .unwrap();

        assert!(outcome.included.is_empty());
        assert_eq!(outcome.rejected.len(), 1);
        assert_eq!(outcome.rejected[0].sender, signer.address());
        assert_eq!(
            outcome.rejected[0].reason,
            Reason::InsufficientFunds,
            "an unfunded sender must be rejected with Reason::InsufficientFunds"
        );
        assert_eq!(outcome.gas_used, 0, "a rejected tx must not consume gas");
        assert_eq!(
            ex.nonce(signer.address()),
            0,
            "a rejected tx must not advance the nonce"
        );
    }

    /// The sealer logs only `included_raw_txs` ("the record is the block"): a rejected tx must leave `RethExecutor`'s
    /// pending `State` byte-for-byte as if it had never been attempted — this is what makes it safe for the sealer to
    /// log (and for derivation to later replay) only `outcome.included`, never a rejected tx's raw
    /// bytes. `revm`'s own `ExecuteCommitEvm::transact_commit` contract already guarantees this ("If
    /// the transaction fails, the journal is finalized (not committed) so it does not leak into the
    /// next transaction" — `revm-handler-42.0.1/src/api.rs`), so this proves it end to end rather than
    /// trusting the doc comment: the SAME two good txs `[a, c]`, sealed into a block two ways — once
    /// with a rejected tx interleaved (`[a, bad, c]`, what a pre-fix sealer would have logged) and
    /// once alone (`[a, c]`, what the fixed sealer now logs) — must produce a byte-identical sealed
    /// block either way.
    #[tokio::test]
    async fn rejected_tx_leaves_pending_state_untouched_sealed_block_identical_with_or_without_it()
    {
        let _serial = serial_mdbx().await;
        let sender_a = PrivateKeySigner::random();
        let sender_bad = PrivateKeySigner::random(); // never funded -> InsufficientFunds
        let sender_c = PrivateKeySigner::random();
        let funded = [
            (sender_a.address(), U256::from(10u128).pow(U256::from(20u8))),
            (sender_c.address(), U256::from(10u128).pow(U256::from(20u8))),
        ];

        let tx_a = signed_transfer(&sender_a, 0);
        let tx_bad = signed_transfer(&sender_bad, 0);
        let tx_c = signed_transfer(&sender_c, 0);

        // Run 1: the full attempted superset [a, bad, c] -- exactly what the pre-fix sealer handed
        // the log.
        let dir1 = tempdir().unwrap();
        let config1 = test_config(dir1.path(), &funded);
        let mut ex1 = RethExecutor::new(config1).unwrap();
        let env1 = block_env(&ex1, 1);
        ex1.open_block(env1.clone()).await.unwrap();
        let outcome1 = ex1
            .execute_sub_block(&[tx_a.clone(), tx_bad, tx_c.clone()], unbounded_limits())
            .await
            .unwrap();
        assert_eq!(outcome1.included.len(), 2, "a and c must be included");
        assert_eq!(outcome1.rejected.len(), 1, "bad must be rejected");
        let sealed1 = ex1
            .seal_block(BlockSealInputs {
                block: 1,
                timestamp_secs: env1.timestamp_secs,
                sub_block_header_hashes: vec![B256::repeat_byte(1)],
                total_gas_used: outcome1.gas_used,
            })
            .await
            .unwrap();

        // Run 2: only the included pair [a, c] -- exactly what the FIXED sealer now logs.
        let dir2 = tempdir().unwrap();
        let config2 = test_config(dir2.path(), &funded);
        let mut ex2 = RethExecutor::new(config2).unwrap();
        let env2 = block_env(&ex2, 1);
        ex2.open_block(env2.clone()).await.unwrap();
        let outcome2 = ex2
            .execute_sub_block(&[tx_a, tx_c], unbounded_limits())
            .await
            .unwrap();
        assert!(outcome2.rejected.is_empty());
        let sealed2 = ex2
            .seal_block(BlockSealInputs {
                block: 1,
                timestamp_secs: env2.timestamp_secs,
                sub_block_header_hashes: vec![B256::repeat_byte(1)],
                total_gas_used: outcome2.gas_used,
            })
            .await
            .unwrap();

        assert_eq!(
            outcome1.included, outcome2.included,
            "the included set itself must match with or without the rejected tx interleaved"
        );
        assert_eq!(outcome1.gas_used, outcome2.gas_used);
        assert_eq!(outcome1.receipts_root, outcome2.receipts_root);
        assert_eq!(
            sealed1, sealed2,
            "sealing with the rejected tx interleaved must produce a byte-identical block \
             (state_root/block_hash/receipts_root) to sealing without it -- proving a rejected tx \
             leaves the pending State, and therefore the sealed block, untouched"
        );
    }

    /// A `gas_limit` too small for every tx cuts an in-order suffix into
    /// `not_executed` (each transfer's own declared `gas_limit` — 21_000 — is the reservation, since
    /// a real block's gas-limit admission works off the tx's declared limit, not its post-hoc usage).
    #[tokio::test]
    async fn gas_limit_cuts_an_in_order_suffix_into_not_executed() {
        let _serial = serial_mdbx().await;
        let dir = tempdir().unwrap();
        let signer = PrivateKeySigner::random();
        let config = test_config(
            dir.path(),
            &[(signer.address(), U256::from(10u128).pow(U256::from(20u8)))],
        );
        let mut ex = RethExecutor::new(config).unwrap();
        ex.open_block(block_env(&ex, 1)).await.unwrap();

        let txs: Vec<Bytes> = (0..5u64).map(|n| signed_transfer(&signer, n)).collect();
        let outcome = ex
            .execute_sub_block(
                &txs,
                SubBlockLimits {
                    gas_limit: 3 * 21_000,
                    deadline: Instant::now() + Duration::from_secs(3600),
                },
            )
            .await
            .unwrap();

        assert_eq!(outcome.included.len(), 3, "floor(3*21000 / 21000) = 3");
        assert!(outcome.rejected.is_empty());
        assert_eq!(outcome.not_executed.len(), 2);
        assert_eq!(
            outcome.not_executed,
            txs[3..],
            "not_executed must be exactly the unreached suffix, in order"
        );
        assert_eq!(ex.nonce(signer.address()), 3);
    }

    /// 20 sub-blocks close a block whose `state_root`/`block_hash` are real
    /// (non-zero, and distinct from a second block's) — this is `seal_block`'s own header, not a
    /// digest invented by this crate; the derivation-equivalence test is the proof that a second,
    /// independent reth agrees with it.
    #[tokio::test]
    async fn twenty_sub_blocks_seal_a_real_block_with_a_non_trivial_root() {
        let _serial = serial_mdbx().await;
        let dir = tempdir().unwrap();
        let signer = PrivateKeySigner::random();
        let config = test_config(
            dir.path(),
            &[(signer.address(), U256::from(10u128).pow(U256::from(20u8)))],
        );
        let mut ex = RethExecutor::new(config).unwrap();

        let block0_env = block_env(&ex, 1);
        ex.open_block(block0_env.clone()).await.unwrap();
        let mut nonce = 0u64;
        for i in 0..20u16 {
            let txs = if i % 4 == 0 {
                let tx = signed_transfer(&signer, nonce);
                nonce += 1;
                vec![tx]
            } else {
                vec![]
            };
            ex.execute_sub_block(&txs, unbounded_limits())
                .await
                .unwrap();
        }
        let block0 = ex
            .seal_block(BlockSealInputs {
                block: 1,
                timestamp_secs: block0_env.timestamp_secs,
                sub_block_header_hashes: vec![B256::repeat_byte(1); 20],
                total_gas_used: nonce * 21_000,
            })
            .await
            .unwrap();
        assert_ne!(block0.state_root, B256::ZERO);
        assert_ne!(block0.block_hash, B256::ZERO);
        assert_eq!(
            ex.head().block,
            1,
            "Head::block echoes BlockSealInputs::block verbatim"
        );
        assert_eq!(ex.head().state_root, block0.state_root);

        // A second, empty block must produce a DIFFERENT hash/root (proves this isn't a constant).
        let block1_env = block_env(&ex, 2);
        ex.open_block(block1_env.clone()).await.unwrap();
        for _ in 0..20u16 {
            ex.execute_sub_block(&[], unbounded_limits()).await.unwrap();
        }
        let block1 = ex
            .seal_block(BlockSealInputs {
                block: 2,
                timestamp_secs: block1_env.timestamp_secs,
                sub_block_header_hashes: vec![B256::repeat_byte(2); 20],
                total_gas_used: 0,
            })
            .await
            .unwrap();
        assert_ne!(block1.block_hash, block0.block_hash);
        assert_eq!(
            block1.state_root, block0.state_root,
            "an empty block must not change the state root"
        );
    }

    fn ex_genesis_timestamp(ex: &RethExecutor) -> u64 {
        ex.canonical_parent.timestamp()
    }

    /// Pre-confirmation integrity. A contract that logs
    /// `block.timestamp` is called in one sub-block (all other 19 empty); the sub-block's own
    /// `receipts_root` (the one a pre-confirmation is signed over) must equal the SEALED BLOCK's own
    /// receipts_root — proving the tx ran under the identical env both times. The block is opened
    /// with a timestamp 5 seconds past `parent + 1` (simulating the sealer resolving a real gap, e.g.
    /// after a slow-sealing block or a missed-tick catch-up) — before this fix, sub-block preview
    /// execution used `canonical_parent.timestamp() + 1` regardless of what the sealer actually
    /// resolved, so this specific case (any block whose real timestamp isn't exactly parent+1) would
    /// have logged a DIFFERENT `block.timestamp` in the pre-confirmed receipt than the one seal_block
    /// re-executed under, and this assertion would fail.
    #[tokio::test]
    async fn sub_block_receipts_root_matches_the_sealed_blocks_for_a_timestamp_reading_contract() {
        let _serial = serial_mdbx().await;
        // Runtime: TIMESTAMP; PUSH1 0; MSTORE; PUSH1 32; PUSH1 0; LOG0; STOP — logs block.timestamp
        // as the sole 32-byte log datum (no topics), so a different timestamp produces a different
        // receipt (different logs -> different receipts_root).
        let runtime: [u8; 10] = [0x42, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xa0, 0x00];
        // Init code: CODECOPY(dest=0, offset=12, size=10) then RETURN(offset=0, size=10), followed by
        // the 10 runtime bytes at offset 12.
        let mut init_code = vec![
            0x60, 0x0a, // PUSH1 10 (size)
            0x60, 0x0c, // PUSH1 12 (offset)
            0x60, 0x00, // PUSH1 0 (destOffset)
            0x39, // CODECOPY
            0x60, 0x0a, // PUSH1 10 (size)
            0x60, 0x00, // PUSH1 0 (offset)
            0xf3, // RETURN
        ];
        init_code.extend_from_slice(&runtime);
        assert_eq!(init_code.len(), 22);

        let contract_addr = Address::repeat_byte(0xAB);
        let dir = tempdir().unwrap();
        let signer = PrivateKeySigner::random();
        let config = test_config_with_contract(
            dir.path(),
            &[(signer.address(), U256::from(10u128).pow(U256::from(20u8)))],
            contract_addr,
            &Bytes::from(runtime.to_vec()),
        );
        let mut ex = RethExecutor::new(config).unwrap();

        let genesis_ts = ex_genesis_timestamp(&ex);
        let env = BlockEnv {
            number: 1,
            // Deliberately NOT `genesis_ts + 1` — see this test's doc comment.
            timestamp_secs: genesis_ts + 5,
            gas_limit: 30_000_000,
            coinbase: Address::ZERO,
            prev_randao: rome_zk_executor_api::prev_randao(CHAIN_ID, 1),
            base_fee: None,
            withdrawals: vec![],
        };
        ex.open_block(env.clone()).await.unwrap();

        let mut sub_block_receipts_root = None;
        for i in 0..20u16 {
            let txs = if i == 7 {
                vec![signed_call(&signer, 0, contract_addr, 100_000)]
            } else {
                vec![]
            };
            let outcome = ex
                .execute_sub_block(&txs, unbounded_limits())
                .await
                .unwrap();
            if i == 7 {
                assert_eq!(outcome.included.len(), 1, "the call must be included");
                assert!(outcome.rejected.is_empty());
                sub_block_receipts_root = Some(outcome.receipts_root);
            }
        }
        let sub_block_receipts_root =
            sub_block_receipts_root.expect("sub-block 7 must have executed the call");

        let block = ex
            .seal_block(BlockSealInputs {
                block: 1,
                timestamp_secs: env.timestamp_secs,
                sub_block_header_hashes: vec![B256::repeat_byte(1); 20],
                total_gas_used: 21_000,
            })
            .await
            .unwrap();

        assert_eq!(
            sub_block_receipts_root, block.receipts_root,
            "the sub-block's own receipts_root (what a pre-confirmation signs over) must equal the \
             sealed block's receipts_root — a mismatch means the tx ran under a different env in \
             preview than at seal time"
        );
    }

    /// Crash after block 2 seals (no clean shutdown) → a fresh `RethExecutor`
    /// over a FRESH datadir/genesis, replaying the identical sub-block/seal sequence from scratch,
    /// reaches the identical head — full re-derivation determinism (what a `MockExecutor` replay
    /// always does, since it has nothing to persist). This test drives `execute_sub_block`/
    /// `seal_block` directly (what `rome_zk_sequencer::recovery::replay_into_executor` does per
    /// record when it has no persisted head to skip past) rather than exercising the sequencer's
    /// log/actor machinery, which is already covered by `recovery::tests` against `MockExecutor` and
    /// is executor-agnostic (generic over the trait). See
    /// `restarting_over_the_same_datadir_resumes_from_the_real_persisted_head` below for the
    /// actual tail-only-replay contract: restarting over the SAME (not fresh) datadir.
    #[tokio::test]
    async fn crash_after_a_sealed_block_then_replay_reaches_the_identical_head() {
        let _serial = serial_mdbx().await;
        let dir = tempdir().unwrap();
        let signer = PrivateKeySigner::random();
        let funded = &[(signer.address(), U256::from(10u128).pow(U256::from(20u8)))];

        async fn run_blocks(ex: &mut RethExecutor, signer: &PrivateKeySigner, blocks: u64) -> u64 {
            let mut nonce = 0u64;
            // A fresh chain's first sealed block is design 1.
            for b in 1..=blocks {
                let env = block_env(ex, b);
                ex.open_block(env.clone()).await.unwrap();
                for i in 0..20u16 {
                    let txs = if i == 0 {
                        let tx = signed_transfer(signer, nonce);
                        nonce += 1;
                        vec![tx]
                    } else {
                        vec![]
                    };
                    ex.execute_sub_block(&txs, unbounded_limits())
                        .await
                        .unwrap();
                }
                ex.seal_block(BlockSealInputs {
                    block: b,
                    timestamp_secs: env.timestamp_secs,
                    sub_block_header_hashes: vec![B256::repeat_byte(b as u8 + 1); 20],
                    total_gas_used: 21_000,
                })
                .await
                .unwrap();
            }
            nonce
        }

        // Live run: 2 blocks, "crash" (drop without any special shutdown).
        let live_dir = tempdir().unwrap();
        let live_head = {
            let mut live = RethExecutor::new(test_config(live_dir.path(), funded)).unwrap();
            run_blocks(&mut live, &signer, 2).await;
            live.head()
        };

        // Independent config pointed at a FRESH datadir with the same genesis — proving full
        // re-derivation determinism, the property `recovery::replay_into_executor` falls back on
        // whenever `last_persisted_block()` is `None` (nothing to skip).
        let mut replayed = RethExecutor::new(test_config(dir.path(), funded)).unwrap();
        run_blocks(&mut replayed, &signer, 2).await;

        assert_eq!(
            replayed.head(),
            live_head,
            "replaying the identical sub-block/seal sequence must reproduce the identical head"
        );
    }

    /// Core persistence test: seal 2 blocks, `flush()` (a
    /// graceful shutdown — this test's own intent, "restart resumes from the persisted head", is
    /// about the ordinary restart path; the SEPARATE guarantee that an UNjoined background persist is
    /// still safe to crash on is covered by `rome-zk-sequencer`'s
    /// `kill_9_right_after_a_block_seals_then_restart_reaches_the_same_state`, which kills a real OS
    /// process so the background persist thread genuinely dies with it — an in-process `drop` here
    /// would NOT: `seal_block`'s spawned MDBX write holds its own `ProviderFactory` clone and keeps
    /// running on its own blocking-pool thread regardless of whether this scope's `ex` is dropped,
    /// which would race a same-process reopen of the SAME datadir below), then reopen a fresh
    /// `RethExecutor` over the SAME datadir — it must read the real head back off MDBX
    /// (`last_persisted_block() == Some(2)`, the sequencer-numbered last sealed block) and its
    /// `head()` must already equal what the live run produced, with NO further execution at all
    /// (this constructor does no replay itself — `recovery::replay_into_executor` is what walks the
    /// log's tail, tested end to end in `rome-zk-sequencer`'s own reth-gated tests). This is the
    /// "restart is a database open" half of the contract; the "plus tail replay" half is
    /// covered where the log lives.
    #[tokio::test]
    async fn restarting_over_the_same_datadir_resumes_from_the_real_persisted_head() {
        let _serial = serial_mdbx().await;
        let dir = tempdir().unwrap();
        let signer = PrivateKeySigner::random();
        let funded = &[(signer.address(), U256::from(10u128).pow(U256::from(20u8)))];
        let config = test_config(dir.path(), funded);

        let live_head = {
            let mut ex = RethExecutor::new(config.clone()).unwrap();
            assert_eq!(
                ex.last_persisted_block(),
                None,
                "a fresh datadir has nothing persisted beyond genesis"
            );
            let mut nonce = 0u64;
            // The sequencer numbers its first sealed block 1 —
            // BlockEnv.number == the resulting header.number for every block.
            for b in 1..=2u64 {
                let env = block_env(&ex, b);
                ex.open_block(env.clone()).await.unwrap();
                for i in 0..20u16 {
                    let txs = if i == 0 {
                        let tx = signed_transfer(&signer, nonce);
                        nonce += 1;
                        vec![tx]
                    } else {
                        vec![]
                    };
                    ex.execute_sub_block(&txs, unbounded_limits())
                        .await
                        .unwrap();
                }
                ex.seal_block(BlockSealInputs {
                    block: b,
                    timestamp_secs: env.timestamp_secs,
                    sub_block_header_hashes: vec![B256::repeat_byte(b as u8 + 1); 20],
                    total_gas_used: 21_000,
                })
                .await
                .unwrap();
            }
            // A graceful shutdown flush — see this test's doc comment above
            // for why (this test's own intent is the ordinary restart path, not the crash race).
            ex.flush().await.unwrap();
            ex.head()
        };

        // Reopen over the SAME datadir. No block/sub-block is executed here — this is purely what
        // `RethExecutor::new` itself reads back off MDBX.
        let reopened = RethExecutor::new(config).unwrap();
        assert_eq!(
            reopened.last_persisted_block(),
            Some(2),
            "2 sealed blocks (sequencer numbering 1, 2) must be durable across the restart"
        );
        assert_eq!(
            reopened.head(),
            live_head,
            "the reopened executor's head must equal the live run's head with zero replay"
        );
        assert_eq!(
            reopened.nonce(signer.address()),
            2,
            "the sender's real on-disk nonce (2 included transfers) must be visible with no replay"
        );
    }

    /// `nonce()` must read from state, not a side map that only knows
    /// about addresses that have sent a tx *through this executor*. An account whose nonce was set in
    /// genesis alloc — never touched by any `execute_sub_block` call — must still report its real
    /// nonce (a side map, seeded only on inclusion, would report 0 here: the exact divergence a side
    /// map cannot avoid for any account it has never seen).
    /// The genesis header's gas limit must equal the profile's block gas limit: a stock reth verifier
    /// can neither build nor accept a block whose gas limit moved from its parent by more than 1/1024, so
    /// the first sealed block must inherit the genesis figure exactly. Found live: Tiber's genesis carried
    /// 100,000,000 while the profile sealed 40,000,000 from block 1, and the verifier halted forever.
    #[tokio::test]
    async fn a_genesis_gas_limit_that_differs_from_the_configured_block_gas_limit_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(dir.path(), &[]);
        config.block_gas_limit = GAS_LIMIT + 1;
        let err = match RethExecutor::new(config) {
            Err(e) => e,
            Ok(_) => panic!("a mismatching genesis gas limit must be refused, not accepted"),
        };
        assert!(
            matches!(
                err,
                ExecutorError::GenesisGasLimitMismatch { genesis, configured }
                    if genesis == GAS_LIMIT && configured == GAS_LIMIT + 1
            ),
            "must be the named refusal: {err}"
        );
    }

    /// Same genesis-building shape as `test_config`, but with the genesis `gasLimit`
    /// and `RethConfig::block_gas_limit` settable independently — used by the two `Profile`-driven tests
    /// below so a match or a mismatch is constructed from real `Profile::effective_block_gas_limit()`
    /// products (the devnet genesis renderer's own formula, reproduced here) rather than the
    /// `GAS_LIMIT`/`GAS_LIMIT_HEX` literal pair above (which matched each other by construction and never
    /// actually exercised `rome_zk_profile::Profile`).
    fn test_config_with_gas_limits(
        dir: &std::path::Path,
        genesis_gas_limit: u64,
        configured_gas_limit: u64,
    ) -> RethConfig {
        let genesis = serde_json::json!({
            "config": {
                "chainId": CHAIN_ID,
                "homesteadBlock": 0, "eip150Block": 0, "eip155Block": 0, "eip158Block": 0,
                "byzantiumBlock": 0, "constantinopleBlock": 0, "petersburgBlock": 0,
                "istanbulBlock": 0, "berlinBlock": 0, "londonBlock": 0,
                "terminalTotalDifficulty": 0, "terminalTotalDifficultyPassed": true,
                "shanghaiTime": 0, "cancunTime": 0, "pragueTime": 0
            },
            "nonce": "0x0",
            "timestamp": "0x0",
            "extraData": "0x",
            "gasLimit": format!("{genesis_gas_limit:#x}"),
            "difficulty": "0x0",
            "mixHash": format!("{:#x}", B256::ZERO),
            "coinbase": "0x0000000000000000000000000000000000000000",
            "alloc": {},
            "number": "0x0",
            "gasUsed": "0x0",
            "parentHash": format!("{:#x}", B256::ZERO),
            "baseFeePerGas": "0x3b9aca00"
        });
        let genesis_path = dir.join("genesis.json");
        std::fs::write(&genesis_path, genesis.to_string()).unwrap();
        RethConfig {
            datadir: dir.join("db"),
            genesis_path,
            block_gas_limit: configured_gas_limit,
        }
    }

    /// The genesis gas limit and `RethConfig::block_gas_limit` must both trace to a
    /// real `rome_zk_profile::Profile::effective_block_gas_limit()` product —
    /// the devnet genesis renderer's own formula (`sub_block_gas_limit * sub_blocks_per_block`)
    /// reproduced here as a plain integer, never re-derived by eye — instead of the tautological
    /// `GAS_LIMIT`/`GAS_LIMIT_HEX` literal pair the test above uses (which agree with each other by
    /// construction and never touch `Profile` at all). Tiber's own profile: 2,000,000
    /// x 20 = 40,000,000. A profile whose product matches the genesis is accepted, and block 1 seals at
    /// exactly that gas limit — inherited from `canonical_parent.gas_limit()`, the same mechanism every
    /// other test's `block_env` helper already relies on.
    #[tokio::test]
    async fn genesis_gas_limit_from_the_profiles_own_product_is_accepted_and_seals_block_one_at_it()
    {
        let _serial = serial_mdbx().await;
        let dir = tempdir().unwrap();

        let profile = rome_zk_profile::Profile {
            sub_block_gas_limit: 2_000_000,
            sub_blocks_per_block: 20,
            ..Default::default()
        };
        let block_gas_limit = profile.effective_block_gas_limit();
        assert_eq!(
            block_gas_limit, 40_000_000,
            "Tiber's own profile: 2,000,000 x 20"
        );

        let config = test_config_with_gas_limits(dir.path(), block_gas_limit, block_gas_limit);
        let mut ex = RethExecutor::new(config).unwrap();

        let block1_env = block_env(&ex, 1);
        ex.open_block(block1_env.clone()).await.unwrap();
        for _ in 0..20u16 {
            ex.execute_sub_block(&[], unbounded_limits()).await.unwrap();
        }
        ex.seal_block(BlockSealInputs {
            block: 1,
            timestamp_secs: block1_env.timestamp_secs,
            sub_block_header_hashes: vec![B256::repeat_byte(1); 20],
            total_gas_used: 0,
        })
        .await
        .unwrap();

        assert_eq!(
            ex.canonical_parent.gas_limit(),
            block_gas_limit,
            "sealed block 1's header gas limit must equal the genesis's, which must equal the \
             profile's own effective_block_gas_limit()"
        );
    }

    /// Sibling negative case: two DIFFERENT profiles' own products (both real
    /// `Profile` values, never hand-picked literals) — genesis rendered at one, `RethConfig::
    /// block_gas_limit` configured at the other — must refuse by name with both real values.
    #[tokio::test]
    async fn mismatched_profile_products_are_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let genesis_profile = rome_zk_profile::Profile {
            sub_block_gas_limit: 2_000_000,
            sub_blocks_per_block: 20,
            ..Default::default()
        };
        let configured_profile = rome_zk_profile::Profile {
            sub_block_gas_limit: 2_000_000,
            sub_blocks_per_block: 10,
            ..Default::default()
        };
        let genesis_gas_limit = genesis_profile.effective_block_gas_limit();
        let configured_gas_limit = configured_profile.effective_block_gas_limit();
        assert_ne!(genesis_gas_limit, configured_gas_limit);

        let config =
            test_config_with_gas_limits(dir.path(), genesis_gas_limit, configured_gas_limit);
        let err = match RethExecutor::new(config) {
            Err(e) => e,
            Ok(_) => panic!("mismatched profile products must be refused, not accepted"),
        };
        assert!(
            matches!(
                err,
                ExecutorError::GenesisGasLimitMismatch { genesis, configured }
                    if genesis == genesis_gas_limit && configured == configured_gas_limit
            ),
            "must be the named refusal with both real profile-derived values: {err}"
        );
    }

    #[tokio::test]
    async fn nonce_reads_a_genesis_preset_value_never_touched_by_a_tx() {
        let _serial = serial_mdbx().await;
        let addr = Address::repeat_byte(0xCD);
        let dir = tempdir().unwrap();
        let genesis = serde_json::json!({
            "config": {
                "chainId": CHAIN_ID,
                "homesteadBlock": 0, "eip150Block": 0, "eip155Block": 0, "eip158Block": 0,
                "byzantiumBlock": 0, "constantinopleBlock": 0, "petersburgBlock": 0,
                "istanbulBlock": 0, "berlinBlock": 0, "londonBlock": 0,
                "terminalTotalDifficulty": 0, "terminalTotalDifficultyPassed": true,
                "shanghaiTime": 0, "cancunTime": 0, "pragueTime": 0
            },
            "nonce": "0x0",
            "timestamp": "0x0",
            "extraData": "0x",
            "gasLimit": GAS_LIMIT_HEX,
            "difficulty": "0x0",
            "mixHash": format!("{:#x}", B256::ZERO),
            "coinbase": "0x0000000000000000000000000000000000000000",
            "alloc": { format!("{addr:#x}"): { "balance": "0x0", "nonce": "0x2a" } },
            "number": "0x0",
            "gasUsed": "0x0",
            "parentHash": format!("{:#x}", B256::ZERO),
            "baseFeePerGas": "0x3b9aca00"
        });
        let genesis_path = dir.path().join("genesis.json");
        std::fs::write(&genesis_path, genesis.to_string()).unwrap();
        let ex = RethExecutor::new(RethConfig {
            datadir: dir.path().join("db"),
            genesis_path,
            block_gas_limit: GAS_LIMIT,
        })
        .unwrap();

        assert_eq!(
            ex.nonce(addr),
            0x2a,
            "an account's genesis-preset nonce must be visible even though no tx from it has ever \
             gone through this executor"
        );
    }

    /// After a CREATE tx, the sender's nonce read back mid-block (the
    /// block still open, `pending` still `Some`) must equal state — read from `pending.state`'s own
    /// cache, the exact entry `CREATE`'s address derivation itself reads, so there is no separate path
    /// for the two to fall out of sync.
    #[tokio::test]
    async fn nonce_after_a_create_tx_reads_back_from_pending_state() {
        let _serial = serial_mdbx().await;
        let dir = tempdir().unwrap();
        let signer = PrivateKeySigner::random();
        let config = test_config(
            dir.path(),
            &[(signer.address(), U256::from(10u128).pow(U256::from(20u8)))],
        );
        let mut ex = RethExecutor::new(config).unwrap();
        ex.open_block(block_env(&ex, 1)).await.unwrap();
        assert_eq!(ex.nonce(signer.address()), 0);

        // Minimal CREATE: init code RETURN(0, 0) — deploys an account with empty code.
        let init_code = Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xf3]);
        let tx = TxEip1559 {
            chain_id: CHAIN_ID,
            nonce: 0,
            gas_limit: 100_000,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: TxKind::Create,
            value: U256::ZERO,
            access_list: Default::default(),
            input: init_code,
        };
        let sig_hash = tx.signature_hash();
        let signature = signer.sign_hash_sync(&sig_hash).unwrap();
        let signed = tx.into_signed(signature);
        let raw = Bytes::from(alloy_consensus::TxEnvelope::from(signed).encoded_2718());

        let outcome = ex
            .execute_sub_block(&[raw], unbounded_limits())
            .await
            .unwrap();
        assert_eq!(outcome.included.len(), 1, "the CREATE tx must be included");
        assert!(outcome.rejected.is_empty());

        assert_eq!(
            ex.nonce(signer.address()),
            1,
            "the sender's nonce must read back as 1 immediately after the CREATE, mid-block"
        );
    }

    /// A real revm rejection classifies as `Reason::NonceTooLow` when
    /// the tx's nonce is behind the sender's real next nonce.
    #[tokio::test]
    async fn stale_nonce_classifies_as_nonce_too_low() {
        let _serial = serial_mdbx().await;
        let dir = tempdir().unwrap();
        let signer = PrivateKeySigner::random();
        let config = test_config(
            dir.path(),
            &[(signer.address(), U256::from(10u128).pow(U256::from(20u8)))],
        );
        let mut ex = RethExecutor::new(config).unwrap();
        ex.open_block(block_env(&ex, 1)).await.unwrap();

        // Nonce 0 twice in the same sub-block: the second is stale the instant the first commits.
        let tx0 = signed_transfer(&signer, 0);
        let replay = signed_transfer(&signer, 0);
        let outcome = ex
            .execute_sub_block(&[tx0, replay], unbounded_limits())
            .await
            .unwrap();
        assert_eq!(outcome.included.len(), 1);
        assert_eq!(outcome.rejected.len(), 1);
        assert_eq!(
            outcome.rejected[0].reason,
            Reason::NonceTooLow { expected: 1 }
        );
    }

    /// A real revm rejection classifies as `Reason::NonceTooHigh` when
    /// the tx's nonce is ahead of the sender's real next nonce.
    #[tokio::test]
    async fn future_nonce_classifies_as_nonce_too_high() {
        let _serial = serial_mdbx().await;
        let dir = tempdir().unwrap();
        let signer = PrivateKeySigner::random();
        let config = test_config(
            dir.path(),
            &[(signer.address(), U256::from(10u128).pow(U256::from(20u8)))],
        );
        let mut ex = RethExecutor::new(config).unwrap();
        ex.open_block(block_env(&ex, 1)).await.unwrap();

        let skip_ahead = signed_transfer(&signer, 5); // real next nonce is 0
        let outcome = ex
            .execute_sub_block(&[skip_ahead], unbounded_limits())
            .await
            .unwrap();
        assert!(outcome.included.is_empty());
        assert_eq!(outcome.rejected.len(), 1);
        assert_eq!(
            outcome.rejected[0].reason,
            Reason::NonceTooHigh { expected: 0 }
        );
    }

    /// A real revm rejection classifies as `Reason::IntrinsicGas` when
    /// the declared `gas_limit` is below the tx's own intrinsic floor (21_000 for a plain transfer).
    #[tokio::test]
    async fn gas_limit_below_intrinsic_floor_classifies_as_intrinsic_gas() {
        let _serial = serial_mdbx().await;
        let dir = tempdir().unwrap();
        let signer = PrivateKeySigner::random();
        let config = test_config(
            dir.path(),
            &[(signer.address(), U256::from(10u128).pow(U256::from(20u8)))],
        );
        let mut ex = RethExecutor::new(config).unwrap();
        ex.open_block(block_env(&ex, 1)).await.unwrap();

        let tx = TxEip1559 {
            chain_id: CHAIN_ID,
            nonce: 0,
            gas_limit: 1_000, // below the 21_000 intrinsic floor
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            access_list: Default::default(),
            input: Bytes::new(),
        };
        let sig_hash = tx.signature_hash();
        let signature = signer.sign_hash_sync(&sig_hash).unwrap();
        let signed = tx.into_signed(signature);
        let raw = Bytes::from(alloy_consensus::TxEnvelope::from(signed).encoded_2718());

        let outcome = ex
            .execute_sub_block(&[raw], unbounded_limits())
            .await
            .unwrap();
        assert!(outcome.included.is_empty());
        assert_eq!(outcome.rejected.len(), 1);
        assert_eq!(outcome.rejected[0].reason, Reason::IntrinsicGas);
    }

    /// A real revm rejection classifies as `Reason::GasLimitExceeded`
    /// when the declared `gas_limit` exceeds the block's own gas limit.
    #[tokio::test]
    async fn gas_limit_above_block_limit_classifies_as_gas_limit_exceeded() {
        let _serial = serial_mdbx().await;
        let dir = tempdir().unwrap();
        let signer = PrivateKeySigner::random();
        let config = test_config(
            dir.path(),
            &[(signer.address(), U256::from(10u128).pow(U256::from(20u8)))],
        );
        let mut ex = RethExecutor::new(config).unwrap();
        let env = block_env(&ex, 1); // gas_limit = genesis gasLimit, 0x2540be400 = 10_000_000_000
        ex.open_block(env.clone()).await.unwrap();

        let tx = TxEip1559 {
            chain_id: CHAIN_ID,
            nonce: 0,
            gas_limit: env.gas_limit + 1, // exceeds the block's own gas limit
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            access_list: Default::default(),
            input: Bytes::new(),
        };
        let sig_hash = tx.signature_hash();
        let signature = signer.sign_hash_sync(&sig_hash).unwrap();
        let signed = tx.into_signed(signature);
        let raw = Bytes::from(alloy_consensus::TxEnvelope::from(signed).encoded_2718());

        let outcome = ex
            .execute_sub_block(&[raw], unbounded_limits())
            .await
            .unwrap();
        assert!(outcome.included.is_empty());
        assert_eq!(outcome.rejected.len(), 1);
        assert_eq!(outcome.rejected[0].reason, Reason::GasLimitExceeded);
    }

    /// Measurement (target: 100M gas/s per host executor cap): sub-block execution
    /// gas/s on this host for 21k-gas transfers, one sub-block, real revm execution against the
    /// 200101 genesis. Debug builds are unrepresentative (unoptimized secp256k1 recover/keccak —
    /// `tests/e2e.rs` documents the same trap for the mock executor's own p99 numbers), so this is
    /// `#[ignore]`d and run explicitly: `cargo test -p rome-zk-executor-reth --release -- --ignored
    /// --nocapture gas_per_second`.
    #[ignore = "performance-sensitive; run with --release --nocapture, see comment above"]
    #[tokio::test]
    async fn gas_per_second_for_21k_gas_transfers() {
        let _serial = serial_mdbx().await;
        const N: u64 = 5_000;
        let dir = tempdir().unwrap();
        let signer = PrivateKeySigner::random();
        let config = test_config(
            dir.path(),
            &[(signer.address(), U256::from(10u128).pow(U256::from(24u8)))],
        );
        let mut ex = RethExecutor::new(config).unwrap();
        ex.open_block(block_env(&ex, 1)).await.unwrap();
        let txs: Vec<Bytes> = (0..N).map(|n| signed_transfer(&signer, n)).collect();

        let start = Instant::now();
        let outcome = ex
            .execute_sub_block(&txs, unbounded_limits())
            .await
            .unwrap();
        let elapsed = start.elapsed();

        assert_eq!(outcome.included.len(), N as usize);
        let gas_per_sec = outcome.gas_used as f64 / elapsed.as_secs_f64();
        println!(
            "MEASURED: {N} transfers ({} gas) in {elapsed:?} = {gas_per_sec:.0} gas/s (target: 100,000,000 gas/s)",
            outcome.gas_used
        );
    }

    /// Measurement (block seal must not stall the 50 ms cadence; this crate's module doc: seal-time
    /// re-execution "doubles execution cost"): wall-clock time
    /// for `seal_block` alone — the one-block re-execution + `BlockBuilder::finish`'s state-root
    /// computation — with 100 transfers spread over the block's 20 sub-blocks (5 per sub-block, a
    /// representative per-block load, not this host's ceiling).
    #[ignore = "performance-sensitive; run with --release --nocapture, see comment above"]
    #[tokio::test]
    async fn seal_block_wall_time_for_a_representative_block() {
        let _serial = serial_mdbx().await;
        init_tracing_for_measurement();
        const TXS_PER_SUB_BLOCK: u64 = 5;
        let dir = tempdir().unwrap();
        let signer = PrivateKeySigner::random();
        let config = test_config(
            dir.path(),
            &[(signer.address(), U256::from(10u128).pow(U256::from(24u8)))],
        );
        let mut ex = RethExecutor::new(config).unwrap();
        ex.open_block(block_env(&ex, 1)).await.unwrap();

        let mut nonce = 0u64;
        for _ in 0..20u16 {
            let txs: Vec<Bytes> = (0..TXS_PER_SUB_BLOCK)
                .map(|_| {
                    let tx = signed_transfer(&signer, nonce);
                    nonce += 1;
                    tx
                })
                .collect();
            ex.execute_sub_block(&txs, unbounded_limits())
                .await
                .unwrap();
        }

        let start = Instant::now();
        ex.seal_block(BlockSealInputs {
            block: 1,
            timestamp_secs: ex_genesis_timestamp(&ex) + 1,
            sub_block_header_hashes: vec![B256::repeat_byte(1); 20],
            total_gas_used: nonce * 21_000,
        })
        .await
        .unwrap();
        let elapsed = start.elapsed();

        println!(
            "MEASURED: seal_block for {} txs (re-execution + state root) took {elapsed:?} \
             (50 ms sub-block cadence budget; seal runs once per 20 sub-blocks / 1 s block, off \
             the 50 ms tick)",
            nonce
        );
    }

    /// Measurement: the same measurement as
    /// `seal_block_wall_time_for_a_representative_block`, but at the design-load point itself —
    /// 5,000 transfers (5k tx/s) spread evenly over the block's 20 sub-blocks (250 per
    /// sub-block), 105M gas total. This is the number that decides whether `seal_block` must move off
    /// the actor's 50 ms tick (if it exceeds ~10 ms, block
    /// sealing must move off the actor's tick).
    #[ignore = "performance-sensitive; run with --release --nocapture, see comment above"]
    #[tokio::test]
    async fn seal_block_wall_time_at_the_5k_tx_design_load_point() {
        let _serial = serial_mdbx().await;
        init_tracing_for_measurement();
        const TXS_PER_SUB_BLOCK: u64 = 250;
        let dir = tempdir().unwrap();
        let signer = PrivateKeySigner::random();
        let config = test_config(
            dir.path(),
            &[(signer.address(), U256::from(10u128).pow(U256::from(24u8)))],
        );
        let mut ex = RethExecutor::new(config).unwrap();
        ex.open_block(block_env(&ex, 1)).await.unwrap();

        let mut nonce = 0u64;
        for _ in 0..20u16 {
            let txs: Vec<Bytes> = (0..TXS_PER_SUB_BLOCK)
                .map(|_| {
                    let tx = signed_transfer(&signer, nonce);
                    nonce += 1;
                    tx
                })
                .collect();
            ex.execute_sub_block(&txs, unbounded_limits())
                .await
                .unwrap();
        }

        let start = Instant::now();
        ex.seal_block(BlockSealInputs {
            block: 1,
            timestamp_secs: ex_genesis_timestamp(&ex) + 1,
            sub_block_header_hashes: vec![B256::repeat_byte(1); 20],
            total_gas_used: nonce * 21_000,
        })
        .await
        .unwrap();
        let elapsed = start.elapsed();

        println!(
            "MEASURED: seal_block at the 5k-tx design load point ({} txs, {} gas) took {elapsed:?} \
             (50 ms sub-block cadence budget; ~10 ms is the threshold for moving it off-tick)",
            nonce,
            nonce * 21_000
        );
    }

    /// `seal_block` must never re-run `decode_and_recover` over the block's
    /// txs at seal time — `pending.included` already carries each tx as a `Recovered<TransactionSigned>`
    /// (computed once, by `execute_sub_block`, when the tx was first previewed), and `seal_block`'s
    /// re-execution loop must consume that directly. This is a HARD, decisive assertion, not a printed
    /// measurement (a reintroduced redundant recovery was measured at ~113ms for 5,000 txs
    /// — as expensive as the re-execution + persistence write together): if a future change
    /// accidentally reintroduces `decode_and_recover` at seal time, `elapsed` below blows past the 10ms
    /// budget by an order of magnitude, not by a noise-sensitive margin — so this test carries real
    /// signal even on a shared/noisy host, unlike a tight timing assertion would.
    #[ignore = "performance-sensitive; run with --release --nocapture, see comment above"]
    #[tokio::test]
    async fn seal_block_never_redecodes_and_stays_under_10ms_at_5k_txs() {
        let _serial = serial_mdbx().await;
        const TXS_PER_SUB_BLOCK: u64 = 250;
        let dir = tempdir().unwrap();
        let signer = PrivateKeySigner::random();
        let config = test_config(
            dir.path(),
            &[(signer.address(), U256::from(10u128).pow(U256::from(24u8)))],
        );
        let mut ex = RethExecutor::new(config).unwrap();
        ex.open_block(block_env(&ex, 1)).await.unwrap();

        let mut nonce = 0u64;
        for _ in 0..20u16 {
            let txs: Vec<Bytes> = (0..TXS_PER_SUB_BLOCK)
                .map(|_| {
                    let tx = signed_transfer(&signer, nonce);
                    nonce += 1;
                    tx
                })
                .collect();
            ex.execute_sub_block(&txs, unbounded_limits())
                .await
                .unwrap();
        }
        assert_eq!(nonce, 5_000, "the design's own 5k-tx load point");

        let start = Instant::now();
        ex.seal_block(BlockSealInputs {
            block: 1,
            timestamp_secs: ex_genesis_timestamp(&ex) + 1,
            sub_block_header_hashes: vec![B256::repeat_byte(1); 20],
            total_gas_used: nonce * 21_000,
        })
        .await
        .unwrap();
        let elapsed = start.elapsed();

        println!(
            "MEASURED: seal_block foreground latency at 5,000 txs = {elapsed:?} (budget: 10ms; a \
             redundant decode_and_recover pass would cost ~113ms here per an earlier \
             measurement)"
        );
        assert!(
            elapsed < Duration::from_millis(10),
            "seal_block foreground latency ({elapsed:?}) exceeded the 10ms design budget at the \
             5k-tx load point — if this is because decode_and_recover is being re-run over the \
             block's txs at seal time, that redundant recovery pass must be removed (reuse \
             pending.included's already-Recovered txs, as seal_block's own body does today)"
        );
    }

    /// Does `seal_block`'s state-root cost grow unacceptably as the chain's
    /// touched-account set grows, given this crate persists only hashed state, never trie NODES (see
    /// `src/lib.rs`'s module doc, "What is/isn't persisted") — so `finish()`'s
    /// `state_root_with_updates` recomputes the WHOLE trie from scratch every block, with nothing
    /// persisted to incrementally update against. Seals 1,000 blocks, 50 transfers/block to a FRESH
    /// recipient address each (50,000 distinct touched accounts by the end), and reports `seal_block`'s wall time at
    /// blocks 1, 500 and 1,000 — the curve, not a single number.
    #[ignore = "performance-sensitive AND slow (1,000 blocks); run with --release --nocapture"]
    #[tokio::test]
    async fn seal_block_wall_time_curve_at_50k_touched_accounts_without_persisted_trie_nodes() {
        let _serial = serial_mdbx().await;
        init_tracing_for_measurement();
        const BLOCKS: u64 = 1_000;
        const TRANSFERS_PER_BLOCK: u64 = 50;

        let dir = tempdir().unwrap();
        let signer = PrivateKeySigner::random();
        let config = test_config(
            dir.path(),
            &[(signer.address(), U256::from(10u128).pow(U256::from(30u8)))],
        );
        let mut ex = RethExecutor::new(config).unwrap();

        let mut nonce = 0u64;
        let mut fresh_addr_seed = 0u64;
        // A fresh chain's first sealed block is design 1.
        for b in 1..=BLOCKS {
            let env = block_env(&ex, b);
            ex.open_block(env.clone()).await.unwrap();
            for _ in 0..20u16 {
                let txs: Vec<Bytes> = (0..(TRANSFERS_PER_BLOCK / 20))
                    .map(|_| {
                        fresh_addr_seed += 1;
                        let mut addr_bytes = [0u8; 20];
                        addr_bytes[12..].copy_from_slice(&fresh_addr_seed.to_be_bytes());
                        let tx = signed_transfer_to(&signer, nonce, Address::from(addr_bytes));
                        nonce += 1;
                        tx
                    })
                    .collect();
                ex.execute_sub_block(&txs, unbounded_limits())
                    .await
                    .unwrap();
            }

            let seal_started = Instant::now();
            ex.seal_block(BlockSealInputs {
                block: b,
                timestamp_secs: env.timestamp_secs,
                sub_block_header_hashes: vec![B256::repeat_byte(1); 20],
                total_gas_used: TRANSFERS_PER_BLOCK * 21_000,
            })
            .await
            .unwrap();
            let seal_elapsed = seal_started.elapsed();

            let block_number = b;
            if block_number == 1 || block_number == 500 || block_number == BLOCKS {
                println!(
                    "MEASURED: seal_block wall time at block {block_number} ({} cumulative touched \
                     accounts, hashed state only — no persisted trie nodes) = {seal_elapsed:?}",
                    block_number * TRANSFERS_PER_BLOCK
                );
            }
        }
    }

    /// Regression guard: `open_block` must NEVER wait for a persist,
    /// however slow — this was the root cause of a residual p99 latency (`open_block` was calling
    /// `join_pending_persist().await` before building the next block's preview state, so the FIRST
    /// sub-block of every block paid the previous block's full MDBX-commit latency). Seals a block
    /// whose persist is artificially slowed to 300ms (`set_persist_delay_for_test`, a test-only hook
    /// — never a production knob), then immediately opens the next block and executes its first
    /// sub-block: both together must complete in under 5ms, an order of magnitude below the 300ms
    /// delay, so this has real signal even on a noisy host, exactly like the existing
    /// `seal_block_never_redecodes_and_stays_under_10ms_at_5k_txs` guard's own margin reasoning. If
    /// `open_block` ever re-introduces an await on the persist handle, this assertion fails by ~60x,
    /// not by a flaky margin.
    #[tokio::test]
    async fn open_block_never_waits_for_a_slow_persist() {
        let _serial = serial_mdbx().await;
        let dir = tempdir().unwrap();
        let signer = PrivateKeySigner::random();
        let config = test_config(
            dir.path(),
            &[(signer.address(), U256::from(10u128).pow(U256::from(20u8)))],
        );
        let mut ex = RethExecutor::new(config).unwrap();
        ex.set_persist_delay_for_test(Duration::from_millis(300));

        let block0_env = block_env(&ex, 1);
        ex.open_block(block0_env.clone()).await.unwrap();
        for _ in 0..20u16 {
            ex.execute_sub_block(&[], unbounded_limits()).await.unwrap();
        }
        ex.seal_block(BlockSealInputs {
            block: 1,
            timestamp_secs: block0_env.timestamp_secs,
            sub_block_header_hashes: vec![B256::repeat_byte(1); 20],
            total_gas_used: 0,
        })
        .await
        .unwrap();

        let started = Instant::now();
        ex.open_block(block_env(&ex, 2)).await.unwrap();
        ex.execute_sub_block(&[], unbounded_limits()).await.unwrap();
        let elapsed = started.elapsed();

        println!(
            "MEASURED: open_block + first execute_sub_block of the NEXT block took \
             {elapsed:?} while block 0's persist is artificially delayed 300ms in the background"
        );
        assert!(
            elapsed < Duration::from_millis(5),
            "open_block + first execute_sub_block took {elapsed:?} — this must never approach the \
             300ms persist delay (open_block must never await the previous block's \
             persist — that was the actual root cause of the residual p99 measured earlier, not the \
             tick model itself)"
        );
    }

    /// A transfer in block N and its recipient spending in block N+1's
    /// very first sub-block must both succeed, proving state is visible through the in-memory
    /// overlay even while block N's persist (artificially slowed) has not landed on disk yet — the
    /// recipient's balance/nonce would be invisible (or stale) if `open_block(N+1)` built its
    /// preview `State` over `factory.latest()` alone, with no overlay.
    #[tokio::test]
    async fn state_visible_through_overlay_across_a_pending_persist() {
        let _serial = serial_mdbx().await;
        let dir = tempdir().unwrap();
        let sender = PrivateKeySigner::random();
        let recipient = PrivateKeySigner::random();
        let config = test_config(
            dir.path(),
            &[(sender.address(), U256::from(10u128).pow(U256::from(20u8)))],
        );
        let mut ex = RethExecutor::new(config).unwrap();
        ex.set_persist_delay_for_test(Duration::from_millis(300));

        // Block 0: sender funds the recipient, and pays its own gas.
        let block0_env = block_env(&ex, 1);
        ex.open_block(block0_env.clone()).await.unwrap();
        let fund_amount = U256::from(10u128).pow(U256::from(19u8));
        let fund_tx = signed_transfer_value(&sender, 0, recipient.address(), fund_amount);
        for i in 0..20u16 {
            let txs = if i == 0 {
                vec![fund_tx.clone()]
            } else {
                vec![]
            };
            let outcome = ex
                .execute_sub_block(&txs, unbounded_limits())
                .await
                .unwrap();
            if i == 0 {
                assert_eq!(
                    outcome.included.len(),
                    1,
                    "the funding transfer must be included"
                );
            }
        }
        ex.seal_block(BlockSealInputs {
            block: 1,
            timestamp_secs: block0_env.timestamp_secs,
            sub_block_header_hashes: vec![B256::repeat_byte(1); 20],
            total_gas_used: 21_000,
        })
        .await
        .unwrap();

        // Block 1, sub-block 0: the recipient spends what it just received — must succeed even
        // though block 0's persist (300ms) is still in flight.
        ex.open_block(block_env(&ex, 2)).await.unwrap();
        let spend_tx = signed_transfer(&recipient, 0);
        let outcome = ex
            .execute_sub_block(&[spend_tx], unbounded_limits())
            .await
            .unwrap();
        assert_eq!(
            outcome.included.len(),
            1,
            "the recipient's spend must be included via the overlay, proving state visibility \
             across a still-pending persist: outcome was {outcome:?}"
        );
        assert!(
            outcome.rejected.is_empty(),
            "unexpected rejection: {:?}",
            outcome.rejected
        );
    }

    /// Once a block's persist actually completes, its overlay must be
    /// dropped and `factory.latest()` ALONE (bypassing the overlay entirely) must already agree with
    /// it — proving the overlay is a transient bridge over the commit latency, not a second, drifting
    /// source of truth. Checked two ways: (1) `not_yet_confirmed` is empty right after `flush()`
    /// (nothing left to drop), and (2) a FRESH `RethExecutor` reopened over the SAME datadir — opened
    /// only after `ex` itself is dropped, since libmdbx's own env-locking in this environment does not
    /// support two independently-opened env handles on the same datadir path alive at once within one
    /// process — reads the persisted nonce with no overlay involved at all.
    #[tokio::test]
    async fn overlay_drops_after_persist_completes_and_disk_alone_agrees() {
        let _serial = serial_mdbx().await;
        let dir = tempdir().unwrap();
        let signer = PrivateKeySigner::random();
        let config = test_config(
            dir.path(),
            &[(signer.address(), U256::from(10u128).pow(U256::from(20u8)))],
        );
        let mut ex = RethExecutor::new(config.clone()).unwrap();
        ex.set_persist_delay_for_test(Duration::from_millis(50));

        // A fresh chain's first sealed block is design 1.
        let block1_env = block_env(&ex, 1);
        ex.open_block(block1_env.clone()).await.unwrap();
        let tx = signed_transfer(&signer, 0);
        for i in 0..20u16 {
            let txs = if i == 0 { vec![tx.clone()] } else { vec![] };
            ex.execute_sub_block(&txs, unbounded_limits())
                .await
                .unwrap();
        }
        ex.seal_block(BlockSealInputs {
            block: 1,
            timestamp_secs: block1_env.timestamp_secs,
            sub_block_header_hashes: vec![B256::repeat_byte(1); 20],
            total_gas_used: 21_000,
        })
        .await
        .unwrap();

        // A graceful flush waits for the (artificially 50ms-delayed) persist to actually land.
        ex.flush().await.unwrap();
        assert!(
            ex.not_yet_confirmed.is_empty(),
            "flush() must drain every outstanding persist — nothing left to overlay"
        );

        // `factory.latest()` alone, through this SAME executor's own factory handle (no overlay
        // consulted, since `nonce()` only falls through to it once `not_yet_confirmed` is empty, which
        // the assertion above just proved) must already show the real, on-disk nonce.
        assert_eq!(
            ex.nonce(signer.address()),
            1,
            "with not_yet_confirmed empty, nonce() reads factory.latest() alone and must already \
             agree with the flushed persist"
        );

        // A completely independent read handle over the SAME datadir, opened only once `ex` (and its
        // own live factory handle) is gone, must independently agree.
        drop(ex);
        let independent = RethExecutor::new(config).unwrap();
        assert_eq!(
            independent.last_persisted_block(),
            Some(1),
            "block 1's persist must be durable on disk once flush() has returned"
        );
        assert_eq!(
            independent.nonce(signer.address()),
            1,
            "a fresh executor reading disk alone (no overlay) must already agree with the flushed \
             persist"
        );
    }

    /// Two blocks' persists genuinely still in flight — neither committed
    /// to MDBX yet — and tail replay must repair BOTH, not just one. Provenance that neither has
    /// landed is deterministic, not a timing race: reading straight off `ex`'s OWN `factory.latest()`
    /// (bypassing the overlay entirely — this crate has no separate handle for that, so this test
    /// reaches into the private field directly, same module) while both delayed persists are still
    /// sleeping/waiting must show the PRE-block-0 nonce (0), never 1 or 2 — this is what the fixed
    /// disk actually holds, independent of anything `not_yet_confirmed`/the overlay is bridging, and is
    /// exactly the view a process genuinely killed before either commit landed would produce, without
    /// racing a real SIGKILL's timing the way the sequencer-level
    /// `kill_9_right_after_a_block_seals_then_restart_reaches_the_same_state` (rome-zk-sequencer
    /// `tests/e2e_reth_node.rs`) does.
    #[tokio::test]
    async fn tail_replay_repairs_two_still_unpersisted_blocks() {
        let _serial = serial_mdbx().await;
        let dir = tempdir().unwrap();
        let signer = PrivateKeySigner::random();
        let funded = &[(signer.address(), U256::from(10u128).pow(U256::from(20u8)))];
        let config = test_config(dir.path(), funded);

        let mut ex = RethExecutor::new(config.clone()).unwrap();
        // Long enough that neither block's persist has committed by the time this test checks below
        // (block 1's own persist task waits INSIDE its spawn_blocking closure for block 0's to finish
        // before it even attempts its own write — see PendingPersist's doc — so at this point neither
        // has started its actual MDBX write at all).
        ex.set_persist_delay_for_test(Duration::from_secs(5));

        async fn seal_one_block(
            ex: &mut RethExecutor,
            signer: &PrivateKeySigner,
            block: u64,
            nonce: u64,
        ) {
            let env = block_env(ex, block);
            ex.open_block(env.clone()).await.unwrap();
            for i in 0..20u16 {
                let txs = if i == 0 {
                    vec![signed_transfer(signer, nonce)]
                } else {
                    vec![]
                };
                ex.execute_sub_block(&txs, unbounded_limits())
                    .await
                    .unwrap();
            }
            ex.seal_block(BlockSealInputs {
                block,
                timestamp_secs: env.timestamp_secs,
                sub_block_header_hashes: vec![B256::repeat_byte(block as u8 + 1); 20],
                total_gas_used: 21_000,
            })
            .await
            .unwrap();
        }

        seal_one_block(&mut ex, &signer, 1, 0).await;
        seal_one_block(&mut ex, &signer, 2, 1).await;
        let live_head = ex.head();
        assert_eq!(
            ex.not_yet_confirmed.len(),
            2,
            "both block 0's and block 1's persists must still be genuinely outstanding at this \
             point (both artificially delayed 5s; block 1's own task is still blocked waiting for \
             block 0's to finish before it even attempts its own write)"
        );

        // Neither block has committed to MDBX yet — see this test's doc comment. Read straight off
        // `ex`'s own `factory.latest()`, bypassing the overlay/not_yet_confirmed path entirely.
        let disk_only_nonce = ex
            .factory
            .latest()
            .unwrap()
            .account_nonce(&signer.address())
            .unwrap()
            .unwrap_or(0);
        assert_eq!(
            disk_only_nonce, 0,
            "factory.latest() alone (bypassing the overlay) must still show the pre-block-0 nonce — \
             proving neither block 0 nor block 1 has actually committed to MDBX yet"
        );

        // Simulate the crash: drop `ex` without flush() — its background persist tasks (both still
        // sleeping out their artificial delay) keep running independently (they hold their own
        // ProviderFactory clone), but this test does not wait for or depend on them landing.
        drop(ex);

        // Tail replay (mirrored here the same way `crash_after_a_sealed_block_then_replay_reaches_the_identical_head`
        // above does, since this crate has no log/recovery machinery of its own) must re-derive both
        // blocks' worth of state from scratch and reach the identical head the live run produced.
        let mut replayed = RethExecutor::new(RethConfig {
            datadir: dir.path().join("replay-db"),
            genesis_path: config.genesis_path.clone(),
            block_gas_limit: GAS_LIMIT,
        })
        .unwrap();
        seal_one_block(&mut replayed, &signer, 1, 0).await;
        seal_one_block(&mut replayed, &signer, 2, 1).await;

        assert_eq!(
            replayed.head(),
            live_head,
            "replaying both not-yet-persisted blocks must reproduce the identical head"
        );
    }

    /// Mutation target: `open_block`
    /// must refuse a `BlockEnv.number` that is not exactly the real reth height + 1 — the premise
    /// "BlockEnv.number == header.number" becomes unconstructable at the source here,
    /// rather than merely arranged by every caller happening to agree. A fresh datadir's
    /// `canonical_parent.number()` is genesis (0), so the only valid first `env.number` is 1; this test
    /// opens at parent + 2 and expects a named `EnvMismatch`, never a silently-accepted mis-numbered
    /// block.
    #[tokio::test]
    async fn open_block_refuses_an_env_number_that_is_not_parent_plus_one() {
        let _serial = serial_mdbx().await;
        let dir = tempdir().unwrap();
        let signer = PrivateKeySigner::random();
        let config = test_config(
            dir.path(),
            &[(signer.address(), U256::from(10u128).pow(U256::from(20u8)))],
        );
        let mut ex = RethExecutor::new(config).unwrap();
        assert_eq!(
            ex.canonical_parent.number(),
            0,
            "a fresh datadir's parent is genesis"
        );

        // parent (0) + 2 = 2, skipping the only valid next height (1).
        let err = ex
            .open_block(block_env(&ex, 2))
            .await
            .expect_err("opening at parent + 2 must be refused, not silently accepted");
        assert!(matches!(err, ExecutorError::EnvMismatch(_)), "{err:?}");
        assert!(
            ex.pending.is_none(),
            "a refused open_block must not leave a block open"
        );

        // The valid number (parent + 1 = 1) must still succeed afterwards — this is a refusal, not a
        // permanent wedge.
        ex.open_block(block_env(&ex, 1))
            .await
            .expect("the correct env.number must still open a block after a refused attempt");
    }

    /// Mutation target: `seal_block`
    /// must refuse when the header it just built carries a different number than the caller's own
    /// `BlockSealInputs::block` — the second half of binding the numbering premise at the source
    /// (`open_block`'s check above ties `env.number` to the real height; this ties the SEALED header
    /// back to what the caller thinks it sealed).
    #[tokio::test]
    async fn seal_block_refuses_when_inputs_block_disagrees_with_the_sealed_header_number() {
        let _serial = serial_mdbx().await;
        let dir = tempdir().unwrap();
        let signer = PrivateKeySigner::random();
        let config = test_config(
            dir.path(),
            &[(signer.address(), U256::from(10u128).pow(U256::from(20u8)))],
        );
        let mut ex = RethExecutor::new(config).unwrap();
        let env = block_env(&ex, 1);
        ex.open_block(env.clone()).await.unwrap();
        for _ in 0..20u16 {
            ex.execute_sub_block(&[], unbounded_limits()).await.unwrap();
        }

        // The block was opened at env.number = 1 (a real reth header 1 will be sealed), but the caller
        // claims it is sealing block 2 — must be refused, not silently recorded as block 2's head.
        let err = ex
            .seal_block(BlockSealInputs {
                block: 2,
                timestamp_secs: env.timestamp_secs,
                sub_block_header_hashes: vec![B256::repeat_byte(1); 20],
                total_gas_used: 0,
            })
            .await
            .expect_err("a mismatched BlockSealInputs.block must be refused, not silently sealed");
        assert!(matches!(err, ExecutorError::EnvMismatch(_)), "{err:?}");
    }

    /// Helper shared by the torn-static-file tests below: seals sequencer blocks `from..=to`
    /// against `ex`, matching every other test's own open/execute-20-empty-sub-blocks/seal shape.
    async fn seal_blocks_for_torn_tests(ex: &mut RethExecutor, from: u64, to: u64) {
        for b in from..=to {
            let env = block_env(ex, b);
            ex.open_block(env.clone()).await.unwrap();
            for _ in 0..20u16 {
                ex.execute_sub_block(&[], unbounded_limits()).await.unwrap();
            }
            ex.seal_block(BlockSealInputs {
                block: b,
                timestamp_secs: env.timestamp_secs,
                sub_block_header_hashes: vec![B256::repeat_byte(b as u8); 20],
                total_gas_used: 0,
            })
            .await
            .unwrap();
        }
    }

    /// Forward heal at open. Builds the
    /// FORWARD-HEAL shape with the real producer (never a hand-built fixture): seal 3 blocks and flush
    /// (durable), snapshot the headers segment's committed config, seal one more block and flush, then
    /// restore the snapshot over the new config — a `.conf` one row behind a COMMITTED MDBX (`Finish`
    /// at block 4), with block 4's header row durable in the segment's data+offsets files.
    ///
    /// Under reth's verified commit order (static data+offsets `sync_all` → `.conf` tmp +
    /// fsync + rename + directory fsync → RocksDB → MDBX commit, one thread) a kill CANNOT leave this
    /// shape behind — the rename is durable strictly before MDBX advances. The forward heal is kept as
    /// defence in depth for it (`static_heal` module doc); the shape a real kill does produce is
    /// pinned by `real_kill_shape_*` in `rome-zk-sequencer/tests/reth_torn_persist_resume.rs`.
    ///
    /// An earlier revision of `RethExecutor::new` healed this shape BACKWARD to block 3, discarding
    /// the durable block-4 row, and this test asserted that as the correct outcome. It now asserts the
    /// forward heal: head 4, `sealed_header(4)` present, `check_consistency()` clean afterwards.
    /// Mutation: comment out this constructor's `static_heal::commit_forward_heal` call (or force
    /// `forward_heal` to `ForwardHealOutcome::Consistent` unconditionally) and this test goes back to
    /// head 3 — i.e. it fails against head 4.
    #[tokio::test]
    async fn torn_headers_conf_heals_forward_at_open_and_resumes_from_the_healed_block() {
        let _serial = serial_mdbx().await;
        let dir = tempdir().unwrap();
        let signer = PrivateKeySigner::random();
        let config = test_config(
            dir.path(),
            &[(signer.address(), U256::from(10u128).pow(U256::from(20u8)))],
        );

        let mut ex = RethExecutor::new(config.clone()).unwrap();
        seal_blocks_for_torn_tests(&mut ex, 1, 3).await;
        ex.flush().await.unwrap();
        drop(ex);

        let conf_path = config
            .datadir
            .join("static_files")
            .join("static_file_headers_0_499999.conf");
        // Snapshot with NOTHING holding the datadir open (matching a real restart's own quiescent
        // point) — reading the config bytes back while a `RethExecutor` is still live over the same
        // static file segment is not equivalent to what is truly durable on disk.
        let snapshot_at_3 = std::fs::read(&conf_path).unwrap();

        let mut ex = RethExecutor::new(config.clone()).unwrap();
        assert_eq!(
            ex.last_persisted_block(),
            Some(3),
            "sanity: 3 sealed blocks must already be durable before the torn shape is built"
        );
        seal_blocks_for_torn_tests(&mut ex, 4, 4).await;
        ex.flush().await.unwrap();
        drop(ex);

        // The torn shape: MDBX (and every OTHER static file segment) committed through block 4;
        // only the headers segment's config is rolled back to its pre-block-4 snapshot.
        std::fs::write(&conf_path, &snapshot_at_3).unwrap();

        // Control varies the world (never trust the scenario is real without checking): open the
        // raw factory and confirm the precondition this test depends on — MDBX's own checkpoint
        // already reports block 4, but the headers segment does not yet expose it. This is a
        // PURE READ: it must never call `factory.check_consistency()` here, which would truncate
        // the very row this test proves gets healed FORWARD before the "real" `RethExecutor::new`
        // call below ever sees it — `static_heal::detect_and_verify_forward_heal` is the one
        // read-only function safe to call in this position, and it is the same function
        // `RethExecutor::new` calls internally, in the identical order.
        //
        // MDBX only ever allows one open environment per datadir per process (a second, concurrent
        // `open_provider_factory` on the SAME datadir races the first for the env's file lock — the
        // same "Resource temporarily unavailable" `reth_tail_replay.rs`'s own `reopen_with_retry` doc
        // names), so this raw-factory check happens in its own scope, fully dropped before
        // `RethExecutor::new` opens its own.
        let expected_heal = {
            let genesis = genesis_from_path(&config.genesis_path).unwrap();
            let chain_spec = Arc::new(ChainSpec::from_genesis(genesis));
            let factory = open_provider_factory(&config.datadir, chain_spec).unwrap();
            let provider = factory.provider().unwrap();
            assert_eq!(
                provider.best_block_number().unwrap(),
                4,
                "precondition: MDBX's own checkpoint must already report block 4"
            );
            assert!(
                provider.sealed_header(4).unwrap().is_none(),
                "precondition: the torn headers segment must not yet expose block 4's header"
            );
            drop(provider);
            match static_heal::detect_and_verify_forward_heal(&factory).unwrap() {
                static_heal::ForwardHealOutcome::Verified(heal) => heal,
                other => panic!(
                    "precondition: the single-block torn-conf shape must verify as a forward heal \
                     candidate, got {other:?}"
                ),
            }
        };
        assert_eq!(expected_heal.block_number, 4);

        // The healer: RethExecutor::new must open at the healed block (4) — the block reth's own
        // check_consistency would otherwise have discarded — log the forward heal, and never fall
        // back to block 3 for this shape.
        let healed = RethExecutor::new(config.clone())
            .expect("the torn shape must self-heal FORWARD at open, not refuse to start");
        assert_eq!(
            healed.last_persisted_block(),
            Some(4),
            "the healed executor's persisted head must be the block the forward heal re-committed, \
             not the last block the (destructive) backward heal would have trusted"
        );
        assert_eq!(healed.head().block, 4);
        assert_eq!(
            healed.head().block_hash,
            expected_heal.hash,
            "the healed head must be block 4's real header, re-committed by commit_forward_heal — \
             not a stale in-memory value"
        );
        // Drop before reopening — MDBX allows only one open environment per datadir per process
        // (`reth_tail_replay.rs`'s own `reopen_with_retry` doc), and the dropped executor's last
        // background persist may still hold the file lock for a few hundred ms.
        drop(healed);

        // The datadir must be fully consistent after the forward heal — no lingering unwind target
        // either storage layer would otherwise report on a subsequent open.
        let factory = open_factory_with_retry(&config.datadir, &config.genesis_path);
        assert_eq!(
            factory.check_consistency().unwrap(),
            (None, None),
            "after the forward heal, both storage layers must agree with no unwind target left"
        );
    }

    /// See `reth_tail_replay.rs`'s own `reopen_with_retry`: the dropped executor's last background
    /// persist may still hold the MDBX environment lock for a few hundred ms — a plain
    /// `open_provider_factory` right after `drop` can race it.
    fn open_factory_with_retry(
        datadir: &std::path::Path,
        genesis_path: &std::path::Path,
    ) -> ProviderFactory<RethTypes> {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let genesis = genesis_from_path(genesis_path).unwrap();
            let chain_spec = Arc::new(ChainSpec::from_genesis(genesis));
            match open_provider_factory(datadir, chain_spec) {
                Ok(factory) => return factory,
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100))
                }
                Err(e) => panic!("reopen over the same datadir failed after 10 s: {e}"),
            }
        }
    }

    /// Mutation: more than one extra row must NOT be healed forward. Two blocks are torn (not one): the
    /// shape reth's OWN backward heal already handled before the forward heal existed, and still handles — the forward
    /// heal must recognise it is outside its single-row scope and fall through, unchanged, to that existing
    /// (documented) behaviour.
    #[tokio::test]
    async fn two_torn_headers_rows_are_not_healed_forward_backward_heal_still_applies() {
        let _serial = serial_mdbx().await;
        let dir = tempdir().unwrap();
        let signer = PrivateKeySigner::random();
        let config = test_config(
            dir.path(),
            &[(signer.address(), U256::from(10u128).pow(U256::from(20u8)))],
        );

        let mut ex = RethExecutor::new(config.clone()).unwrap();
        seal_blocks_for_torn_tests(&mut ex, 1, 3).await;
        ex.flush().await.unwrap();
        drop(ex);

        let conf_path = config
            .datadir
            .join("static_files")
            .join("static_file_headers_0_499999.conf");
        let snapshot_at_3 = std::fs::read(&conf_path).unwrap();

        let mut ex = RethExecutor::new(config.clone()).unwrap();
        seal_blocks_for_torn_tests(&mut ex, 4, 5).await;
        ex.flush().await.unwrap();
        drop(ex);

        // Two rows torn: blocks 4 AND 5 are durable in data+offsets, but the config is rolled all
        // the way back to its pre-block-4 snapshot.
        std::fs::write(&conf_path, &snapshot_at_3).unwrap();

        {
            let genesis = genesis_from_path(&config.genesis_path).unwrap();
            let chain_spec = Arc::new(ChainSpec::from_genesis(genesis));
            let factory = open_provider_factory(&config.datadir, chain_spec).unwrap();
            assert_eq!(
                static_heal::detect_and_verify_forward_heal(&factory).unwrap(),
                static_heal::ForwardHealOutcome::UnexpectedExtraRowCount { extra: 2 },
                "two torn rows must be named as outside the single-row forward heal's scope"
            );
        }

        // Falls through to reth's own backward heal — unchanged, documented behaviour: opens at the
        // last mutually consistent block (3), never at 4 or 5.
        let healed = RethExecutor::new(config.clone()).expect(
            "two torn rows still self-heal BACKWARD at open (forward heal: one extra row only)",
        );
        assert_eq!(healed.last_persisted_block(), Some(3));
        assert_eq!(healed.head().block, 3);
    }

    /// Mutations of the forward heal's four checks. This
    /// test and the three that follow it (checks 4, 3, 2 and 1 in that order) all start from the SAME
    /// real single-block torn-conf shape as the main forward-heal test
    /// above, then swap the extra row's own durable bytes for a DIFFERENT (but internally
    /// self-consistent — a real header whose own `hash_slow()` matches its own stored hash column)
    /// header — built by a second, independent chain that diverges from block 4 onward. Real
    /// producer throughout; never hand-rolled bytes.
    #[tokio::test]
    async fn a_row_whose_hash_mdbx_never_committed_is_refused_by_name_not_healed_forward() {
        let _serial = serial_mdbx().await;
        let (config, _dir_a_guard) =
            build_torn_conf_shape_with_foreign_block_4_row(ForeignRow::WholeRowSameSigner).await;

        let genesis = genesis_from_path(&config.genesis_path).unwrap();
        let chain_spec = Arc::new(ChainSpec::from_genesis(genesis));
        let factory = open_provider_factory(&config.datadir, chain_spec).unwrap();
        match static_heal::detect_and_verify_forward_heal(&factory).unwrap() {
            static_heal::ForwardHealOutcome::Refused(
                static_heal::ForwardHealRefusal::HeaderNumbersMismatch { .. },
            ) => {}
            other => panic!("expected a HeaderNumbersMismatch refusal, got {other:?}"),
        }
        drop(factory);

        // The full constructor path: falls through to the existing (documented) backward heal,
        // exactly as it does for any other unverifiable extra row.
        let healed = RethExecutor::new(config.clone())
            .expect("a refused forward heal still falls back to backward heal, not a hard refusal");
        assert_eq!(healed.head().block, 3);
    }

    #[tokio::test]
    async fn a_row_with_the_wrong_parent_hash_is_refused_by_name_not_healed_forward() {
        let _serial = serial_mdbx().await;
        let (config, _dir_a_guard) =
            build_torn_conf_shape_with_foreign_block_4_row(ForeignRow::WholeRowDifferentSigner)
                .await;

        let genesis = genesis_from_path(&config.genesis_path).unwrap();
        let chain_spec = Arc::new(ChainSpec::from_genesis(genesis));
        let factory = open_provider_factory(&config.datadir, chain_spec).unwrap();
        match static_heal::detect_and_verify_forward_heal(&factory).unwrap() {
            static_heal::ForwardHealOutcome::Refused(
                static_heal::ForwardHealRefusal::ParentHashMismatch { .. },
            ) => {}
            other => panic!("expected a ParentHashMismatch refusal, got {other:?}"),
        }
        drop(factory);

        let healed = RethExecutor::new(config.clone())
            .expect("a refused forward heal still falls back to backward heal, not a hard refusal");
        assert_eq!(healed.head().block, 3);
    }

    /// Check 2 pinned: a row whose HEADER column is corrupt while its hash column is
    /// intact must be refused as `HeaderHashMismatch` — the one check that catches a damaged header
    /// with a stale, still-valid-looking hash beside it.
    #[tokio::test]
    async fn a_row_whose_header_column_is_corrupt_but_hash_column_intact_is_refused_by_name() {
        let _serial = serial_mdbx().await;
        let (config, _dir_a_guard) =
            build_torn_conf_shape_with_foreign_block_4_row(ForeignRow::HeaderColumnOnly).await;

        let genesis = genesis_from_path(&config.genesis_path).unwrap();
        let chain_spec = Arc::new(ChainSpec::from_genesis(genesis));
        let factory = open_provider_factory(&config.datadir, chain_spec).unwrap();
        match static_heal::detect_and_verify_forward_heal(&factory).unwrap() {
            static_heal::ForwardHealOutcome::Refused(
                static_heal::ForwardHealRefusal::HeaderHashMismatch { .. },
            ) => {}
            other => panic!("expected a HeaderHashMismatch refusal, got {other:?}"),
        }
        drop(factory);

        let healed = RethExecutor::new(config.clone())
            .expect("a refused forward heal still falls back to backward heal, not a hard refusal");
        assert_eq!(healed.head().block, 3);
    }

    /// Check 1 pinned: a row that is internally self-consistent (its hash column
    /// matches its header, its parent hash is A's real block 3) but carries the WRONG block number
    /// must be refused as `HeaderNumberMismatch { expected: 4, found: 5 }` — by the number check,
    /// not by anything downstream. Mutation: drop the `header.number != expected_number` branch in
    /// `static_heal::detect_and_verify_forward_heal` and this row is refused as
    /// `HeaderNumbersMismatch` (check 4: MDBX never committed the bumped header's hash) instead —
    /// this test then fails by name.
    #[tokio::test]
    async fn a_self_consistent_row_with_the_wrong_block_number_is_refused_by_name() {
        let _serial = serial_mdbx().await;
        let (config, _dir_a_guard) =
            build_torn_conf_shape_with_foreign_block_4_row(ForeignRow::NumberBumped).await;

        let genesis = genesis_from_path(&config.genesis_path).unwrap();
        let chain_spec = Arc::new(ChainSpec::from_genesis(genesis));
        let factory = open_provider_factory(&config.datadir, chain_spec).unwrap();
        match static_heal::detect_and_verify_forward_heal(&factory).unwrap() {
            static_heal::ForwardHealOutcome::Refused(
                static_heal::ForwardHealRefusal::HeaderNumberMismatch { expected, found },
            ) => {
                assert_eq!((expected, found), (4, 5));
            }
            other => panic!("expected a HeaderNumberMismatch refusal, got {other:?}"),
        }
        drop(factory);

        let healed = RethExecutor::new(config.clone())
            .expect("a refused forward heal still falls back to backward heal, not a hard refusal");
        assert_eq!(healed.head().block, 3);
    }

    /// Reads row `row`'s three RAW (still LZ4-compressed, on-disk) column byte slices directly off
    /// `data_path`'s own committed `.conf`/`.off` — the same offset arithmetic
    /// `static_heal::read_column` uses internally, exposed here only for the refusal tests' own
    /// real-producer row splice (see [`splice_row_bytes`]).
    fn read_raw_row_columns(data_path: &std::path::Path, row: usize) -> [Vec<u8>; 3] {
        let jar =
            reth_nippy_jar::NippyJar::<reth_static_file_types::SegmentHeader>::load(data_path)
                .unwrap();
        let columns = jar.columns();
        let reader = jar.open_data_reader().unwrap();
        std::array::from_fn(|col| {
            let offset_pos = row * columns + col;
            let start = reader.offset(offset_pos).unwrap() as usize;
            let end = reader.offset(offset_pos + 1).unwrap() as usize;
            reader.data(start..end).to_vec()
        })
    }

    /// Replaces row `row`'s own bytes in `data_path`/`off_path` (which must currently be the LAST
    /// row committed in both files) with `columns`, keeping every earlier row's bytes untouched.
    /// Truncates both files back to the byte position where row `row` begins, then appends
    /// `columns` and recomputes the cumulative offsets for them — real NippyJar layout, no
    /// hand-rolled framing.
    fn splice_row_bytes(
        data_path: &std::path::Path,
        off_path: &std::path::Path,
        row: usize,
        columns: &[Vec<u8>; 3],
    ) {
        let jar =
            reth_nippy_jar::NippyJar::<reth_static_file_types::SegmentHeader>::load(data_path)
                .unwrap();
        let num_columns = jar.columns();
        let row_start_entry = row * num_columns;
        let row_start_byte = {
            let reader = jar.open_data_reader().unwrap();
            reader.offset(row_start_entry).unwrap() as usize
        };

        let mut data = std::fs::read(data_path).unwrap();
        data.truncate(row_start_byte);
        for col in columns {
            data.extend_from_slice(col);
        }
        std::fs::write(data_path, &data).unwrap();

        let off_bytes = std::fs::read(off_path).unwrap();
        let keep_entries = row_start_entry + 1; // entries 0..=row_start_entry, i.e. through row's own start
        let keep_len = 1 + keep_entries * 8; // 1 leading offset-size byte + `keep_entries` u64s
        let mut off = off_bytes[..keep_len].to_vec();
        let mut cursor = row_start_byte as u64;
        for col in columns {
            cursor += col.len() as u64;
            off.extend_from_slice(&cursor.to_le_bytes());
        }
        std::fs::write(off_path, &off).unwrap();
    }

    fn headers_segment_paths(
        config: &RethConfig,
    ) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let base = config
            .datadir
            .join("static_files")
            .join("static_file_headers_0_499999");
        (
            base.clone(),
            base.with_extension("off"),
            base.with_extension("conf"),
        )
    }

    /// Shared setup for the four refusal-mutation tests above. Builds chain A (the one under test:
    /// sealed 1-3, flushed, `.conf` snapshotted, then sealed its own real block 4 and flushed — the
    /// ordinary single-block torn-conf shape) and an independent chain B that diverges only at block
    /// 4 (either a different `total_gas_used`, when testing the `HeaderNumbers` check, so its own
    /// block-4 header/hash differ from A's while its parent hash still matches A's real block 3 —
    /// isolating the `HeaderNumbers` check, since the number/hash/parent checks all still pass; or
    /// built from a DIFFERENT signer/genesis entirely, when testing the parent-hash check, so B's own
    /// block 3 — and therefore B's block 4's parent hash — differs from A's real block 3, while B's
    /// block 4 is otherwise self-consistent, isolating the parent-hash check specifically). Splices
    /// B's own real (raw, still-compressed) block-4 row bytes over A's real block-4 row — never A's
    /// own rows 0-3, which is what keeps the parent-hash check meaningful in the `false` case (A's
    /// real row 3 stays A's real row 3) — then tears A's `.conf` back to its pre-block-4 snapshot.
    /// Returns `config` (chain A) and its `TempDir` (must be kept alive by the caller — dropping a
    /// `TempDir` deletes the directory it names).
    /// Which of the forward heal's four checks a spliced foreign row is built to trip.
    #[derive(Clone, Copy)]
    enum ForeignRow {
        /// Chain B = same genesis + same signer, different gas usage: number, hash and parent all pass;
        /// only MDBX's `HeaderNumbers` refuses (check 4).
        WholeRowSameSigner,
        /// Chain B = different signer → different state root from block 1: the parent hash of row 4
        /// disagrees (check 3).
        WholeRowDifferentSigner,
        /// Chain B's header COLUMN only, over chain A's own row 4 (hash column intact): the row's
        /// `keccak256(rlp(header))` no longer matches its stored hash (check 2).
        HeaderColumnOnly,
        /// Chain A's OWN row 4 with `header.number` bumped to 5 and the hash column recomputed from
        /// the bumped header, so the row is internally self-consistent (check 2 passes) and its parent
        /// hash is still A's real block 3 (check 3 passes): only the number check refuses it (check 1).
        NumberBumped,
    }

    async fn build_torn_conf_shape_with_foreign_block_4_row(
        mode: ForeignRow,
    ) -> (RethConfig, tempfile::TempDir) {
        let dir_a = tempdir().unwrap();
        let signer_a = PrivateKeySigner::random();
        let config_a = test_config(
            dir_a.path(),
            &[(signer_a.address(), U256::from(10u128).pow(U256::from(20u8)))],
        );

        let signer_b = match mode {
            ForeignRow::WholeRowSameSigner => signer_a.clone(),
            ForeignRow::WholeRowDifferentSigner | ForeignRow::HeaderColumnOnly => {
                PrivateKeySigner::random()
            }
            // Chain B is unused for this mode (the row is A's own, edited) but is built the same way
            // so the shared setup below stays one straight path.
            ForeignRow::NumberBumped => signer_a.clone(),
        };
        let dir_b = tempdir().unwrap();
        let config_b = test_config(
            dir_b.path(),
            &[(signer_b.address(), U256::from(10u128).pow(U256::from(20u8)))],
        );

        let mut ex_a = RethExecutor::new(config_a.clone()).unwrap();
        seal_blocks_for_torn_tests(&mut ex_a, 1, 3).await;
        ex_a.flush().await.unwrap();
        drop(ex_a);
        let (data_path_a, off_path_a, conf_path_a) = headers_segment_paths(&config_a);
        let snapshot_a_at_3 = std::fs::read(&conf_path_a).unwrap();

        let mut ex_b = RethExecutor::new(config_b.clone()).unwrap();
        seal_blocks_for_torn_tests(&mut ex_b, 1, 3).await;
        ex_b.flush().await.unwrap();

        if matches!(
            mode,
            ForeignRow::WholeRowDifferentSigner | ForeignRow::HeaderColumnOnly
        ) {
            // Chain B's own block 3 differs from chain A's (different genesis alloc address baked
            // into every block's state root from block 1 onward) — so B's block 4, sealed identically
            // to A's own block 4, still ends up with a DIFFERENT parent hash than A's real block 3.
            seal_blocks_for_torn_tests(&mut ex_b, 4, 4).await;
        } else {
            // Same chain content through block 3; block 4 diverges only in its own gas usage, via a
            // real included transaction A's block 4 (built by `seal_blocks_for_torn_tests`, which
            // seals empty sub-blocks) does not have — giving B's block 4 a different header (and
            // hash) than A's, while its parent hash (block 3) still matches A's real block 3 exactly.
            let env = block_env(&ex_b, 4);
            ex_b.open_block(env.clone()).await.unwrap();
            let tx = alloy_consensus::TxEip1559 {
                chain_id: CHAIN_ID,
                nonce: 0,
                gas_limit: 21_000,
                max_fee_per_gas: 1_000_000_000,
                max_priority_fee_per_gas: 0,
                to: TxKind::Call(Address::ZERO),
                value: U256::from(1u64),
                access_list: Default::default(),
                input: Bytes::default(),
            };
            let signature = signer_a.sign_hash_sync(&tx.signature_hash()).unwrap();
            let signed = tx.into_signed(signature);
            let raw = Bytes::from(signed.encoded_2718());
            for i in 0..20u16 {
                let txs = if i == 0 { vec![raw.clone()] } else { vec![] };
                ex_b.execute_sub_block(&txs, unbounded_limits())
                    .await
                    .unwrap();
            }
            ex_b.seal_block(BlockSealInputs {
                block: 4,
                timestamp_secs: env.timestamp_secs,
                sub_block_header_hashes: vec![B256::repeat_byte(4u8); 20],
                total_gas_used: 21_000,
            })
            .await
            .unwrap();
        }
        ex_b.flush().await.unwrap();
        drop(ex_b);

        let (data_path_b, _off_path_b, _conf_path_b) = headers_segment_paths(&config_b);
        let foreign_row4 = read_raw_row_columns(&data_path_b, 4);

        // Build chain A's own real block 4 (matching A's own chain), flush, splice chain B's own
        // row-4 bytes over it (A's rows 0-3 untouched), then restore the pre-block-4 config
        // snapshot — the ordinary single-block torn-conf shape, now with a foreign row 4.
        let mut ex_a = RethExecutor::new(config_a.clone()).unwrap();
        seal_blocks_for_torn_tests(&mut ex_a, 4, 4).await;
        ex_a.flush().await.unwrap();
        drop(ex_a);
        let spliced = match mode {
            ForeignRow::WholeRowSameSigner | ForeignRow::WholeRowDifferentSigner => foreign_row4,
            ForeignRow::HeaderColumnOnly => {
                let mut own = read_raw_row_columns(&data_path_a, 4);
                own[0] = foreign_row4[0].clone();
                own
            }
            ForeignRow::NumberBumped => {
                // Decode A's own real row 4 through the segment's own compressor + `Compact`
                // (exactly what `static_heal::read_column` does), bump the number, re-encode the
                // header AND recompute the hash column so checks 2 and 3 still pass.
                use reth_codecs::Compact;
                use reth_nippy_jar::compression::Compression;
                let jar = reth_nippy_jar::NippyJar::<reth_static_file_types::SegmentHeader>::load(
                    &data_path_a,
                )
                .unwrap();
                let compressor = jar.compressor().expect("Headers segment is LZ4-compressed");
                let own = read_raw_row_columns(&data_path_a, 4);
                let header_bytes = compressor.decompress(&own[0]).unwrap();
                let (mut header, _) = Header::from_compact(&header_bytes, header_bytes.len());
                assert_eq!(
                    header.number, 4,
                    "A's own row 4 must carry number 4 before the bump"
                );
                header.number = 5;
                let mut header_out = Vec::new();
                header.to_compact(&mut header_out);
                let mut hash_out = Vec::new();
                header.hash_slow().to_compact(&mut hash_out);
                [
                    compressor.compress(&header_out).unwrap(),
                    own[1].clone(),
                    compressor.compress(&hash_out).unwrap(),
                ]
            }
        };
        splice_row_bytes(&data_path_a, &off_path_a, 4, &spliced);
        std::fs::write(&conf_path_a, &snapshot_a_at_3).unwrap();

        (config_a, dir_a)
    }

    /// The static-ahead shape — a kill between
    /// the headers `.conf` rename and the MDBX commit that follows it, so the STATIC side is fully,
    /// consistently committed one block ahead of MDBX's own `StageId::Finish` checkpoint. Reth's own
    /// `check_consistency` (`ensure_invariants`'s "checkpoint behind → prune extra static rows"
    /// branch) already handles this — it is not the forward heal's shape at all (`extra ==
    /// 0`, no torn NippyJar file: the divergence is between MDBX and the static file, not within the
    /// static file itself) — this test pins that this executor still opens correctly and reports
    /// `(None, None)`.
    #[tokio::test]
    async fn static_ahead_of_mdbx_prunes_silently_and_opens_at_mdbxs_own_checkpoint() {
        let _serial = serial_mdbx().await;
        let dir = tempdir().unwrap();
        let signer = PrivateKeySigner::random();
        let config = test_config(
            dir.path(),
            &[(signer.address(), U256::from(10u128).pow(U256::from(20u8)))],
        );

        let mut ex = RethExecutor::new(config.clone()).unwrap();
        seal_blocks_for_torn_tests(&mut ex, 1, 3).await;
        ex.flush().await.unwrap();
        drop(ex);

        // Snapshot the ENTIRE mdbx directory (not just one file — MDBX is a single-file-per-table
        // env; `config.datadir` already names the `db` directory `open_provider_factory` places
        // `db/` under, per this crate's own `RethConfig.datadir` convention) with nothing holding it
        // open, matching a real restart's own quiescent point.
        let mdbx_dir = config.datadir.join("db");
        let mdbx_snapshot_dir = dir.path().join("mdbx_snapshot_at_3");
        copy_dir_recursive(&mdbx_dir, &mdbx_snapshot_dir);

        let mut ex = RethExecutor::new(config.clone()).unwrap();
        assert_eq!(ex.last_persisted_block(), Some(3));
        seal_blocks_for_torn_tests(&mut ex, 4, 4).await;
        ex.flush().await.unwrap();
        drop(ex);

        // The tear: MDBX rolled back to its pre-block-4 snapshot; the headers static file (and every
        // other segment) stays fully committed through block 4 — the inverse of the torn-conf shape
        // above, and NOT a torn NippyJar file at all (data+offsets+config all agree on 4 rows).
        std::fs::remove_dir_all(&mdbx_dir).unwrap();
        copy_dir_recursive(&mdbx_snapshot_dir, &mdbx_dir);

        {
            let genesis = genesis_from_path(&config.genesis_path).unwrap();
            let chain_spec = Arc::new(ChainSpec::from_genesis(genesis));
            let factory = open_provider_factory(&config.datadir, chain_spec).unwrap();
            assert_eq!(
                static_heal::detect_and_verify_forward_heal(&factory).unwrap(),
                static_heal::ForwardHealOutcome::Consistent,
                "the static-ahead shape has no torn NippyJar row — nothing for the forward heal to \
                 find; reth's own checkpoint-vs-static invariant check is what repairs this shape"
            );
            let static_file_provider = factory.static_file_provider();
            assert_eq!(
                static_file_provider.get_highest_static_file_block(StaticFileSegment::Headers),
                Some(4),
                "precondition: the headers static file already claims block 4"
            );
            let provider = factory.provider().unwrap();
            assert_eq!(
                provider.best_block_number().unwrap(),
                3,
                "precondition: MDBX's own checkpoint still reports block 3"
            );
            drop(provider);
            assert_eq!(
                factory.check_consistency().unwrap(),
                (None, None),
                "the static-ahead shape prunes silently, reporting no unwind target"
            );
        }

        let healed = RethExecutor::new(config.clone())
            .expect("the static-ahead shape must open cleanly, not refuse to start");
        assert_eq!(
            healed.last_persisted_block(),
            Some(3),
            "the executor opens at MDBX's own (lower) checkpoint; the ordered log replays block 4"
        );
        assert_eq!(healed.head().block, 3);
    }

    fn copy_dir_recursive(from: &std::path::Path, to: &std::path::Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let dest = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_dir_recursive(&entry.path(), &dest);
            } else {
                std::fs::copy(entry.path(), &dest).unwrap();
            }
        }
    }

    /// `assert_healed_to_within_best` is the pure guard
    /// `RethExecutor::new` runs on every path. Mutation: this test IS the mutation — it forces
    /// `healed_to` one past `best_block_number` directly (a real torn datadir can never actually
    /// produce this, by construction: every value `check_consistency`/the forward heal ever computes
    /// is bounded by the checkpoint it read) and asserts the named refusal fires.
    #[test]
    fn healed_to_beyond_best_block_number_is_refused_by_name() {
        let err = assert_healed_to_within_best(Some(5), 4).unwrap_err();
        match err {
            ExecutorError::Backend(msg) => assert!(
                msg.contains("healed_to 5 exceeds"),
                "refusal message must name both values: {msg}"
            ),
            other => panic!("expected ExecutorError::Backend, got {other:?}"),
        }
    }

    #[test]
    fn healed_to_within_best_block_number_is_accepted() {
        assert!(assert_healed_to_within_best(Some(4), 4).is_ok());
        assert!(assert_healed_to_within_best(Some(3), 4).is_ok());
        assert!(assert_healed_to_within_best(None, 10).is_ok());
    }

    /// The inverse class: the headers config WAS
    /// renamed to claim block 4, but the underlying NippyJar data (+ offset index) is truncated back
    /// to its pre-block-4 length — as if the data write itself never completed even though the
    /// commit-marker did. Proves reth's `check_consistency` names the SAME unwind target for this
    /// inverse shape (a checkpoint-vs-static-file mismatch, regardless of which side of the
    /// data-then-commit-marker write actually tore) rather than a distinct, silent failure — the
    /// "heals or refuses BY NAME" contract item, confirmed here to be "heals, by the same path".
    #[tokio::test]
    async fn torn_headers_data_heals_at_open_the_same_way_as_a_torn_config() {
        let _serial = serial_mdbx().await;
        let dir = tempdir().unwrap();
        let signer = PrivateKeySigner::random();
        let config = test_config(
            dir.path(),
            &[(signer.address(), U256::from(10u128).pow(U256::from(20u8)))],
        );

        let data_path = config
            .datadir
            .join("static_files")
            .join("static_file_headers_0_499999");
        let off_path = config
            .datadir
            .join("static_files")
            .join("static_file_headers_0_499999.off");

        let mut ex = RethExecutor::new(config.clone()).unwrap();
        seal_blocks_for_torn_tests(&mut ex, 1, 3).await;
        ex.flush().await.unwrap();
        drop(ex);
        // Snapshot with nothing holding the datadir open — see the sibling torn-config test's own
        // doc for why a snapshot taken while a `RethExecutor` is still live over the same static file
        // segment is not equivalent to what is truly durable on disk.
        let data_snapshot_at_3 = std::fs::read(&data_path).unwrap();
        let off_snapshot_at_3 = std::fs::read(&off_path).unwrap();

        let mut ex = RethExecutor::new(config.clone()).unwrap();
        seal_blocks_for_torn_tests(&mut ex, 4, 4).await;
        ex.flush().await.unwrap();
        drop(ex);

        // The inverse torn shape: config claims block 4 (untouched), but the data + offset index
        // are rolled back to their pre-block-4 snapshot.
        std::fs::write(&data_path, &data_snapshot_at_3).unwrap();
        std::fs::write(&off_path, &off_snapshot_at_3).unwrap();

        {
            let genesis = genesis_from_path(&config.genesis_path).unwrap();
            let chain_spec = Arc::new(ChainSpec::from_genesis(genesis));
            let factory = open_provider_factory(&config.datadir, chain_spec).unwrap();
            assert!(
                factory
                    .provider()
                    .unwrap()
                    .sealed_header(4)
                    .unwrap()
                    .is_none(),
                "precondition: the truncated headers data must not yet expose block 4's header"
            );
            assert_eq!(
                factory.check_consistency().unwrap(),
                (None, Some(3)),
                "reth names the same unwind target for this inverse shape as for a torn config"
            );
        }

        let healed = RethExecutor::new(config.clone())
            .expect("the inverse torn shape must also self-heal at open, not refuse to start");
        assert_eq!(healed.last_persisted_block(), Some(3));
        assert_eq!(healed.head().block, 3);
    }

    // ---- blocks that carry withdrawals (deposits) --------------------------------------------------------------------

    /// `attrs_from_env` hands reth the env's withdrawals unchanged, and the rule it checks against carries the same
    /// list: with an empty list the root is the shared `EMPTY_WITHDRAWALS`, as every block had before deposits.
    #[test]
    fn attrs_from_env_passes_the_withdrawals_list_and_the_rule_carries_it() {
        let list = vec![
            rome_zk_executor_api::deposit_withdrawal(4, Address::repeat_byte(0xE1), 7_000),
            rome_zk_executor_api::deposit_withdrawal(5, Address::repeat_byte(0xE2), 9_000),
        ];
        let env = BlockEnv {
            number: 3,
            timestamp_secs: 1_000,
            gas_limit: 10_000_000_000,
            coinbase: Address::ZERO,
            prev_randao: B256::ZERO,
            base_fee: None,
            withdrawals: list.clone(),
        };
        let attrs = RethExecutor::attrs_from_env(&env, CHAIN_ID);
        assert_eq!(attrs.withdrawals, Some(Withdrawals::new(list.clone())));
        assert_eq!(
            RethExecutor::rule_from_env(&env, CHAIN_ID).withdrawals_root,
            rome_zk_executor_api::withdrawals_root(&list)
        );

        let empty = BlockEnv {
            withdrawals: vec![],
            ..env
        };
        let attrs = RethExecutor::attrs_from_env(&empty, CHAIN_ID);
        assert_eq!(attrs.withdrawals, Some(Withdrawals::default()));
        assert_eq!(
            RethExecutor::rule_from_env(&empty, CHAIN_ID).withdrawals_root,
            rome_zk_executor_api::EMPTY_WITHDRAWALS
        );
    }

    fn balance_of(ex: &RethExecutor, addr: Address) -> U256 {
        ex.overlay_state_provider()
            .unwrap()
            .account_balance(&addr)
            .unwrap()
            .unwrap_or_default()
    }

    /// Seals block 1 of a fresh chain with one transfer from `signer` and `withdrawals`; returns the executor so the
    /// caller can read the sealed header and the balances.
    async fn seal_one_block(
        dir: &std::path::Path,
        signer: &PrivateKeySigner,
        withdrawals: Vec<alloy_eips::eip4895::Withdrawal>,
    ) -> RethExecutor {
        let config = test_config(
            dir,
            &[(signer.address(), U256::from(10u128).pow(U256::from(20u8)))],
        );
        let mut ex = RethExecutor::new(config).unwrap();
        let env = BlockEnv {
            withdrawals,
            ..block_env(&ex, 1)
        };
        let timestamp_secs = env.timestamp_secs;
        ex.open_block(env).await.unwrap();
        for i in 0..20u16 {
            let txs = if i == 0 {
                vec![signed_transfer(signer, 0)]
            } else {
                vec![]
            };
            ex.execute_sub_block(&txs, unbounded_limits())
                .await
                .unwrap();
        }
        ex.seal_block(BlockSealInputs {
            block: 1,
            timestamp_secs,
            sub_block_header_hashes: vec![B256::repeat_byte(1); 20],
            total_gas_used: 21_000,
        })
        .await
        .unwrap();
        ex
    }

    /// A block with withdrawals credits them (amount in gwei, so `amount * 1e9` wei) after its transactions, and its
    /// header carries the shared withdrawals_root over exactly that list. Compared with the same block sealed without them: the
    /// transaction's effects are identical, only the credits differ, and the block hash changes with them.
    #[tokio::test]
    async fn a_block_with_withdrawals_credits_them_after_its_transactions_with_the_shared_root() {
        let _serial = serial_mdbx().await;
        let signer = PrivateKeySigner::random();
        let to_sender = rome_zk_executor_api::deposit_withdrawal(20, signer.address(), 5_000_000);
        let fresh_a = rome_zk_executor_api::deposit_withdrawal(21, Address::repeat_byte(0xE1), 1);
        let fresh_b =
            rome_zk_executor_api::deposit_withdrawal(22, Address::repeat_byte(0xE2), 123_456_789);
        let list = vec![fresh_a, to_sender, fresh_b];

        let plain_dir = tempdir().unwrap();
        let plain = seal_one_block(plain_dir.path(), &signer, vec![]).await;
        let with_dir = tempdir().unwrap();
        let with = seal_one_block(with_dir.path(), &signer, list.clone()).await;

        // The empty list gives the shared constant; the list gives withdrawals_root over it.
        assert_eq!(
            plain.canonical_parent.withdrawals_root(),
            Some(rome_zk_executor_api::EMPTY_WITHDRAWALS)
        );
        assert_eq!(
            with.canonical_parent.withdrawals_root(),
            Some(rome_zk_executor_api::withdrawals_root(&list))
        );
        assert_ne!(plain.canonical_parent.hash(), with.canonical_parent.hash());
        assert_ne!(
            plain.canonical_parent.state_root(),
            with.canonical_parent.state_root()
        );

        // Credits: `amount_gwei * 1e9` wei each, after the transaction ran (the sender paid for gas, then was credited).
        let wei = |w: &alloy_eips::eip4895::Withdrawal| {
            U256::from(w.amount) * U256::from(1_000_000_000u64)
        };
        assert_eq!(balance_of(&plain, fresh_a.address), U256::ZERO);
        assert_eq!(balance_of(&with, fresh_a.address), wei(&fresh_a));
        assert_eq!(balance_of(&with, fresh_b.address), wei(&fresh_b));
        assert_eq!(
            balance_of(&with, signer.address()),
            balance_of(&plain, signer.address()) + wei(&to_sender),
            "the sender's own transaction ran as it always did; the credit lands on top of it"
        );
        // A withdrawal uses no gas and runs no transaction.
        assert_eq!(with.nonce(signer.address()), plain.nonce(signer.address()));
        assert_eq!(
            with.canonical_parent.gas_used(),
            plain.canonical_parent.gas_used()
        );
    }
}
