//! Pipeline-level proofs for atomic batch attempts (a batch attempt is atomic w.r.t. the engine's own
//! position), the settlement-root resume anchor, and cross-batch continuity actually threaded at the
//! pipeline level — not just inside `batch_queue::decode_batch`'s own unit tests.
//!
//! Fixture pattern (batch/chunk account bytes, `channel::cut_frames`) mirrors
//! `tests/pipeline_strict_policy.rs` — this crate's convention is to duplicate small fixture builders
//! per test file rather than share a `tests/common` module.

use alloy_eips::eip7685::RequestsOrHash;
use alloy_primitives::{Bytes, B256};
use alloy_rpc_types_engine::{
    ExecutionPayloadV3, ForkchoiceState, ForkchoiceUpdated, PayloadAttributes, PayloadStatus,
};
use rome_zk_channel as channel;
use rome_zk_channel::Block;
use rome_zk_derive::engine::mock::MockEngineApi;
use rome_zk_derive::engine::{EngineApi, EngineController, ExistingBlock};
use rome_zk_derive::inbox::InboxRetrieval;
use rome_zk_derive::pipeline::{DerivePipeline, StepOutcome};
use rome_zk_derive::resume::{resume_anchor, ResumeAnchor};
use rome_zk_derive::testutil::FakeAccountReader;
use rome_zk_derive::traversal::SolanaTraversal;
use rome_zk_derive::PipelineError;
use solana_program::pubkey::Pubkey;

/// Stand-in settlement program the test chain is registered under (inbox accounts are keyed by it).
const SETTLEMENT_PROGRAM: Pubkey = Pubkey::new_from_array([9u8; 32]);

const CHAIN_ID: u64 = 200_101;

fn block(number: u64) -> Block {
    Block {
        number,
        timestamp: 1_757_000_000 + number,
        gas_limit: 100_000_000,
        txs: vec![],
        deposits_end: None,
    }
}

fn batch_account_bytes(
    chain_id: u64,
    batch: u64,
    open_slot: u64,
    expected_count: u32,
    chunk_bodies: &[&[u8]],
) -> Vec<u8> {
    let mut d = vec![
        0u8;
        rome_zk_layouts::batch::account_len_for(
            rome_zk_layouts::batch::VERSION,
            expected_count
        )
        .unwrap()
    ];
    d[rome_zk_layouts::batch::OFF_MAGIC..rome_zk_layouts::batch::OFF_MAGIC + 4]
        .copy_from_slice(&rome_zk_layouts::batch::MAGIC.to_le_bytes());
    d[rome_zk_layouts::batch::OFF_VERSION] = rome_zk_layouts::batch::VERSION;
    d[rome_zk_layouts::batch::OFF_CHAIN_ID..rome_zk_layouts::batch::OFF_CHAIN_ID + 8]
        .copy_from_slice(&chain_id.to_le_bytes());
    d[rome_zk_layouts::batch::OFF_BATCH..rome_zk_layouts::batch::OFF_BATCH + 8]
        .copy_from_slice(&batch.to_le_bytes());
    d[rome_zk_layouts::batch::OFF_OPEN_SLOT..rome_zk_layouts::batch::OFF_OPEN_SLOT + 8]
        .copy_from_slice(&open_slot.to_le_bytes());
    d[rome_zk_layouts::batch::OFF_EXPECTED_COUNT..rome_zk_layouts::batch::OFF_EXPECTED_COUNT + 4]
        .copy_from_slice(&expected_count.to_le_bytes());
    d[rome_zk_layouts::batch::OFF_FINALIZED] = 1; // finalized

    // A plausible committed clock reading (header v2) — this file's own drift bound
    // stays at the default `DriftBound::unbounded()` (never `.with_drift_bound(..)`), so the exact value
    // is inert here; `tests/pipeline_drift_bound.rs` is where the bound itself is exercised.
    d[rome_zk_layouts::batch::OFF_OPEN_UNIX_TS..rome_zk_layouts::batch::OFF_OPEN_UNIX_TS + 8]
        .copy_from_slice(&1_757_000_000i64.to_le_bytes());
    let chunk_hashes: Vec<[u8; 32]> = chunk_bodies
        .iter()
        .map(|b| alloy_primitives::keccak256(b).0)
        .collect();
    let (root, forced_root, acc) =
        zk_inbox_client::reference_commitment(chain_id, batch, open_slot, &chunk_hashes);
    d[rome_zk_layouts::batch::OFF_ROOT..rome_zk_layouts::batch::OFF_ROOT + 32]
        .copy_from_slice(&root);
    d[rome_zk_layouts::batch::OFF_FORCED_ROOT..rome_zk_layouts::batch::OFF_FORCED_ROOT + 32]
        .copy_from_slice(&forced_root);
    d[rome_zk_layouts::batch::OFF_ACC..rome_zk_layouts::batch::OFF_ACC + 32].copy_from_slice(&acc);
    d
}

/// Seeds one finalized batch (account + every sealed chunk) into `reader` — `blocks` encoded and cut
/// into frames exactly as the batcher would (`channel::encode_stream`/`cut_frames`).
fn seed_batch(
    reader: &mut FakeAccountReader,
    program_id: &Pubkey,
    chain_id: u64,
    batch: u64,
    open_slot: u64,
    blocks: &[Block],
) {
    let compressed = channel::encode_stream(blocks);
    let frames = channel::cut_frames(chain_id, batch, &compressed, 3_681);
    let bodies: Vec<Vec<u8>> = frames.iter().map(|f| f.to_bytes()).collect();
    let body_refs: Vec<&[u8]> = bodies.iter().map(|b| b.as_slice()).collect();

    let (batch_pda, _) =
        zk_inbox_client::batch_pda(program_id, &SETTLEMENT_PROGRAM, chain_id, batch);
    reader.accounts.insert(
        batch_pda,
        batch_account_bytes(chain_id, batch, open_slot, frames.len() as u32, &body_refs),
    );
    for (idx, body) in bodies.iter().enumerate() {
        let (chunk_pda, _) = zk_inbox_client::chunk_pda(
            program_id,
            &SETTLEMENT_PROGRAM,
            chain_id,
            batch,
            idx as u32,
        );
        let mut chunk = vec![0u8; zk_inbox::HEADER_LEN + body.len()];
        chunk[zk_inbox::OFF_MAGIC..zk_inbox::OFF_MAGIC + 4]
            .copy_from_slice(&zk_inbox::MAGIC.to_le_bytes());
        chunk[zk_inbox::OFF_CHAIN_ID..zk_inbox::OFF_CHAIN_ID + 8]
            .copy_from_slice(&chain_id.to_le_bytes());
        chunk[zk_inbox::OFF_BATCH..zk_inbox::OFF_BATCH + 8].copy_from_slice(&batch.to_le_bytes());
        chunk[zk_inbox::OFF_IDX..zk_inbox::OFF_IDX + 4]
            .copy_from_slice(&(idx as u32).to_le_bytes());
        chunk[zk_inbox::OFF_LEN..zk_inbox::OFF_LEN + 4]
            .copy_from_slice(&(body.len() as u32).to_le_bytes());
        chunk[zk_inbox::OFF_SEALED] = 1;
        chunk[zk_inbox::HEADER_LEN..].copy_from_slice(body);
        reader.accounts.insert(chunk_pda, chunk);
    }
}

/// A thin decorator over any [`EngineApi`] that fails exactly the Nth `build_forced_block` call ever
/// made (1-based) as [`PipelineError::Temporary`], **before** delegating to the inner engine — so the
/// inner mock's own bookkeeping (`existing_blocks`, its internal build counter) is untouched by the
/// failing call, exactly like a real `testing_buildBlockV1` request that times out before the server
/// does any work. Every other call, and every later call to the SAME build number bucket (there is
/// only ever one), delegates straight through — simulating exactly one transient engine hiccup mid- (or
/// cross-) batch.
struct FlakyOnce<E> {
    inner: E,
    fail_build_at_call: usize,
    calls: usize,
}

impl<E: EngineApi> EngineApi for FlakyOnce<E> {
    async fn build_forced_block(
        &mut self,
        parent_block_hash: B256,
        attrs: PayloadAttributes,
        transactions: Vec<Bytes>,
    ) -> Result<(ExecutionPayloadV3, RequestsOrHash), PipelineError> {
        self.calls += 1;
        if self.calls == self.fail_build_at_call {
            return Err(PipelineError::Temporary(format!(
                "probe: testing_buildBlockV1 timed out (call {})",
                self.calls
            )));
        }
        self.inner
            .build_forced_block(parent_block_hash, attrs, transactions)
            .await
    }

    async fn new_payload(
        &mut self,
        payload: ExecutionPayloadV3,
        requests: RequestsOrHash,
    ) -> Result<PayloadStatus, PipelineError> {
        self.inner.new_payload(payload, requests).await
    }

    async fn forkchoice_updated(
        &mut self,
        state: ForkchoiceState,
    ) -> Result<ForkchoiceUpdated, PipelineError> {
        self.inner.forkchoice_updated(state).await
    }

    async fn block_at(&mut self, number: u64) -> Result<Option<ExistingBlock>, PipelineError> {
        self.inner.block_at(number).await
    }
}

/// A transient engine failure on the second block of a 3-block batch must not turn the retry into
/// a spurious Critical. Before the fix, the retry's `target_height == env.number` assertion fired
/// because `next_height` was left one block ahead of where the retry re-decodes from; the test requires
/// the failure to stay retryable.
#[tokio::test]
async fn a_temporary_mid_batch_is_retryable_not_a_spurious_critical() {
    let program_id = Pubkey::new_unique();
    let mut reader = FakeAccountReader::default();
    seed_batch(
        &mut reader,
        &program_id,
        CHAIN_ID,
        0,
        1,
        &[block(1), block(2), block(3)],
    );

    let traversal =
        SolanaTraversal::new(reader.clone(), program_id, SETTLEMENT_PROGRAM, CHAIN_ID, 0);
    let inbox = InboxRetrieval::new(reader, program_id, SETTLEMENT_PROGRAM);
    // Block 1 is the 1st build_forced_block call, block 2 the 2nd — fail exactly that one, once.
    let flaky = FlakyOnce {
        inner: MockEngineApi::default(),
        fail_build_at_call: 2,
        calls: 0,
    };
    let engine = EngineController::new(flaky, B256::ZERO, 0);
    let mut pipeline = DerivePipeline::new(
        traversal,
        inbox,
        engine,
        CHAIN_ID,
        alloy_primitives::Address::ZERO,
        16,
        10,
    );

    let err = pipeline.step().await.unwrap_err();
    assert!(matches!(err, PipelineError::Temporary(_)), "got {err:?}");
    assert_eq!(
        pipeline.next_batch(),
        0,
        "the batch must not be considered advanced on a Temporary failure"
    );

    let retry = pipeline.step().await.unwrap();
    match retry {
        StepOutcome::Derived { batch, blocks } => {
            assert_eq!(batch, 0);
            assert_eq!(blocks.len(), 3, "all three blocks derived on retry");
        }
        StepOutcome::Idle => panic!("expected the batch to derive on retry"),
    }
    assert_eq!(
        pipeline.next_batch(),
        1,
        "cursor advances only once the whole batch succeeds"
    );
}

/// Two batches that continue correctly both derive.
#[tokio::test]
async fn two_correctly_continuing_batches_both_derive() {
    let program_id = Pubkey::new_unique();
    let mut reader = FakeAccountReader::default();
    seed_batch(
        &mut reader,
        &program_id,
        CHAIN_ID,
        0,
        1,
        &[block(1), block(2)],
    );
    seed_batch(
        &mut reader,
        &program_id,
        CHAIN_ID,
        1,
        2,
        &[block(3), block(4)],
    );

    let traversal =
        SolanaTraversal::new(reader.clone(), program_id, SETTLEMENT_PROGRAM, CHAIN_ID, 0);
    let inbox = InboxRetrieval::new(reader, program_id, SETTLEMENT_PROGRAM);
    let engine = EngineController::new(MockEngineApi::default(), B256::ZERO, 0);
    let mut pipeline = DerivePipeline::new(
        traversal,
        inbox,
        engine,
        CHAIN_ID,
        alloy_primitives::Address::ZERO,
        16,
        10,
    );

    let first = pipeline.step().await.unwrap();
    assert!(
        matches!(first, StepOutcome::Derived { batch: 0, .. }),
        "got {first:?}"
    );
    let second = pipeline.step().await.unwrap();
    match second {
        StepOutcome::Derived { batch, blocks } => {
            assert_eq!(batch, 1);
            assert_eq!(blocks.len(), 2);
        }
        StepOutcome::Idle => panic!("expected batch 1 to derive"),
    }
}

/// A batch that does not continue from the previous one's last block is Critical, but only
/// once the pipeline has actually threaded `last_design_block` across the two `step` calls — forcing
/// `expected_first_block` to `None` turns this red.
#[tokio::test]
async fn a_gap_between_batches_is_critical_after_the_first_derives() {
    let program_id = Pubkey::new_unique();
    let mut reader = FakeAccountReader::default();
    seed_batch(
        &mut reader,
        &program_id,
        CHAIN_ID,
        0,
        1,
        &[block(1), block(2)],
    );
    // Should continue at block 3; starts at block 4 instead (a gap).
    seed_batch(
        &mut reader,
        &program_id,
        CHAIN_ID,
        1,
        2,
        &[block(4), block(5)],
    );

    let traversal =
        SolanaTraversal::new(reader.clone(), program_id, SETTLEMENT_PROGRAM, CHAIN_ID, 0);
    let inbox = InboxRetrieval::new(reader, program_id, SETTLEMENT_PROGRAM);
    let engine = EngineController::new(MockEngineApi::default(), B256::ZERO, 0);
    let mut pipeline = DerivePipeline::new(
        traversal,
        inbox,
        engine,
        CHAIN_ID,
        alloy_primitives::Address::ZERO,
        16,
        10,
    );

    let first = pipeline.step().await.unwrap();
    assert!(matches!(first, StepOutcome::Derived { batch: 0, .. }));
    let err = pipeline.step().await.unwrap_err();
    // Deliberately specific, not just `matches!(err, Critical(_))`: `derive_one_batch` calls
    // `batch_queue::decode_batch` (where the continuity check lives) strictly before it ever reaches
    // `EngineController::advance` (whose own, unrelated height assertion would ALSO turn
    // Critical on this exact fixture, masking whether the continuity check under test fired at all). With
    // the continuity check removed, the error becomes the engine's height-assertion message instead, which
    // is how this test tells the two apart.
    match &err {
        PipelineError::Critical(msg) => assert!(
            msg.contains("continuity") && msg.contains("expected 3"),
            "expected the batch_queue continuity message, got {msg:?}"
        ),
        other => panic!("expected Critical, got {other:?}"),
    }
}

/// A Temporary failure on the SECOND batch must not corrupt the continuity check its own
/// retry relies on — `last_design_block` (set once by batch 0's success) must survive the failed
/// attempt at batch 1 intact, so the retry still expects last(batch 0) + 1.
#[tokio::test]
async fn a_temporary_failure_on_the_second_batch_does_not_corrupt_continuity_for_its_retry() {
    let program_id = Pubkey::new_unique();
    let mut reader = FakeAccountReader::default();
    seed_batch(
        &mut reader,
        &program_id,
        CHAIN_ID,
        0,
        1,
        &[block(1), block(2)],
    );
    seed_batch(
        &mut reader,
        &program_id,
        CHAIN_ID,
        1,
        2,
        &[block(3), block(4)],
    );

    let traversal =
        SolanaTraversal::new(reader.clone(), program_id, SETTLEMENT_PROGRAM, CHAIN_ID, 0);
    let inbox = InboxRetrieval::new(reader, program_id, SETTLEMENT_PROGRAM);
    // batch 0 uses build calls 1-2; batch 1's first block is the 3rd build call overall.
    let flaky = FlakyOnce {
        inner: MockEngineApi::default(),
        fail_build_at_call: 3,
        calls: 0,
    };
    let engine = EngineController::new(flaky, B256::ZERO, 0);
    let mut pipeline = DerivePipeline::new(
        traversal,
        inbox,
        engine,
        CHAIN_ID,
        alloy_primitives::Address::ZERO,
        16,
        10,
    );

    let first = pipeline.step().await.unwrap();
    assert!(matches!(first, StepOutcome::Derived { batch: 0, .. }));

    let err = pipeline.step().await.unwrap_err();
    assert!(matches!(err, PipelineError::Temporary(_)), "got {err:?}");

    let retry = pipeline.step().await.unwrap();
    match retry {
        StepOutcome::Derived { batch, blocks } => {
            assert_eq!(batch, 1);
            assert_eq!(blocks.len(), 2);
        }
        StepOutcome::Idle => {
            panic!("expected batch 1 to derive on retry, continuity preserved from batch 0")
        }
    }
}

/// The closed-batch case, modeled exactly: batch 0 (design/real blocks 1..=5 —
/// design number == real height, no offset) is closed (no account —
/// `CloseBatch` reallocs to 0 and reassigns to the system program) after its root finalized; batch 1
/// (design blocks 6..=8) is present. A node using the OLD always-start-at-batch-0 path is stuck forever;
/// one using [`resume_anchor`] resumes at batch 1 and derives it.
#[tokio::test]
async fn old_always_from_batch_zero_is_stuck_once_batch_zero_is_closed() {
    let program_id = Pubkey::new_unique();
    let mut reader = FakeAccountReader::default();
    // Batch 0 has NO account at all (closed) and no batch_cursor account either — SolanaTraversal
    // cannot tell "closed" apart from "not posted yet" without the settlement anchor.
    seed_batch(
        &mut reader,
        &program_id,
        CHAIN_ID,
        1,
        10,
        &[block(6), block(7), block(8)],
    );

    let traversal =
        SolanaTraversal::new(reader.clone(), program_id, SETTLEMENT_PROGRAM, CHAIN_ID, 0); // old: always 0
    let inbox = InboxRetrieval::new(reader, program_id, SETTLEMENT_PROGRAM);
    let engine = EngineController::from_engine_head(mock_with_genesis())
        .await
        .unwrap(); // old default
    let mut pipeline = DerivePipeline::new(
        traversal,
        inbox,
        engine,
        CHAIN_ID,
        alloy_primitives::Address::ZERO,
        16,
        10,
    );

    let outcome = pipeline.step().await.unwrap();
    assert!(
        matches!(outcome, StepOutcome::Idle),
        "the old path can only ever see batch 0 as 'not posted yet' — got {outcome:?}"
    );
}

fn mock_with_genesis() -> MockEngineApi {
    let mut mock = MockEngineApi::default();
    mock.existing_blocks.insert(
        0,
        ExistingBlock {
            block_hash: B256::ZERO,
            timestamp: 0,
            prev_randao: B256::ZERO,
            gas_limit: 0,
            state_root: B256::ZERO,
            tx_hashes: vec![],
            // Real height 0 is never checked against `canonical_header_rule` (module doc: consolidation
            // only ever queries height >= 1) — harmless placeholders.
            beneficiary: alloy_primitives::Address::ZERO,
            extra_data: alloy_primitives::Bytes::new(),
            withdrawals_root: rome_zk_executor_api::EMPTY_WITHDRAWALS,
            parent_beacon_block_root: B256::ZERO,
            blob_gas_used: 0,
            excess_blob_gas: 0,
        },
    );
    mock
}

/// Built through `rome_zk_layouts::root::write` — the real producer's own writer
/// (`root.rs:120`) — instead of hand-writing four offsets into a zeroed buffer by hand. Every field this
/// fixture doesn't care about (`parent_hash`, `state_root`, `updates`, `profile`,
/// `challenge_window_slots`, ...) is zeroed explicitly via `RootFields`, matching exactly what the old
/// hand-rolled version left as zero — same semantics, real encoder.
fn root_account_bytes(
    chain_id: u64,
    number: u64,
    block_hash: [u8; 32],
    head_final_batch: u64,
) -> Vec<u8> {
    rome_zk_layouts::root::write(&rome_zk_layouts::root::RootFields {
        chain_id,
        number,
        parent_hash: [0u8; 32],
        state_root: [0u8; 32],
        block_hash,
        updates: 0,
        profile: 0,
        challenge_window_slots: 0,
        prove_window_slots: 0,
        proving_policy: 0,
        poster_bond: 0,
        exit_cap_per_window: 0,
        authority: [0u8; 32],
        head_pending_batch: 0,
        head_final_batch,
        pending_count: 0,
        max_pending: 0,
    })
    .to_vec()
}

/// The same fixture as the test above, but resumed via [`resume_anchor`] — the node
/// resumes at batch 1 and derives it, never touching batch 0's (closed, absent) account at all.
#[tokio::test]
async fn resume_anchor_derives_batch_one_after_batch_zero_was_closed() {
    let program_id = Pubkey::new_unique();
    let settlement_program_id = Pubkey::new_unique();
    let mut reader = FakeAccountReader::default();

    // The engine already holds real heights 1..=5 (design blocks 1..=5, no
    // offset — batch 0 — already derived before batch 0's rent was reclaimed).
    let mut engine_mock = MockEngineApi::default();
    let hashes: Vec<B256> = (1..=5u64).map(|h| B256::repeat_byte(h as u8)).collect();
    for h in 1..=5u64 {
        let rule = rome_zk_executor_api::canonical_header_rule(
            CHAIN_ID,
            h,
            alloy_primitives::Address::ZERO,
        );
        engine_mock.existing_blocks.insert(
            h,
            ExistingBlock {
                block_hash: hashes[(h - 1) as usize],
                timestamp: 1_757_000_000 + h,
                prev_randao: rome_zk_executor_api::prev_randao(CHAIN_ID, h),
                gas_limit: 100_000_000,
                state_root: B256::ZERO,
                tx_hashes: vec![],
                beneficiary: rule.beneficiary,
                extra_data: rule.extra_data,
                withdrawals_root: rule.withdrawals_root,
                parent_beacon_block_root: rule.parent_beacon_block_root,
                blob_gas_used: rule.blob_gas_used,
                excess_blob_gas: rule.excess_blob_gas,
            },
        );
    }
    // The settlement root: batch 0 final at real height 5 (design block 5 — no offset).
    let (root_pda, _) = zk_settlement_client::root_pda(&settlement_program_id, CHAIN_ID);
    reader
        .accounts
        .insert(root_pda, root_account_bytes(CHAIN_ID, 5, hashes[4].0, 0));

    // Batch 0 has no account (closed); batch 1 (design blocks 6..=8) is present.
    seed_batch(
        &mut reader,
        &program_id,
        CHAIN_ID,
        1,
        10,
        &[block(6), block(7), block(8)],
    );

    let anchor = resume_anchor(
        &mut reader,
        &settlement_program_id,
        CHAIN_ID,
        &mut engine_mock,
    )
    .await
    .unwrap();
    let (start_at_batch, last_design_block, number, block_hash) = match anchor {
        ResumeAnchor::Confirmed {
            start_at_batch,
            last_design_block,
            number,
            block_hash,
        } => (start_at_batch, last_design_block, number, block_hash),
        ResumeAnchor::FromGenesis => panic!("expected a confirmed anchor"),
    };
    assert_eq!(start_at_batch, 1);
    assert_eq!(last_design_block, Some(5));
    assert_eq!(number, 5);

    let traversal = SolanaTraversal::new(
        reader.clone(),
        program_id,
        SETTLEMENT_PROGRAM,
        CHAIN_ID,
        start_at_batch,
    );
    let inbox = InboxRetrieval::new(reader, program_id, SETTLEMENT_PROGRAM);
    let engine = EngineController::new(engine_mock, block_hash, number);
    let mut pipeline = DerivePipeline::new(
        traversal,
        inbox,
        engine,
        CHAIN_ID,
        alloy_primitives::Address::ZERO,
        16,
        10,
    )
    .with_last_design_block(last_design_block);

    let outcome = pipeline.step().await.unwrap();
    match outcome {
        StepOutcome::Derived { batch, blocks } => {
            assert_eq!(
                batch, 1,
                "must resume directly at batch 1, never touching batch 0"
            );
            assert_eq!(blocks.len(), 3);
        }
        StepOutcome::Idle => panic!("expected batch 1 to derive"),
    }
}

/// At the settlement genesis sentinel
/// (`head_final_batch = 0`, `number = 0` — `InitChain`'s own initial values, `programs/zk-settlement/
/// src/chain.rs`) a fresh chain must be able to derive its very first batch — design blocks 1..=2, real
/// heights 1..=2 (design number == real height, no offset) — instead of the old
/// `saturating_sub` offset demanding design block 1 first when the rest of the (older) pipeline still
/// started numbering at 0 (which no chain could ever satisfy).
///
/// Batch ids are 1-based; 0 is the sentinel everywhere. A fresh chain's
/// first real batch is id 1, not 0 — settlement's own `PostRootProved` requires the first postable batch
/// to be `head_pending_batch + 1 = 1` (`settle.rs:246-256`), so an inbox whose first opened batch is 0
/// could never be finalized. `resume_anchor` used to return `start_at_batch: 0`; the test requires 1.
#[tokio::test]
async fn a_fresh_chain_at_the_genesis_sentinel_derives_its_first_batch() {
    let program_id = Pubkey::new_unique();
    let settlement_program_id = Pubkey::new_unique();
    let mut reader = FakeAccountReader::default();

    // The engine already has its own real genesis at height 0 (every engine does) — nothing has been
    // derived through this pipeline yet.
    let genesis_hash = B256::repeat_byte(0x11);
    let mut engine_mock = MockEngineApi::default();
    engine_mock.existing_blocks.insert(
        0,
        ExistingBlock {
            block_hash: genesis_hash,
            timestamp: 0,
            prev_randao: B256::ZERO,
            gas_limit: 0,
            state_root: B256::ZERO,
            tx_hashes: vec![],
            beneficiary: alloy_primitives::Address::ZERO,
            extra_data: alloy_primitives::Bytes::new(),
            withdrawals_root: rome_zk_executor_api::EMPTY_WITHDRAWALS,
            parent_beacon_block_root: B256::ZERO,
            blob_gas_used: 0,
            excess_blob_gas: 0,
        },
    );

    // InitChain's genesis sentinel: head_final_batch = 0 ("none"), number = 0, block_hash = genesis.
    let (root_pda, _) = zk_settlement_client::root_pda(&settlement_program_id, CHAIN_ID);
    reader
        .accounts
        .insert(root_pda, root_account_bytes(CHAIN_ID, 0, genesis_hash.0, 0));

    // A fresh chain's FIRST batch is id 1 (the tooling-initialised cursor starts at 1); its
    // blocks are design 1..=2 (real heights, no offset).
    seed_batch(
        &mut reader,
        &program_id,
        CHAIN_ID,
        1,
        10,
        &[block(1), block(2)],
    );

    let anchor = resume_anchor(
        &mut reader,
        &settlement_program_id,
        CHAIN_ID,
        &mut engine_mock,
    )
    .await
    .unwrap();
    let (start_at_batch, last_design_block, number, block_hash) = match anchor {
        ResumeAnchor::Confirmed {
            start_at_batch,
            last_design_block,
            number,
            block_hash,
        } => (start_at_batch, last_design_block, number, block_hash),
        ResumeAnchor::FromGenesis => panic!("expected a confirmed anchor at the genesis sentinel"),
    };
    assert_eq!(start_at_batch, 1);
    assert_eq!(
        last_design_block, None,
        "nothing has been derived yet at the genesis sentinel"
    );
    assert_eq!(number, 0);

    let traversal = SolanaTraversal::new(
        reader.clone(),
        program_id,
        SETTLEMENT_PROGRAM,
        CHAIN_ID,
        start_at_batch,
    );
    let inbox = InboxRetrieval::new(reader, program_id, SETTLEMENT_PROGRAM);
    let engine = EngineController::new(engine_mock, block_hash, number);
    let mut pipeline = DerivePipeline::new(
        traversal,
        inbox,
        engine,
        CHAIN_ID,
        alloy_primitives::Address::ZERO,
        16,
        10,
    )
    .with_last_design_block(last_design_block);

    let outcome = pipeline.step().await.unwrap();
    match outcome {
        StepOutcome::Derived { batch, blocks } => {
            assert_eq!(batch, 1);
            assert_eq!(blocks.len(), 2);
        }
        StepOutcome::Idle => panic!("expected batch 1 to derive from the genesis sentinel"),
    }
}
