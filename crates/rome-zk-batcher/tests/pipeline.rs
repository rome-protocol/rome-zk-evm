//! `solana-program-test` integration test for the full batcher pipeline: a 3-block batch
//! with ~40 txs -> `OpenBatch` -> one `Open`+`Write`+`Seal`+`SealLeaf` transaction per frame
//! (driven here via `BanksClient`, not `Sender`/RPC — see the module doc below for why) ->
//! `FinalizeBatch` -> `acc` on chain == `zk_inbox_client::reference_commitment(...)`. CU per instruction
//! printed.
//!
//! Loads the real, `cargo build-sbf`-compiled `zk_inbox.so` (not a native/builtin shortcut), following
//! `programs/zk-inbox/tests/accumulator.rs`'s own `ProgramTest` setup pattern (its helpers are copied
//! here rather than imported — that file is a test binary in a different crate, not a library).
//!
//! ## Why BanksClient here, not `pipeline::send_frame`/`send_all_frames`/`finalize_and_verify`
//! Those functions are written against the [`rome_zk_batcher::sender::Sender`] trait (an async RPC-shaped
//! seam) and a live `solana_client::nonblocking::rpc_client::RpcClient` for account reads —
//! `solana-program-test`'s `BanksClient` is a different transport with no `RpcClient`-compatible surface,
//! so this test instead drives the *exact same instruction-planning function*
//! (`rome_zk_batcher::pipeline::plan_chunk`, made `pub` for this) through `BanksClient` directly. This
//! proves the instructions the pipeline builds actually execute correctly on the real program and that
//! `verify_acc`'s reference commitment matches on-chain state; it does not exercise the resubmit-on-
//! blockhash-expiry or N-in-flight concurrency logic in `sender.rs`/`send_all_frames` — those are exercised
//! by the devnet measurement instead.

use rome_zk_batcher::channel::{self, Block};
use rome_zk_batcher::pipeline;
use rome_zk_testkit::{cursor_account, root_account_with_authority};
use solana_program::pubkey::Pubkey;
use solana_sdk::{
    account::Account,
    signature::{Keypair, Signer},
    transaction::TransactionError,
};
use solana_system_interface::program as system_program;

/// Thin adapter over `rome_zk_testkit::send_measuring_cu` — this file's callers only ever want the CU
/// figure on success.
async fn send(
    ctx: &mut solana_program_test::ProgramTestContext,
    ixs: &[solana_program::instruction::Instruction],
    payer: &Keypair,
) -> Result<u64, TransactionError> {
    let (result, cu, _logs) = rome_zk_testkit::send_measuring_cu(ctx, ixs, payer, &[]).await;
    result.map(|()| cu)
}

fn tx_bytes(sender_tag: u64, nonce: u64, len: usize) -> alloy_primitives::Bytes {
    // Not a real signed EVM tx — the pipeline/program layer treats tx bytes as opaque, so a deterministic
    // fixture is enough here (source.rs's own tests already cover reading *real* signed txs off the log).
    let mut bytes = vec![0u8; len];
    bytes[0..8].copy_from_slice(&sender_tag.to_le_bytes());
    bytes[8..16].copy_from_slice(&nonce.to_le_bytes());
    alloy_primitives::Bytes::from(bytes)
}

fn three_block_batch_with_about_40_txs() -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut tx_seq = 0u64;
    for (block_number, tx_count) in [(100u64, 14usize), (101, 13), (102, 13)] {
        let txs = (0..tx_count)
            .map(|i| {
                let t = tx_bytes(block_number, i as u64, 40 + (tx_seq as usize % 30));
                tx_seq += 1;
                t
            })
            .collect();
        blocks.push(Block {
            number: block_number,
            timestamp: 1_757_000_000 + block_number,
            gas_limit: 100_000_000,
            txs,
        });
    }
    blocks
}

#[tokio::test]
async fn full_pipeline_three_block_batch_finalizes_and_acc_matches_reference() {
    let blocks = three_block_batch_with_about_40_txs();
    let total_txs: usize = blocks.iter().map(|b| b.txs.len()).sum();
    assert!(
        (35..=45).contains(&total_txs),
        "fixture must be ~40 txs across 3 blocks (got {total_txs})"
    );

    // Encode, then re-derive-before-send exactly as the pipeline would before spending any fee.
    let compressed = channel::encode_stream(&blocks);
    pipeline::re_derive_and_check(&blocks, &compressed)
        .expect("must re-derive cleanly before sending");

    // A small max_frame_body_len so this ~40-tx batch actually needs several frames (a
    // block spanning several frames), not just one.
    let max_frame_body_len = 256;
    let chain_id = 200_198u64;
    let batch_id = 7u64;
    let frames = channel::cut_frames(chain_id, batch_id, &compressed, max_frame_body_len);
    assert!(frames.len() > 1, "fixture must need multiple frames");

    let program_id = Pubkey::new_unique();
    let settlement_program = Pubkey::new_unique();
    let payer = Keypair::new();

    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let (root, _) = zk_inbox_client::root_pda(&settlement_program, chain_id);
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &payer.pubkey(), settlement_program),
    );
    pt.add_account(
        zk_inbox_client::cursor_pda(&program_id, chain_id).0,
        cursor_account(program_id, chain_id, batch_id),
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

    let mut cu_report: Vec<(&'static str, u64)> = Vec::new();

    // --- OpenBatch (+ GrowBatch if expected_count needs it), one transaction ---
    let open_and_grow_ixs = zk_inbox_client::open_and_grow_batch_ixs(
        &program_id,
        &payer.pubkey(),
        chain_id,
        batch_id,
        frames.len() as u32,
        &settlement_program,
    );
    let cu = send(&mut ctx, &open_and_grow_ixs, &payer)
        .await
        .expect("OpenBatch(+Grow)");
    cu_report.push(("OpenBatch(+Grow)", cu));

    // --- every frame: one Open+Write+Seal+SealLeaf transaction (always this shape,
    // the withdrawn 3-hop split plan is gone) via the pipeline's own planning function ---
    for frame in &frames {
        let payload = frame.to_bytes();
        let ixs = pipeline::plan_chunk(
            &program_id,
            &payer.pubkey(),
            chain_id,
            batch_id,
            frame.frame_no as u32,
            &payload,
        );
        let cu = send(&mut ctx, &ixs, &payer).await.unwrap_or_else(|e| {
            panic!(
                "chunk[{}] Open+Write+Seal+SealLeaf failed: {e:?}",
                frame.frame_no
            )
        });
        cu_report.push(("chunk Open+Write+Seal+SealLeaf", cu));
    }

    // --- leaves_present == expected_count before FinalizeBatch ---
    let (batch_pda, _) = zk_inbox_client::batch_pda(&program_id, chain_id, batch_id);
    let account = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .expect("batch account exists");
    let decoded =
        zk_inbox_client::decode_batch_account(&account.data).expect("decode batch account");
    assert_eq!(decoded.leaves_present, frames.len() as u32);
    assert!(
        !decoded.finalized,
        "must not be finalized before FinalizeBatch"
    );

    // --- FinalizeBatch ---
    let finalize_ix =
        zk_inbox_client::finalize_batch_ix(&program_id, &payer.pubkey(), chain_id, batch_id, 0);
    let cu = send(&mut ctx, &[finalize_ix], &payer)
        .await
        .expect("FinalizeBatch");
    cu_report.push(("FinalizeBatch", cu));

    let account = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .expect("batch account exists");
    let decoded =
        zk_inbox_client::decode_batch_account(&account.data).expect("decode batch account");
    assert!(
        decoded.finalized,
        "batch must be finalized after FinalizeBatch"
    );

    // --- acc on chain == reference_commitment ---
    let reference_acc =
        pipeline::verify_acc(&decoded, &frames).expect("on-chain acc must match reference");
    assert_eq!(decoded.acc, reference_acc);

    // --- CU per instruction, printed ---
    println!(
        "=== CU per instruction (3-block batch, {} txs, {} frames) ===",
        total_txs,
        frames.len()
    );
    for (label, cu) in &cu_report {
        println!("{label}: {cu} CU");
    }
    let total_cu: u64 = cu_report.iter().map(|(_, cu)| cu).sum();
    println!("total CU across the whole batch: {total_cu}");
    println!(
        "compressed bytes: {} (rlp {} -> zstd-19 {}), frames: {}, bytes/tx: {:.1}",
        compressed.len(),
        alloy_rlp::encode(blocks.clone()).len(),
        compressed.len(),
        frames.len(),
        compressed.len() as f64 / total_txs as f64
    );

    // Re-decode every frame body back to blocks/txs and confirm it matches the source one more time,
    // proving the on-chain-sealed bytes (not just the in-memory `compressed` buffer) round-trip. Each
    // chunk PDA holds `[chunk account header (64 B)] || [frame header (19 B)] || [frame body]` — the
    // pipeline wrote `frame.to_bytes()` (frame header + body) as the chunk's own body via `Write`.
    let mut ordered = frames.clone();
    ordered.sort_by_key(|f| f.frame_no);
    let mut reassembled_from_chain = Vec::new();
    for frame in &ordered {
        let (chunk_pda, _) =
            zk_inbox_client::chunk_pda(&program_id, chain_id, batch_id, frame.frame_no as u32);
        let account = ctx
            .banks_client
            .get_account(chunk_pda)
            .await
            .unwrap()
            .expect("chunk account exists");
        let stored_chunk_body = &account.data[zk_inbox_client::CHUNK_HEADER_LEN..];
        assert_eq!(
            stored_chunk_body,
            frame.to_bytes().as_slice(),
            "on-chain chunk body must be exactly the frame (header + body) this test sent"
        );
        let on_chain_frame =
            channel::Frame::from_bytes(stored_chunk_body).expect("decode on-chain frame");
        assert_eq!(&on_chain_frame, frame);
        reassembled_from_chain.extend_from_slice(&on_chain_frame.body);
    }
    let decoded_from_chain =
        channel::decode_stream(&reassembled_from_chain).expect("decode on-chain bytes");
    assert_eq!(
        decoded_from_chain, blocks,
        "on-chain DA must decode back to exactly the source blocks"
    );
}
