//! Contract: the one-sided timestamp drift bound, driven through
//! the real `DerivePipeline` (traversal + `enforce_drift_bound`), against a batch account's own
//! committed `open_unix_ts` (header v2) — not a stage-local unit test of `batch_queue` in isolation
//! (that lives in `src/batch_queue.rs`'s own tests). Fixture pattern mirrors
//! `tests/pipeline_atomic_retry_and_resume.rs`.

use rome_zk_channel as channel;
use rome_zk_channel::Block;
use rome_zk_derive::engine::mock::MockEngineApi;
use rome_zk_derive::engine::EngineController;
use rome_zk_derive::inbox::InboxRetrieval;
use rome_zk_derive::pipeline::{DerivePipeline, StepOutcome};
use rome_zk_derive::testutil::FakeAccountReader;
use rome_zk_derive::traversal::SolanaTraversal;
use rome_zk_derive::PipelineError;
use solana_program::pubkey::Pubkey;

const CHAIN_ID: u64 = 200_101;
const MAX_DRIFT_SECS: u64 = 60;
const OPEN_UNIX_TS: i64 = 1_757_000_000;

fn block_with_timestamp(number: u64, timestamp: u64) -> Block {
    Block {
        number,
        timestamp,
        gas_limit: 100_000_000,
        txs: vec![],
    }
}

/// Builds a finalized batch account's raw bytes with an explicit `open_unix_ts` (header v2) — every
/// offset via the shared `rome_zk_layouts::batch` constants, never a literal number, so a header-shape
/// change here turns red exactly like the production writer.
#[allow(clippy::too_many_arguments)]
fn batch_account_bytes(
    chain_id: u64,
    batch: u64,
    open_slot: u64,
    open_unix_ts: i64,
    expected_count: u32,
    chunk_bodies: &[&[u8]],
) -> Vec<u8> {
    let mut d = vec![0u8; rome_zk_layouts::batch::account_len(expected_count)];
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
    d[rome_zk_layouts::batch::OFF_OPEN_UNIX_TS..rome_zk_layouts::batch::OFF_OPEN_UNIX_TS + 8]
        .copy_from_slice(&open_unix_ts.to_le_bytes());
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

/// A v1-shaped (`VERSION = 1`) batch account — no `open_unix_ts` field at all, the old 202-byte shape.
fn v1_batch_account_bytes(chain_id: u64, batch: u64, expected_count: u32) -> Vec<u8> {
    let mut d = vec![0u8; 202];
    d[rome_zk_layouts::batch::OFF_MAGIC..rome_zk_layouts::batch::OFF_MAGIC + 4]
        .copy_from_slice(&rome_zk_layouts::batch::MAGIC.to_le_bytes());
    d[rome_zk_layouts::batch::OFF_VERSION] = 1;
    d[rome_zk_layouts::batch::OFF_CHAIN_ID..rome_zk_layouts::batch::OFF_CHAIN_ID + 8]
        .copy_from_slice(&chain_id.to_le_bytes());
    d[rome_zk_layouts::batch::OFF_BATCH..rome_zk_layouts::batch::OFF_BATCH + 8]
        .copy_from_slice(&batch.to_le_bytes());
    d[rome_zk_layouts::batch::OFF_EXPECTED_COUNT..rome_zk_layouts::batch::OFF_EXPECTED_COUNT + 4]
        .copy_from_slice(&expected_count.to_le_bytes());
    d[rome_zk_layouts::batch::OFF_FINALIZED] = 1;
    d
}

fn seed_batch(
    reader: &mut FakeAccountReader,
    program_id: &Pubkey,
    chain_id: u64,
    batch: u64,
    open_slot: u64,
    open_unix_ts: i64,
    blocks: &[Block],
) {
    let compressed = channel::encode_stream(blocks);
    let frames = channel::cut_frames(chain_id, batch, &compressed, 3_681);
    let bodies: Vec<Vec<u8>> = frames.iter().map(|f| f.to_bytes()).collect();
    let body_refs: Vec<&[u8]> = bodies.iter().map(|b| b.as_slice()).collect();

    let (batch_pda, _) = zk_inbox_client::batch_pda(program_id, chain_id, batch);
    reader.accounts.insert(
        batch_pda,
        batch_account_bytes(
            chain_id,
            batch,
            open_slot,
            open_unix_ts,
            frames.len() as u32,
            &body_refs,
        ),
    );
    for (idx, body) in bodies.iter().enumerate() {
        let (chunk_pda, _) = zk_inbox_client::chunk_pda(program_id, chain_id, batch, idx as u32);
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

fn pipeline_with_drift_bound(
    reader: FakeAccountReader,
    program_id: Pubkey,
) -> DerivePipeline<FakeAccountReader, MockEngineApi> {
    let traversal = SolanaTraversal::new(reader.clone(), program_id, CHAIN_ID, 0);
    let inbox = InboxRetrieval::new(reader, program_id);
    let engine = EngineController::new(MockEngineApi::default(), alloy_primitives::B256::ZERO, 0);
    DerivePipeline::new(
        traversal,
        inbox,
        engine,
        CHAIN_ID,
        alloy_primitives::Address::ZERO,
        16,
        10,
    )
    .with_drift_bound(MAX_DRIFT_SECS)
}

/// A block timestamped exactly at the anchor's bound (`open_unix_ts + max_drift_secs`) passes — the
/// bound is inclusive (`<=`, `batch_queue::enforce_drift_bound`).
#[tokio::test]
async fn a_block_exactly_at_the_bound_passes() {
    let program_id = Pubkey::new_unique();
    let mut reader = FakeAccountReader::default();
    let at_bound = OPEN_UNIX_TS as u64 + MAX_DRIFT_SECS;
    seed_batch(
        &mut reader,
        &program_id,
        CHAIN_ID,
        0,
        1,
        OPEN_UNIX_TS,
        &[block_with_timestamp(1, at_bound)],
    );
    let mut pipeline = pipeline_with_drift_bound(reader, program_id);

    let outcome = pipeline.step().await.unwrap();
    match outcome {
        StepOutcome::Derived { batch, blocks } => {
            assert_eq!(batch, 0);
            assert_eq!(blocks.len(), 1);
        }
        StepOutcome::Idle => panic!("expected the batch to derive"),
    }
}

/// A block timestamped one second past the bound is `PipelineError::Critical`, naming the drift bound —
/// not a silent skip, not Temporary.
#[tokio::test]
async fn a_block_one_second_past_the_bound_is_critical() {
    let program_id = Pubkey::new_unique();
    let mut reader = FakeAccountReader::default();
    let past_bound = OPEN_UNIX_TS as u64 + MAX_DRIFT_SECS + 1;
    seed_batch(
        &mut reader,
        &program_id,
        CHAIN_ID,
        0,
        1,
        OPEN_UNIX_TS,
        &[block_with_timestamp(1, past_bound)],
    );
    let mut pipeline = pipeline_with_drift_bound(reader, program_id);

    let err = pipeline.step().await.unwrap_err();
    match &err {
        PipelineError::Critical(msg) => {
            assert!(
                msg.contains("drift bound"),
                "error must name the drift bound, got: {msg}"
            );
        }
        other => panic!("expected Critical naming the drift bound, got {other:?}"),
    }
}

/// A v1-shaped batch account (no `open_unix_ts`) is refused by name (`BadVersion`) through the real
/// traversal — `Critical`, never a silent skip, and never treated as `open_unix_ts == 0` (which would
/// make every real block's timestamp look like a drift violation).
#[tokio::test]
async fn a_v1_batch_account_is_critical_naming_bad_version() {
    let program_id = Pubkey::new_unique();
    let mut reader = FakeAccountReader::default();
    let (batch_pda, _) = zk_inbox_client::batch_pda(&program_id, CHAIN_ID, 0);
    reader
        .accounts
        .insert(batch_pda, v1_batch_account_bytes(CHAIN_ID, 0, 1));
    let mut pipeline = pipeline_with_drift_bound(reader, program_id);

    let err = pipeline.step().await.unwrap_err();
    match &err {
        PipelineError::Critical(msg) => {
            assert!(
                msg.to_lowercase().contains("version"),
                "error must name BadVersion, got: {msg}"
            );
        }
        other => panic!("expected Critical naming BadVersion, got {other:?}"),
    }
}

/// A committed `open_unix_ts` of `-1` (impossible from a
/// real `Clock` — `OpenBatch` itself now refuses this at the source, but this pipeline must still refuse
/// a batch account that predates that refusal, or reached one some other way, rather than silently
/// treating it as anchor 0) is `Critical`, naming the real cause — never surfaced as a drift-bound
/// violation against a phantom anchor of 0.
#[tokio::test]
async fn a_negative_committed_open_unix_ts_is_refused_by_name() {
    let program_id = Pubkey::new_unique();
    let mut reader = FakeAccountReader::default();
    seed_batch(
        &mut reader,
        &program_id,
        CHAIN_ID,
        0,
        1,
        -1,
        &[block_with_timestamp(1, 1_700_000_000)],
    );
    let mut pipeline = pipeline_with_drift_bound(reader, program_id);

    let err = pipeline.step().await.unwrap_err();
    match &err {
        PipelineError::Critical(msg) => {
            assert!(
                msg.contains("negative"),
                "error must name the value as negative, not a drift-bound violation against anchor 0, \
                 got: {msg}"
            );
            assert!(
                !msg.contains("drift bound"),
                "a negative open_unix_ts must never be diagnosed as a drift-bound violation: {msg}"
            );
        }
        other => panic!("expected Critical naming the negative open_unix_ts, got {other:?}"),
    }
}

/// A batch whose DA lands long after it was sealed — every block's timestamp far *below*
/// the anchor, not above it — must still derive. The bound is one-sided (only "too far in the
/// future" is ever refused); this drives that honest late-post case through the real pipeline end to
/// end, not just `batch_queue`'s own unit test (`a_late_anchor_from_batcher_backlog_never_rejects`).
/// Making the bound two-sided would turn only that unit test red, with every pipeline-level suite
/// staying green; this test closes that gap.
#[tokio::test]
async fn a_batch_posted_long_after_sealing_derives() {
    let program_id = Pubkey::new_unique();
    let mut reader = FakeAccountReader::default();
    let long_after_sealing = OPEN_UNIX_TS as u64 - 3_600;
    seed_batch(
        &mut reader,
        &program_id,
        CHAIN_ID,
        0,
        1,
        OPEN_UNIX_TS,
        &[block_with_timestamp(1, long_after_sealing)],
    );
    let mut pipeline = pipeline_with_drift_bound(reader, program_id);

    let outcome = pipeline.step().await.unwrap();
    match outcome {
        StepOutcome::Derived { batch, blocks } => {
            assert_eq!(batch, 0);
            assert_eq!(blocks.len(), 1);
        }
        StepOutcome::Idle => panic!("expected the late-posted batch to derive, not idle"),
    }
}

// --- The drift bound holds across an idle gap of any length, because it
// is checked against the batch's own open_unix_ts (written fresh at OPEN time, after the gap) rather than
// the previous batch's last block. ---

/// A batch sealed after a one-hour idle gap (no blocks sealed in between, with
/// `empty_block_interval_secs = 0`) still passes: the next block's design number continues immediately
/// from the previous batch's last one (`batch_queue::decode_batch`'s own continuity check, unaffected by
/// how much wall-clock time passed), and its timestamp — 3,600 s ahead of the previous block's — is
/// checked only against THIS batch's own `open_unix_ts`, written when the batcher opened this batch after
/// the gap closed (`batch_close_after_secs`), not against the previous block or any
/// wall-clock delta from it. `open_unix_ts` here is set 100 s *after* the block's own timestamp (a
/// realistic batch_close_after_secs wait plus send/confirm latency) — enough to show the bound is checked
/// against the anchor, without weakening the honest-gap claim: the one-sided bound only ever asks
/// `block.timestamp <= open_unix_ts + max_drift`, which a same-or-later anchor always satisfies trivially.
#[tokio::test]
async fn a_block_sealed_after_a_one_hour_idle_gap_passes_the_drift_bound() {
    let program_id = Pubkey::new_unique();
    let mut reader = FakeAccountReader::default();
    let before_gap_ts = OPEN_UNIX_TS as u64 - 3_700;
    seed_batch(
        &mut reader,
        &program_id,
        CHAIN_ID,
        0,
        1,
        OPEN_UNIX_TS - 3_700,
        &[block_with_timestamp(1, before_gap_ts)],
    );
    // The idle gap: design block 2 seals 3,600 s after block 1, with nothing sealed in between (no
    // batch, no block, for the whole gap). Its own batch (id 1) opens 100 s after ITS block's timestamp —
    // well within the drift bound (one-sided, checked against open_unix_ts, never against block 1).
    let after_gap_ts = before_gap_ts + 3_600;
    let batch_1_open_unix_ts = after_gap_ts as i64 + 100;
    seed_batch(
        &mut reader,
        &program_id,
        CHAIN_ID,
        1,
        2,
        batch_1_open_unix_ts,
        &[block_with_timestamp(2, after_gap_ts)],
    );
    let mut pipeline = pipeline_with_drift_bound(reader, program_id);

    let first = pipeline.step().await.unwrap();
    assert!(
        matches!(first, StepOutcome::Derived { batch: 0, .. }),
        "got {first:?}"
    );

    let second = pipeline.step().await.unwrap();
    match second {
        StepOutcome::Derived { batch, blocks } => {
            assert_eq!(batch, 1);
            assert_eq!(blocks.len(), 1);
            assert_eq!(blocks[0].block_number, 2);
        }
        StepOutcome::Idle => panic!("expected the post-gap batch to derive, not idle"),
    }
}

/// Companion to the gap test above: exceeding the anchor by more than `max_drift_secs` is STILL Critical
/// — the gap test above must never be read as having weakened the bound itself.
/// `timestamp = open_unix_ts + max_drift + 1`.
#[tokio::test]
async fn a_block_ahead_of_the_anchor_by_more_than_max_drift_is_still_critical() {
    let program_id = Pubkey::new_unique();
    let mut reader = FakeAccountReader::default();
    let too_far_ahead = OPEN_UNIX_TS as u64 + MAX_DRIFT_SECS + 1;
    seed_batch(
        &mut reader,
        &program_id,
        CHAIN_ID,
        0,
        1,
        OPEN_UNIX_TS,
        &[block_with_timestamp(1, too_far_ahead)],
    );
    let mut pipeline = pipeline_with_drift_bound(reader, program_id);

    let err = pipeline.step().await.unwrap_err();
    match &err {
        PipelineError::Critical(msg) => {
            assert!(
                msg.contains("drift bound"),
                "error must name the drift bound, got: {msg}"
            );
        }
        other => panic!("expected Critical naming the drift bound, got {other:?}"),
    }
    // The Critical raised inside `derive_one_batch` must reach the counter — the traversal-side
    // Critical has its own test; this pins the other arm.
    let rendered = pipeline.metrics().render();
    assert!(
        rendered.contains("rome_zk_derive_critical_total 1"),
        "a drift-bound Critical must count once, rendered: {rendered}"
    );
    assert!(
        rendered.contains("rome_zk_derive_batches_derived_total 0"),
        "a refused batch is never counted as derived"
    );
}
