//! The sealing mechanics: turn one tick's ordered raw txs into a signed, fsynced sub-block, and close a
//! block every 20th sub-block.
//!
//! This module is deliberately split from the timer loop that drives it (that lives in
//! [`crate::sequencer`]): [`SealerState::seal_sub_block`] is a plain async function with no timer inside
//! it, so the 20-sub-blocks-per-block invariant, the block timestamp rule, and the header hash chain can
//! all be tested by calling it directly, without fighting `tokio::time`.
//!
//! The timing is adopted from the OP conductor/sequencer (`op-node/rollup/sequencing/sequencer.go`:
//! `nextAction = payload_time - sealingDuration`, default `sealingDuration` 50 ms): the timer loop in
//! `sequencer.rs` schedules against a fixed start instant via `tokio::time::interval` with
//! `MissedTickBehavior::Skip`, so a slow sub-block does not cause a catch-up burst — it seals late, once,
//! and the next tick resumes on the original 50 ms grid. Whatever admission didn't drain in time simply
//! stays queued and is carried into the next sub-block (there is no separate "carry" list — the ready
//! queue already is one).
//!
//! **The record is the block.** Each sub-block record holds exactly the transactions
//! the executor *included*, in execution order — the same set whose hashes form `tx_root`. A transaction
//! the executor rejected at execution (bad nonce, insufficient funds, ...) is never written to the log
//! and therefore never reaches DA; its sender is told via the `Rejected` reply and admission's reconcile
//! path (`sequencer.rs`). A transaction the executor did not reach (`not_executed`) is carried to the
//! next sub-block instead. Consequence: the batcher, derivation and the guest can treat a record's tx
//! list as the exact executed block, with no per-tx included/rejected marker needed — and the strict
//! policy (a logged transaction that fails re-execution is divergence, not a skip) is what it says,
//! because a rejected transaction is never logged in the first place (`included_raw_txs` is the boundary
//! that enforces this; the first version of
//! this module logged the *attempted* prefix instead, `txs[..attempted_len]` verbatim).

use alloy::primitives::{Address, Bytes, B256};
use alloy::signers::local::PrivateKeySigner;
use std::io;

use crate::deposits::DepositFeed;
use crate::executor::{
    prev_randao, BlockEnv, BlockOutcome, BlockSealInputs, Executor, ExecutorError, SubBlockLimits,
    SubBlockOutcome,
};
use crate::header::SubBlockHeader;
use crate::log::LogWriter;
use crate::preconf::{SealedSubBlock, SubBlockSink};
use crate::signing::sign_header;

/// Everything that can stop [`SealerState::seal_sub_block`] from completing. Both are fatal to the caller —
/// the sequencer's on-disk state or the executor's in-memory state is now of unknown shape — but neither
/// is a reason to panic the process: the caller shuts the actor down cleanly instead (see
/// `sequencer::Actor::run`).
#[derive(Debug, thiserror::Error)]
pub enum SealError {
    #[error("ordered log fsync failed: {0}")]
    Log(#[from] io::Error),
    #[error("executor error: {0}")]
    Executor(#[from] ExecutorError),
    /// The executor's contract requires (a) `outcome.not_executed` to be
    /// exactly the in-order suffix of the attempted txs, and (b) `outcome.included` ∪
    /// `outcome.rejected` to be exactly the attempted (non-`not_executed`) prefix, with every
    /// included hash actually found among the attempted raw txs — the log is the block,
    /// built from `outcome.included` alone, so this is the boundary that must never trust the
    /// executor blind. An executor that violates either half is fatal: trusting it would silently log
    /// the wrong tx list as this sub-block's permanent record, which the challenger protocol treats as
    /// the ground truth.
    #[error("executor contract violated at block {block} index {index}: {reason}")]
    ExecutorContract {
        block: u64,
        index: u16,
        reason: String,
    },
}

/// Builds the list of raw tx bytes this sub-block actually logs — `outcome.included`'s hashes, mapped
/// back to their raw bytes from `attempted` (the executor-attempted prefix of this tick's txs) and kept in
/// `outcome.included`'s own order, which is `tx_root`'s own leaf order. **Never** `attempted` itself:
/// `attempted` is a superset that can hold a tx the executor `rejected` (bad nonce, insufficient
/// funds, ...) — a rejected tx is not part of the block (the log record is the block),
/// and logging it anyway is exactly the bug this function exists to close (a rejected tx used to ride
/// along in the record despite never being covered by `tx_root`/`receipts_root`/`gas_used`).
///
/// Also enforces the executor's contract at this same boundary: every `attempted` tx must be
/// classified as either included or rejected, never both, never neither — `attempted_hashes` is
/// derived from `attempted` by decoding each raw tx's own EIP-2718 hash (no signature recovery
/// needed: the hash covers the signed bytes as-is), not from a count, so a violation is caught even
/// if `included.len() + rejected.len() == attempted.len()` by coincidence.
fn included_raw_txs(attempted: &[Bytes], outcome: &SubBlockOutcome) -> Result<Vec<Bytes>, String> {
    use alloy::consensus::TxEnvelope;
    use alloy::primitives::TxHash;
    use alloy_eips::eip2718::Decodable2718;
    use std::collections::{HashMap, HashSet};

    let mut raw_by_hash: HashMap<TxHash, &Bytes> = HashMap::with_capacity(attempted.len());
    let mut attempted_hashes: HashSet<TxHash> = HashSet::with_capacity(attempted.len());
    for raw in attempted {
        let mut slice = raw.as_ref();
        let envelope = TxEnvelope::decode_2718(&mut slice).map_err(|e| {
            format!("attempted tx failed to decode while building the log record: {e}")
        })?;
        let hash = *envelope.tx_hash();
        attempted_hashes.insert(hash);
        raw_by_hash.insert(hash, raw);
    }

    let included_set: HashSet<TxHash> = outcome.included.iter().copied().collect();
    if included_set.len() != outcome.included.len() {
        return Err("outcome.included contains a duplicate tx hash".to_string());
    }
    let rejected_set: HashSet<TxHash> = outcome.rejected.iter().map(|r| r.tx_hash).collect();
    if rejected_set.len() != outcome.rejected.len() {
        return Err("outcome.rejected contains a duplicate tx hash".to_string());
    }
    if !included_set.is_disjoint(&rejected_set) {
        return Err("a tx hash appears in both outcome.included and outcome.rejected".to_string());
    }
    let union: HashSet<TxHash> = included_set.union(&rejected_set).copied().collect();
    if union != attempted_hashes {
        return Err(format!(
            "outcome.included ({} hashes) union outcome.rejected ({} hashes) does not equal the \
             {} attempted txs",
            included_set.len(),
            rejected_set.len(),
            attempted_hashes.len()
        ));
    }

    let mut logged = Vec::with_capacity(outcome.included.len());
    for hash in &outcome.included {
        let raw = raw_by_hash
            .get(hash)
            .ok_or_else(|| format!("included tx hash {hash:#x} not found among attempted txs"))?;
        logged.push((*raw).clone());
    }
    Ok(logged)
}

/// Sub-blocks per block (a block is 20 sub-blocks) — the profile default. It is
/// owned by the standalone `rome-zk-profile` crate now ([`rome_zk_profile::DEFAULT_SUB_BLOCKS_PER_BLOCK`]);
/// re-exported here under this crate's historical name. `SealerState` carries its own
/// `sub_blocks_per_block` at runtime (a chain's profile can declare a different value, e.g. 40 at a 25 ms
/// sub-block period, so long as the product is a whole number of seconds — see [`crate::profile`]); this
/// constant remains the shape every existing caller that hasn't been handed a profile yet passes
/// explicitly.
pub const SUB_BLOCKS_PER_BLOCK: u16 = rome_zk_profile::DEFAULT_SUB_BLOCKS_PER_BLOCK;

/// The default published into every block's [`BlockEnv::gas_limit`] —
/// the 100M gas/s executor cap × the 1 s block (20 sub-blocks), i.e.
/// [`crate::executor::DEFAULT_SUB_BLOCK_GAS_LIMIT`] × [`SUB_BLOCKS_PER_BLOCK`]. Chains whose genesis
/// gas limit differs configure their own value (see `config.rs`'s `block_gas_limit`).
pub const DEFAULT_BLOCK_GAS_LIMIT: u64 =
    crate::executor::DEFAULT_SUB_BLOCK_GAS_LIMIT * SUB_BLOCKS_PER_BLOCK as u64;

/// Sub-block timestamp monotonicity: `max(now_us, prev + 1)`, so a clock
/// that stalls, steps backward, or reports the same instant twice in a row (missed ticks catching up)
/// still produces a strictly increasing sequence of sub-block timestamps.
pub(crate) fn resolve_sub_block_timestamp_us(now_us: u64, prev_sub_block_ts_us: u64) -> u64 {
    now_us.max(prev_sub_block_ts_us + 1)
}

/// Block timestamp monotonicity: `max(first_sub_block_secs,
/// prev_block_timestamp_secs + 1)`. Two blocks sealed inside the same wall-clock second (e.g. after
/// catching up on missed ticks) must still get strictly increasing EVM block timestamps. Exposed
/// `pub(crate)` so [`crate::recovery`] recomputes the exact same value replaying the log — this input is
/// never itself logged, so replay must derive it identically or the recomputed block outcome would
/// diverge from the one the live sealer actually produced.
pub(crate) fn resolve_block_timestamp_secs(
    first_sub_block_secs: u64,
    prev_block_timestamp_secs: u64,
) -> u64 {
    first_sub_block_secs.max(prev_block_timestamp_secs + 1)
}

/// Where to resume sealing after a fresh start or a log replay. Beyond the block/index/prev-hash chain
/// position, this carries every piece of in-progress-block state a crash mid-block would otherwise lose:
/// the current block's first timestamp and accumulated sub-block hashes and gas, plus the two
/// monotonicity trackers. [`crate::recovery::replay_into_executor`] rebuilds all of it from the log; a
/// fresh start uses [`Default`], which sets every field to zero/empty
/// **except `next_block`, which is 1**: the sequencer numbers blocks
/// from 1 (design number 0 names only the EL's genesis, which the sequencer never seals), so the first
/// block a fresh sealer ever seals is design block 1 — the same number the executor's underlying chain
/// (genesis = reth block 0) will assign it. See the manual [`Default`] impl below.
#[derive(Debug, Clone)]
pub struct ResumePoint {
    pub next_block: u64,
    pub next_index: u16,
    pub prev_header_hash: B256,
    /// The current (possibly partial) block's first sub-block timestamp, in µs. Zero at a block boundary
    /// (`next_index == 0`) or on a fresh start.
    pub first_timestamp_us_in_block: u64,
    /// Header hashes of the sub-blocks already sealed in the current (possibly partial) block, in order.
    /// Empty at a block boundary or on a fresh start.
    pub sub_block_header_hashes: Vec<B256>,
    /// Gas used so far in the current (possibly partial) block.
    pub gas_in_block: u64,
    /// The last sealed sub-block's timestamp, µs — the monotonicity floor for the next one.
    pub prev_sub_block_ts_us: u64,
    /// The last **closed** block's EVM timestamp, seconds — the monotonicity floor for the next block.
    pub prev_block_timestamp_secs: u64,
    /// One past the last deposit the log shows a block credited (the last `deposits_end` any record carries), 0
    /// when none did. The next block that credits deposits starts at this queue index.
    pub deposits_end: u64,
}

impl Default for ResumePoint {
    /// A fresh chain's first sealed block is design/header block **1**, not 0 — 0 is the
    /// genesis height the sequencer never seals. Every other field is genuinely
    /// zero/empty on a fresh start.
    fn default() -> Self {
        Self {
            next_block: 1,
            next_index: 0,
            prev_header_hash: B256::ZERO,
            first_timestamp_us_in_block: 0,
            sub_block_header_hashes: Vec::new(),
            gas_in_block: 0,
            prev_sub_block_ts_us: 0,
            prev_block_timestamp_secs: 0,
            deposits_end: 0,
        }
    }
}

/// The result of sealing one sub-block.
#[derive(Debug)]
pub struct SealResult {
    pub header: SubBlockHeader,
    pub header_hash: B256,
    pub signature: alloy::primitives::Signature,
    pub outcome: SubBlockOutcome,
    /// `Some` when this sub-block was the 20th and closed a block.
    pub block_sealed: Option<BlockOutcome>,
    /// Wall time of `LogWriter::append`'s own `write_all` + `sync_data` call —
    /// isolates the log's fsync cost from everything else `seal_sub_block` does (admission drain,
    /// execution, signing), so the caller (`sequencer::Actor::tick`) can record it into
    /// `Metrics::log_fsync_duration_seconds` without this module needing its own `Arc<Metrics>` handle.
    pub log_fsync_seconds: f64,
    /// `Some(total_gas_used)` when this sub-block closed a block — `Metrics::block_gas_used`'s
    /// own observation, the gas/s source for the load programme. `None` otherwise.
    pub block_gas_used: Option<u64>,
    /// `Some(block_timestamp_secs - first_sub_block_secs)` when this sub-block closed a
    /// block — how far the resolved EVM block timestamp ran ahead of its own first sub-block's wall
    /// clock reading (the catch-up debt `resolve_block_timestamp_secs`'s `prev + 1` clamp can
    /// introduce). Zero on every ordinary block; nonzero only when blocks are sealing faster than 1/s.
    /// `None` when this sub-block did not close a block.
    /// Whole seconds the sealed block's timestamp sits AHEAD of its own first sub-block's second —
    /// the `prev + 1` catch-up debt, computed on integer seconds (never negative, never
    /// noise from where inside a second the first sub-block landed): 0 whenever the wall clock won.
    pub block_timestamp_ahead_seconds: Option<f64>,
    /// How many deposits this sub-block's block credits: nonzero only on a block's first sub-block.
    pub deposits_credited: usize,
    /// When each deposit the just-closed block credited was enqueued, Unix seconds. Filled only on the
    /// sub-block that closed the block (a block reopened after a restart reports none).
    pub closed_block_deposit_enqueue_ts: Vec<i64>,
}

/// One call to [`SealerState::seal_sub_block`]: either nothing happened — no log
/// record, no executor call, no state mutation — or a sub-block genuinely sealed (and, on the 20th,
/// closed a block).
#[derive(Debug)]
pub enum Tick {
    /// `index == 0`, no transactions were ready, and this profile's `empty_block_interval_secs` is not
    /// yet due (or is 0, "never") — the tick touched nothing. `now_us` is the caller's own wall-clock
    /// reading, unchanged, purely for the caller's own logging/metrics.
    Idle { now_us: u64 },
    /// A sub-block was actually sealed (with or without transactions). Boxed: `Idle` is 8 bytes and
    /// `SealResult` is not, and this type is matched on every tick — clippy's `large_enum_variant`.
    Sealed(Box<SealResult>),
}

impl Tick {
    /// True for [`Tick::Idle`].
    pub fn is_idle(&self) -> bool {
        matches!(self, Tick::Idle { .. })
    }

    /// Unwrap the [`SealResult`] of a tick expected to have sealed — panics naming the idle timestamp
    /// otherwise. A convenience for callers (tests, fixture-building code in other crates) that know
    /// idle cannot happen for their own call pattern; `sequencer::Actor::tick` itself always matches
    /// explicitly instead, since idle is exactly the case it must handle.
    pub fn sealed(self) -> SealResult {
        match self {
            Tick::Sealed(result) => *result,
            Tick::Idle { now_us } => {
                panic!("expected Tick::Sealed, got Tick::Idle {{ now_us: {now_us} }}")
            }
        }
    }
}

pub struct SealerState<E: Executor, S: SubBlockSink> {
    pub executor: E,
    log: LogWriter,
    signer: PrivateKeySigner,
    sink: S,
    chain_id: u64,
    /// This chain's genesis gas limit, published into every block's
    /// [`BlockEnv`] — a fixed constant this design holds steady per block (see
    /// `rome-zk-executor-reth`'s derivation-equivalence test comment), not derived from the per-tick
    /// [`SubBlockLimits::gas_limit`] passed to `seal_sub_block`.
    block_gas_limit: u64,
    /// The chain's own fee recipient (genesis `coinbase` —
    /// `Address::ZERO` on Tiber), published into every block's [`BlockEnv`] — the SAME value the
    /// guest embeds at compile time and `rome-zk-derive` reads off its own engine connection, never a
    /// hardcoded literal here.
    fee_recipient: Address,
    /// Sub-blocks per block, from the chain's profile — replaces the compile-time
    /// [`SUB_BLOCKS_PER_BLOCK`] default as the value that actually decides where a block boundary
    /// falls (see the doc on that constant).
    sub_blocks_per_block: u16,
    /// 0 (the constructor's own default, matching `rome_zk_profile::Profile`'s own
    /// default) = never seal a block with no transactions; N = seal one at most every N seconds. Set
    /// via [`Self::with_empty_block_interval_secs`], never a constructor argument — see that method's
    /// doc for why.
    empty_block_interval_secs: u64,
    next_block: u64,
    next_index: u16,
    prev_header_hash: B256,
    sub_block_hashes_in_block: Vec<B256>,
    gas_in_block: u64,
    first_timestamp_us_in_block: u64,
    prev_sub_block_ts_us: u64,
    prev_block_timestamp_secs: u64,
    /// The deposit queue's view, when this sequencer runs with deposits; `None` is the sequencer as it was
    /// before deposits existed.
    deposits: Option<DepositFeed>,
    /// One past the last deposit a block has credited.
    deposits_end: u64,
    /// When each deposit the open block credits was enqueued; handed out when the block closes.
    block_deposit_enqueue_ts: Vec<i64>,
}

impl<E: Executor, S: SubBlockSink> SealerState<E, S> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        executor: E,
        log: LogWriter,
        signer: PrivateKeySigner,
        sink: S,
        chain_id: u64,
        block_gas_limit: u64,
        fee_recipient: Address,
        sub_blocks_per_block: u16,
        resume: ResumePoint,
    ) -> Self {
        Self {
            executor,
            log,
            signer,
            sink,
            chain_id,
            block_gas_limit,
            fee_recipient,
            sub_blocks_per_block,
            // The constructor's own default is 0 ("never") — matches
            // `rome_zk_profile::Profile::default()`. Every existing caller of this constructor (this
            // crate's own tests, `rome-zk-batcher`'s cross-crate fixtures, the integration suite) keeps
            // compiling and behaving exactly as before it existed; a caller that wants a different
            // cadence opts in via `with_empty_block_interval_secs` — see that method's doc for why this
            // is a builder step rather than a tenth positional argument.
            empty_block_interval_secs: 0,
            next_block: resume.next_block,
            next_index: resume.next_index,
            prev_header_hash: resume.prev_header_hash,
            sub_block_hashes_in_block: resume.sub_block_header_hashes,
            gas_in_block: resume.gas_in_block,
            first_timestamp_us_in_block: resume.first_timestamp_us_in_block,
            prev_sub_block_ts_us: resume.prev_sub_block_ts_us,
            prev_block_timestamp_secs: resume.prev_block_timestamp_secs,
            deposits: None,
            deposits_end: resume.deposits_end,
            block_deposit_enqueue_ts: Vec::new(),
        }
    }

    /// Credit deposits: each block then takes the finalized deposits `feed` has waiting (oldest first, up to the
    /// queue's per-block cap), and a waiting deposit opens a block even with no transactions and the idle rule
    /// not yet due. A builder step, like [`Self::with_empty_block_interval_secs`], so the many existing
    /// construction sites stay as they are. Tells the feed where the log resumed.
    pub fn with_deposits(mut self, feed: DepositFeed) -> Self {
        feed.set_included(self.deposits_end);
        self.deposits = Some(feed);
        self
    }

    /// Set this profile's idle-block cadence (`rome_zk_profile::Profile`'s own
    /// `empty_block_interval_secs`) — 0 (the default, never called) means "never seal a block with no
    /// transactions"; N means "seal one at most every N seconds". A builder step, not a constructor
    /// argument: `SealerState::new`'s ten other arguments are already position-sensitive, and this crate
    /// alone has dozens of existing construction sites (this module's own tests, `sequencer::spawn`,
    /// the recovery/replay/torn-persist suites, and `rome-zk-batcher`'s cross-crate fixtures) that have
    /// no reason to know this profile field exists at all — a chained call here is additive and cannot
    /// silently shift an unrelated positional argument.
    pub fn with_empty_block_interval_secs(mut self, empty_block_interval_secs: u64) -> Self {
        self.empty_block_interval_secs = empty_block_interval_secs;
        self
    }

    /// Is a new, empty block due? Only meaningful at `index == 0` with no transactions
    /// ready — `0` (never) always answers `false`; a positive `N` answers `true` once at least `N`
    /// seconds have elapsed, by this tick's own wall clock, since the last block's own EVM timestamp.
    fn empty_block_due(&self, now_secs: u64) -> bool {
        self.empty_block_interval_secs > 0
            && now_secs >= self.prev_block_timestamp_secs + self.empty_block_interval_secs
    }

    pub fn next_block(&self) -> u64 {
        self.next_block
    }

    pub fn next_index(&self) -> u16 {
        self.next_index
    }

    /// Execute `txs` as the next sub-block, sign the header, fsync it to the log, publish it to the
    /// sink, and — if this was the block's 20th sub-block — ask the executor for the block's state root.
    ///
    /// `now_us` is the caller's wall-clock reading; the sub-block's actual `timestamp_us` is
    /// `max(now_us, prev_sub_block_ts_us + 1)`, never `now_us` verbatim,
    /// so the sealed sequence strictly increases even if the clock stalls or steps backward.
    ///
    /// `limits` bounds the executor: a tx the executor did not reach
    /// comes back in `outcome.not_executed` and is **not** logged as part of this sub-block. Nor
    /// is a tx the executor `rejected` at execution (bad nonce, insufficient
    /// funds, ...) — the record is the block: only `outcome.included` is logged, in its
    /// own order, the exact set `tx_root`/`receipts_root`/`gas_used` cover (see
    /// [`included_raw_txs`]). The caller (the sequencer actor) is responsible for re-queuing
    /// `not_executed` at the front of admission for the next sub-block and for telling a rejected
    /// tx's sender via the `Rejected` reply / admission's reconcile path; this function only reports
    /// both.
    ///
    /// Returns `Err` on log I/O failure or an executor error. Both are
    /// fatal to the caller by design (never answer a user before the fsync — a log write
    /// is on the path every pre-confirmation depends on, and an executor error leaves its in-memory state
    /// of unknown shape) — this function does not try to paper over either with a retry or a skipped ack;
    /// the caller shuts the actor down cleanly instead of panicking.
    pub async fn seal_sub_block(
        &mut self,
        txs: Vec<Bytes>,
        now_us: u64,
        limits: SubBlockLimits,
    ) -> Result<Tick, SealError> {
        let index = self.next_index;
        // The idle rule is evaluated FIRST — before `prev_sub_block_ts_us` is
        // touched, before `first_timestamp_us_in_block` is set, before `open_block` is ever called.
        // Only the very first sub-block of a not-yet-opened block can be idle (`index == 0`); once a
        // block has opened, every later sub-block seals as it always has (a block that opened always
        // completes its full `sub_blocks_per_block` records) regardless of whether it carries
        // transactions. An idle tick touches nothing: no log record, no executor call, no state
        // mutation of any kind — `now_us` is handed back verbatim for the caller's own bookkeeping.
        // The deposits this block credits are decided here, once, from the feed. A block opened with a waiting
        // deposit is never idle: the deposit is a reason to seal as good as a transaction is.
        let credits = if index == 0 {
            self.deposits
                .as_ref()
                .map(|f| f.next_block())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        if index == 0
            && txs.is_empty()
            && credits.is_empty()
            && !self.empty_block_due(now_us / 1_000_000)
        {
            return Ok(Tick::Idle { now_us });
        }
        let withdrawals: Vec<_> = credits.iter().map(|d| d.withdrawal()).collect();

        let block = self.next_block;
        let timestamp_us = resolve_sub_block_timestamp_us(now_us, self.prev_sub_block_ts_us);
        self.prev_sub_block_ts_us = timestamp_us;
        if index == 0 {
            self.first_timestamp_us_in_block = timestamp_us;
            // This block's environment is resolved once, right here —
            // before its first sub-block executes — and handed to the executor via `open_block`, never
            // re-derived at seal time. `resolve_block_timestamp_secs` is pure and its inputs
            // (`first_timestamp_us_in_block`, `prev_block_timestamp_secs`) are unchanged for the rest of
            // this block, so the identical computation the block-close branch below performs for
            // `BlockSealInputs::timestamp_secs` is guaranteed to agree with what was opened here.
            let block_timestamp_secs = resolve_block_timestamp_secs(
                timestamp_us / 1_000_000,
                self.prev_block_timestamp_secs,
            );
            let env = BlockEnv {
                number: block,
                timestamp_secs: block_timestamp_secs,
                gas_limit: self.block_gas_limit,
                // This chain's own fee recipient (genesis `coinbase`).
                coinbase: self.fee_recipient,
                // `prev_randao` is a function of `(chain_id, number)`
                // only — its epoch length is the fixed `PREV_RANDAO_EPOCH_BLOCKS` constant, never this
                // chain's `[profile].blocks_per_batch` (a different quantity: the batcher's own posting
                // cadence).
                prev_randao: prev_randao(self.chain_id, block),
                base_fee: None,
                withdrawals: withdrawals.clone(),
            };
            self.executor.open_block(env).await?;
        }

        let outcome = self.executor.execute_sub_block(&txs, limits).await?;

        let tx_root = crate::merkle::root(&outcome.included);
        let header = SubBlockHeader {
            chain_id: self.chain_id,
            block,
            index,
            timestamp_us,
            tx_root,
            receipts_root: outcome.receipts_root,
            gas_used: outcome.gas_used,
            prev_hash: self.prev_header_hash,
            // Only the index-0 header of a block that credits deposits carries it: one past the last deposit.
            deposits_end: credits.last().map(|d| d.index + 1),
        };
        let signature = sign_header(&self.signer, &header);
        let header_hash = header.hash();

        // Verify the executor's contract before trusting it — check
        // this before doing any arithmetic on the lengths, since `not_executed.len() > txs.len()` would
        // otherwise panic the subtraction below rather than fail cleanly.
        if !txs.ends_with(&outcome.not_executed) {
            return Err(SealError::ExecutorContract {
                block,
                index,
                reason: format!(
                    "not_executed ({} txs) is not the in-order suffix of the {} attempted txs",
                    outcome.not_executed.len(),
                    txs.len()
                ),
            });
        }

        // Only the txs the executor actually reached are candidates for this sub-block's record — a
        // not_executed tx will be logged whenever it's finally reached, in a later sub-block.
        let attempted_len = txs.len() - outcome.not_executed.len();
        // The record IS the block — logged txs are `outcome.included`
        // alone, in `tx_root`'s own order, never the wider `attempted` slice a rejected tx also sits
        // in. See `included_raw_txs`'s doc for the full rationale and the contract check it runs.
        let logged_txs = included_raw_txs(&txs[..attempted_len], &outcome).map_err(|reason| {
            SealError::ExecutorContract {
                block,
                index,
                reason,
            }
        })?;
        // Isolates `LogWriter::append`'s own write+fsync cost (see
        // `SealResult::log_fsync_seconds`'s doc) — brackets exactly this call, nothing else.
        let log_append_started = std::time::Instant::now();
        self.log
            .append_with_withdrawals(&header, &signature, &logged_txs, &withdrawals)?;
        let log_fsync_seconds = log_append_started.elapsed().as_secs_f64();
        self.prev_header_hash = header_hash;
        // The credits are durable now; only now does the sealer move past them.
        if let Some(last) = credits.last() {
            self.deposits_end = last.index + 1;
            if let Some(feed) = &self.deposits {
                feed.set_included(self.deposits_end);
            }
            self.block_deposit_enqueue_ts = credits.iter().map(|d| d.enqueue_unix_ts).collect();
        }

        self.sink.publish(SealedSubBlock {
            header,
            header_hash,
            signature,
            included: outcome.included.clone(),
        });

        self.sub_block_hashes_in_block.push(header_hash);
        self.gas_in_block += outcome.gas_used;

        let mut block_gas_used = None;
        let mut block_timestamp_ahead_seconds = None;
        let mut closed_block_deposit_enqueue_ts = Vec::new();
        let block_sealed = if index + 1 == self.sub_blocks_per_block {
            let first_sub_block_secs = self.first_timestamp_us_in_block / 1_000_000;
            let block_timestamp_secs =
                resolve_block_timestamp_secs(first_sub_block_secs, self.prev_block_timestamp_secs);
            let inputs = BlockSealInputs {
                block,
                timestamp_secs: block_timestamp_secs,
                sub_block_header_hashes: std::mem::take(&mut self.sub_block_hashes_in_block),
                total_gas_used: self.gas_in_block,
            };
            // Captured before `gas_in_block`/`first_timestamp_us_in_block` reset below.
            block_gas_used = Some(self.gas_in_block);
            block_timestamp_ahead_seconds =
                Some(block_timestamp_secs as f64 - first_sub_block_secs as f64);
            self.gas_in_block = 0;
            self.first_timestamp_us_in_block = 0;
            self.prev_block_timestamp_secs = block_timestamp_secs;
            let block_outcome = self.executor.seal_block(inputs).await?;
            self.next_block = block + 1;
            self.next_index = 0;
            closed_block_deposit_enqueue_ts = std::mem::take(&mut self.block_deposit_enqueue_ts);
            Some(block_outcome)
        } else {
            self.next_index = index + 1;
            None
        };

        Ok(Tick::Sealed(Box::new(SealResult {
            header,
            header_hash,
            signature,
            outcome,
            block_sealed,
            log_fsync_seconds,
            block_gas_used,
            block_timestamp_ahead_seconds,
            deposits_credited: credits.len(),
            closed_block_deposit_enqueue_ts,
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{BlockEnv, ExecutorError, Head, MockExecutor};
    use crate::preconf::ChannelSink;
    use alloy::primitives::Address;
    use tempfile::tempdir;

    /// `empty_block_interval_secs: 1` — the minimum legal nonzero cadence at this
    /// profile's 1 s block time — so every existing test below (none of which is testing idle
    /// behaviour) keeps sealing a sub-block on every tick exactly as it did before this knob existed,
    /// with no other change to any test body. Tests that DO exercise idle behaviour build their own
    /// `SealerState` directly, at the constructor's true default of 0 (see `idle_state` below).
    fn state(dir: &std::path::Path) -> SealerState<MockExecutor, ChannelSink> {
        SealerState::new(
            MockExecutor::new(),
            LogWriter::open(dir, 1_000).unwrap(),
            PrivateKeySigner::random(),
            ChannelSink::new(16),
            1,
            DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            SUB_BLOCKS_PER_BLOCK,
            ResumePoint::default(),
        )
        .with_empty_block_interval_secs(1)
    }

    /// A chain profile declaring 25 ms sub-blocks x 40 per block
    /// (still a 1 s block time — the whole-seconds invariant `crate::profile::Profile::validate`
    /// enforces) must close a block at sub-block index 40, not the design-default 20 — proving
    /// `sub_blocks_per_block` is a genuine runtime parameter of the sealer, not the compile-time
    /// `SUB_BLOCKS_PER_BLOCK` constant. `SealerState` once had no such parameter at all —
    /// this profile could not even be expressed. `sub_block_gas_limit` is left at the design default
    /// (5,000,000), which at 25 ms doubles gas/s to 200M — the profile's own budgets are raised to
    /// match (this test's choice, not a design premise), and validated here rather than assumed.
    #[tokio::test]
    async fn forty_sub_blocks_at_25ms_close_one_block_at_index_40_not_20() {
        let profile = crate::profile::Profile {
            sub_block_ms: 25,
            sub_blocks_per_block: 40,
            prover_gas_per_sec: 200_000_000,
            da_bytes_per_sec: ((200_000_000u128 * 74) / 21_000) as u64,
            ..crate::profile::Profile::default()
        };
        profile.validate().unwrap();
        assert_eq!(profile.block_time_ms(), 1_000, "still a 1 s block");

        let dir = tempdir().unwrap();
        let mut s = SealerState::new(
            MockExecutor::new(),
            LogWriter::open(dir.path(), 1_000).unwrap(),
            PrivateKeySigner::random(),
            ChannelSink::new(16),
            1,
            profile.effective_block_gas_limit(),
            Address::ZERO,
            profile.sub_blocks_per_block,
            ResumePoint::default(),
        )
        .with_empty_block_interval_secs(1);
        let base_ts = 1_757_000_000_000_000u64;

        for i in 0..40u16 {
            assert_eq!(s.next_index(), i);
            assert_eq!(s.next_block(), 1);
            let result = s
                .seal_sub_block(
                    vec![],
                    base_ts + i as u64 * 25_000,
                    SubBlockLimits::unbounded(),
                )
                .await
                .unwrap()
                .sealed();
            assert_eq!(result.header.index, i);
            assert_eq!(result.header.block, 1);
            if i == 39 {
                assert!(
                    result.block_sealed.is_some(),
                    "the 40th sub-block must close the block at this profile's cadence"
                );
            } else {
                assert!(
                    result.block_sealed.is_none(),
                    "sub-block {i} must not close the block early"
                );
            }
        }
        assert_eq!(s.next_block(), 2);
        assert_eq!(s.next_index(), 0);
    }

    #[tokio::test]
    async fn twenty_sub_blocks_close_one_block_with_indices_0_to_19() {
        let dir = tempdir().unwrap();
        let mut s = state(dir.path());
        let base_ts = 1_757_000_000_000_000u64;

        for i in 0..20u16 {
            assert_eq!(s.next_index(), i);
            assert_eq!(s.next_block(), 1);
            let result = s
                .seal_sub_block(
                    vec![],
                    base_ts + i as u64 * 50_000,
                    SubBlockLimits::unbounded(),
                )
                .await
                .unwrap()
                .sealed();
            assert_eq!(result.header.index, i);
            assert_eq!(result.header.block, 1);
            if i == 19 {
                assert!(
                    result.block_sealed.is_some(),
                    "the 20th sub-block must close the block"
                );
            } else {
                assert!(result.block_sealed.is_none());
            }
        }
        assert_eq!(s.next_block(), 2);
        assert_eq!(s.next_index(), 0);
    }

    /// The sealed header's `tx_root` must be the binary Merkle root over the *included* tx hashes, in
    /// order — not the old flat `keccak(concat(hashes))`. Three included
    /// txs exercises the odd-level duplication case.
    #[tokio::test]
    async fn tx_root_is_the_merkle_root_of_included_tx_hashes() {
        use crate::testutil::signed_raw_tx;
        let dir = tempdir().unwrap();
        let mut s = state(dir.path());
        let sender = PrivateKeySigner::random();
        let txs: Vec<_> = (0..3u64).map(|n| signed_raw_tx(&sender, 1, n)).collect();

        let result = s
            .seal_sub_block(txs, 1_757_000_000_000_000, SubBlockLimits::unbounded())
            .await
            .unwrap()
            .sealed();
        assert_eq!(result.outcome.included.len(), 3);
        let expected = crate::merkle::root(&result.outcome.included);
        assert_eq!(result.header.tx_root, expected);
    }

    /// A sub-block with attempted txs `[a, b_rejected, c]` must log exactly `[a, c]` — never `[a, b, c]`
    /// — because the record is the block and `b` was never part of it. `b` is a
    /// wrong-nonce tx from a sender the executor has never seen, so `MockExecutor` rejects it
    /// (`NonceTooHigh`) without touching state; `a` and `c` are ordinary nonce-0 sends from two other
    /// senders and are included either side of it.
    #[tokio::test]
    async fn logged_txs_are_exactly_the_included_set_not_the_attempted_superset() {
        use crate::testutil::signed_raw_tx;
        let dir = tempdir().unwrap();
        let mut s = state(dir.path());

        let sender_a = PrivateKeySigner::random();
        let sender_b = PrivateKeySigner::random();
        let sender_c = PrivateKeySigner::random();
        let tx_a = signed_raw_tx(&sender_a, 1, 0);
        let tx_b = signed_raw_tx(&sender_b, 1, 5); // wrong nonce (expected 0) -> rejected
        let tx_c = signed_raw_tx(&sender_c, 1, 0);

        let result = s
            .seal_sub_block(
                vec![tx_a.clone(), tx_b.clone(), tx_c.clone()],
                1_757_000_000_000_000,
                SubBlockLimits::unbounded(),
            )
            .await
            .unwrap()
            .sealed();

        assert_eq!(result.outcome.included.len(), 2, "a and c must be included");
        assert_eq!(result.outcome.rejected.len(), 1, "b must be rejected");
        let expected_tx_root = crate::merkle::root(&result.outcome.included);
        assert_eq!(
            result.header.tx_root, expected_tx_root,
            "tx_root already only ever covered outcome.included"
        );

        let mut logged = Vec::new();
        let torn = crate::log::replay(dir.path(), false, |r| logged.push(r.clone())).unwrap();
        assert!(torn.is_none());
        assert_eq!(logged.len(), 1, "exactly one sub-block record");
        assert_eq!(
            logged[0].txs,
            vec![tx_a, tx_c],
            "the logged record must hold exactly the included txs [a, c] — the rejected tx b must \
             never reach the log or DA"
        );
    }

    /// `SealResult::log_fsync_seconds` must report a real, positive measurement
    /// of `LogWriter::append`'s own write+fsync call — the investigation into the residual
    /// idle-host p99 needs this isolated from `seal_sub_block`'s other costs (admission drain,
    /// execution, signing) to test the I/O-contention-with-MDBX-commit hypothesis.
    #[tokio::test]
    async fn seal_result_reports_a_real_log_fsync_duration() {
        let dir = tempdir().unwrap();
        let mut s = state(dir.path());
        let result = s
            .seal_sub_block(vec![], 1_757_000_000_000_000, SubBlockLimits::unbounded())
            .await
            .unwrap()
            .sealed();
        assert!(
            result.log_fsync_seconds > 0.0,
            "a real write_all + sync_data call must take measurable, positive wall time, got \
             {}",
            result.log_fsync_seconds
        );
        assert!(
            result.log_fsync_seconds < 1.0,
            "a single small-frame fsync must not plausibly take a full second on any real disk, \
             got {}",
            result.log_fsync_seconds
        );
    }

    /// End to end at the sealer level: 300 txs against a 5,000,000 gas
    /// sub-block budget include exactly 238 (`floor(5_000_000 / 21_000)`), carrying the other 62. Fed as
    /// the very next sub-block's txs — exactly what the sequencer actor does via
    /// `Admission::requeue_front` — all 62 are included, in their original order, and this sub-block's own
    /// `tx_root` covers only them (nothing double-counted, nothing dropped).
    #[tokio::test]
    async fn not_executed_txs_are_carried_and_included_in_the_next_sub_block_in_order() {
        use crate::executor::SubBlockLimits;
        use crate::testutil::sender_and_hash;
        use crate::tx::parse;
        use std::time::{Duration, Instant};

        let dir = tempdir().unwrap();
        let mut s = state(dir.path());
        let sender = PrivateKeySigner::random();
        let txs: Vec<_> = (0..300u64)
            .map(|n| crate::testutil::signed_raw_tx(&sender, 1, n))
            .collect();

        let first = s
            .seal_sub_block(
                txs.clone(),
                1_757_000_000_000_000,
                SubBlockLimits {
                    gas_limit: 5_000_000,
                    deadline: Instant::now() + Duration::from_secs(3600),
                },
            )
            .await
            .unwrap()
            .sealed();
        assert_eq!(first.outcome.included.len(), 238);
        assert_eq!(first.outcome.not_executed.len(), 62);
        assert_eq!(
            first.outcome.not_executed,
            txs[238..],
            "carried txs must be exactly the unreached suffix, in order"
        );

        // What the sequencer actor does next: feed the carried txs as the next sub-block's input.
        let second = s
            .seal_sub_block(
                first.outcome.not_executed.clone(),
                1_757_000_000_050_000,
                SubBlockLimits::unbounded(),
            )
            .await
            .unwrap()
            .sealed();
        assert_eq!(
            second.outcome.included.len(),
            62,
            "all 62 must now be included"
        );
        assert!(second.outcome.not_executed.is_empty());

        let expected_hashes: Vec<_> = txs[238..]
            .iter()
            .map(|raw| sender_and_hash(raw).1)
            .collect();
        assert_eq!(
            second.outcome.included, expected_hashes,
            "the 62 carried txs must be included in their original order"
        );
        // Sanity: every carried tx really was reachable this time (sequential nonces 238..300 against a
        // nonce cache that had already advanced past 238 in the first call).
        for raw in &txs[238..] {
            parse(raw.clone()).unwrap();
        }
    }

    #[tokio::test]
    async fn block_timestamp_is_the_first_sub_blocks_timestamp_truncated_to_seconds() {
        struct SpyExecutor {
            inner: MockExecutor,
            last_seal_inputs: Option<BlockSealInputs>,
        }
        impl Executor for SpyExecutor {
            async fn open_block(&mut self, env: BlockEnv) -> Result<(), ExecutorError> {
                self.inner.open_block(env).await
            }
            async fn execute_sub_block(
                &mut self,
                txs: &[Bytes],
                limits: SubBlockLimits,
            ) -> Result<SubBlockOutcome, ExecutorError> {
                self.inner.execute_sub_block(txs, limits).await
            }
            async fn seal_block(
                &mut self,
                inputs: BlockSealInputs,
            ) -> Result<BlockOutcome, ExecutorError> {
                self.last_seal_inputs = Some(inputs.clone());
                self.inner.seal_block(inputs).await
            }
            fn head(&self) -> Head {
                self.inner.head()
            }
            fn nonce(&self, addr: Address) -> u64 {
                self.inner.nonce(addr)
            }
        }

        let dir = tempdir().unwrap();
        let mut s = SealerState::new(
            SpyExecutor {
                inner: MockExecutor::new(),
                last_seal_inputs: None,
            },
            LogWriter::open(dir.path(), 1_000).unwrap(),
            PrivateKeySigner::random(),
            ChannelSink::new(16),
            1,
            DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            SUB_BLOCKS_PER_BLOCK,
            ResumePoint::default(),
        )
        .with_empty_block_interval_secs(1);

        let first_ts_us = 1_757_000_000_123_456u64; // truncates to 1_757_000_000
        for i in 0..20u16 {
            s.seal_sub_block(
                vec![],
                first_ts_us + i as u64 * 50_000,
                SubBlockLimits::unbounded(),
            )
            .await
            .unwrap();
        }
        let inputs = s.executor.last_seal_inputs.as_ref().unwrap();
        assert_eq!(inputs.timestamp_secs, 1_757_000_000);
        assert_eq!(inputs.sub_block_header_hashes.len(), 20);
    }

    #[tokio::test]
    async fn header_prev_hash_chains_across_the_block_boundary() {
        let dir = tempdir().unwrap();
        let mut s = state(dir.path());
        let mut prev = B256::ZERO;
        for i in 0..25u64 {
            let r = s
                .seal_sub_block(
                    vec![],
                    1_757_000_000_000_000 + i * 50_000,
                    SubBlockLimits::unbounded(),
                )
                .await
                .unwrap()
                .sealed();
            assert_eq!(
                r.header.prev_hash, prev,
                "sub-block {i} must chain to the previous header hash"
            );
            prev = r.header_hash;
        }
    }

    /// Sub-block `timestamp_us` = `max(now_us, prev_sub_block_ts_us + 1)`.
    /// If the wall clock reports a time at or before the previous sub-block's timestamp (NTP step,
    /// scheduler jitter, a paused-then-resumed clock), the sealed timestamp must still strictly increase.
    #[tokio::test]
    async fn sub_block_timestamp_never_goes_backward_even_if_the_clock_does() {
        let dir = tempdir().unwrap();
        let mut s = state(dir.path());

        let r1 = s
            .seal_sub_block(vec![], 1_757_000_000_000_000, SubBlockLimits::unbounded())
            .await
            .unwrap()
            .sealed();
        assert_eq!(r1.header.timestamp_us, 1_757_000_000_000_000);

        // The clock goes backward on the next tick.
        let r2 = s
            .seal_sub_block(vec![], 1_756_999_999_999_000, SubBlockLimits::unbounded())
            .await
            .unwrap()
            .sealed();
        assert_eq!(
            r2.header.timestamp_us,
            r1.header.timestamp_us + 1,
            "timestamp must still strictly increase despite the clock going backward"
        );

        // And again forward from a clock that is still behind the sealed sequence.
        let r3 = s
            .seal_sub_block(vec![], 1_756_999_999_999_500, SubBlockLimits::unbounded())
            .await
            .unwrap()
            .sealed();
        assert_eq!(r3.header.timestamp_us, r2.header.timestamp_us + 1);
    }

    #[derive(Default)]
    struct RecordingExecutor {
        inner: MockExecutor,
        seal_inputs: Vec<BlockSealInputs>,
    }
    impl Executor for RecordingExecutor {
        async fn open_block(&mut self, env: BlockEnv) -> Result<(), ExecutorError> {
            self.inner.open_block(env).await
        }
        async fn execute_sub_block(
            &mut self,
            txs: &[Bytes],
            limits: SubBlockLimits,
        ) -> Result<SubBlockOutcome, ExecutorError> {
            self.inner.execute_sub_block(txs, limits).await
        }
        async fn seal_block(
            &mut self,
            inputs: BlockSealInputs,
        ) -> Result<BlockOutcome, ExecutorError> {
            self.seal_inputs.push(inputs.clone());
            self.inner.seal_block(inputs).await
        }
        fn head(&self) -> Head {
            self.inner.head()
        }
        fn nonce(&self, addr: Address) -> u64 {
            self.inner.nonce(addr)
        }
    }

    /// Block timestamp = `max(first_sub_block_secs,
    /// prev_block_timestamp_secs + 1)`. Missed ticks can seal 40 sub-blocks (two full blocks) all within
    /// the same wall-clock second; block 1's and block 2's EVM timestamps must still be strictly
    /// increasing, never equal.
    #[tokio::test]
    async fn two_blocks_sealed_within_one_wall_clock_second_still_get_strictly_increasing_timestamps(
    ) {
        let dir = tempdir().unwrap();
        let mut s = SealerState::new(
            RecordingExecutor::default(),
            LogWriter::open(dir.path(), 1_000).unwrap(),
            PrivateKeySigner::random(),
            ChannelSink::new(16),
            1,
            DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            SUB_BLOCKS_PER_BLOCK,
            ResumePoint::default(),
        );

        // Every sub-block reports the exact same wall-clock instant, on a whole-second boundary — the
        // clamp inside seal_sub_block advances each sub-block's own timestamp by at least 1us, so all 40
        // stay within the same wall-clock second (40us of drift can't cross a second boundary).
        // At this profile's default `empty_block_interval_secs` (0), a genuinely empty index-0
        // tick would be idle, so each block's own opening sub-block (i == 0, i == 20) carries one real
        // tx — nothing else about this test (or what it measures) changes; every later sub-block in each
        // block stays empty, exactly as before, since an opened block always completes regardless of
        // content.
        let now_us = 1_757_000_000_000_000u64;
        let opener = PrivateKeySigner::random();
        for i in 0..40u16 {
            let txs = if i % SUB_BLOCKS_PER_BLOCK == 0 {
                vec![crate::testutil::signed_raw_tx(
                    &opener,
                    1,
                    i as u64 / SUB_BLOCKS_PER_BLOCK as u64,
                )]
            } else {
                vec![]
            };
            s.seal_sub_block(txs, now_us, SubBlockLimits::unbounded())
                .await
                .unwrap();
        }

        assert_eq!(
            s.executor.seal_inputs.len(),
            2,
            "two blocks must have closed"
        );
        let block0_secs = s.executor.seal_inputs[0].timestamp_secs;
        let block1_secs = s.executor.seal_inputs[1].timestamp_secs;
        assert!(
            block1_secs > block0_secs,
            "block 2's timestamp ({block1_secs}) must strictly exceed block 1's ({block0_secs}) \
             even though both blocks' sub-blocks landed in the same wall-clock second"
        );
    }

    /// Returns a `not_executed` set that is deliberately **not** the in-order suffix of the attempted
    /// txs — the sealer's contract check must catch this rather than
    /// silently computing `attempted_len = txs.len() - not_executed.len()` and logging the wrong prefix
    /// (or, when `not_executed` is longer than `txs`, panicking on subtraction overflow).
    #[derive(Default)]
    struct SuffixViolatingExecutor {
        inner: MockExecutor,
    }
    impl Executor for SuffixViolatingExecutor {
        async fn open_block(&mut self, env: BlockEnv) -> Result<(), ExecutorError> {
            self.inner.open_block(env).await
        }
        async fn execute_sub_block(
            &mut self,
            txs: &[Bytes],
            limits: SubBlockLimits,
        ) -> Result<SubBlockOutcome, ExecutorError> {
            let mut outcome = self.inner.execute_sub_block(txs, limits).await?;
            // Claim the *first* tx was not reached, even though every tx was actually included — not the
            // in-order suffix `txs[attempted_len..]` the sealer's contract requires.
            outcome.not_executed = vec![txs[0].clone()];
            Ok(outcome)
        }
        async fn seal_block(
            &mut self,
            inputs: BlockSealInputs,
        ) -> Result<BlockOutcome, ExecutorError> {
            self.inner.seal_block(inputs).await
        }
        fn head(&self) -> Head {
            self.inner.head()
        }
        fn nonce(&self, addr: Address) -> u64 {
            self.inner.nonce(addr)
        }
    }

    /// `sealer.rs` assumes `outcome.not_executed` is exactly the in-order
    /// suffix of the attempted txs (`txs[attempted_len..]`) — sealer.rs's own log-slicing and
    /// sequencer.rs's carry-forward both depend on that. An executor that violates it must be caught at
    /// the boundary as a fatal `SealError`, never silently trusted (which would log the wrong prefix as
    /// this sub-block's record, permanently corrupting the log the challenger protocol depends on).
    #[tokio::test]
    async fn executor_returning_a_non_suffix_not_executed_is_a_fatal_seal_error() {
        use crate::testutil::signed_raw_tx;
        let dir = tempdir().unwrap();
        let mut s = SealerState::new(
            SuffixViolatingExecutor::default(),
            LogWriter::open(dir.path(), 1_000).unwrap(),
            PrivateKeySigner::random(),
            ChannelSink::new(16),
            1,
            DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            SUB_BLOCKS_PER_BLOCK,
            ResumePoint::default(),
        );
        let sender = PrivateKeySigner::random();
        let txs: Vec<_> = (0..3u64).map(|n| signed_raw_tx(&sender, 1, n)).collect();

        let err = s
            .seal_sub_block(txs, 1_757_000_000_000_000, SubBlockLimits::unbounded())
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                SealError::ExecutorContract {
                    block: 1,
                    index: 0,
                    ..
                }
            ),
            "expected SealError::ExecutorContract{{block: 1, index: 0, ..}}, got {err:?}"
        );
    }

    #[derive(Clone, Default)]
    struct SlowMockExecutor {
        inner: std::sync::Arc<tokio::sync::Mutex<MockExecutor>>,
        delay: std::time::Duration,
        /// A separate, independent delay for `seal_block` itself — the
        /// measured 122 ms at the 5k-tx design load point is a `seal_block` cost, not an
        /// `execute_sub_block` one, so the overrun-safety guarantee needs its own coverage there.
        seal_delay: std::time::Duration,
    }
    impl Executor for SlowMockExecutor {
        async fn open_block(&mut self, env: BlockEnv) -> Result<(), ExecutorError> {
            self.inner.lock().await.open_block(env).await
        }
        async fn execute_sub_block(
            &mut self,
            txs: &[Bytes],
            limits: SubBlockLimits,
        ) -> Result<SubBlockOutcome, ExecutorError> {
            tokio::time::sleep(self.delay).await;
            self.inner.lock().await.execute_sub_block(txs, limits).await
        }
        async fn seal_block(
            &mut self,
            inputs: BlockSealInputs,
        ) -> Result<BlockOutcome, ExecutorError> {
            tokio::time::sleep(self.seal_delay).await;
            self.inner.lock().await.seal_block(inputs).await
        }
        fn head(&self) -> Head {
            self.inner.try_lock().map(|g| g.head()).unwrap_or_default()
        }
        fn nonce(&self, addr: Address) -> u64 {
            self.inner
                .try_lock()
                .map(|g| g.nonce(addr))
                .unwrap_or_default()
        }
    }

    /// If execution of a sub-block runs past the 50 ms deadline, the sealer must still seal what was
    /// executed (never block forever, never drop the tick) — it just seals late. This is the mechanics
    /// half of "deadline overrun carries txs"; the timer-loop half (the next tick is not a catch-up
    /// burst) is exercised by `sequencer::tests`.
    #[tokio::test(start_paused = true)]
    async fn overrun_execution_still_seals_instead_of_blocking_forever() {
        let dir = tempdir().unwrap();
        let mut s = SealerState::new(
            SlowMockExecutor {
                delay: std::time::Duration::from_millis(200),
                ..Default::default()
            },
            LogWriter::open(dir.path(), 1_000).unwrap(),
            PrivateKeySigner::random(),
            ChannelSink::new(16),
            1,
            DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            SUB_BLOCKS_PER_BLOCK,
            ResumePoint::default(),
        )
        .with_empty_block_interval_secs(1);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            s.seal_sub_block(vec![], 1_757_000_000_000_000, SubBlockLimits::unbounded()),
        )
        .await;
        assert!(
            result.is_ok(),
            "a sub-block that overruns its deadline must still complete and seal"
        );
    }

    /// `seal_block` itself (not sub-block execution) is what measured
    /// 122 ms at the 5k-tx design load point — well past the 50 ms tick. This proves the SAME
    /// overrun-safety guarantee holds for a slow `seal_block`: the 20th sub-block still completes and
    /// seals (never blocks the actor forever), it just seals late — exactly the `MissedTickBehavior::
    /// Skip` contract `sequencer.rs`'s tick loop already relies on. The timer-loop half (the NEXT tick
    /// resumes on the original 50 ms grid rather than bursting) is exercised by `sequencer::tests` and
    /// is unaffected by which of the two calls (`execute_sub_block` or `seal_block`) was slow.
    #[tokio::test(start_paused = true)]
    async fn overrun_seal_block_still_completes_instead_of_blocking_forever() {
        let dir = tempdir().unwrap();
        let mut s = SealerState::new(
            SlowMockExecutor {
                seal_delay: std::time::Duration::from_millis(200),
                ..Default::default()
            },
            LogWriter::open(dir.path(), 1_000).unwrap(),
            PrivateKeySigner::random(),
            ChannelSink::new(16),
            1,
            DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            SUB_BLOCKS_PER_BLOCK,
            ResumePoint::default(),
        )
        .with_empty_block_interval_secs(1);
        for i in 0..19u16 {
            s.seal_sub_block(
                vec![],
                1_757_000_000_000_000 + i as u64 * 50_000,
                SubBlockLimits::unbounded(),
            )
            .await
            .unwrap();
        }
        // The 20th sub-block closes the block — this is the call whose `seal_block` is slow.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            s.seal_sub_block(
                vec![],
                1_757_000_000_000_000 + 19 * 50_000,
                SubBlockLimits::unbounded(),
            ),
        )
        .await;
        let seal_result = result.expect("a slow seal_block must still complete, not hang forever");
        assert!(
            seal_result.unwrap().sealed().block_sealed.is_some(),
            "the 20th sub-block must still close the block despite seal_block's overrun"
        );
    }

    // ---- the sequencer seals nothing when idle --------------------------------

    /// Counts every call an executor receives — used to prove an idle tick makes none at all.
    #[derive(Default)]
    struct CountingExecutor {
        inner: MockExecutor,
        open_block_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        execute_sub_block_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        seal_block_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Executor for CountingExecutor {
        async fn open_block(&mut self, env: BlockEnv) -> Result<(), ExecutorError> {
            self.open_block_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.open_block(env).await
        }
        async fn execute_sub_block(
            &mut self,
            txs: &[Bytes],
            limits: SubBlockLimits,
        ) -> Result<SubBlockOutcome, ExecutorError> {
            self.execute_sub_block_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.execute_sub_block(txs, limits).await
        }
        async fn seal_block(
            &mut self,
            inputs: BlockSealInputs,
        ) -> Result<BlockOutcome, ExecutorError> {
            self.seal_block_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.seal_block(inputs).await
        }
        fn head(&self) -> Head {
            self.inner.head()
        }
        fn nonce(&self, addr: Address) -> u64 {
            self.inner.nonce(addr)
        }
    }

    /// At this profile's true default (`empty_block_interval_secs == 0`), 100
    /// ticks with no transactions must write no log record, call the executor zero times, and leave
    /// the sealer's own head untouched. **Mutation** (drop the `index == 0` guard, or move the idle
    /// check to run after `open_block` is called): `open_block_calls`/`execute_sub_block_calls` go
    /// nonzero and this test goes red.
    #[tokio::test]
    async fn idle_ticks_write_no_record_and_call_no_executor() {
        let dir = tempdir().unwrap();
        let mut s = SealerState::new(
            CountingExecutor::default(),
            LogWriter::open(dir.path(), 1_000).unwrap(),
            PrivateKeySigner::random(),
            ChannelSink::new(16),
            1,
            DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            SUB_BLOCKS_PER_BLOCK,
            ResumePoint::default(),
        ); // empty_block_interval_secs stays 0 — the constructor's true default, "never".

        let base_ts = 1_757_000_000_000_000u64;
        for i in 0..100u64 {
            let tick = s
                .seal_sub_block(vec![], base_ts + i * 50_000, SubBlockLimits::unbounded())
                .await
                .unwrap();
            assert!(
                tick.is_idle(),
                "tick {i} must be idle: no transactions, interval 0 (never)"
            );
        }

        use std::sync::atomic::Ordering::SeqCst;
        assert_eq!(s.executor.open_block_calls.load(SeqCst), 0);
        assert_eq!(s.executor.execute_sub_block_calls.load(SeqCst), 0);
        assert_eq!(s.executor.seal_block_calls.load(SeqCst), 0);
        assert_eq!(s.next_block(), 1, "no block ever opened");
        assert_eq!(
            s.next_index(),
            0,
            "still waiting to open block 1's first sub-block"
        );

        let mut records = Vec::new();
        let torn = crate::log::replay(dir.path(), false, |r| records.push(r.clone())).unwrap();
        assert!(torn.is_none());
        assert!(records.is_empty(), "an idle tick must write no log record");
    }

    /// After 10s of idle (200 ticks at the 50ms cadence, no
    /// transactions, interval 0), a real tx opens block 1 at index 0 with the WALL-CLOCK timestamp —
    /// never a `prev + 1` catch-up off a timestamp an idle tick might otherwise have advanced.
    /// **Mutation** (evaluate the idle check after `first_timestamp_us_in_block`/`prev_sub_block_ts_us`
    /// are mutated): idle ticks perturb those trackers and this test's exact-equality assertion goes
    /// red.
    #[tokio::test]
    async fn first_tx_after_idle_opens_block_index_0_with_wall_clock_timestamp_not_prev_plus_one() {
        let dir = tempdir().unwrap();
        let mut s = SealerState::new(
            MockExecutor::new(),
            LogWriter::open(dir.path(), 1_000).unwrap(),
            PrivateKeySigner::random(),
            ChannelSink::new(16),
            1,
            DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            SUB_BLOCKS_PER_BLOCK,
            ResumePoint::default(),
        );

        let base_ts = 1_757_000_000_000_000u64;
        for i in 0..200u64 {
            let tick = s
                .seal_sub_block(vec![], base_ts + i * 50_000, SubBlockLimits::unbounded())
                .await
                .unwrap();
            assert!(
                tick.is_idle(),
                "tick {i} must be idle during the 10s idle gap"
            );
        }

        // A transient clock spike, still idle (no transactions, interval 0): if an idle tick mutated
        // `prev_sub_block_ts_us`/`first_timestamp_us_in_block` from this reading (the "check moved
        // after the mutation" bug), the NEXT real tick — at a LOWER, perfectly ordinary timestamp —
        // would be clamped up to the spike's own `+ 1`, not its own real wall clock. Idle touching
        // nothing means this spike leaves no trace at all.
        let spike_tick = s
            .seal_sub_block(
                vec![],
                base_ts + 1_000_000_000_000, // ~11.5 days ahead — a bogus one-off reading
                SubBlockLimits::unbounded(),
            )
            .await
            .unwrap();
        assert!(spike_tick.is_idle(), "the spike tick must also be idle");

        let now_us = base_ts + 200 * 50_000; // back to ordinary, ~10s after the gap started.
        let sender = PrivateKeySigner::random();
        let tx = crate::testutil::signed_raw_tx(&sender, 1, 0);
        let result = s
            .seal_sub_block(vec![tx], now_us, SubBlockLimits::unbounded())
            .await
            .unwrap()
            .sealed();

        assert_eq!(result.header.block, 1);
        assert_eq!(result.header.index, 0);
        assert_eq!(
            result.header.timestamp_us, now_us,
            "the block that finally opens after an idle gap must carry the real wall-clock \
             timestamp, not a value derived from a stale prev_sub_block_ts_us + 1 — an idle tick \
             must not have advanced that value at all"
        );
    }

    /// Once a block has opened (its first sub-block sealed, real or not), every
    /// later tick in that block seals — even a totally empty one — until the 20th closes it. Confirms
    /// the idle gate applies ONLY at `index == 0`. **Mutation** (apply the idle check regardless of
    /// `index`): sub-blocks 1..19 would go idle too and this test goes red.
    #[tokio::test]
    async fn an_open_block_seals_all_20_sub_blocks_even_when_later_ticks_are_empty() {
        let dir = tempdir().unwrap();
        let mut s = SealerState::new(
            MockExecutor::new(),
            LogWriter::open(dir.path(), 1_000).unwrap(),
            PrivateKeySigner::random(),
            ChannelSink::new(16),
            1,
            DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            SUB_BLOCKS_PER_BLOCK,
            ResumePoint::default(),
        );
        let base_ts = 1_757_000_000_000_000u64;
        let sender = PrivateKeySigner::random();
        let tx = crate::testutil::signed_raw_tx(&sender, 1, 0);

        // Sub-block 0 opens the block with a real tx; every later sub-block (1..19) is empty.
        let opened = s
            .seal_sub_block(vec![tx], base_ts, SubBlockLimits::unbounded())
            .await
            .unwrap()
            .sealed();
        assert_eq!(opened.header.index, 0);
        assert!(opened.block_sealed.is_none());

        for i in 1..20u16 {
            let tick = s
                .seal_sub_block(
                    vec![],
                    base_ts + i as u64 * 50_000,
                    SubBlockLimits::unbounded(),
                )
                .await
                .unwrap();
            let result = tick.sealed();
            assert_eq!(
                result.header.index, i,
                "sub-block {i} must seal, empty or not"
            );
            if i == 19 {
                assert!(
                    result.block_sealed.is_some(),
                    "the 20th sub-block must close the block"
                );
            } else {
                assert!(result.block_sealed.is_none());
            }
        }
        assert_eq!(s.next_block(), 2);
        assert_eq!(s.next_index(), 0);
    }

    /// Interval 0 never seals an empty block, however many idle ticks fire.
    #[tokio::test]
    async fn interval_0_never_seals_empty() {
        let dir = tempdir().unwrap();
        let mut s = SealerState::new(
            MockExecutor::new(),
            LogWriter::open(dir.path(), 1_000).unwrap(),
            PrivateKeySigner::random(),
            ChannelSink::new(16),
            1,
            DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            SUB_BLOCKS_PER_BLOCK,
            ResumePoint::default(),
        );
        let base_ts = 1_757_000_000_000_000u64;
        for i in 0..1_000u64 {
            let tick = s
                .seal_sub_block(vec![], base_ts + i * 50_000, SubBlockLimits::unbounded())
                .await
                .unwrap();
            assert!(
                tick.is_idle(),
                "tick {i} must stay idle: interval 0 means never"
            );
        }
        assert_eq!(s.next_block(), 1);
        assert_eq!(s.next_index(), 0);
    }

    /// With a nonzero interval, an empty block seals at most once per N seconds —
    /// never more often. Interval 5s over 20s of fake-clock ticks (50ms/tick): exactly 4 empty blocks
    /// close (at t = 0, 5, 10, 15s), each a full 20-record block. **Mutation** (`empty_block_due`
    /// ignores N, e.g. is due whenever nonzero): more than 4 blocks would close and this test goes red.
    #[tokio::test]
    async fn interval_n_seals_an_empty_block_at_most_every_n_seconds() {
        let dir = tempdir().unwrap();
        let mut s = SealerState::new(
            MockExecutor::new(),
            LogWriter::open(dir.path(), 1_000).unwrap(),
            PrivateKeySigner::random(),
            ChannelSink::new(16),
            1,
            DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            SUB_BLOCKS_PER_BLOCK,
            ResumePoint::default(),
        )
        .with_empty_block_interval_secs(5);

        let base_ts = 1_757_000_000_000_000u64;
        let mut blocks_closed = 0u32;
        for i in 0..400u64 {
            let tick = s
                .seal_sub_block(vec![], base_ts + i * 50_000, SubBlockLimits::unbounded())
                .await
                .unwrap();
            if let Tick::Sealed(result) = tick {
                if result.block_sealed.is_some() {
                    blocks_closed += 1;
                    assert_eq!(
                        result.block_timestamp_ahead_seconds,
                        Some(0.0),
                        "an empty block sealed when due carries no catch-up debt: the wall clock won"
                    );
                }
            }
        }
        assert_eq!(
            blocks_closed, 4,
            "exactly one empty block per 5s window over 20s of idle ticks, never more often"
        );
        assert_eq!(s.next_block(), 5);

        let mut records = Vec::new();
        let torn = crate::log::replay(dir.path(), false, |r| records.push(r.clone())).unwrap();
        assert!(torn.is_none());
        assert_eq!(
            records.len(),
            4 * SUB_BLOCKS_PER_BLOCK as usize,
            "each of the 4 empty blocks must still be a full 20-record block"
        );
    }

    /// Once a block has sealed, a clock stepping BACKWARD during idle
    /// must never satisfy the interval and must write nothing more.
    #[tokio::test]
    async fn a_clock_stepping_backward_during_idle_writes_nothing() {
        let dir = tempdir().unwrap();
        let mut s = SealerState::new(
            MockExecutor::new(),
            LogWriter::open(dir.path(), 1_000).unwrap(),
            PrivateKeySigner::random(),
            ChannelSink::new(16),
            1,
            DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            SUB_BLOCKS_PER_BLOCK,
            ResumePoint::default(),
        )
        .with_empty_block_interval_secs(5);

        let base_ts = 1_757_000_000_000_000u64;
        // One full block closes immediately (due at genesis: `prev_block_timestamp_secs` starts at 0,
        // and any real wall-clock reading vastly exceeds a 5s interval).
        for i in 0..20u16 {
            s.seal_sub_block(
                vec![],
                base_ts + i as u64 * 50_000,
                SubBlockLimits::unbounded(),
            )
            .await
            .unwrap()
            .sealed();
        }
        assert_eq!(s.next_block(), 2);

        // The clock steps backward by an hour, then creeps forward a little — nowhere near
        // `prev_block_timestamp_secs + 5`.
        for now_us in [
            base_ts - 3_600_000_000_000,
            base_ts - 3_600_000_000_000 + 1_000_000,
            base_ts - 3_600_000_000_000 + 2_000_000,
        ] {
            let tick = s
                .seal_sub_block(vec![], now_us, SubBlockLimits::unbounded())
                .await
                .unwrap();
            assert!(
                tick.is_idle(),
                "a clock stepping backward must never satisfy the idle interval"
            );
        }
        assert_eq!(s.next_block(), 2, "no second block must have opened");

        let mut records = Vec::new();
        let torn = crate::log::replay(dir.path(), false, |r| records.push(r.clone())).unwrap();
        assert!(torn.is_none());
        assert_eq!(
            records.len(),
            SUB_BLOCKS_PER_BLOCK as usize,
            "the backward-clock idle ticks must write no additional records"
        );
    }

    // ---- Deposits -------------------------------------------------------------------------------------

    /// A sealer with deposits on and the idle interval at its default (0: never an empty block), over a feed
    /// that already read `count` finalized deposits from a mock chain capped at `cap` per block.
    async fn deposit_state(
        dir: &std::path::Path,
        resume: ResumePoint,
        count: u64,
        cap: u16,
    ) -> (
        SealerState<MockExecutor, ChannelSink>,
        crate::deposits::DepositFeed,
        crate::deposits::test_support::MockChain,
    ) {
        use crate::deposits::test_support::{feed, MockChain};
        let chain = MockChain::new(cap);
        chain.deposit_up_to(count, cap, 1_757_000_000);
        let feed = feed();
        let s = SealerState::new(
            MockExecutor::new(),
            LogWriter::open(dir, 1_000).unwrap(),
            PrivateKeySigner::random(),
            ChannelSink::new(16),
            1,
            DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            SUB_BLOCKS_PER_BLOCK,
            resume,
        )
        .with_deposits(feed.clone());
        chain.poller(&feed).poll_once().await.unwrap();
        (s, feed, chain)
    }

    /// Seals one whole block of empty ticks starting at `base_ts`, returning its sealed results.
    async fn seal_block(
        s: &mut SealerState<MockExecutor, ChannelSink>,
        base_ts: u64,
    ) -> Vec<SealResult> {
        let mut out = Vec::new();
        for i in 0..SUB_BLOCKS_PER_BLOCK as u64 {
            let tick = s
                .seal_sub_block(vec![], base_ts + i * 50_000, SubBlockLimits::unbounded())
                .await
                .unwrap();
            out.push(tick.sealed());
        }
        out
    }

    /// A waiting deposit makes an otherwise idle tick seal: with the idle interval at 0 an empty tick never
    /// opens a block, and with a finalized deposit waiting it does. Once the deposit is credited the sealer is
    /// idle again.
    #[tokio::test]
    async fn a_waiting_deposit_wakes_an_idle_sealer() {
        let dir = tempdir().unwrap();
        let (mut s, feed, chain) = deposit_state(dir.path(), ResumePoint::default(), 0, 10).await;
        let base_ts = 1_757_000_000_000_000u64;
        let tick = s
            .seal_sub_block(vec![], base_ts, SubBlockLimits::unbounded())
            .await
            .unwrap();
        assert!(tick.is_idle(), "no deposit waiting: idle, as before");

        // A deposit becomes finalized.
        chain.deposit_up_to(1, 10, 1_757_000_000);
        chain.poller(&feed).poll_once().await.unwrap();
        let results = seal_block(&mut s, base_ts + 50_000).await;
        assert_eq!(results.len(), SUB_BLOCKS_PER_BLOCK as usize);
        assert_eq!(results[0].header.deposits_end, Some(1));
        assert_eq!(results[0].deposits_credited, 1);
        assert!(results[1..].iter().all(|r| r.header.deposits_end.is_none()));
        assert_eq!(
            results[19].closed_block_deposit_enqueue_ts,
            vec![1_757_000_000]
        );
        assert_eq!(s.next_block(), 2);

        // Credited, so the sealer is idle again.
        let tick = s
            .seal_sub_block(vec![], base_ts + 2_000_000, SubBlockLimits::unbounded())
            .await
            .unwrap();
        assert!(tick.is_idle());
        assert_eq!(s.next_block(), 2);
    }

    /// Order and cap, through the sealer: three blocks credit 2, 2 and 1 of five finalized deposits, oldest
    /// first, every credit the `deposit_withdrawal` the log carries, each header's `deposits_end` one past the
    /// last credit. A sixth, not yet finalized, is never credited.
    #[tokio::test]
    async fn blocks_credit_in_order_up_to_the_cap_never_past_the_finalized_count() {
        let dir = tempdir().unwrap();
        let (mut s, _feed, chain) = deposit_state(dir.path(), ResumePoint::default(), 5, 2).await;
        // An account for deposit 5 exists on the mock, but the queue's count stays 5.
        chain.put_record(5, 1_757_000_000);
        let base_ts = 1_757_000_000_000_000u64;
        for block in 0..3u64 {
            seal_block(&mut s, base_ts + block * 2_000_000).await;
        }
        // Everything finalized is credited; deposit 5 is not finalized, so the next tick is idle.
        assert!(s
            .seal_sub_block(vec![], base_ts + 9_000_000, SubBlockLimits::unbounded())
            .await
            .unwrap()
            .is_idle());
        let mut records = Vec::new();
        let torn = crate::log::replay(dir.path(), false, |r| records.push(r.clone())).unwrap();
        assert!(torn.is_none());

        let credited: Vec<(u64, Option<u64>, Vec<u64>)> = records
            .iter()
            .filter(|r| r.header.index == 0)
            .map(|r| {
                (
                    r.header.block,
                    r.header.deposits_end,
                    r.withdrawals.iter().map(|w| w.index).collect(),
                )
            })
            .collect();
        // Blocks 1..=3 credit; a fourth never opens.
        assert_eq!(
            credited,
            vec![
                (1, Some(2), vec![0, 1]),
                (2, Some(4), vec![2, 3]),
                (3, Some(5), vec![4]),
            ]
        );
        for r in records.iter().filter(|r| r.header.index == 0) {
            for w in &r.withdrawals {
                let expected = rome_zk_executor_api::deposit_withdrawal(
                    w.index,
                    Address::from([w.index as u8 + 1; 20]),
                    100 + w.index,
                );
                assert_eq!(
                    *w, expected,
                    "every credit is the withdrawal deposit_withdrawal builds"
                );
            }
        }
        assert_eq!(s.next_block(), 4);
    }

    /// Restart: replaying a log whose blocks credited deposits 0..4 resumes `deposits_end` at 4, and the next
    /// block credits from 4 on, with no deposit credited twice and none skipped.
    #[tokio::test]
    async fn after_a_restart_deposits_resume_from_the_log() {
        let dir = tempdir().unwrap();
        let sequencer_key = PrivateKeySigner::random();
        let address = sequencer_key.address();
        let base_ts = 1_757_000_000_000_000u64;
        {
            use crate::deposits::test_support::{feed, MockChain};
            let chain = MockChain::new(2);
            chain.deposit_up_to(6, 2, 1_757_000_000);
            let feed = feed();
            let mut s = SealerState::new(
                MockExecutor::new(),
                LogWriter::open(dir.path(), 1_000).unwrap(),
                sequencer_key,
                ChannelSink::new(16),
                1,
                DEFAULT_BLOCK_GAS_LIMIT,
                Address::ZERO,
                SUB_BLOCKS_PER_BLOCK,
                ResumePoint::default(),
            )
            .with_deposits(feed.clone());
            chain.poller(&feed).poll_once().await.unwrap();
            seal_block(&mut s, base_ts).await;
            seal_block(&mut s, base_ts + 2_000_000).await;
        }

        let mut executor = MockExecutor::new();
        let resume = crate::recovery::replay_into_executor(
            dir.path(),
            &mut executor,
            address,
            false,
            DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            SUB_BLOCKS_PER_BLOCK,
        )
        .await
        .unwrap();
        assert_eq!(resume.deposits_end, 4);
        assert_eq!(resume.next_block, 3);

        // A new process: a fresh feed that reads the same chain.
        use crate::deposits::test_support::{feed, MockChain};
        let chain = MockChain::new(2);
        chain.deposit_up_to(6, 2, 1_757_000_000);
        // The settlement cursor is at the point the log resumed, so the chain check starts there too.
        chain.set_cursor(4, chain.hash_before(4));
        let feed = feed();
        let mut s = SealerState::new(
            executor,
            LogWriter::open(dir.path(), 1_000).unwrap(),
            PrivateKeySigner::random(),
            ChannelSink::new(16),
            1,
            DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            SUB_BLOCKS_PER_BLOCK,
            resume,
        )
        .with_deposits(feed.clone());
        chain.poller(&feed).poll_once().await.unwrap();
        let results = seal_block(&mut s, base_ts + 4_000_000).await;
        assert_eq!(results[0].header.block, 3);
        assert_eq!(results[0].header.deposits_end, Some(6));
        assert_eq!(results[0].deposits_credited, 2);
        // The next poll after the restart never re-read what the log already credited.
        let first_key = crate::deposits::test_support::record_key(&chain, 0);
        assert!(!chain
            .reader
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|keys| keys.contains(&first_key)));
    }

    /// Without a feed the sealer is what it was: an empty tick is idle, and a block with transactions carries
    /// no `deposits_end` and no withdrawals.
    #[tokio::test]
    async fn without_deposits_blocks_carry_no_deposits_end_and_no_withdrawals() {
        use crate::testutil::signed_raw_tx;
        let dir = tempdir().unwrap();
        let mut s = SealerState::new(
            MockExecutor::new(),
            LogWriter::open(dir.path(), 1_000).unwrap(),
            PrivateKeySigner::random(),
            ChannelSink::new(16),
            1,
            DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            SUB_BLOCKS_PER_BLOCK,
            ResumePoint::default(),
        );
        let base_ts = 1_757_000_000_000_000u64;
        assert!(s
            .seal_sub_block(vec![], base_ts, SubBlockLimits::unbounded())
            .await
            .unwrap()
            .is_idle());
        let sender = PrivateKeySigner::random();
        for i in 0..20u64 {
            let txs = if i == 0 {
                vec![signed_raw_tx(&sender, 1, 0)]
            } else {
                vec![]
            };
            let r = s
                .seal_sub_block(txs, base_ts + i * 50_000, SubBlockLimits::unbounded())
                .await
                .unwrap()
                .sealed();
            assert_eq!(r.header.deposits_end, None);
            assert_eq!(r.deposits_credited, 0);
        }
        let mut records = Vec::new();
        crate::log::replay(dir.path(), false, |r| records.push(r.clone())).unwrap();
        assert_eq!(records.len(), 20);
        assert!(records.iter().all(|r| r.withdrawals.is_empty()));
    }
}
