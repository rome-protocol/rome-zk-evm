//! Real-program test: chunks written to `solana-program-test` via `zk-inbox-client`
//! (the real, `cargo build-sbf`-compiled `zk_inbox.so` — not a native/builtin shortcut), read back
//! through [`InboxRetrieval`] and turned into the identical block list `batch_queue`/`attributes` would
//! hand the engine. Follows `rome-zk-batcher/tests/pipeline.rs`'s own `ProgramTest` setup pattern (its
//! helpers are copied here rather than imported — that file is a test binary in a different crate, not
//! a library) and reuses `rome_zk_batcher::pipeline::plan_chunk`/`open_and_grow_batch_ixs` to actually
//! post a real batch on chain before reading it back.
//!
//! [`InboxRetrieval`] is generic over [`AccountReader`]; this test's whole point is exercising that seam
//! against a REAL account reader backed by `BanksClient` (`BanksAccountReader` below), not the
//! in-memory `FakeAccountReader` every other test in this crate uses.

use rome_zk_batcher::channel::{self, Block, Frame};
use rome_zk_batcher::pipeline::{self, ChunkPlan};
use rome_zk_derive::inbox::InboxRetrieval;
use rome_zk_derive::reader::AccountReader;
use rome_zk_derive::PipelineError;
use rome_zk_testkit::{cursor_account, root_account_with_authority};
use solana_program::pubkey::Pubkey;
use solana_sdk::{
    account::Account,
    signature::{Keypair, Signer},
};
use solana_system_interface::program as system_program;

const CHAIN_ID: u64 = 200_198;
const BATCH_ID: u64 = 7;

/// Thin adapter over `rome_zk_testkit::send_measuring_cu` — this file never needs the CU figure and
/// panics on failure rather than returning a `Result`.
async fn send(
    ctx: &mut solana_program_test::ProgramTestContext,
    ixs: &[solana_program::instruction::Instruction],
    payer: &Keypair,
) {
    rome_zk_testkit::send_measuring_cu(ctx, ixs, payer, &[])
        .await
        .0
        .unwrap_or_else(|e| panic!("tx failed: {e:?}"));
}

fn tx_bytes(seed: u64, len: usize) -> alloy_primitives::Bytes {
    // Opaque, incompressible-ish bytes — this program/pipeline layer never parses tx contents
    // (`batch_queue`'s real EVM tx decode is covered separately, off a batcher-produced channel
    // stream, not a real Solana account). Incompressible so zstd cannot collapse the fixture down to
    // fewer chunks than the `max_frame_body_len` below needs for a real multi-frame batch.
    let bytes: Vec<u8> = (0..len as u64)
        .map(|i| ((seed + 1).wrapping_mul(2_654_435_761).wrapping_add(i)) as u8)
        .collect();
    alloy_primitives::Bytes::from(bytes)
}

fn two_block_batch() -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut seq = 0u64;
    for (block_number, tx_count) in [(BATCH_ID * 10, 6usize), (BATCH_ID * 10 + 1, 5)] {
        let txs = (0..tx_count)
            .map(|i| {
                let t = tx_bytes(seq, 40 + (i % 20));
                seq += 1;
                t
            })
            .collect();
        blocks.push(Block {
            number: block_number,
            timestamp: 1_757_000_000 + block_number,
            gas_limit: 100_000_000,
            txs,
            deposits_end: None,
        });
    }
    blocks
}

/// [`AccountReader`] over a real `BanksClient` (`solana-program-test`'s in-process validator) — proves
/// [`InboxRetrieval`]'s seam is genuinely reader-agnostic: production uses [`rome_zk_derive::reader::RpcAccountReader`]
/// (a real Solana RPC client), this test uses this instead, with zero changes to `InboxRetrieval` itself.
#[derive(Clone)]
struct BanksAccountReader(solana_program_test::BanksClient);

impl AccountReader for BanksAccountReader {
    async fn get_account_data(&mut self, pubkey: Pubkey) -> Result<Option<Vec<u8>>, PipelineError> {
        self.0
            .get_account(pubkey)
            .await
            .map(|opt| opt.map(|a| a.data))
            .map_err(|e| PipelineError::Temporary(format!("get_account: {e}")))
    }
}

#[tokio::test]
async fn chunks_written_via_the_real_program_read_back_through_inbox_retrieval_match_the_source_frames(
) {
    let blocks = two_block_batch();
    let compressed = channel::encode_stream(&blocks);
    pipeline::re_derive_and_check(&blocks, &compressed).expect("must re-derive cleanly");

    let max_frame_body_len = 256;
    let frames = channel::cut_frames(CHAIN_ID, BATCH_ID, &compressed, max_frame_body_len);
    assert!(frames.len() > 1, "fixture must need multiple frames");

    let program_id = Pubkey::new_unique();
    let settlement_program = Pubkey::new_unique();
    let payer = Keypair::new();

    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let (root, _) = zk_inbox_client::root_pda(&settlement_program, CHAIN_ID);
    pt.add_account(
        root,
        root_account_with_authority(CHAIN_ID, &payer.pubkey(), settlement_program),
    );
    pt.add_account(
        zk_inbox_client::cursor_pda(&program_id, &settlement_program, CHAIN_ID).0,
        cursor_account(program_id, CHAIN_ID, BATCH_ID),
    );
    pt.add_account(
        payer.pubkey(),
        Account {
            lamports: 100_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    let mut ctx = pt.start_with_context().await;

    let open_and_grow_ixs = zk_inbox_client::open_and_grow_batch_ixs(
        &program_id,
        &payer.pubkey(),
        CHAIN_ID,
        BATCH_ID,
        frames.len() as u32,
        &settlement_program,
    );
    send(&mut ctx, &open_and_grow_ixs, &payer).await;

    for frame in &frames {
        let payload = frame.to_bytes();
        // `ChunkPlan` is `Vec<Instruction>` — SIMD-0385 V1's one-tx-per-frame rule means every chunk fits
        // Open+Write+Seal+SealLeaf in one transaction; the old Combined/Split distinction no longer exists.
        let ixs: ChunkPlan = pipeline::plan_chunk(
            &program_id,
            &payer.pubkey(),
            &settlement_program,
            CHAIN_ID,
            BATCH_ID,
            frame.frame_no as u32,
            &payload,
        );
        send(&mut ctx, &ixs, &payer).await;
    }

    // FinalizeBatch — authority-gated, `payer` is the batch's own authority; may need
    // several calls for a large `step`, each needing that same signer. One call with `step: 0`
    // ("transform every remaining leaf, then combine") suffices at this fixture's size.
    send(
        &mut ctx,
        &[zk_inbox_client::finalize_batch_ix(
            &program_id,
            &payer.pubkey(),
            &settlement_program,
            CHAIN_ID,
            BATCH_ID,
            0,
        )],
        &payer,
    )
    .await;

    let batch_account = ctx
        .banks_client
        .get_account(
            zk_inbox_client::batch_pda(&program_id, &settlement_program, CHAIN_ID, BATCH_ID).0,
        )
        .await
        .unwrap()
        .expect("batch account exists");
    let decoded = zk_inbox_client::decode_batch_account(&batch_account.data).unwrap();
    assert!(
        decoded.finalized,
        "batch must be finalized before InboxRetrieval reads it"
    );

    // --- the actual seam under test: InboxRetrieval, over a real BanksClient-backed AccountReader ---
    let reader = BanksAccountReader(ctx.banks_client.clone());
    let mut retrieval = InboxRetrieval::new(reader, program_id, settlement_program);
    let batch_ref = rome_zk_derive::traversal::BatchRef {
        chain_id: CHAIN_ID,
        batch: BATCH_ID,
        open_slot: decoded.open_slot,
        open_unix_ts: decoded.open_unix_ts,
        expected_count: decoded.expected_count,
        root: decoded.root,
        forced_root: decoded.forced_root,
        acc: decoded.acc,
    };
    let chunk_bodies = retrieval.chunks(batch_ref).await.unwrap();
    assert_eq!(chunk_bodies.len(), frames.len());

    let read_back_frames: Vec<Frame> =
        rome_zk_derive::frame_queue::parse_frames(chunk_bodies, CHAIN_ID, BATCH_ID).unwrap();
    assert_eq!(
        read_back_frames, frames,
        "frames read back through InboxRetrieval must be byte-identical to what was sent"
    );

    let reassembled = channel::reassemble(&read_back_frames).unwrap();
    assert_eq!(reassembled, compressed);
    let decoded_blocks = channel::decode_stream(&reassembled).unwrap();
    assert_eq!(
        decoded_blocks, blocks,
        "must reconstruct the exact source block list"
    );
}

/// On the real program: a chunk account tampered *after* finalization (its
/// bytes no longer match what `FinalizeBatch` committed the accumulator's `root`/`acc` to) must be
/// rejected by [`InboxRetrieval::chunks`]. Chunk bodies used to be trusted outright once `sealed` +
/// `finalized`; the test requires the tampered body to be refused.
#[tokio::test]
async fn a_chunk_body_tampered_after_finalization_is_critical_acc_mismatch() {
    let blocks = two_block_batch();
    let compressed = channel::encode_stream(&blocks);
    let max_frame_body_len = 256;
    let frames = channel::cut_frames(CHAIN_ID, BATCH_ID, &compressed, max_frame_body_len);
    assert!(frames.len() > 1, "fixture must need multiple frames");

    let program_id = Pubkey::new_unique();
    let settlement_program = Pubkey::new_unique();
    let payer = Keypair::new();

    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let (root, _) = zk_inbox_client::root_pda(&settlement_program, CHAIN_ID);
    pt.add_account(
        root,
        root_account_with_authority(CHAIN_ID, &payer.pubkey(), settlement_program),
    );
    pt.add_account(
        zk_inbox_client::cursor_pda(&program_id, &settlement_program, CHAIN_ID).0,
        cursor_account(program_id, CHAIN_ID, BATCH_ID),
    );
    pt.add_account(
        payer.pubkey(),
        Account {
            lamports: 100_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    let mut ctx = pt.start_with_context().await;

    let open_and_grow_ixs = zk_inbox_client::open_and_grow_batch_ixs(
        &program_id,
        &payer.pubkey(),
        CHAIN_ID,
        BATCH_ID,
        frames.len() as u32,
        &settlement_program,
    );
    send(&mut ctx, &open_and_grow_ixs, &payer).await;

    for frame in &frames {
        let payload = frame.to_bytes();
        // `ChunkPlan` is `Vec<Instruction>` — SIMD-0385 V1's one-tx-per-frame rule means every chunk fits
        // Open+Write+Seal+SealLeaf in one transaction; the old Combined/Split distinction no longer exists.
        let ixs: ChunkPlan = pipeline::plan_chunk(
            &program_id,
            &payer.pubkey(),
            &settlement_program,
            CHAIN_ID,
            BATCH_ID,
            frame.frame_no as u32,
            &payload,
        );
        send(&mut ctx, &ixs, &payer).await;
    }

    send(
        &mut ctx,
        &[zk_inbox_client::finalize_batch_ix(
            &program_id,
            &payer.pubkey(),
            &settlement_program,
            CHAIN_ID,
            BATCH_ID,
            0,
        )],
        &payer,
    )
    .await;

    let batch_pda =
        zk_inbox_client::batch_pda(&program_id, &settlement_program, CHAIN_ID, BATCH_ID).0;
    let batch_account = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .unwrap();
    let decoded = zk_inbox_client::decode_batch_account(&batch_account.data).unwrap();
    assert!(decoded.finalized);

    // Tamper chunk 0's stored body *after* finalization — bypassing every on-chain instruction, exactly
    // as a corrupted/lying account read would look. The batch account's committed root/acc still
    // reflects the ORIGINAL bytes.
    let (chunk0_pda, _) =
        zk_inbox_client::chunk_pda(&program_id, &settlement_program, CHAIN_ID, BATCH_ID, 0);
    let mut chunk0 = ctx
        .banks_client
        .get_account(chunk0_pda)
        .await
        .unwrap()
        .expect("chunk 0 exists");
    let last = chunk0.data.len() - 1;
    chunk0.data[last] ^= 0xff; // flip a byte inside the sealed body
    ctx.set_account(
        &chunk0_pda,
        &solana_sdk::account::AccountSharedData::from(chunk0),
    );

    let reader = BanksAccountReader(ctx.banks_client.clone());
    let mut retrieval = InboxRetrieval::new(reader, program_id, settlement_program);
    let batch_ref = rome_zk_derive::traversal::BatchRef {
        chain_id: CHAIN_ID,
        batch: BATCH_ID,
        open_slot: decoded.open_slot,
        open_unix_ts: decoded.open_unix_ts,
        expected_count: decoded.expected_count,
        root: decoded.root,
        forced_root: decoded.forced_root,
        acc: decoded.acc,
    };
    let err = retrieval.chunks(batch_ref).await.unwrap_err();
    assert!(
        matches!(err, rome_zk_derive::PipelineError::Critical(_)),
        "a chunk tampered after finalization must be Critical (acc mismatch), got {err:?}"
    );
}
