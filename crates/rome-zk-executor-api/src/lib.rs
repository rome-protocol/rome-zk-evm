//! The `Executor` trait (the sequencer's seam) and its associated types, factored out of
//! `rome-zk-sequencer::executor` into their own crate for one reason only: Cargo forbids a
//! dependency cycle, and `rome-zk-executor-reth` (the reth implementation) must depend on the trait
//! it implements. `rome-zk-sequencer` re-exports everything here as `rome_zk_sequencer::executor::*`
//! (`pub use rome_zk_executor_api::*;`) so every existing caller (`admission`, `sealer`,
//! `sequencer`, `recovery`, the binary) is unaffected — this crate is not a new public surface, it
//! is the same seam moved one level down so both a sequencer-side consumer and a reth-side
//! implementor can sit on top of it without depending on each other.
//!
//! Shaped after reth's `PayloadJob` contract (`reth_payload_builder::PayloadJob`,
//! `crates/payload/builder/src/lib.rs`: `best_payload()` must always return a currently-valid
//! payload, never an error for "nothing built yet") so that `rome-zk-executor-reth` can implement this trait
//! with an in-process reth driven like reth's `LocalMiner`, without changing anything above this
//! seam: admission, the log, the sealer and the RPC layer all depend only on this trait.

#![forbid(unsafe_code)]

use alloy_eips::eip4895::Withdrawal;
use alloy_primitives::{keccak256, Address, Bytes, TxHash, B256};
use std::time::{Duration, Instant};

/// Why a tx was rejected by the executor at execution time (distinct from an admission-time rejection —
/// see `rome_zk_sequencer::admission::AdmissionError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// The tx's nonce was less than the sender's expected next nonce at execution time — already
    /// consumed; this tx can never become valid again as-is. `expected` is the executor's real next
    /// nonce for this sender: admission must reset its own cache to
    /// it, never re-admit this tx unchanged.
    NonceTooLow { expected: u64 },
    /// The tx's nonce was greater than the sender's expected next nonce at execution time — a
    /// genuine future nonce that will become valid once the gap fills. `expected` is the executor's
    /// real next nonce for this sender: admission should park this
    /// tx and wait for the gap, not bounce it back to the sender as terminally rejected.
    NonceTooHigh { expected: u64 },
    /// Sender's balance cannot cover `gas_limit * max_fee_per_gas + value`.
    InsufficientFunds,
    /// Declared `gas_limit` is below the tx's own intrinsic gas floor (base cost + calldata + access
    /// list, and — post EIP-7623 — the calldata floor).
    IntrinsicGas,
    /// Declared `gas_limit` exceeds the block's own gas limit (or a configured per-tx cap).
    GasLimitExceeded,
    /// Any other execution-time rejection reth/revm's own tx validation surfaces (gas price below
    /// base fee, invalid chain id, an unsupported tx feature pre-fork, and so on) — carries revm's
    /// own message for operator diagnostics; never machine-parsed beyond this point.
    /// Consumes no gas either way and is excluded from the block.
    Other(String),
}

/// The per-sub-block execution budget: the executor must stop reaching
/// new txs once either bound is hit, and hand back everything it didn't reach as `not_executed` — never
/// block past the sub-block's timer, and never let one sub-block's gas run unbounded.
#[derive(Debug, Clone, Copy)]
pub struct SubBlockLimits {
    /// Total gas this sub-block may spend. Design default: `DEFAULT_SUB_BLOCK_GAS_LIMIT`
    /// (100M gas/s executor cap × 50ms cadence) — configurable per chain.
    pub gas_limit: u64,
    /// The instant by which execution must stop reaching new txs, even if gas remains.
    pub deadline: Instant,
}

impl SubBlockLimits {
    /// A limit that will not bind in practice — used by `rome_zk_sequencer::recovery` to replay a
    /// record's txs exactly as originally attempted, without reapplying a (non-reproducible,
    /// wall-clock-relative) deadline or a gas cutoff a second time: the record already holds only
    /// the txs that were actually attempted live.
    pub fn unbounded() -> Self {
        Self {
            gas_limit: u64::MAX,
            deadline: Instant::now() + Duration::from_secs(3600),
        }
    }
}

/// One tx the executor rejected at execution time, carrying the sender:
/// admission's own nonce cache advanced this tx to `Ready` on the assumption it would succeed; when the
/// executor rejects it instead, the sequencer actor must reconcile `sender`'s admission-side nonce back to
/// the executor's real value — the hash alone is not enough to do that reconciliation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedTx {
    pub tx_hash: TxHash,
    pub sender: Address,
    pub reason: Reason,
}

/// The result of executing one sub-block's ordered tx list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubBlockOutcome {
    pub included: Vec<TxHash>,
    pub rejected: Vec<RejectedTx>,
    pub receipts_root: B256,
    pub gas_used: u64,
    /// Txs the executor did not reach at all before `gas_limit`/`deadline` cut it off, in order — neither
    /// included nor rejected, since they were never evaluated. The sealer re-queues these at the front of
    /// admission's ready queue for the next sub-block.
    pub not_executed: Vec<Bytes>,
}

/// The result of sealing a block (every 20th sub-block).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockOutcome {
    pub state_root: B256,
    pub block_hash: B256,
    /// The sealed block's own receipts root. For a block whose only
    /// included tx sits in one otherwise-empty sub-block, this equals that sub-block's own
    /// `SubBlockOutcome::receipts_root` (both degenerate to the same single-receipt trie) — the exact
    /// equality a pre-confirmation-vs-seal divergence test checks.
    pub receipts_root: B256,
}

/// Inputs the sealer hands the executor when a block boundary is reached (the state root is computed once
/// per block).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockSealInputs {
    pub block: u64,
    pub timestamp_secs: u64,
    /// The 20 sub-block header hashes that make up this block, in order.
    pub sub_block_header_hashes: Vec<B256>,
    pub total_gas_used: u64,
}

/// The executor's current head, as it knows it — used both to seed the admission nonce cache and to
/// verify replay recovery reproduces the same head.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Head {
    pub block: u64,
    pub sub_block_index: u16,
    pub state_root: B256,
    pub block_hash: B256,
}

/// One block's committed environment (the block environment is published, not inferred), resolved by the sealer once,
/// before this block's first sub-block (index 0) executes, and handed to the executor via [`Executor::open_block`] —
/// never re-derived, never overridden at seal time. Every sub-block's preview execution and the seal-time
/// block-building both run under exactly this env, so a signed pre-confirmation can never diverge from the sealed block
/// it pre-confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockEnv {
    pub number: u64,
    pub timestamp_secs: u64,
    /// This chain's genesis gas limit — a fixed constant this design holds steady per block, not an
    /// elastic target (see `rome-zk-executor-reth`'s derivation-equivalence test comment); sourced from
    /// chain config, not computed here.
    pub gas_limit: u64,
    /// `Address::ZERO` for v1; chain-config fee recipient later.
    pub coinbase: Address,
    pub prev_randao: B256,
    /// `None` lets the executor derive EIP-1559 base fee from its own parent header
    /// (baseFee = EIP-1559 from the parent header) — reserved for an executor/chain that needs to
    /// override it explicitly.
    pub base_fee: Option<u64>,
    /// This block's EIP-4895 withdrawals, credited to balances after the block's transactions: the deposits this
    /// block includes, each built with [`deposit_withdrawal`]. Empty for a block with no deposits, which then builds
    /// exactly the block it always built. The header's `withdrawals_root` is [`withdrawals_root`] of this list
    /// (see [`canonical_header_rule_with_withdrawals`]).
    pub withdrawals: Vec<Withdrawal>,
}

/// `prevRandao`'s epoch length is a **fixed constant**, never a
/// profile value — `prevRandao` is a function of `(chain_id, number)` only. It does NOT bind a block to
/// its DA batch id: `rome_zk_sequencer::profile::Profile::blocks_per_batch` is a different quantity (the
/// batcher's own posting cadence) that happens to share today's value (10) but must never be threaded
/// into this formula — a chain profile that changes `blocks_per_batch` must not change `prev_randao` for
/// any existing block. Changing this constant changes every already-committed block's `prev_randao`, so
/// it is a formula change, not a runtime knob.
pub const PREV_RANDAO_EPOCH_BLOCKS: u64 = 10;

/// `prevRandao = keccak256(chain_id ‖ number ‖ channel_id)`, where
/// `channel_id = keccak256(chain_id ‖ epoch_id)[..16]` and `epoch_id = number / PREV_RANDAO_EPOCH_BLOCKS`
/// (a fixed constant, 10). Deterministic and sequencer-predictable, not entropy (contracts needing
/// randomness use an oracle, as on every single-sequencer rollup). All integers are little-endian,
/// matching the inbox chunk header's own encoding.
///
/// A pure function of `(chain_id, number)` — there is no profile parameter to thread, by construction
/// (a prior revision of this crate took
/// `blocks_per_batch` as a third argument and let `Config.blocks_per_batch` change already-committed
/// blocks' `prev_randao`; that revision is reverted).
///
/// Golden vector (independently cross-checked against two separate keccak256 implementations —
/// `pycryptodome` and Foundry's `cast keccak` — before being pinned in this module's tests): chain_id
/// 200101, number 5 →
/// `0x904b4464c9fda66bedf657a3308502700f0f971d6bef10815ff46ab0b338f28d`.
pub fn prev_randao(chain_id: u64, number: u64) -> B256 {
    let epoch_id = number / PREV_RANDAO_EPOCH_BLOCKS;
    let mut channel_input = [0u8; 16];
    channel_input[..8].copy_from_slice(&chain_id.to_le_bytes());
    channel_input[8..].copy_from_slice(&epoch_id.to_le_bytes());
    let channel_id = &keccak256(channel_input)[..16];

    let mut preimage = [0u8; 32];
    preimage[..8].copy_from_slice(&chain_id.to_le_bytes());
    preimage[8..16].copy_from_slice(&number.to_le_bytes());
    preimage[16..].copy_from_slice(channel_id);
    keccak256(preimage)
}

/// The EMPTY withdrawals-trie root — the RLP root of an empty withdrawals list, the same
/// constant every consumer of an empty-withdrawals block (a stock reth builder, a stateless validator)
/// computes; pinned here as a literal so no consumer needs its own RLP/trie dependency just to derive it.
/// Golden value cross-checked against `alloy_trie::EMPTY_ROOT_HASH` (the canonical empty-Merkle-Patricia-
/// trie root, keccak256 of RLP's empty-string encoding `0x80`) — the same constant reth uses for an empty
/// withdrawals list, since an empty list's trie root does not depend on what the list is *of*.
pub const EMPTY_WITHDRAWALS: B256 =
    alloy_primitives::b256!("56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421");

/// One block's rule-fixed header fields: every header field the derivation RULE
/// itself pins, independent of any block's own content — as opposed to `gas_limit` (from the DA stream)
/// or `requests_hash`/`base_fee`/`state_root`/the trie roots (reth-validated from execution). A
/// stateless validator proves consensus-validity, not derivation-canonicality — every one of
/// these fields used to be host-supplied and only consensus-bounded, so a host could commit a different
/// (still consensus-valid) value for any of them and the guest would accept it. This struct is the ONE
/// shared definition three consumers build from — the sequencer's block env
/// (`rome-zk-executor-reth::executor::attrs_from_env`), derive's `PayloadAttributes` construction
/// (`rome-zk-derive::engine`), and the guest's per-block assertion (`guest-rome::chain`) — never three
/// independently-pinned copies of the same constants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderRule {
    /// `keccak256(chain_id ‖ number ‖ channel_id)` — see [`prev_randao`]. Carried in the
    /// header's `mixHash` field (post-merge repurposing of that field, EIP-4399).
    pub prev_randao: B256,
    /// The chain's fee recipient (`header.beneficiary`/`coinbase`) — `Address::ZERO` on Tiber.
    pub beneficiary: Address,
    /// Always empty.
    pub extra_data: Bytes,
    /// The withdrawals-trie root of the block's withdrawals: [`EMPTY_WITHDRAWALS`] for a block with none (every block
    /// today), otherwise [`withdrawals_root`] of the block's deposit withdrawals.
    pub withdrawals_root: B256,
    /// Always `B256::ZERO` (no beacon chain behind this rollup).
    pub parent_beacon_block_root: B256,
    /// Always `0` (no blobs on this chain).
    pub blob_gas_used: u64,
    /// Always `0` (no blobs on this chain).
    pub excess_blob_gas: u64,
}

/// Builds this block's [`HeaderRule`] — a pure function of `(chain_id, number,
/// fee_recipient)`, so every consumer computes the identical rule from the identical inputs rather than
/// re-deriving an equivalent one. `fee_recipient` is threaded in (not itself a fixed constant here)
/// because it is a chain-config value (`Address::ZERO` on Tiber today), not a derivation-formula constant
/// like the other six fields.
pub fn canonical_header_rule(chain_id: u64, number: u64, fee_recipient: Address) -> HeaderRule {
    HeaderRule {
        prev_randao: prev_randao(chain_id, number),
        beneficiary: fee_recipient,
        extra_data: Bytes::new(),
        withdrawals_root: EMPTY_WITHDRAWALS,
        parent_beacon_block_root: B256::ZERO,
        blob_gas_used: 0,
        excess_blob_gas: 0,
    }
}

/// One deposit as the block's withdrawal: `{ index, validator_index: 0, address: recipient, amount: amount_gwei }`.
/// The amount is already in gwei, the unit a withdrawal carries, so nothing is converted. The index is the deposit's
/// position in the queue (not its position in the block). A deposit has no validator, so `validator_index` is
/// always 0.
pub fn deposit_withdrawal(index: u64, recipient: Address, amount_gwei: u64) -> Withdrawal {
    Withdrawal {
        index,
        validator_index: 0,
        address: recipient,
        amount: amount_gwei,
    }
}

/// The withdrawals-trie root of a block's withdrawals list (EIP-4895: the ordered trie of each withdrawal's RLP).
/// For an empty list this is [`EMPTY_WITHDRAWALS`]. The one function the sequencer, derive and the guest all call, so
/// they cannot disagree on a block's root.
pub fn withdrawals_root(withdrawals: &[Withdrawal]) -> B256 {
    alloy_trie::root::ordered_trie_root(withdrawals)
}

/// [`canonical_header_rule`] for a block that carries `withdrawals` (its slice of the deposit queue): the same rule,
/// with `withdrawals_root` set to the root of that list. With an empty list it is exactly [`canonical_header_rule`].
pub fn canonical_header_rule_with_withdrawals(
    chain_id: u64,
    number: u64,
    fee_recipient: Address,
    withdrawals: &[Withdrawal],
) -> HeaderRule {
    HeaderRule {
        withdrawals_root: withdrawals_root(withdrawals),
        ..canonical_header_rule(chain_id, number, fee_recipient)
    }
}

/// Execution backend. The sequencer's admission, log and sealer code is generic over this trait;
/// `rome-zk-executor-reth` implements it with an in-process reth and nothing above this line changes.
pub trait Executor: Send {
    /// Commit this block's environment, before its first sub-block
    /// (index 0) executes. Must be called exactly once per block, before that block's first
    /// `execute_sub_block` call — every sub-block of the block, and the block's own `seal_block`, run
    /// under this same `env`; an executor that lets any of them diverge (e.g. by still deriving its own
    /// provisional timestamp) is the exact bug this method exists to close.
    fn open_block(
        &mut self,
        env: BlockEnv,
    ) -> impl std::future::Future<Output = Result<(), ExecutorError>> + Send;

    /// Execute one sub-block's ordered raw txs, up to `limits`. Must not reorder — arrival order is
    /// execution order (never a fee-ordered pool). Must stop reaching new txs once `limits`
    /// binds and return the unreached suffix as `SubBlockOutcome::not_executed`, in order.
    fn execute_sub_block(
        &mut self,
        txs: &[Bytes],
        limits: SubBlockLimits,
    ) -> impl std::future::Future<Output = Result<SubBlockOutcome, ExecutorError>> + Send;

    /// Called once per 20 sub-blocks to compute the block's state root.
    fn seal_block(
        &mut self,
        inputs: BlockSealInputs,
    ) -> impl std::future::Future<Output = Result<BlockOutcome, ExecutorError>> + Send;

    /// The executor's current head.
    fn head(&self) -> Head;

    /// The sender's next expected nonce, as the executor currently knows it. Used to seed the admission
    /// nonce cache on first sight of a sender.
    fn nonce(&self, addr: Address) -> u64;

    /// Tail-only replay: the highest sequencer block number (the same numbering
    /// `BlockSealInputs::block`/`Head::block` use) this executor's own durable storage already
    /// reflects, captured once at construction — `None` means nothing is durable and a fresh replay
    /// must start at block 0 (this is every executor's behavior before tail-only replay existed, and remains
    /// [`MockExecutor`]'s behavior forever: it has no persistence to skip past).
    ///
    /// `rome_zk_sequencer::recovery::replay_into_executor` calls this exactly once, before its replay
    /// loop starts, and never re-derives it mid-replay: it is a property of what was durable at
    /// *startup*, not a moving target the replay loop's own `seal_block` calls should be allowed to
    /// perturb. An executor that persists (like `RethExecutor`) returns `Some(n)` when its own MDBX
    /// already contains sequencer blocks `0..=n`; replay then skips re-executing those records
    /// (verifying only their signer, never their tx_root/receipts_root/gas_used — there is nothing to
    /// recompute without executing) and resumes real `open_block`/`execute_sub_block`/`seal_block`
    /// calls only for the log's tail beyond `n`.
    fn last_persisted_block(&self) -> Option<u64> {
        None
    }

    /// Join any outstanding background persistence before a
    /// gracefully-requested shutdown, so the durable state is fully caught up before the process
    /// exits normally. Default no-op — an executor with nothing to flush (like [`MockExecutor`],
    /// which is entirely in-memory) never needs to override it; `RethExecutor` overrides this to
    /// join its own still-outstanding `spawn_blocking` persist task, if any. Never required for
    /// correctness (a hard kill skips it entirely) — only tail replay off the ordered log is: see
    /// `rome_zk_sequencer::recovery::replay_into_executor`.
    fn flush(&mut self) -> impl std::future::Future<Output = Result<(), ExecutorError>> + Send {
        async { Ok(()) }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExecutorError {
    #[error("malformed tx: {0}")]
    Malformed(String),
    /// Any reth/revm-side failure that is not a per-tx rejection (`Reason`) — a
    /// database error, a chain-spec mismatch, a block-building invariant reth itself refused. Fatal
    /// to the caller exactly like `Malformed` (see `sealer::SealError::Executor`'s doc).
    #[error("reth executor error: {0}")]
    Backend(String),
    /// The env `seal_block` computed from `BlockSealInputs` does not
    /// match the env this block was opened with — an integrity check on the executor's own internal
    /// state, never expected to fire if `open_block`/`seal_block` are called correctly by the sealer.
    #[error("block env mismatch: {0}")]
    EnvMismatch(String),
    /// The genesis header's gas limit is not the profile's block gas limit. Every block this executor
    /// seals carries the profile's limit, and a stock reth verifier can neither build nor accept a block
    /// whose gas limit moved from its parent by more than 1/1024 — so the two must be equal from block 1,
    /// and a change of the block gas limit is a chain reset, never an in-place restart.
    #[error("genesis gas limit {genesis} != configured block gas limit {configured}: genesis.gasLimit must equal the profile's block gas limit (a block-gas-limit change is a chain reset)")]
    GenesisGasLimitMismatch { genesis: u64, configured: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden vector for `prev_randao`, independently cross-checked against two separate keccak256
    /// implementations (pycryptodome and Foundry's `cast keccak`) before being pinned here — a future
    /// change to the formula (field order, byte width, endianness, or `PREV_RANDAO_EPOCH_BLOCKS` itself)
    /// must be deliberate, not silent. `assert_eq!(PREV_RANDAO_EPOCH_BLOCKS, 10)` pins the epoch constant
    /// alongside the golden value it depends on — a change to either without the other is exactly what
    /// this test exists to catch (mutation: bump `PREV_RANDAO_EPOCH_BLOCKS` to 11 and this test goes red).
    #[test]
    fn prev_randao_matches_the_independently_computed_golden_vector() {
        assert_eq!(PREV_RANDAO_EPOCH_BLOCKS, 10);
        let got = prev_randao(200_101, 5);
        let expected: B256 = "0x904b4464c9fda66bedf657a3308502700f0f971d6bef10815ff46ab0b338f28d"
            .parse()
            .unwrap();
        assert_eq!(got, expected);
    }

    /// `prev_randao` must actually depend on `chain_id` and `number` (never a constant) — a different
    /// chain or a different block must produce a different value.
    #[test]
    fn prev_randao_varies_with_chain_id_and_number() {
        let a = prev_randao(1, 5);
        let b = prev_randao(2, 5);
        let c = prev_randao(1, 6);
        assert_ne!(a, b, "different chain_id must change prev_randao");
        assert_ne!(a, c, "different number must change prev_randao");
    }

    /// Blocks within the same 10-block epoch share a `channel_id`, so consecutive numbers
    /// inside one epoch must still diverge (only via the `number` term of the preimage, not the
    /// channel) — proven here by checking two numbers straddling an epoch boundary (9 -> epoch 0, 10 ->
    /// epoch 1) are different, and two numbers within the same epoch (10, 11) are also different.
    #[test]
    fn prev_randao_differs_across_and_within_epochs() {
        assert_ne!(prev_randao(1, 9), prev_randao(1, 10));
        assert_ne!(prev_randao(1, 10), prev_randao(1, 11));
    }

    /// `prev_randao` takes no
    /// profile parameter — there is no `blocks_per_batch` argument to vary, by construction. This is the
    /// compile-time guarantee that a chain profile's `blocks_per_batch` can never reach this formula; the
    /// two-argument signature itself is the fix, not a runtime check. (The prior, reverted 3-arg
    /// signature's own test asserted the opposite — that a different `blocks_per_batch` must change
    /// `prev_randao` for the same `(chain_id, number)` — which is exactly the bug the two-argument signature closes.)
    #[test]
    fn prev_randao_takes_no_profile_parameter() {
        fn assert_two_args(_f: fn(u64, u64) -> B256) {}
        assert_two_args(prev_randao);
    }

    /// `canonical_header_rule`'s constants pinned against the REAL Tiber header
    /// fixture (`fixtures/prover-input/rpc/eth_getBlockByNumber-990d-full.json`, block 0x990d = 39181,
    /// chain 200101) — `mixHash` 0x4e2660af…, `withdrawalsRoot` 0x56e81f… (the canonical empty-trie
    /// root), `parentBeaconBlockRoot` ZERO, `blobGasUsed`/`excessBlobGas` 0, `miner`/`extraData` empty
    /// (Tiber's own fee recipient is `Address::ZERO`). Mutation: change any field this test
    /// pins and it goes red (e.g. flip `EMPTY_WITHDRAWALS`'s literal — this test catches a drift from the
    /// real fixture the guest asserts against, not merely "the function returns something").
    #[test]
    fn canonical_header_rule_matches_the_real_tiber_header_fixture() {
        let rule = canonical_header_rule(200_101, 39_181, Address::ZERO);
        let expected_prev_randao: B256 =
            "0x4e2660afe4d85debc5753bf32c6d96c237d9b26486f8c872216b0bec718b87d1"
                .parse()
                .unwrap();
        assert_eq!(rule.prev_randao, expected_prev_randao);
        assert_eq!(rule.beneficiary, Address::ZERO);
        assert_eq!(rule.extra_data, Bytes::new());
        assert_eq!(rule.withdrawals_root, EMPTY_WITHDRAWALS);
        let expected_withdrawals: B256 =
            "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421"
                .parse()
                .unwrap();
        assert_eq!(rule.withdrawals_root, expected_withdrawals);
        assert_eq!(rule.parent_beacon_block_root, B256::ZERO);
        assert_eq!(rule.blob_gas_used, 0);
        assert_eq!(rule.excess_blob_gas, 0);
    }

    /// The rule's `beneficiary` is the passed-in fee recipient, not a hardcoded constant — a chain with a
    /// non-zero fee recipient must get a different rule.
    #[test]
    fn canonical_header_rule_beneficiary_follows_the_fee_recipient_argument() {
        let non_zero = Address::repeat_byte(0xAB);
        let rule = canonical_header_rule(200_101, 39_181, non_zero);
        assert_eq!(rule.beneficiary, non_zero);
    }

    /// `canonical_header_rule`'s `prev_randao` field must be exactly `prev_randao(chain_id, number)` —
    /// never a separately-derived value that could silently drift from the standalone function.
    #[test]
    fn canonical_header_rule_reuses_the_shared_prev_randao_formula() {
        let rule = canonical_header_rule(7, 42, Address::ZERO);
        assert_eq!(rule.prev_randao, prev_randao(7, 42));
    }

    // ---- the withdrawals rule ------------------------------------------------------------------

    /// One withdrawal: index 0, validator 0, recipient `0x11..11` (20 bytes), 1_000_000_000 gwei.
    fn one_withdrawal() -> Vec<Withdrawal> {
        vec![deposit_withdrawal(
            0,
            Address::repeat_byte(0x11),
            1_000_000_000,
        )]
    }

    /// Three withdrawals with indexes 7, 8, 9 (a block's slice of a deposit queue does not start at 0), distinct
    /// recipients, and the extreme amounts 32e9, 1 and `u64::MAX`. Their `validator_index` is set by hand (3, 4,
    /// 5) so the golden root below, computed with that field set, is also checked against the production
    /// constructor's zero in a separate test.
    fn three_withdrawals() -> Vec<Withdrawal> {
        vec![
            Withdrawal {
                index: 7,
                validator_index: 3,
                address: Address::repeat_byte(0x01),
                amount: 32_000_000_000,
            },
            Withdrawal {
                index: 8,
                validator_index: 4,
                address: Address::repeat_byte(0xab),
                amount: 1,
            },
            Withdrawal {
                index: 9,
                validator_index: 5,
                address: Address::repeat_byte(0xff),
                amount: u64::MAX,
            },
        ]
    }

    /// Golden roots, computed outside this crate in Python (pycryptodome keccak, a hand-built
    /// RLP encoder and a hand-built Merkle-Patricia trie: a single leaf for one withdrawal; a root branch with
    /// a sub-branch at nibble 0 and a leaf at nibble 8 for three).
    const GOLDEN_ONE: &str = "0xcf22af342ad9b28194e609001599c3312b76400414b9eb7f88322a9307cde213";
    const GOLDEN_THREE: &str = "0x11d99a0fc8898d01a71b68074bb96884aee589d101cffdaaec6c39b1dadded50";

    #[test]
    fn deposit_withdrawal_maps_the_record_to_the_four_fields() {
        let w = deposit_withdrawal(41, Address::repeat_byte(0x22), 5_000);
        assert_eq!(w.index, 41);
        assert_eq!(w.validator_index, 0, "a deposit has no validator");
        assert_eq!(w.address, Address::repeat_byte(0x22));
        assert_eq!(w.amount, 5_000);
    }

    #[test]
    fn withdrawals_root_of_an_empty_list_is_the_pinned_empty_constant() {
        assert_eq!(withdrawals_root(&[]), EMPTY_WITHDRAWALS);
    }

    #[test]
    fn withdrawals_root_matches_the_golden_roots() {
        assert_eq!(
            withdrawals_root(&one_withdrawal()),
            GOLDEN_ONE.parse::<B256>().unwrap()
        );
        assert_eq!(
            withdrawals_root(&three_withdrawals()),
            GOLDEN_THREE.parse::<B256>().unwrap()
        );
    }

    /// Independent computation for one withdrawal, by hand in the test: the trie of one item is one leaf node
    /// `[compact(key nibbles), value]` with key `rlp(0) = 0x80` (nibbles 8, 0; even-length leaf prefix 0x20) and
    /// value the withdrawal's RLP `[index, validator_index, address, amount]`; a root node is always hashed.
    #[test]
    fn withdrawals_root_of_one_withdrawal_equals_a_hand_built_leaf_node() {
        // rlp([0, 0, 0x11 * 20, 1_000_000_000]) = list(0xdc) of 0x80, 0x80, 0x94 ++ addr, 0x84 ++ 3b9aca00
        let mut value = vec![0xdc, 0x80, 0x80, 0x94];
        value.extend_from_slice(&[0x11; 20]);
        value.extend_from_slice(&[0x84, 0x3b, 0x9a, 0xca, 0x00]);
        assert_eq!(value.len(), 29);
        // leaf node = list(0xc0 + len) of bytes(0x20 0x80) = 0x82 0x20 0x80, and bytes(value) = 0x9d ++ value
        let mut node = vec![0u8];
        node.extend_from_slice(&[0x82, 0x20, 0x80, 0x80 + 29]);
        node.extend_from_slice(&value);
        node[0] = 0xc0 + (node.len() as u8 - 1);
        assert_eq!(withdrawals_root(&one_withdrawal()), keccak256(&node));
    }

    /// The root is the same one alloy-consensus (what a stock reth builder and the stateless validator use)
    /// computes, for every list length 0..=40 (every small trie shape), for the golden lists, and for a list of 300
    /// (indexes past 0x7f, whose RLP key is more than one byte).
    #[test]
    fn withdrawals_root_agrees_with_alloy_consensus() {
        for n in 0..=40u64 {
            let list: Vec<Withdrawal> = (0..n)
                .map(|i| Withdrawal {
                    index: 1_000 + i * 3,
                    validator_index: i % 5,
                    address: Address::repeat_byte(i as u8 ^ 0x5a),
                    amount: i.wrapping_mul(0x9e37_79b9_7f4a_7c15),
                })
                .collect();
            assert_eq!(
                withdrawals_root(&list),
                alloy_consensus::proofs::calculate_withdrawals_root(&list),
                "length {n}"
            );
        }
        for list in [one_withdrawal(), three_withdrawals()] {
            assert_eq!(
                withdrawals_root(&list),
                alloy_consensus::proofs::calculate_withdrawals_root(&list)
            );
        }
        // A list long enough that an index needs more than one RLP byte (index 128 and up).
        let long: Vec<Withdrawal> = (0..300u64)
            .map(|i| deposit_withdrawal(i, Address::repeat_byte(i as u8), i + 1))
            .collect();
        assert_eq!(
            withdrawals_root(&long),
            alloy_consensus::proofs::calculate_withdrawals_root(&long)
        );
    }

    #[test]
    fn withdrawals_root_depends_on_every_field_and_on_order() {
        let base = three_withdrawals();
        let root = withdrawals_root(&base);
        let mut w = base.clone();
        w[1].amount += 1;
        assert_ne!(withdrawals_root(&w), root, "amount");
        let mut w = base.clone();
        w[1].address = Address::repeat_byte(0xac);
        assert_ne!(withdrawals_root(&w), root, "recipient");
        let mut w = base.clone();
        w[1].index += 1;
        assert_ne!(withdrawals_root(&w), root, "index");
        let mut w = base.clone();
        w[1].validator_index += 1;
        assert_ne!(withdrawals_root(&w), root, "validator index");
        let mut w = base.clone();
        w.swap(0, 2);
        assert_ne!(withdrawals_root(&w), root, "order");
        let mut w = base;
        w.pop();
        assert_ne!(withdrawals_root(&w), root, "length");
    }

    /// With an empty list the new rule is byte-for-byte the old one, over a sweep of chains, block numbers
    /// (including both sides of the prevRandao epoch boundary and the integer extremes) and fee recipients.
    #[test]
    fn rule_with_an_empty_withdrawal_list_equals_the_old_rule() {
        let chains = [0u64, 1, 7, 200_101, 200_010, u64::MAX];
        let numbers = [0u64, 1, 9, 10, 11, 39_181, 1 << 32, u64::MAX - 1, u64::MAX];
        let recipients = [
            Address::ZERO,
            Address::repeat_byte(0xab),
            Address::repeat_byte(0xff),
            Address::repeat_byte(0x01),
        ];
        for chain in chains {
            for number in numbers {
                for fee in recipients {
                    assert_eq!(
                        canonical_header_rule_with_withdrawals(chain, number, fee, &[]),
                        canonical_header_rule(chain, number, fee),
                        "chain {chain} number {number} fee {fee}"
                    );
                }
            }
        }
    }

    /// A non-empty list changes the withdrawals root and nothing else.
    #[test]
    fn rule_with_withdrawals_changes_only_the_withdrawals_root() {
        let fee = Address::repeat_byte(0xcd);
        let old = canonical_header_rule(200_101, 12, fee);
        let new = canonical_header_rule_with_withdrawals(200_101, 12, fee, &three_withdrawals());
        assert_eq!(new.withdrawals_root, GOLDEN_THREE.parse::<B256>().unwrap());
        assert_ne!(new.withdrawals_root, old.withdrawals_root);
        assert_eq!(
            new,
            HeaderRule {
                withdrawals_root: new.withdrawals_root,
                ..old
            }
        );
    }
}
