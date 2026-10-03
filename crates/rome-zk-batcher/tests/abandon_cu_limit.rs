//! `AbandonBatch` and chunk `Close` are cheap and
//! O(1) in a batch's own leaf count — `programs/zk-inbox/src/batch.rs::abandon_batch_inner` only ever
//! reads the batch account's fixed 210-byte header (`rome_zk_layouts::batch::read`), never its
//! `leaf_hashes`/bitmap — but that was reasoned from the code, not measured. This pins the real CU cost,
//! against the real `cargo build-sbf`-compiled `zk_inbox.so`, on a 900-leaf-sized batch account (Tiber's
//! own real size: batch 4005 has 899 chunks), and against `chunk_compute_unit_limit`
//! (40,000, from the deploy config), which `pipeline::abandon_open_batches_in_pending_window` reuses for
//! both `AbandonBatch` and packed chunk `Close` sends.
//!
//! Follows `tests/finalize_cu_limit.rs`'s own `ProgramTest` + `BanksClient` pattern.

use rome_zk_batcher::pipeline::{BatchTarget, CLOSE_IXS_PER_TX};
use rome_zk_testkit::{cursor_account, root_account_with_authority};
use solana_program::instruction::Instruction;
use solana_sdk::{
    account::Account,
    signature::{Keypair, Signer},
    transaction::TransactionError,
};
use solana_system_interface::program as system_program;

/// Thin adapter over `rome_zk_testkit::send_measuring_cu` — this file's callers only ever want the CU
/// figure on success.
async fn send_measuring_cu(
    ctx: &mut solana_program_test::ProgramTestContext,
    ixs: &[Instruction],
    payer: &Keypair,
) -> Result<u64, TransactionError> {
    let (result, cu, _logs) = rome_zk_testkit::send_measuring_cu(ctx, ixs, payer, &[]).await;
    result.map(|()| cu)
}

const CHUNK_COMPUTE_UNIT_LIMIT: u64 = 40_000; // `config::default_chunk_compute_unit_limit`, Tiber's own value.

/// `AbandonBatch` on a 900-leaf-sized batch account (Tiber's real batch 4005 has 899 chunks) must stay
/// well under `chunk_compute_unit_limit` — `abandon_batch_inner` only ever reads the account's fixed
/// 210-byte header, never proportional to `expected_count`.
#[tokio::test]
async fn abandon_batch_cu_on_a_900_leaf_account_fits_the_chunk_compute_unit_limit() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        false,
    );
    let chain_id = 200_198u64;
    let authority = Keypair::new();
    let (root, _) = zk_inbox_client::root_pda(&settlement_program, chain_id);
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &authority.pubkey(), settlement_program),
    );
    pt.add_account(
        zk_inbox_client::cursor_pda(&program_id, &settlement_program, chain_id).0,
        cursor_account(program_id, chain_id, 0),
    );
    pt.add_account(
        authority.pubkey(),
        Account {
            lamports: 50_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    let mut ctx = pt.start_with_context().await;

    let target = BatchTarget {
        program_id,
        settlement_program,
        payer: authority.pubkey(),
        chain_id,
        batch: 0,
    };
    let open_and_grow_ixs = zk_inbox_client::open_and_grow_batch_ixs(
        &program_id,
        &authority.pubkey(),
        chain_id,
        target.batch,
        900,
        &settlement_program,
    );
    for ix in &open_and_grow_ixs {
        send_measuring_cu(&mut ctx, std::slice::from_ref(ix), &authority)
            .await
            .expect("OpenBatch/GrowBatch must succeed");
    }
    let (batch_pda, _) =
        zk_inbox_client::batch_pda(&program_id, &settlement_program, chain_id, target.batch);
    let account = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(account.data.len(), rome_zk_layouts::batch::account_len(900));

    let abandon_ix = zk_inbox_client::abandon_batch_ix(
        &program_id,
        &authority.pubkey(),
        &settlement_program,
        chain_id,
        target.batch,
    );
    let cu = send_measuring_cu(&mut ctx, std::slice::from_ref(&abandon_ix), &authority)
        .await
        .expect("AbandonBatch on a 900-leaf account must succeed");
    eprintln!("AbandonBatch CU (900-leaf account): {cu}");
    assert!(
        cu <= CHUNK_COMPUTE_UNIT_LIMIT,
        "AbandonBatch measured {cu} CU on a 900-leaf account, over chunk_compute_unit_limit \
         ({CHUNK_COMPUTE_UNIT_LIMIT}) — abandon_open_batches_in_pending_window sends it under this same tuning"
    );

    assert!(
        ctx.banks_client
            .get_account(batch_pda)
            .await
            .unwrap()
            .is_none(),
        "the batch account must be gone after AbandonBatch"
    );
}

/// A single chunk `Close` (the cheap, authority-only path once the batch account is already gone — exactly
/// what `pipeline::abandon_open_batches_in_pending_window` does for each of a half-written batch's chunk PDAs)
/// and a transaction packing `CLOSE_IXS_PER_TX` of them together (the packed form this file measures): a single
/// one fits under `chunk_compute_unit_limit`, and the packed group fits under one `chunk_compute_unit_limit` per
/// Close (the budget the group asks for).
#[tokio::test]
async fn packed_chunk_close_fits_the_chunk_compute_unit_limit() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        false,
    );
    let chain_id = 200_199u64;
    let authority = Keypair::new();
    let (root, _) = zk_inbox_client::root_pda(&settlement_program, chain_id);
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &authority.pubkey(), settlement_program),
    );
    pt.add_account(
        zk_inbox_client::cursor_pda(&program_id, &settlement_program, chain_id).0,
        cursor_account(program_id, chain_id, 0),
    );
    pt.add_account(
        authority.pubkey(),
        Account {
            lamports: 50_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    let mut ctx = pt.start_with_context().await;

    // Open a small batch (enough chunks for one single-Close measurement plus one full packed group),
    // open every chunk (tiny, unwritten — Close never reads a chunk's body), then AbandonBatch so the
    // chunks reach the cheap "batch pda absent, authority alone may close" path.
    let batch = 0u64;
    let chunk_count = 1 + CLOSE_IXS_PER_TX as u32;
    let open_and_grow_ixs = zk_inbox_client::open_and_grow_batch_ixs(
        &program_id,
        &authority.pubkey(),
        chain_id,
        batch,
        chunk_count,
        &settlement_program,
    );
    for ix in &open_and_grow_ixs {
        send_measuring_cu(&mut ctx, std::slice::from_ref(ix), &authority)
            .await
            .expect("OpenBatch/GrowBatch must succeed");
    }
    for idx in 0..chunk_count {
        let open_ix = zk_inbox_client::open_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            8,
        );
        send_measuring_cu(&mut ctx, std::slice::from_ref(&open_ix), &authority)
            .await
            .unwrap_or_else(|e| panic!("Open chunk {idx} must succeed: {e:?}"));
    }
    let abandon_ix = zk_inbox_client::abandon_batch_ix(
        &program_id,
        &authority.pubkey(),
        &settlement_program,
        chain_id,
        batch,
    );
    send_measuring_cu(&mut ctx, std::slice::from_ref(&abandon_ix), &authority)
        .await
        .expect("AbandonBatch must succeed");

    // Single Close (idx 0) — the per-instruction baseline.
    let close0 = zk_inbox_client::close_chunk_ix(
        &program_id,
        &authority.pubkey(),
        &settlement_program,
        chain_id,
        batch,
        0,
    );
    let single_cu = send_measuring_cu(&mut ctx, std::slice::from_ref(&close0), &authority)
        .await
        .expect("a single Close on an abandoned batch's chunk must succeed");
    eprintln!("single chunk Close CU: {single_cu}");
    assert!(
        single_cu <= CHUNK_COMPUTE_UNIT_LIMIT,
        "a single Close measured {single_cu} CU, over chunk_compute_unit_limit ({CHUNK_COMPUTE_UNIT_LIMIT})"
    );

    // A packed transaction of CLOSE_IXS_PER_TX Close instructions (idx 1..=CLOSE_IXS_PER_TX) — exactly the
    // shape `abandon_open_batches_in_pending_window` now sends.
    let packed_ixs: Vec<Instruction> = (1..=CLOSE_IXS_PER_TX as u32)
        .map(|idx| {
            zk_inbox_client::close_chunk_ix(
                &program_id,
                &authority.pubkey(),
                &settlement_program,
                chain_id,
                batch,
                idx,
            )
        })
        .collect();
    let packed_cu = send_measuring_cu(&mut ctx, &packed_ixs, &authority)
        .await
        .expect("a packed transaction of CLOSE_IXS_PER_TX Close instructions must succeed");
    eprintln!("packed {CLOSE_IXS_PER_TX}-Close CU: {packed_cu}");
    // `abandon_open_batches_in_pending_window` gives a packed group one `chunk_compute_unit_limit` per Close.
    let group_limit = CHUNK_COMPUTE_UNIT_LIMIT * CLOSE_IXS_PER_TX as u64;
    assert!(
        packed_cu <= group_limit,
        "packing {CLOSE_IXS_PER_TX} Close instructions measured {packed_cu} CU, over the group's budget of \
         {CLOSE_IXS_PER_TX} x chunk_compute_unit_limit ({group_limit}) — abandon_open_batches_in_pending_window \
         sends packed groups under this tuning"
    );
}
