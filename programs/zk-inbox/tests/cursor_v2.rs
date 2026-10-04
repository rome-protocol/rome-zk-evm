//! `solana-program-test` tests for the v2 batch cursor: `InitBatchCursor` creates it as a 69-byte v2
//! account, and `CloseBatch` (which now takes the cursor as its fourth, writable account) moves
//! `deposit_final` up to a closed v3 batch's `deposit_to`.
//!
//! Loads the real `cargo build-sbf` `.so`, so the CU figures printed are real BPF numbers — run
//! `cargo build-sbf --manifest-path programs/zk-inbox/Cargo.toml` first.

use rome_zk_layouts::{
    batch::{account_len_for, write_header, write_header_v3, BatchDeposit, BatchFields},
    cursor::{self, CursorDeposit, CursorFields},
    root,
};
use rome_zk_testkit::{
    cursor_account_for, funded_keypair, rent_exempt, root_account_with_authority,
};
use solana_program::{instruction::InstructionError, pubkey::Pubkey};
use solana_sdk::{
    account::Account,
    signature::{Keypair, Signer},
    transaction::TransactionError,
};
use solana_system_interface::program as system_program;
use zk_inbox_client as client;

const CHAIN_ID: u64 = 7;

async fn send(
    ctx: &mut solana_program_test::ProgramTestContext,
    ixs: &[solana_program::instruction::Instruction],
    payer: &Keypair,
) -> Result<u64, TransactionError> {
    let (result, cu, _logs) = rome_zk_testkit::send_measuring_cu(ctx, ixs, payer, &[]).await;
    result.map(|()| cu)
}

fn hex32(s: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
    }
    out
}

/// A root account whose `head_final_batch` is `final_batch`, owned by `owner`.
fn final_root(chain_id: u64, authority: &Pubkey, final_batch: u64, owner: Pubkey) -> Account {
    let mut a = root_account_with_authority(chain_id, authority, owner);
    a.data[root::OFF_HEAD_FINAL_BATCH..root::OFF_HEAD_FINAL_BATCH + 8]
        .copy_from_slice(&final_batch.to_le_bytes());
    a
}

/// A finalized batch account with two leaves. `deposit_to` `Some(to)` makes it a v3 batch whose range is
/// `[0, to)`; `None` makes it a v2 batch.
fn batch_account(
    program_id: Pubkey,
    settlement_program: Pubkey,
    authority: Pubkey,
    batch: u64,
    deposit_to: Option<u64>,
) -> Account {
    let fields = BatchFields {
        chain_id: CHAIN_ID,
        batch,
        open_slot: 7,
        expected_count: 2,
        leaves_present: 2,
        finalized: true,
        settlement_program: settlement_program.to_bytes(),
        authority: authority.to_bytes(),
        root: [1; 32],
        forced_root: [2; 32],
        acc: [3; 32],
        finalize_cursor: 0,
        open_unix_ts: 1_700_000_000,
        deposit: deposit_to.map(|to| BatchDeposit {
            from: 0,
            to,
            hash_from: [4; 32],
            hash_to: [5; 32],
        }),
    };
    let (version, header): (u8, Vec<u8>) = match deposit_to {
        Some(_) => (3, write_header_v3(&fields).unwrap().to_vec()),
        None => (2, write_header(&fields).to_vec()),
    };
    let mut d = vec![0u8; account_len_for(version, 2).unwrap()];
    d[..header.len()].copy_from_slice(&header);
    if version == 2 {
        // In a real v2 batch the bytes where a v3 header keeps deposit_to (218..226) are bitmap and leaf
        // bytes, never zero. Make them non-zero here so a v2 batch misread as v3 would move deposit_final.
        d[218..226].fill(0xFF);
    }
    Account {
        lamports: rent_exempt(d.len()),
        data: d,
        owner: program_id,
        executable: false,
        rent_epoch: 0,
    }
}

fn funded(lamports: u64) -> Account {
    Account {
        lamports,
        data: vec![],
        owner: system_program::id(),
        executable: false,
        rent_epoch: 0,
    }
}

fn v2_cursor(program_id: Pubkey, next_batch: u64, deposit_final: u64) -> Account {
    let mut a = cursor_account_for(2, program_id, CHAIN_ID, next_batch);
    a.data[cursor::OFF_DEPOSIT_FINAL..cursor::OFF_DEPOSIT_FINAL + 8]
        .copy_from_slice(&deposit_final.to_le_bytes());
    a
}

async fn deposit_final_of(
    ctx: &mut solana_program_test::ProgramTestContext,
    key: Pubkey,
) -> (usize, Option<u64>) {
    let data = ctx
        .banks_client
        .get_account(key)
        .await
        .unwrap()
        .unwrap()
        .data;
    let f = cursor::read(&data).unwrap();
    (data.len(), f.deposit.map(|d| d.final_))
}

/// The cursor `InitBatchCursor` creates is the 69-byte v2 account: the literal bytes below, with
/// `deposit_next` 0, `deposit_hash` the queue seed hash for settlement program `[0x33; 32]` on chain 7
/// (the golden value in `rome_zk_layouts::deposit`'s tests) and `deposit_final` 0.
#[tokio::test]
async fn init_batch_cursor_creates_a_v2_cursor_with_the_queue_seed_hash() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = Pubkey::new_from_array([0x33; 32]);
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let authority = funded_keypair();
    pt.add_account(
        client::root_pda(&settlement_program, CHAIN_ID).0,
        root_account_with_authority(CHAIN_ID, &authority.pubkey(), settlement_program),
    );
    pt.add_account(authority.pubkey(), funded(10_000_000_000));
    let mut ctx = pt.start_with_context().await;

    let ix = client::init_batch_cursor_ix(
        &program_id,
        &authority.pubkey(),
        CHAIN_ID,
        42,
        &settlement_program,
    );
    let cu = send(&mut ctx, &[ix], &authority)
        .await
        .expect("InitBatchCursor");
    eprintln!("InitBatchCursor (v2 cursor) consumed {cu} CU");

    let cursor_key = client::cursor_pda(&program_id, &settlement_program, CHAIN_ID).0;
    let acct = ctx
        .banks_client
        .get_account(cursor_key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(acct.owner, program_id);
    #[rustfmt::skip]
    let expected: [u8; 69] = [
        // magic 'ZKBC'
        0x43, 0x42, 0x4b, 0x5a,
        // version 2
        2,
        // chain_id = 7
        7, 0, 0, 0, 0, 0, 0, 0,
        // next_batch = 42
        42, 0, 0, 0, 0, 0, 0, 0,
        // deposit_next = 0
        0, 0, 0, 0, 0, 0, 0, 0,
        // deposit_hash = h_0([0x33; 32], 7)
        0x7b, 0x62, 0x29, 0x7f, 0x72, 0xfe, 0x3a, 0x90, 0xee, 0xca, 0xde, 0x8f, 0x81, 0xe0, 0x19, 0x7b,
        0x8f, 0xef, 0x15, 0xf5, 0xb4, 0xc5, 0xa1, 0x09, 0x30, 0xe1, 0xfd, 0x3b, 0xff, 0xd7, 0x77, 0xf3,
        // deposit_final = 0
        0, 0, 0, 0, 0, 0, 0, 0,
    ];
    // The expected hash row is typed out above; check it against the same golden in hex too.
    assert_eq!(
        &expected[29..61],
        &hex32("7b62297f72fe3a90eecade8f81e0197b8fef15f5b4c5a10930e1fd3bffd777f3")[..]
    );
    assert_eq!(acct.data, expected.to_vec());
    assert_eq!(
        cursor::read(&acct.data).unwrap(),
        CursorFields {
            chain_id: CHAIN_ID,
            next_batch: 42,
            deposit: Some(CursorDeposit {
                next: 0,
                hash: hex32("7b62297f72fe3a90eecade8f81e0197b8fef15f5b4c5a10930e1fd3bffd777f3"),
                final_: 0,
            }),
        }
    );
}

struct World {
    ctx: solana_program_test::ProgramTestContext,
    program_id: Pubkey,
    settlement_program: Pubkey,
    authority: Keypair,
    cursor_key: Pubkey,
}

/// Batches 2 and 3 (v3, ranges `[0, 4)` and `[0, 7)`), batch 5 (a v2 batch), a final root at batch 5,
/// and `cursor` as the chain's cursor account (`None` leaves it absent).
async fn world(cursor: Option<fn(Pubkey) -> Account>) -> World {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let authority = funded_keypair();
    for (batch, to) in [(2, Some(4)), (3, Some(7)), (5, None)] {
        pt.add_account(
            client::batch_pda(&program_id, &settlement_program, CHAIN_ID, batch).0,
            batch_account(
                program_id,
                settlement_program,
                authority.pubkey(),
                batch,
                to,
            ),
        );
    }
    pt.add_account(
        client::root_pda(&settlement_program, CHAIN_ID).0,
        final_root(CHAIN_ID, &authority.pubkey(), 5, settlement_program),
    );
    pt.add_account(authority.pubkey(), funded(10_000_000_000));
    let cursor_key = client::cursor_pda(&program_id, &settlement_program, CHAIN_ID).0;
    if let Some(make) = cursor {
        pt.add_account(cursor_key, make(program_id));
    }
    World {
        ctx: pt.start_with_context().await,
        program_id,
        settlement_program,
        authority,
        cursor_key,
    }
}

impl World {
    async fn close(&mut self, batch: u64) -> Result<u64, TransactionError> {
        let ix = client::close_batch_ix(
            &self.program_id,
            &self.authority.pubkey(),
            &self.settlement_program,
            CHAIN_ID,
            batch,
        );
        let payer = self.authority.insecure_clone();
        send(&mut self.ctx, &[ix], &payer).await
    }
}

fn cursor_v2_at_6(program_id: Pubkey) -> Account {
    v2_cursor(program_id, 6, 0)
}

fn cursor_v1_at_6(program_id: Pubkey) -> Account {
    cursor_account_for(1, program_id, CHAIN_ID, 6)
}

/// Closing batch 3 and then batch 2 keeps 3's value; the figure never goes down.
#[tokio::test]
async fn close_batch_advances_deposit_final_and_never_lowers_it() {
    let mut w = world(Some(cursor_v2_at_6)).await;
    let key = w.cursor_key;

    let cu = w.close(3).await.expect("close batch 3");
    eprintln!("CloseBatch (v3 batch, v2 cursor, advances deposit_final) consumed {cu} CU");
    assert_eq!(deposit_final_of(&mut w.ctx, key).await, (69, Some(7)));

    let cu = w.close(2).await.expect("close batch 2");
    eprintln!("CloseBatch (v3 batch, v2 cursor, no advance) consumed {cu} CU");
    assert_eq!(
        deposit_final_of(&mut w.ctx, key).await,
        (69, Some(7)),
        "closing the lower batch 2 after batch 3 must keep 3's value"
    );
}

/// Closing in order moves the figure up each time, and only the one field changes.
#[tokio::test]
async fn close_batch_in_order_moves_only_deposit_final() {
    let mut w = world(Some(cursor_v2_at_6)).await;
    let key = w.cursor_key;
    let before = w.ctx.banks_client.get_account(key).await.unwrap().unwrap();

    w.close(2).await.expect("close batch 2");
    assert_eq!(deposit_final_of(&mut w.ctx, key).await.1, Some(4));
    w.close(3).await.expect("close batch 3");
    let after = w.ctx.banks_client.get_account(key).await.unwrap().unwrap();
    assert_eq!(
        cursor::read(&after.data).unwrap().deposit.unwrap().final_,
        7
    );

    let mut expected = before.data.clone();
    expected[cursor::OFF_DEPOSIT_FINAL..cursor::OFF_DEPOSIT_FINAL + 8]
        .copy_from_slice(&7u64.to_le_bytes());
    assert_eq!(after.data, expected, "no other cursor byte may change");
    assert_eq!(after.lamports, before.lamports);
}

/// A v2 batch credited nothing, so it leaves `deposit_final` alone.
#[tokio::test]
async fn close_batch_of_a_v2_batch_leaves_deposit_final_alone() {
    let mut w = world(Some(|p| v2_cursor(p, 6, 9))).await;
    let key = w.cursor_key;
    w.close(5).await.expect("close batch 5");
    assert_eq!(deposit_final_of(&mut w.ctx, key).await, (69, Some(9)));
}

/// A v1 cursor is left alone (same bytes, same length) when a v3 batch closes.
#[tokio::test]
async fn close_batch_leaves_a_v1_cursor_alone() {
    let mut w = world(Some(cursor_v1_at_6)).await;
    let key = w.cursor_key;
    let before = w.ctx.banks_client.get_account(key).await.unwrap().unwrap();
    w.close(3).await.expect("close batch 3 over a v1 cursor");
    let after = w.ctx.banks_client.get_account(key).await.unwrap().unwrap();
    assert_eq!(after.data, before.data);
    assert_eq!(after.data.len(), 21);
}

/// A missing cursor is refused, and so is a cursor of another chain, one under another owner, and a
/// call that leaves the account out. The batch survives each refusal.
// `NotEnoughAccountKeys` is what the program's account iterator returns when an account is left out.
#[allow(deprecated)]
#[tokio::test]
async fn close_batch_refuses_a_missing_or_wrong_cursor() {
    // Missing: nothing at the cursor address.
    let mut w = world(None).await;
    let err = w.close(3).await.unwrap_err();
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::IncorrectProgramId)
    );
    let batch_key = client::batch_pda(&w.program_id, &w.settlement_program, CHAIN_ID, 3).0;
    assert!(w
        .ctx
        .banks_client
        .get_account(batch_key)
        .await
        .unwrap()
        .is_some_and(|a| a.lamports > 0));

    // Wrong address: a cursor-shaped account that is not this chain's PDA.
    let mut w = world(Some(cursor_v2_at_6)).await;
    let other = Pubkey::new_unique();
    w.ctx
        .set_account(&other, &v2_cursor(w.program_id, 6, 0).into());
    let mut ix = client::close_batch_ix(
        &w.program_id,
        &w.authority.pubkey(),
        &w.settlement_program,
        CHAIN_ID,
        3,
    );
    ix.accounts[3].pubkey = other;
    let payer = w.authority.insecure_clone();
    let err = send(&mut w.ctx, &[ix.clone()], &payer).await.unwrap_err();
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::InvalidSeeds)
    );

    // Wrong owner: the right address, held by another program.
    let mut w = world(Some(|_| v2_cursor(Pubkey::new_unique(), 6, 0))).await;
    let err = w.close(3).await.unwrap_err();
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::IncorrectProgramId)
    );

    // Left out.
    let mut w = world(Some(cursor_v2_at_6)).await;
    let mut ix = client::close_batch_ix(
        &w.program_id,
        &w.authority.pubkey(),
        &w.settlement_program,
        CHAIN_ID,
        3,
    );
    ix.accounts.truncate(3);
    let payer = w.authority.insecure_clone();
    let err = send(&mut w.ctx, &[ix], &payer).await.unwrap_err();
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::NotEnoughAccountKeys)
    );
    assert_eq!(deposit_final_of(&mut w.ctx, w.cursor_key).await.1, Some(0));
}

/// A cursor named read-only is refused, since `CloseBatch` takes it writable.
#[tokio::test]
async fn close_batch_refuses_a_read_only_cursor() {
    let mut w = world(Some(cursor_v2_at_6)).await;
    let mut ix = client::close_batch_ix(
        &w.program_id,
        &w.authority.pubkey(),
        &w.settlement_program,
        CHAIN_ID,
        3,
    );
    ix.accounts[3].is_writable = false;
    let payer = w.authority.insecure_clone();
    let err = send(&mut w.ctx, &[ix], &payer).await.unwrap_err();
    assert!(matches!(err, TransactionError::InstructionError(0, _)));
    assert_eq!(deposit_final_of(&mut w.ctx, w.cursor_key).await.1, Some(0));
}
