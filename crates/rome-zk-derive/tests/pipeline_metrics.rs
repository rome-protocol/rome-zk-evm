//! Contract: `rome-zk-derive`'s Prometheus metrics, driven through the real
//! `DerivePipeline` — not a stage-local unit test of `metrics::Metrics` in isolation (that lives in
//! `src/metrics.rs`'s own tests). Fixture pattern mirrors `tests/pipeline_drift_bound.rs`.

use rome_zk_channel as channel;
use rome_zk_channel::Block;
use rome_zk_derive::engine::mock::MockEngineApi;
use rome_zk_derive::engine::EngineController;
use rome_zk_derive::inbox::InboxRetrieval;
use rome_zk_derive::pipeline::{DerivePipeline, StepOutcome};
use rome_zk_derive::testutil::FakeAccountReader;
use rome_zk_derive::traversal::SolanaTraversal;
use solana_program::pubkey::Pubkey;

/// Stand-in settlement program the test chain is registered under (inbox accounts are keyed by it).
const SETTLEMENT_PROGRAM: Pubkey = Pubkey::new_from_array([9u8; 32]);

const CHAIN_ID: u64 = 200_101;
const OPEN_UNIX_TS: i64 = 1_757_000_000;

fn block_with_timestamp(number: u64, timestamp: u64) -> Block {
    Block {
        number,
        timestamp,
        gas_limit: 100_000_000,
        txs: vec![],
        deposits_end: None,
    }
}

/// A finalized v2 batch account's raw bytes with an explicit `open_unix_ts` — identical shape to
/// `pipeline_drift_bound.rs`'s own `batch_account_bytes` helper (each test file keeps its own copy, the
/// existing convention in this crate's `tests/` directory).
fn batch_account_bytes(
    chain_id: u64,
    batch: u64,
    open_slot: u64,
    open_unix_ts: i64,
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
/// `decode_batch_account` refuses it by name (`BadVersion`) — the simplest way to force a
/// `PipelineError::Critical` without constructing any frames at all.
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

    let (batch_pda, _) =
        zk_inbox_client::batch_pda(program_id, &SETTLEMENT_PROGRAM, chain_id, batch);
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

fn pipeline(
    reader: FakeAccountReader,
    program_id: Pubkey,
) -> DerivePipeline<FakeAccountReader, MockEngineApi> {
    let traversal =
        SolanaTraversal::new(reader.clone(), program_id, SETTLEMENT_PROGRAM, CHAIN_ID, 0);
    let inbox = InboxRetrieval::new(reader, program_id, SETTLEMENT_PROGRAM);
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
}

/// `derive_critical_total` increments on a Critical batch, and only that counter.
#[tokio::test]
async fn derive_critical_total_increments_on_a_critical_batch() {
    let program_id = Pubkey::new_unique();
    let mut reader = FakeAccountReader::default();
    let (batch_pda, _) = zk_inbox_client::batch_pda(&program_id, &SETTLEMENT_PROGRAM, CHAIN_ID, 0);
    reader
        .accounts
        .insert(batch_pda, v1_batch_account_bytes(CHAIN_ID, 0, 1));
    let mut pipeline = pipeline(reader, program_id);

    let err = pipeline.step().await.unwrap_err();
    assert!(
        matches!(err, rome_zk_derive::PipelineError::Critical(_)),
        "expected Critical, got {err:?}"
    );

    let rendered = pipeline.metrics().render();
    assert!(
        rendered.contains("rome_zk_derive_critical_total 1"),
        "critical_total must be exactly 1, got:\n{rendered}"
    );
    assert!(
        rendered.contains("rome_zk_derive_batches_derived_total 0"),
        "a Critical batch must never count as derived, got:\n{rendered}"
    );
}

/// A successfully derived batch moves `last_batch`/`head_block` and observes `batch_seconds`.
#[tokio::test]
async fn a_derived_batch_moves_last_batch_head_block_and_observes_batch_seconds() {
    let program_id = Pubkey::new_unique();
    let mut reader = FakeAccountReader::default();
    seed_batch(
        &mut reader,
        &program_id,
        CHAIN_ID,
        0,
        1,
        OPEN_UNIX_TS,
        &[block_with_timestamp(1, 1_700_000_000)],
    );
    let mut pipeline = pipeline(reader, program_id);

    let outcome = pipeline.step().await.unwrap();
    assert!(matches!(outcome, StepOutcome::Derived { batch: 0, .. }));

    let rendered = pipeline.metrics().render();
    assert!(
        rendered.contains("rome_zk_derive_batches_derived_total 1"),
        "got:\n{rendered}"
    );
    assert!(
        rendered.contains("rome_zk_derive_last_batch 0"),
        "got:\n{rendered}"
    );
    assert!(
        rendered.contains("rome_zk_derive_head_block 1"),
        "got:\n{rendered}"
    );
    assert!(
        rendered.contains("rome_zk_derive_batch_seconds_count 1"),
        "batch_seconds must carry exactly one observation, got:\n{rendered}"
    );
    assert!(
        rendered.contains("rome_zk_derive_critical_total 0"),
        "a clean derive must never touch critical_total, got:\n{rendered}"
    );
}

/// Mutation target: dropping the `critical_total.inc()` call in
/// `DerivePipeline::step`'s Critical arm must turn the first test in this file red — asserted here as a
/// standing sanity check that the counter really moves off zero only on a real Critical, never by
/// default.
#[tokio::test]
async fn critical_total_stays_zero_until_a_critical_batch_actually_occurs() {
    let program_id = Pubkey::new_unique();
    let reader = FakeAccountReader::default(); // no batch seeded at all -> traversal.next() is Idle
    let mut pipeline = pipeline(reader, program_id);

    let outcome = pipeline.step().await.unwrap();
    assert!(matches!(outcome, StepOutcome::Idle));

    let rendered = pipeline.metrics().render();
    assert!(
        rendered.contains("rome_zk_derive_critical_total 0"),
        "got:\n{rendered}"
    );
}
