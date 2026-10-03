//! `solana-program-test` integration test for the one-V1-tx-per-frame plan (it replaced the withdrawn
//! "hop collapse" 3-hop split this file used to test): the real program accepts
//! `plan_chunk`'s single `Open`+`Write`(whole body)+`Seal`+`SealLeaf` instruction list for both a
//! design-max (3,681-B body) frame and a small frame — the chunk seals, `SealLeaf` accepts it, and the
//! finalized batch's on-chain `acc` matches `zk_inbox_client::reference_commitment` computed off-chain
//! from the same frame.
//!
//! This test builds a legacy `Transaction` for `BanksClient`/`ProgramTest`, as it always has; the harness moved to
//! `solana-program-test = 4.3.0` and that choice was left alone. So this test proves the *instruction set* executes
//! correctly through a legacy `Transaction` (same account list, same instruction data, same order `plan_chunk`
//! builds) — the real wire-size proof that this exact instruction list, compiled as a **V1** transaction, fits the
//! 4,096-byte SIMD-0385 envelope lives in `sender.rs`'s own
//! `design_frame_v1_tx_fits_4096_and_carries_both_header_limits` test (which signs a real V1 message with a real
//! `Keypair` and measures the real serialized wire size — this file cannot do that against program-test's own
//! transport, only against on-chain execution).
//!
//! Loads the real, `cargo build-sbf`-compiled `zk_inbox.so` (not a native/builtin shortcut), following
//! `tests/pipeline.rs`'s own `ProgramTest` setup pattern (helpers copied here rather than imported — see
//! that file's module doc for why).

use rome_zk_batcher::channel::Frame;
use rome_zk_batcher::pipeline;
use rome_zk_testkit::{cursor_account, root_account_with_authority};
use solana_program::pubkey::Pubkey;
use solana_sdk::{
    account::Account,
    signature::{Keypair, Signer},
    transaction::TransactionError,
};
use solana_system_interface::program as system_program;

/// Thin adapter over `rome_zk_testkit::send_measuring_cu` — this file never needs the CU figure.
async fn send(
    ctx: &mut solana_program_test::ProgramTestContext,
    ixs: &[solana_program::instruction::Instruction],
    payer: &Keypair,
) -> Result<(), TransactionError> {
    rome_zk_testkit::send_measuring_cu(ctx, ixs, payer, &[])
        .await
        .0
}

/// Common scaffolding every test in this file needs: a fresh `ProgramTestContext` with the real program,
/// a funded payer that is also the chain's root authority, a `batch_cursor` at `next_batch = 0`, and
/// `OpenBatch(+Grow)` already sent for `batch 0` sized for exactly one frame (`expected_count = 1`).
struct Scaffold {
    ctx: solana_program_test::ProgramTestContext,
    program_id: Pubkey,
    settlement_program: Pubkey,
    payer: Keypair,
    chain_id: u64,
    batch: u64,
}

async fn scaffold_with_one_frame_opened() -> Scaffold {
    let program_id = Pubkey::new_unique();
    let settlement_program = Pubkey::new_unique();
    let payer = Keypair::new();
    let chain_id = 200_198u64;
    let batch = 0u64;

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
        zk_inbox_client::cursor_pda(&program_id, &settlement_program, chain_id).0,
        cursor_account(program_id, chain_id, batch),
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
        chain_id,
        batch,
        1, // expected_count: exactly one frame in this batch
        &settlement_program,
    );
    send(&mut ctx, &open_and_grow_ixs, &payer)
        .await
        .expect("OpenBatch(+Grow)");

    Scaffold {
        ctx,
        program_id,
        settlement_program,
        payer,
        chain_id,
        batch,
    }
}

/// Finalizes `batch` (assumes every leaf is already sealed) and returns the decoded, finalized batch
/// account.
async fn finalize_and_decode(
    ctx: &mut solana_program_test::ProgramTestContext,
    program_id: Pubkey,
    settlement_program: Pubkey,
    payer: &Keypair,
    chain_id: u64,
    batch: u64,
) -> zk_inbox_client::BatchAccount {
    let (batch_pda, _) =
        zk_inbox_client::batch_pda(&program_id, &settlement_program, chain_id, batch);
    let account = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .expect("batch account exists");
    let decoded =
        zk_inbox_client::decode_batch_account(&account.data).expect("decode batch account");
    assert_eq!(
        decoded.leaves_present, decoded.expected_count,
        "every leaf must be sealed before FinalizeBatch"
    );
    assert!(!decoded.finalized, "must not already be finalized");

    let finalize_ix = zk_inbox_client::finalize_batch_ix(
        &program_id,
        &payer.pubkey(),
        &settlement_program,
        chain_id,
        batch,
        0,
    );
    send(ctx, &[finalize_ix], payer)
        .await
        .expect("FinalizeBatch");

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
    decoded
}

/// Runs one frame's whole `plan_chunk` instruction list (`Open`+`Write`+`Seal`+`SealLeaf`;
/// always exactly this shape now) as a single transaction, finalizes the batch, and asserts the
/// on-chain `acc` matches the off-chain reference commitment and the stored chunk body is byte-identical
/// to what was sent.
async fn run_one_frame_and_check(body: Vec<u8>) {
    let Scaffold {
        mut ctx,
        program_id,
        settlement_program,
        payer,
        chain_id,
        batch,
    } = scaffold_with_one_frame_opened().await;

    let frame = Frame {
        channel_id: rome_zk_batcher::channel::channel_id(chain_id, batch),
        frame_no: 0,
        is_last: true,
        body,
    };
    let payload = frame.to_bytes();
    let ixs = pipeline::plan_chunk(
        &program_id,
        &payer.pubkey(),
        &settlement_program,
        chain_id,
        batch,
        0,
        &payload,
    );
    assert_eq!(
        ixs.len(),
        4,
        "always exactly Open + Write + Seal + SealLeaf, one transaction"
    );

    send(&mut ctx, &ixs, &payer)
        .await
        .expect("Open+Write+Seal+SealLeaf transaction");

    let decoded = finalize_and_decode(
        &mut ctx,
        program_id,
        settlement_program,
        &payer,
        chain_id,
        batch,
    )
    .await;

    let chunk_hash = solana_program::keccak::hashv(&[&payload]).to_bytes();
    let (_, _, reference_acc) =
        zk_inbox_client::reference_commitment(chain_id, batch, decoded.open_slot, &[chunk_hash]);
    assert_eq!(
        decoded.acc, reference_acc,
        "on-chain acc must match the off-chain reference commitment"
    );

    let (chunk_pda, _) =
        zk_inbox_client::chunk_pda(&program_id, &settlement_program, chain_id, batch, 0);
    let chunk_account = ctx
        .banks_client
        .get_account(chunk_pda)
        .await
        .unwrap()
        .expect("chunk account exists");
    let stored_body = &chunk_account.data[zk_inbox_client::CHUNK_HEADER_LEN..];
    assert_eq!(
        stored_body,
        payload.as_slice(),
        "on-chain chunk body must be exactly the frame this test sent"
    );
}

/// The design's own max frame body (3,681 B) — the shape `sender.rs`'s own V1 wire-size test proves fits
/// one V1 transaction (4,052 B measured, ≤ 4,096) — executes correctly end to end on the real program as
/// a single `Open`+`Write`+`Seal`+`SealLeaf` transaction.
#[tokio::test]
async fn a_design_max_frame_in_one_transaction_finalizes_and_acc_matches() {
    run_one_frame_and_check(vec![0xABu8; 3_681]).await;
}

/// A small frame — same single-transaction shape, far under the design max — must behave identically:
/// no special-casing by size (there is no combined-vs-split decision any more).
#[tokio::test]
async fn a_small_frame_in_one_transaction_finalizes_and_acc_matches() {
    run_one_frame_and_check(vec![0xCDu8; 100]).await;
}
