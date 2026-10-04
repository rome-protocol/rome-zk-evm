//! Strict policy, at the *pipeline* level, not just one stage in isolation: a
//! tampered chunk body must produce [`PipelineError::Critical`], and — the part a stage-local unit test
//! cannot show — the engine must never have been touched at all (the strict policy: a decode failure
//! rejects the whole batch before anything reaches `forkchoiceUpdated`/`getPayload`/`newPayload`).

use rome_zk_derive::engine::mock::MockEngineApi;
use rome_zk_derive::engine::EngineController;
use rome_zk_derive::inbox::InboxRetrieval;
use rome_zk_derive::pipeline::{DerivePipeline, StepOutcome};
use rome_zk_derive::testutil::FakeAccountReader;
use rome_zk_derive::traversal::SolanaTraversal;
use rome_zk_derive::PipelineError;
use solana_program::pubkey::Pubkey;

/// Stand-in settlement program the test chain is registered under (inbox accounts are keyed by it).
const SETTLEMENT_PROGRAM: Pubkey = Pubkey::new_from_array([9u8; 32]);

const CHAIN_ID: u64 = 200_101;

/// `chunk_bodies` (in idx order) are hashed and reduced through the same `reference_commitment`
/// [`rome_zk_derive::inbox::InboxRetrieval`] itself uses — a fixture with an empty slice
/// gets zeroed commitment fields, fine for a test whose expected failure is raised before that check
/// (e.g. a chunk body too short to even parse).
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
    // stays at the default `DriftBound::unbounded()`, so the exact value is inert here;
    // `tests/pipeline_drift_bound.rs` is where the bound itself is exercised.
    d[rome_zk_layouts::batch::OFF_OPEN_UNIX_TS..rome_zk_layouts::batch::OFF_OPEN_UNIX_TS + 8]
        .copy_from_slice(&1_757_000_000i64.to_le_bytes());
    if !chunk_bodies.is_empty() {
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
        d[rome_zk_layouts::batch::OFF_ACC..rome_zk_layouts::batch::OFF_ACC + 32]
            .copy_from_slice(&acc);
    }
    d
}

/// A chunk account whose body is too short to be a valid frame (< the 19-byte frame header) — a
/// tampered/corrupt chunk, caught by the decode-failure gate.
fn tampered_chunk_account(chain_id: u64, batch: u64, idx: u32) -> Vec<u8> {
    let body = vec![0xffu8; 5]; // shorter than channel::FRAME_HEADER_LEN (19)
    let mut d = vec![0u8; zk_inbox::HEADER_LEN + body.len()];
    d[zk_inbox::OFF_MAGIC..zk_inbox::OFF_MAGIC + 4].copy_from_slice(&zk_inbox::MAGIC.to_le_bytes());
    d[zk_inbox::OFF_CHAIN_ID..zk_inbox::OFF_CHAIN_ID + 8].copy_from_slice(&chain_id.to_le_bytes());
    d[zk_inbox::OFF_BATCH..zk_inbox::OFF_BATCH + 8].copy_from_slice(&batch.to_le_bytes());
    d[zk_inbox::OFF_IDX..zk_inbox::OFF_IDX + 4].copy_from_slice(&idx.to_le_bytes());
    d[zk_inbox::OFF_LEN..zk_inbox::OFF_LEN + 4].copy_from_slice(&(body.len() as u32).to_le_bytes());
    d[zk_inbox::OFF_SEALED] = 1; // sealed
    d[zk_inbox::HEADER_LEN..].copy_from_slice(&body);
    d
}

#[tokio::test]
async fn a_tampered_chunk_is_critical_and_the_engine_is_never_touched() {
    let program_id = Pubkey::new_unique();
    let mut reader = FakeAccountReader::default();

    let (batch_pda, _) = zk_inbox_client::batch_pda(&program_id, &SETTLEMENT_PROGRAM, CHAIN_ID, 0);
    reader
        .accounts
        .insert(batch_pda, batch_account_bytes(CHAIN_ID, 0, 1, 1, &[]));
    let (chunk_pda, _) =
        zk_inbox_client::chunk_pda(&program_id, &SETTLEMENT_PROGRAM, CHAIN_ID, 0, 0);
    reader
        .accounts
        .insert(chunk_pda, tampered_chunk_account(CHAIN_ID, 0, 0));

    let traversal =
        SolanaTraversal::new(reader.clone(), program_id, SETTLEMENT_PROGRAM, CHAIN_ID, 0);
    let inbox = InboxRetrieval::new(reader, program_id, SETTLEMENT_PROGRAM);
    let engine = EngineController::new(MockEngineApi::default(), alloy_primitives::B256::ZERO, 0);
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
    assert!(
        matches!(err, PipelineError::Critical(_)),
        "a tampered chunk must be Critical, got {err:?}"
    );
    assert!(
        pipeline.engine_controller().engine().calls.is_empty(),
        "the engine must never be called when a batch fails strict validity before reaching it"
    );
    // The cursor must not have advanced past the rejected batch — a caller retrying (after fixing
    // whatever produced the tampered chunk, or accepting the chain is stuck) resumes at the same id.
    assert_eq!(pipeline.next_batch(), 0);
}

/// The companion positive case: an untampered batch derives cleanly through the same pipeline and DOES
/// reach the engine — proves the negative-case assertion above is actually discriminating (design
/// principle: "control varies the world, not the ruler").
#[tokio::test]
async fn an_untampered_batch_derives_and_does_reach_the_engine() {
    use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
    use alloy_primitives::{Address, Bytes, TxKind, U256};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use rome_zk_channel as channel;
    use rome_zk_channel::Block;

    let program_id = Pubkey::new_unique();
    let signer = PrivateKeySigner::random();
    let tx = TxEip1559 {
        chain_id: CHAIN_ID,
        nonce: 0,
        gas_limit: 21_000,
        max_fee_per_gas: 1_000_000_000,
        max_priority_fee_per_gas: 1_000_000_000,
        to: TxKind::Call(Address::ZERO),
        value: U256::ZERO,
        access_list: Default::default(),
        input: Bytes::new(),
    };
    let sig_hash = tx.signature_hash();
    let signature = signer.sign_hash_sync(&sig_hash).unwrap();
    let raw = Bytes::from(alloy_eips::eip2718::Encodable2718::encoded_2718(
        &TxEnvelope::from(tx.into_signed(signature)),
    ));
    let blocks = vec![Block {
        number: 1,
        timestamp: 1_757_000_001,
        gas_limit: 100_000_000,
        txs: vec![raw],
        deposits_end: None,
    }];
    // Batch 0's blocks are design-numbered starting at 1 (the sequencer
    // numbers its first sealed block 1, so design number == real height, no offset — this fixture's
    // fresh `EngineController` starts at real height 1 too). A batch's blocks continue from
    // wherever the previous one left off — there is no fixed `batch * blocks_per_batch` arithmetic.
    let compressed = channel::encode_stream(&blocks);
    let frames = channel::cut_frames(CHAIN_ID, 0, &compressed, 3_681);
    assert_eq!(frames.len(), 1, "fixture fits in one chunk");

    let mut reader = FakeAccountReader::default();
    let (batch_pda, _) = zk_inbox_client::batch_pda(&program_id, &SETTLEMENT_PROGRAM, CHAIN_ID, 0);
    let frame_bytes = frames[0].to_bytes();
    reader.accounts.insert(
        batch_pda,
        batch_account_bytes(CHAIN_ID, 0, 1, 1, &[&frame_bytes]),
    );
    let (chunk_pda, _) =
        zk_inbox_client::chunk_pda(&program_id, &SETTLEMENT_PROGRAM, CHAIN_ID, 0, 0);
    let mut chunk = vec![0u8; zk_inbox::HEADER_LEN + frame_bytes.len()];
    chunk[zk_inbox::OFF_MAGIC..zk_inbox::OFF_MAGIC + 4]
        .copy_from_slice(&zk_inbox::MAGIC.to_le_bytes());
    chunk[zk_inbox::OFF_CHAIN_ID..zk_inbox::OFF_CHAIN_ID + 8]
        .copy_from_slice(&CHAIN_ID.to_le_bytes());
    chunk[zk_inbox::OFF_BATCH..zk_inbox::OFF_BATCH + 8].copy_from_slice(&0u64.to_le_bytes());
    chunk[zk_inbox::OFF_IDX..zk_inbox::OFF_IDX + 4].copy_from_slice(&0u32.to_le_bytes());
    chunk[zk_inbox::OFF_LEN..zk_inbox::OFF_LEN + 4]
        .copy_from_slice(&(frame_bytes.len() as u32).to_le_bytes());
    chunk[zk_inbox::OFF_SEALED] = 1;
    chunk[zk_inbox::HEADER_LEN..].copy_from_slice(&frame_bytes);
    reader.accounts.insert(chunk_pda, chunk);

    let traversal =
        SolanaTraversal::new(reader.clone(), program_id, SETTLEMENT_PROGRAM, CHAIN_ID, 0);
    let inbox = InboxRetrieval::new(reader, program_id, SETTLEMENT_PROGRAM);
    let engine = EngineController::new(MockEngineApi::default(), alloy_primitives::B256::ZERO, 0);
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
    match outcome {
        StepOutcome::Derived { batch, blocks } => {
            assert_eq!(batch, 0);
            assert_eq!(blocks.len(), 1);
        }
        StepOutcome::Idle => panic!("expected a derived batch"),
    }
    assert!(
        !pipeline.engine_controller().engine().calls.is_empty(),
        "an untampered batch must reach the engine"
    );
    assert_eq!(
        pipeline.next_batch(),
        1,
        "cursor must advance past a derived batch"
    );
}
