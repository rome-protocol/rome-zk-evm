//! `solana-program-test` integration tests for `GrowBatch` and the per-chain `batch_cursor` PDA — the two
//! properties they close at the core: sequential, never-reused batch ids, and an `OpenBatch` that can
//! reach any `expected_count` via `GrowBatch` rather than failing above the single-CPI `create_account`
//! ceiling (an earlier stopgap capped it at 312 leaves; `GrowBatch` removes that cap).
//!
//! Loads the real, `cargo build-sbf`-compiled `.so` (not a native/builtin shortcut) so CU numbers reflect
//! real BPF execution — run `cargo build-sbf --manifest-path programs/zk-inbox/Cargo.toml` first.

use rome_zk_testkit::{cursor_account, prefund_pda, rent_exempt, root_account_with_authority};
use solana_program::{keccak, pubkey::Pubkey};
use solana_sdk::{
    account::{Account, AccountSharedData},
    instruction::InstructionError,
    signature::{Keypair, Signer},
    transaction::TransactionError,
};
use solana_system_interface::program as system_program;
use zk_inbox_client as client;

fn funded_account() -> Account {
    Account {
        lamports: 10_000_000_000,
        data: vec![],
        owner: system_program::id(),
        executable: false,
        rent_epoch: 0,
    }
}

fn chunk_body_hash(body: &[u8]) -> [u8; 32] {
    keccak::hashv(&[body]).to_bytes()
}

/// A sealed chunk account (chunk header, then `body`), owned by `program_id`.
fn chunk_account(
    program_id: Pubkey,
    authority: &Pubkey,
    chain_id: u64,
    batch: u64,
    idx: u32,
    body: &[u8],
) -> Account {
    let mut d = vec![0u8; client::CHUNK_HEADER_LEN + body.len()];
    d[0..4].copy_from_slice(&client::CHUNK_MAGIC.to_le_bytes());
    d[4..36].copy_from_slice(authority.as_ref());
    d[36..44].copy_from_slice(&chain_id.to_le_bytes());
    d[44..52].copy_from_slice(&batch.to_le_bytes());
    d[52..56].copy_from_slice(&idx.to_le_bytes());
    d[56..60].copy_from_slice(&(body.len() as u32).to_le_bytes());
    d[60] = 1; // sealed
    d[client::CHUNK_HEADER_LEN..].copy_from_slice(body);
    Account {
        lamports: rent_exempt(d.len()),
        data: d,
        owner: program_id,
        executable: false,
        rent_epoch: 0,
    }
}

/// Thin adapter over `rome_zk_testkit::send_measuring_cu` — this file additionally needs the raw log
/// messages (aggregate CU over every instruction in the transaction; see [`per_instruction_cu`] to split
/// that out for `program_id`'s own instructions).
async fn send(
    ctx: &mut solana_program_test::ProgramTestContext,
    ixs: &[solana_program::instruction::Instruction],
    payer: &Keypair,
    extra_signers: &[&Keypair],
) -> Result<(u64, Option<Vec<String>>), TransactionError> {
    let (result, cu, logs) =
        rome_zk_testkit::send_measuring_cu(ctx, ixs, payer, extra_signers).await;
    result.map(|()| (cu, logs))
}

/// Parses `"Program <program_id> consumed <N> of <M> compute units"` lines out of a transaction's log
/// messages, in order — one such line per **top-level** instruction that program executed in the
/// transaction (a nested CPI logs its own line under its own program id, which this filter excludes),
/// letting a single combined transaction's per-instruction CU be recovered even though
/// `compute_units_consumed` on the metadata is only the transaction-wide aggregate.
fn per_instruction_cu(log_messages: &Option<Vec<String>>, program_id: &Pubkey) -> Vec<u64> {
    let prefix = format!("Program {program_id} consumed ");
    log_messages
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter_map(|line| {
            let rest = line.strip_prefix(&prefix)?;
            let n = rest.split_whitespace().next()?;
            n.parse::<u64>().ok()
        })
        .collect()
}

const INBOX_ERR_CURSOR_MISMATCH: &str = "Custom(12)";
const INBOX_ERR_BATCH_NOT_GROWN: &str = "Custom(11)";
const INBOX_ERR_CURSOR_ALREADY_INITIALIZED: &str = "Custom(13)";

/// An attacker's keypair sending the 0-byte rent-exempt minimum to a predictable PDA before the real
// ---------------------------------------------------------------------------------------------
// 1. batch_cursor: sequential ids, never reused
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn open_batch_rejects_a_batch_id_that_is_not_the_cursors_next_batch() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let chain_id = 1;
    let authority = Keypair::new();
    let (root, _) = client::root_pda(&settlement_program, chain_id);
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &authority.pubkey(), settlement_program),
    );
    pt.add_account(
        client::cursor_pda(&program_id, chain_id).0,
        cursor_account(program_id, chain_id, 0),
    );
    pt.add_account(authority.pubkey(), funded_account());
    let mut ctx = pt.start_with_context().await;

    // next_batch is 0; opening at 1 (skipping ahead) must be rejected.
    let ix = client::open_batch_ix(
        &program_id,
        &authority.pubkey(),
        chain_id,
        1,
        5,
        &settlement_program,
    );
    let err = send(&mut ctx, &[ix], &authority, &[]).await.unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains(INBOX_ERR_CURSOR_MISMATCH),
        "expected CursorMismatch, got {msg}"
    );

    // Opening at the actual next_batch (0) must succeed, and increment the cursor to 1.
    let ix = client::open_batch_ix(
        &program_id,
        &authority.pubkey(),
        chain_id,
        0,
        5,
        &settlement_program,
    );
    send(&mut ctx, &[ix], &authority, &[]).await.unwrap();
    let (cursor_pda, _) = client::cursor_pda(&program_id, chain_id);
    let data = ctx
        .banks_client
        .get_account(cursor_pda)
        .await
        .unwrap()
        .unwrap()
        .data;
    assert_eq!(client::decode_batch_cursor(&data).unwrap().next_batch, 1);

    // Batch 0 again (reopening what was just opened) must now also be rejected — never reused.
    let ix = client::open_batch_ix(
        &program_id,
        &authority.pubkey(),
        chain_id,
        0,
        5,
        &settlement_program,
    );
    let err = send(&mut ctx, &[ix], &authority, &[]).await.unwrap_err();
    assert!(format!("{err:?}").contains(INBOX_ERR_CURSOR_MISMATCH));
}

#[tokio::test]
async fn abandon_batch_never_decrements_the_cursor_so_the_same_id_stays_rejected() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let chain_id = 2;
    let authority = Keypair::new();
    let (root, _) = client::root_pda(&settlement_program, chain_id);
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &authority.pubkey(), settlement_program),
    );
    pt.add_account(
        client::cursor_pda(&program_id, chain_id).0,
        cursor_account(program_id, chain_id, 0),
    );
    pt.add_account(authority.pubkey(), funded_account());
    let mut ctx = pt.start_with_context().await;

    let open_ix = client::open_batch_ix(
        &program_id,
        &authority.pubkey(),
        chain_id,
        0,
        3,
        &settlement_program,
    );
    send(&mut ctx, &[open_ix], &authority, &[]).await.unwrap();

    let abandon_ix = client::abandon_batch_ix(&program_id, &authority.pubkey(), chain_id, 0);
    send(&mut ctx, &[abandon_ix], &authority, &[])
        .await
        .unwrap();

    let (cursor_pda, _) = client::cursor_pda(&program_id, chain_id);
    let data = ctx
        .banks_client
        .get_account(cursor_pda)
        .await
        .unwrap()
        .unwrap()
        .data;
    assert_eq!(
        client::decode_batch_cursor(&data).unwrap().next_batch,
        1,
        "AbandonBatch must never decrement the cursor"
    );

    // Re-opening the abandoned id (0) must still be rejected — closed at the core: a
    // stale chunk PDA from the abandoned attempt can never be sealed into a later batch at this id,
    // because this id can never be opened again at all.
    let reopen_ix = client::open_batch_ix(
        &program_id,
        &authority.pubkey(),
        chain_id,
        0,
        3,
        &settlement_program,
    );
    let err = send(&mut ctx, &[reopen_ix], &authority, &[])
        .await
        .unwrap_err();
    assert!(format!("{err:?}").contains(INBOX_ERR_CURSOR_MISMATCH));

    // The real next id (1) is still open for business.
    let ix = client::open_batch_ix(
        &program_id,
        &authority.pubkey(),
        chain_id,
        1,
        3,
        &settlement_program,
    );
    send(&mut ctx, &[ix], &authority, &[]).await.unwrap();
}

#[tokio::test]
async fn init_batch_cursor_twice_is_rejected() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let chain_id = 3;
    let authority = Keypair::new();
    let (root, _) = client::root_pda(&settlement_program, chain_id);
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &authority.pubkey(), settlement_program),
    );
    pt.add_account(authority.pubkey(), funded_account());
    let mut ctx = pt.start_with_context().await;

    let ix = client::init_batch_cursor_ix(
        &program_id,
        &authority.pubkey(),
        chain_id,
        50,
        &settlement_program,
    );
    send(&mut ctx, &[ix], &authority, &[])
        .await
        .expect("first InitBatchCursor must succeed");

    let (cursor_pda, _) = client::cursor_pda(&program_id, chain_id);
    let data = ctx
        .banks_client
        .get_account(cursor_pda)
        .await
        .unwrap()
        .unwrap()
        .data;
    assert_eq!(client::decode_batch_cursor(&data).unwrap().next_batch, 50);

    let ix2 = client::init_batch_cursor_ix(
        &program_id,
        &authority.pubkey(),
        chain_id,
        999,
        &settlement_program,
    );
    let err = send(&mut ctx, &[ix2], &authority, &[])
        .await
        .expect_err("a second InitBatchCursor for the same chain must be rejected");
    assert!(
        matches!(err, TransactionError::InstructionError(_, _)),
        "expected an instruction error (AccountAlreadyInUse), got {err:?}"
    );
    // The cursor must be unchanged by the rejected second call.
    let data = ctx
        .banks_client
        .get_account(cursor_pda)
        .await
        .unwrap()
        .unwrap()
        .data;
    assert_eq!(client::decode_batch_cursor(&data).unwrap().next_batch, 50);
}

#[tokio::test]
async fn init_batch_cursor_rejects_a_non_authority_signer() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let chain_id = 4;
    let real_authority = Keypair::new();
    let impostor = Keypair::new();
    let (root, _) = client::root_pda(&settlement_program, chain_id);
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &real_authority.pubkey(), settlement_program),
    );
    pt.add_account(impostor.pubkey(), funded_account());
    let mut ctx = pt.start_with_context().await;

    let ix = client::init_batch_cursor_ix(
        &program_id,
        &impostor.pubkey(),
        chain_id,
        0,
        &settlement_program,
    );
    let err = send(&mut ctx, &[ix], &impostor, &[])
        .await
        .expect_err("a signer that is not the root's authority must not bootstrap the cursor");
    assert!(matches!(err, TransactionError::InstructionError(_, _)));
    let (cursor_pda, _) = client::cursor_pda(&program_id, chain_id);
    assert!(
        ctx.banks_client
            .get_account(cursor_pda)
            .await
            .unwrap()
            .is_none(),
        "the cursor must not have been created"
    );
}

// ---------------------------------------------------------------------------------------------
// 1b. create_or_adopt_pda: a pre-funded PDA must never permanently block creation
// ---------------------------------------------------------------------------------------------

/// The exact repro: an attacker sends `batch_pda(chain, next_batch)` the 0-byte rent-exempt minimum
/// before the real authority's `OpenBatch` lands. Before the fix this fails `Custom(0)`
/// (`AccountAlreadyInUse`) and, because the cursor only advances *inside* a successful `OpenBatch`, a
/// second attempt at the very next id then also fails — permanently, since the id is never reused
/// (`CursorMismatch`, `Custom(12)`) and the griefed id can never be revisited. Verified by hand before
/// the fix: `OpenBatch(0)` → `Custom(0)`; `OpenBatch(1)` → `Custom(12)`. This test pins the fixed
/// behavior: `OpenBatch` succeeds despite the prefund, adopting the donated lamports, and the resulting
/// state is exactly as if the PDA had never been touched.
#[tokio::test]
async fn open_batch_succeeds_even_when_an_attacker_prefunds_its_pda() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let chain_id = 500;
    let authority = Keypair::new();
    let (root, _) = client::root_pda(&settlement_program, chain_id);
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &authority.pubkey(), settlement_program),
    );
    pt.add_account(
        client::cursor_pda(&program_id, chain_id).0,
        cursor_account(program_id, chain_id, 0),
    );
    pt.add_account(authority.pubkey(), funded_account());
    let mut ctx = pt.start_with_context().await;

    let (batch_pda, _) = client::batch_pda(&program_id, chain_id, 0);
    prefund_pda(&mut ctx, batch_pda).await;
    let donated_lamports = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .expect("the prefund transfer must have created a system-owned account")
        .lamports;
    assert!(donated_lamports > 0);

    let expected_count = 5u32;
    let ix = client::open_batch_ix(
        &program_id,
        &authority.pubkey(),
        chain_id,
        0,
        expected_count,
        &settlement_program,
    );
    send(&mut ctx, &[ix], &authority, &[])
        .await
        .expect("OpenBatch must adopt a pre-funded PDA rather than fail AccountAlreadyInUse");

    let acct = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .expect("batch account must exist");
    assert_eq!(acct.owner, program_id);
    assert_eq!(
        acct.data.len(),
        rome_zk_layouts::batch::account_len(expected_count),
        "adopted account must be sized exactly as a freshly-created one would be"
    );
    assert!(
        acct.lamports >= donated_lamports,
        "the attacker's donated lamports must still be part of the account, not lost or refunded"
    );
    let decoded = client::decode_batch_account(&acct.data).unwrap();
    assert_eq!(decoded.chain_id, chain_id);
    assert_eq!(decoded.batch, 0);
    assert_eq!(decoded.expected_count, expected_count);
    assert_eq!(
        decoded.leaves_present, 0,
        "header must read as freshly opened, not carrying any garbage from the pre-funded account"
    );
    assert!(!decoded.finalized);
    assert_eq!(decoded.authority, authority.pubkey());

    // The cursor must have advanced exactly as it would for an unfunded PDA — the core liveness
    // property these tests protect.
    let (cursor_pda, _) = client::cursor_pda(&program_id, chain_id);
    let cursor_data = ctx
        .banks_client
        .get_account(cursor_pda)
        .await
        .unwrap()
        .unwrap()
        .data;
    assert_eq!(
        client::decode_batch_cursor(&cursor_data)
            .unwrap()
            .next_batch,
        1,
        "the cursor must advance past a griefed id exactly as it would past a clean one"
    );
}

/// The same repro on the cursor PDA itself: an attacker prefunds `batch_cursor(chain_id)`
/// before `InitBatchCursor` ever runs. Must adopt, not permanently block bootstrap — and, once adopted, a
/// second `InitBatchCursor` for the same chain must still be rejected (the "already initialised" guard,
/// checked by `owner == program_id && data_len != 0`, not by `create_account`'s own
/// error, which after this fix no longer distinguishes "already initialised" from "merely griefed").
#[tokio::test]
async fn init_batch_cursor_succeeds_even_when_an_attacker_prefunds_its_pda() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let chain_id = 501;
    let authority = Keypair::new();
    let (root, _) = client::root_pda(&settlement_program, chain_id);
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &authority.pubkey(), settlement_program),
    );
    pt.add_account(authority.pubkey(), funded_account());
    let mut ctx = pt.start_with_context().await;

    let (cursor_pda, _) = client::cursor_pda(&program_id, chain_id);
    prefund_pda(&mut ctx, cursor_pda).await;
    let donated_lamports = ctx
        .banks_client
        .get_account(cursor_pda)
        .await
        .unwrap()
        .expect("the prefund transfer must have created a system-owned account")
        .lamports;
    assert!(donated_lamports > 0);

    let ix = client::init_batch_cursor_ix(
        &program_id,
        &authority.pubkey(),
        chain_id,
        7,
        &settlement_program,
    );
    send(&mut ctx, &[ix], &authority, &[])
        .await
        .expect("InitBatchCursor must adopt a pre-funded PDA rather than fail AccountAlreadyInUse");

    let acct = ctx
        .banks_client
        .get_account(cursor_pda)
        .await
        .unwrap()
        .expect("cursor account must exist");
    assert_eq!(acct.owner, program_id);
    assert_eq!(acct.data.len(), rome_zk_layouts::cursor::LEN);
    assert!(acct.lamports >= donated_lamports);
    let decoded = client::decode_batch_cursor(&acct.data).unwrap();
    assert_eq!(decoded.chain_id, chain_id);
    assert_eq!(decoded.next_batch, 7);

    // A genuine second InitBatchCursor for this chain must still be rejected — adopting a griefed PDA
    // must not weaken the once-per-chain guard.
    let ix2 = client::init_batch_cursor_ix(
        &program_id,
        &authority.pubkey(),
        chain_id,
        999,
        &settlement_program,
    );
    let err = send(&mut ctx, &[ix2], &authority, &[])
        .await
        .expect_err("a second InitBatchCursor for an already-initialised chain must be rejected");
    assert!(
        format!("{err:?}").contains(INBOX_ERR_CURSOR_ALREADY_INITIALIZED),
        "expected CursorAlreadyInitialized, got {err:?}"
    );
    let data = ctx
        .banks_client
        .get_account(cursor_pda)
        .await
        .unwrap()
        .unwrap()
        .data;
    assert_eq!(
        client::decode_batch_cursor(&data).unwrap().next_batch,
        7,
        "the rejected second call must not have changed the cursor"
    );
}

// ---------------------------------------------------------------------------------------------
// 1c. Guard coverage for load_cursor's three checks and GrowBatch's owner/PDA/header checks (these five
// checks once survived mutation — `if false && …` — with no test noticing). Each test below defeats
// exactly one check while satisfying every other one. Verified by disabling each check in turn
// (`if false && …`, `.so` rebuilt) and confirming its own test goes red: (a), (b), (d), (e), (f) each go
// red *only* on their own check being disabled, in isolation. (c) — the cursor's owner check in
// `load_cursor` — is the one exception: `OpenBatch` always writes to the cursor on success (incrementing
// `next_batch`), so the runtime's own "instruction modified data of an account it does not own" rule
// independently rejects a foreign-owned cursor regardless of this check; disabling it alone does not
// turn this test red. The test stays (a foreign-owned cursor must be rejected is still a real,
// worth-pinning property) but the guard itself is redundant defense-in-depth here, not the sole barrier
// — noted rather than overclaimed.
// ---------------------------------------------------------------------------------------------

fn cursor_bytes(chain_id: u64, next_batch: u64) -> Vec<u8> {
    let mut d = vec![0u8; rome_zk_layouts::cursor::LEN];
    d[rome_zk_layouts::cursor::OFF_MAGIC..rome_zk_layouts::cursor::OFF_MAGIC + 4]
        .copy_from_slice(&rome_zk_layouts::cursor::MAGIC.to_le_bytes());
    d[rome_zk_layouts::cursor::OFF_VERSION] = rome_zk_layouts::cursor::VERSION;
    d[rome_zk_layouts::cursor::OFF_CHAIN_ID..rome_zk_layouts::cursor::OFF_CHAIN_ID + 8]
        .copy_from_slice(&chain_id.to_le_bytes());
    d[rome_zk_layouts::cursor::OFF_NEXT_BATCH..rome_zk_layouts::cursor::OFF_NEXT_BATCH + 8]
        .copy_from_slice(&next_batch.to_le_bytes());
    d
}

/// (a) `load_cursor`'s PDA/seeds check: a cursor-shaped account, correctly owned by the program and
/// holding valid cursor bytes for the right chain, but sitting at an address that is **not** the derived
/// `["batch_cursor", chain_id]` PDA — must be rejected before ever reaching the chain_id data check.
#[tokio::test]
async fn open_batch_rejects_a_cursor_shaped_account_at_a_non_pda_address() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let chain_id = 600;
    let authority = Keypair::new();
    let (root, _) = client::root_pda(&settlement_program, chain_id);
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &authority.pubkey(), settlement_program),
    );
    let not_a_pda = Pubkey::new_unique();
    pt.add_account(
        not_a_pda,
        Account {
            lamports: rent_exempt(rome_zk_layouts::cursor::LEN),
            data: cursor_bytes(chain_id, 0),
            owner: program_id,
            executable: false,
            rent_epoch: 0,
        },
    );
    pt.add_account(authority.pubkey(), funded_account());
    let mut ctx = pt.start_with_context().await;

    let mut ix = client::open_batch_ix(
        &program_id,
        &authority.pubkey(),
        chain_id,
        0,
        5,
        &settlement_program,
    );
    ix.accounts[3].pubkey = not_a_pda; // swap in the non-PDA cursor account
    let err = send(&mut ctx, &[ix], &authority, &[]).await.unwrap_err();
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::InvalidSeeds),
        "a cursor-shaped account at the wrong address must be rejected with InvalidSeeds (load_cursor's \
         PDA/seeds check): {err:?}"
    );
}

/// (b) `load_cursor`'s chain_id data check: the cursor account sits at the *correct* derived PDA for
/// `chain_id` and is owned correctly, but its own stored `chain_id` field says a different chain.
#[tokio::test]
async fn open_batch_rejects_a_cursor_pda_whose_stored_chain_id_does_not_match() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let chain_id = 601;
    let other_chain_id = 602;
    let authority = Keypair::new();
    let (root, _) = client::root_pda(&settlement_program, chain_id);
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &authority.pubkey(), settlement_program),
    );
    let (cursor_pda, _) = client::cursor_pda(&program_id, chain_id);
    pt.add_account(
        cursor_pda,
        Account {
            lamports: rent_exempt(rome_zk_layouts::cursor::LEN),
            // Right address for `chain_id`, but the data inside claims `other_chain_id`.
            data: cursor_bytes(other_chain_id, 0),
            owner: program_id,
            executable: false,
            rent_epoch: 0,
        },
    );
    pt.add_account(authority.pubkey(), funded_account());
    let mut ctx = pt.start_with_context().await;

    let ix = client::open_batch_ix(
        &program_id,
        &authority.pubkey(),
        chain_id,
        0,
        5,
        &settlement_program,
    );
    let err = send(&mut ctx, &[ix], &authority, &[]).await.unwrap_err();
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::InvalidAccountData),
        "a cursor whose stored chain_id disagrees with the requested chain must be rejected with \
         InvalidAccountData (load_cursor's chain_id data check): {err:?}"
    );
}

/// (c) `load_cursor`'s owner check: the cursor account sits at the correct PDA with correct-looking
/// data, but is owned by a different program entirely.
#[tokio::test]
async fn open_batch_rejects_a_cursor_pda_owned_by_a_foreign_program() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let foreign_program = Pubkey::new_unique();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let chain_id = 603;
    let authority = Keypair::new();
    let (root, _) = client::root_pda(&settlement_program, chain_id);
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &authority.pubkey(), settlement_program),
    );
    let (cursor_pda, _) = client::cursor_pda(&program_id, chain_id);
    pt.add_account(
        cursor_pda,
        Account {
            lamports: rent_exempt(rome_zk_layouts::cursor::LEN),
            data: cursor_bytes(chain_id, 0),
            owner: foreign_program,
            executable: false,
            rent_epoch: 0,
        },
    );
    pt.add_account(authority.pubkey(), funded_account());
    let mut ctx = pt.start_with_context().await;

    let ix = client::open_batch_ix(
        &program_id,
        &authority.pubkey(),
        chain_id,
        0,
        5,
        &settlement_program,
    );
    let err = send(&mut ctx, &[ix], &authority, &[]).await.unwrap_err();
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::IncorrectProgramId),
        "a cursor PDA owned by a foreign program must be rejected with IncorrectProgramId \
         (load_cursor's owner check, checked before seeds/data): {err:?}"
    );
}

/// (d) `GrowBatch`'s PDA/seeds check, isolated from the header check below it: the substituted account
/// is owned by the program and its *header* matches the instruction's `(chain_id, batch)` exactly (so
/// the header check alone could never catch this) — only its *address* is wrong.
#[tokio::test]
async fn grow_batch_rejects_a_well_formed_batch_account_at_the_wrong_address() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let chain_id = 604u64;
    let batch = 0u64;
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let payer = Keypair::new();
    let wrong_address = Pubkey::new_unique(); // never the real seeds(chain_id, batch) PDA
    let n = 5u32;
    let mut data = vec![0u8; rome_zk_layouts::batch::account_len(n)];
    data[rome_zk_layouts::batch::OFF_MAGIC..rome_zk_layouts::batch::OFF_MAGIC + 4]
        .copy_from_slice(&rome_zk_layouts::batch::MAGIC.to_le_bytes());
    data[rome_zk_layouts::batch::OFF_VERSION] = rome_zk_layouts::batch::VERSION;
    // Header claims exactly the instruction's own (chain_id, batch) — the header check alone would
    // accept this; only the address is wrong.
    data[rome_zk_layouts::batch::OFF_CHAIN_ID..rome_zk_layouts::batch::OFF_CHAIN_ID + 8]
        .copy_from_slice(&chain_id.to_le_bytes());
    data[rome_zk_layouts::batch::OFF_BATCH..rome_zk_layouts::batch::OFF_BATCH + 8]
        .copy_from_slice(&batch.to_le_bytes());
    data[rome_zk_layouts::batch::OFF_EXPECTED_COUNT
        ..rome_zk_layouts::batch::OFF_EXPECTED_COUNT + 4]
        .copy_from_slice(&n.to_le_bytes());
    pt.add_account(
        wrong_address,
        Account {
            lamports: rent_exempt(data.len()),
            data,
            owner: program_id,
            executable: false,
            rent_epoch: 0,
        },
    );
    pt.add_account(payer.pubkey(), funded_account());
    let mut ctx = pt.start_with_context().await;

    let mut ix = client::grow_batch_ix(&program_id, &payer.pubkey(), chain_id, batch);
    ix.accounts[1].pubkey = wrong_address;
    let err = send(&mut ctx, &[ix], &payer, &[]).await.unwrap_err();
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::Custom(8)), // BatchError::WrongBatchAccount
        "GrowBatch must reject a well-formed, correctly-owned batch account at the wrong address with \
         WrongBatchAccount (grow_batch_inner's PDA/seeds check): {err:?}"
    );
}

/// (e) `GrowBatch`'s header chain_id/batch check: a batch account at the exact address the instruction
/// args derive, correctly owned, but whose *stored* header claims a different `(chain_id, batch)` pair —
/// defense-in-depth beyond the address check alone.
#[tokio::test]
async fn grow_batch_rejects_a_batch_account_whose_stored_header_does_not_match_the_args() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let chain_id = 605u64;
    let batch = 0u64;
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let payer = Keypair::new();
    let (batch_pda, _) = client::batch_pda(&program_id, chain_id, batch);
    let n = 5u32;
    let mut data = vec![0u8; rome_zk_layouts::batch::account_len(n)];
    data[rome_zk_layouts::batch::OFF_MAGIC..rome_zk_layouts::batch::OFF_MAGIC + 4]
        .copy_from_slice(&rome_zk_layouts::batch::MAGIC.to_le_bytes());
    data[rome_zk_layouts::batch::OFF_VERSION] = rome_zk_layouts::batch::VERSION;
    // Address above is the real PDA for (chain_id, batch) — but the stored header claims a different
    // chain_id entirely.
    data[rome_zk_layouts::batch::OFF_CHAIN_ID..rome_zk_layouts::batch::OFF_CHAIN_ID + 8]
        .copy_from_slice(&(chain_id + 1).to_le_bytes());
    data[rome_zk_layouts::batch::OFF_BATCH..rome_zk_layouts::batch::OFF_BATCH + 8]
        .copy_from_slice(&batch.to_le_bytes());
    data[rome_zk_layouts::batch::OFF_EXPECTED_COUNT
        ..rome_zk_layouts::batch::OFF_EXPECTED_COUNT + 4]
        .copy_from_slice(&n.to_le_bytes());
    pt.add_account(
        batch_pda,
        Account {
            lamports: rent_exempt(data.len()),
            data,
            owner: program_id,
            executable: false,
            rent_epoch: 0,
        },
    );
    pt.add_account(payer.pubkey(), funded_account());
    let mut ctx = pt.start_with_context().await;

    let ix = client::grow_batch_ix(&program_id, &payer.pubkey(), chain_id, batch);
    let err = send(&mut ctx, &[ix], &payer, &[]).await.unwrap_err();
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::Custom(8)), // BatchError::WrongBatchAccount
        "GrowBatch must reject a batch account whose stored header disagrees with the instruction args \
         with WrongBatchAccount (grow_batch_inner's header chain_id/batch check): {err:?}"
    );
}

/// (f) `GrowBatch`'s owner check: a batch-shaped account at the right address, but owned by a foreign
/// program.
#[tokio::test]
async fn grow_batch_rejects_a_batch_account_owned_by_a_foreign_program() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let foreign_program = Pubkey::new_unique();
    let chain_id = 606u64;
    let batch = 0u64;
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let payer = Keypair::new();
    let (batch_pda, _) = client::batch_pda(&program_id, chain_id, batch);
    let n = 5u32;
    let mut data = vec![0u8; rome_zk_layouts::batch::account_len(n)];
    data[rome_zk_layouts::batch::OFF_MAGIC..rome_zk_layouts::batch::OFF_MAGIC + 4]
        .copy_from_slice(&rome_zk_layouts::batch::MAGIC.to_le_bytes());
    data[rome_zk_layouts::batch::OFF_VERSION] = rome_zk_layouts::batch::VERSION;
    data[rome_zk_layouts::batch::OFF_CHAIN_ID..rome_zk_layouts::batch::OFF_CHAIN_ID + 8]
        .copy_from_slice(&chain_id.to_le_bytes());
    data[rome_zk_layouts::batch::OFF_BATCH..rome_zk_layouts::batch::OFF_BATCH + 8]
        .copy_from_slice(&batch.to_le_bytes());
    data[rome_zk_layouts::batch::OFF_EXPECTED_COUNT
        ..rome_zk_layouts::batch::OFF_EXPECTED_COUNT + 4]
        .copy_from_slice(&n.to_le_bytes());
    pt.add_account(
        batch_pda,
        Account {
            lamports: rent_exempt(data.len()),
            data,
            owner: foreign_program,
            executable: false,
            rent_epoch: 0,
        },
    );
    pt.add_account(payer.pubkey(), funded_account());
    let mut ctx = pt.start_with_context().await;

    let ix = client::grow_batch_ix(&program_id, &payer.pubkey(), chain_id, batch);
    let err = send(&mut ctx, &[ix], &payer, &[]).await.unwrap_err();
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::IncorrectProgramId),
        "GrowBatch must reject a batch account owned by a foreign program with IncorrectProgramId \
         (grow_batch_inner's owner check, checked before seeds/header): {err:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// 2. GrowBatch
// ---------------------------------------------------------------------------------------------

/// `account_len(313) = 10,258 > MAX_PERMITTED_DATA_INCREASE (10,240)` — the exact case that failed on
/// devnet before `GrowBatch`: `OpenBatch` alone creates the account capped at 10,240 bytes; one `GrowBatch`
/// finishes it, and the batch then finalizes normally.
#[tokio::test]
async fn open_batch_313_leaves_opens_capped_then_grows_and_finalizes() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let chain_id = 313;
    let batch = 0u64;
    let n = 313u32;
    let authority = Keypair::new();
    let (root, _) = client::root_pda(&settlement_program, chain_id);
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &authority.pubkey(), settlement_program),
    );
    pt.add_account(
        client::cursor_pda(&program_id, chain_id).0,
        cursor_account(program_id, chain_id, batch),
    );
    pt.add_account(authority.pubkey(), funded_account());
    // Pre-seed 313 sealed chunk accounts (not the batch account itself — see the module doc: only the
    // batch account's creation/growth is under test here).
    let bodies: Vec<Vec<u8>> = (0..n).map(|i| i.to_le_bytes().to_vec()).collect();
    for (idx, body) in bodies.iter().enumerate() {
        let (cpda, _) = client::chunk_pda(&program_id, chain_id, batch, idx as u32);
        pt.add_account(
            cpda,
            chunk_account(
                program_id,
                &authority.pubkey(),
                chain_id,
                batch,
                idx as u32,
                body,
            ),
        );
    }
    let mut ctx = pt.start_with_context().await;

    let open_ix = client::open_batch_ix(
        &program_id,
        &authority.pubkey(),
        chain_id,
        batch,
        n,
        &settlement_program,
    );
    let (open_cu, _) = send(&mut ctx, &[open_ix], &authority, &[])
        .await
        .expect("OpenBatch must succeed even though it cannot reach full size in one CPI");
    eprintln!("OpenBatch(313 leaves, capped) consumed {open_cu} CU");

    let (batch_pda, _) = client::batch_pda(&program_id, chain_id, batch);
    let target = rome_zk_layouts::batch::account_len(n);
    let capped_len = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .unwrap()
        .data
        .len();
    assert_eq!(
        capped_len,
        client::MAX_PERMITTED_DATA_INCREASE,
        "OpenBatch must cap at MAX_PERMITTED_DATA_INCREASE, not reach account_len(313) = {target} in one CPI"
    );
    assert!(capped_len < target);

    // Fully grown before `GrowBatch`: chunk Open (defensive, no chunk pdas pre-created for this check —
    // reuse idx 0's account we seeded) and SealLeaf must both refuse (`BatchNotGrown`).
    let seal_before_grow = client::seal_leaf_ix(&program_id, chain_id, batch, 0);
    let err = send(&mut ctx, &[seal_before_grow], &authority, &[])
        .await
        .expect_err("SealLeaf must refuse before the batch is fully grown");
    assert!(format!("{err:?}").contains(INBOX_ERR_BATCH_NOT_GROWN));

    let grow_ix = client::grow_batch_ix(&program_id, &authority.pubkey(), chain_id, batch);
    let (grow_cu, _) = send(&mut ctx, &[grow_ix], &authority, &[])
        .await
        .expect("GrowBatch must succeed");
    eprintln!("GrowBatch(313 leaves, only call) consumed {grow_cu} CU");

    let grown_len = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .unwrap()
        .data
        .len();
    assert_eq!(
        grown_len, target,
        "one GrowBatch must reach account_len(313) exactly"
    );

    // Now SealLeaf every idx, FinalizeBatch, verify acc.
    for idx in 0..n {
        send(
            &mut ctx,
            &[client::seal_leaf_ix(&program_id, chain_id, batch, idx)],
            &authority,
            &[],
        )
        .await
        .unwrap_or_else(|e| panic!("SealLeaf({idx}) failed: {e:?}"));
    }
    let finalize_ix =
        client::finalize_batch_ix(&program_id, &authority.pubkey(), chain_id, batch, 0);
    let (finalize_cu, _) = send(&mut ctx, &[finalize_ix], &authority, &[])
        .await
        .expect(
            "FinalizeBatch must succeed once every leaf is sealed and the account is fully grown",
        );
    eprintln!("FinalizeBatch(313 leaves) consumed {finalize_cu} CU");

    let account = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .unwrap();
    let decoded = client::decode_batch_account(&account.data).unwrap();
    assert!(decoded.finalized);
    let chunk_hashes: Vec<[u8; 32]> = bodies.iter().map(|b| chunk_body_hash(b)).collect();
    let (_, _, expected_acc) =
        client::reference_commitment(chain_id, batch, decoded.open_slot, &chunk_hashes);
    assert_eq!(decoded.acc, expected_acc);
}

#[tokio::test]
async fn grow_batch_at_full_size_is_a_no_op() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let chain_id = 5;
    let batch = 0u64;
    let n = 5u32; // account_len(5) is far under MAX_PERMITTED_DATA_INCREASE: OpenBatch reaches full size alone.
    let authority = Keypair::new();
    let (root, _) = client::root_pda(&settlement_program, chain_id);
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &authority.pubkey(), settlement_program),
    );
    pt.add_account(
        client::cursor_pda(&program_id, chain_id).0,
        cursor_account(program_id, chain_id, batch),
    );
    pt.add_account(authority.pubkey(), funded_account());
    let mut ctx = pt.start_with_context().await;

    let open_ix = client::open_batch_ix(
        &program_id,
        &authority.pubkey(),
        chain_id,
        batch,
        n,
        &settlement_program,
    );
    send(&mut ctx, &[open_ix], &authority, &[]).await.unwrap();

    let (batch_pda, _) = client::batch_pda(&program_id, chain_id, batch);
    let len_before = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .unwrap()
        .data
        .len();
    assert_eq!(len_before, rome_zk_layouts::batch::account_len(n));
    let lamports_before = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .unwrap()
        .lamports;

    let grow_ix = client::grow_batch_ix(&program_id, &authority.pubkey(), chain_id, batch);
    send(&mut ctx, &[grow_ix], &authority, &[])
        .await
        .expect("GrowBatch at full size must be a no-op Ok, not an error");

    let account_after = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        account_after.data.len(),
        len_before,
        "must not shrink or grow further"
    );
    assert_eq!(
        account_after.lamports, lamports_before,
        "a no-op GrowBatch must not move any lamports"
    );
}

/// `GrowBatch` is permissionless — a random, unrelated payer may fund and grow a batch account they did
/// not open — and it can never exceed `account_len(expected_count)` no matter how many times it is
/// called.
#[tokio::test]
async fn grow_batch_by_a_random_payer_succeeds_and_cannot_exceed_account_len() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let chain_id = 6;
    let batch = 0u64;
    let n = 400u32; // account_len(400) > MAX_PERMITTED_DATA_INCREASE: needs exactly one GrowBatch.
    let authority = Keypair::new();
    let random_payer = Keypair::new();
    let (root, _) = client::root_pda(&settlement_program, chain_id);
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &authority.pubkey(), settlement_program),
    );
    pt.add_account(
        client::cursor_pda(&program_id, chain_id).0,
        cursor_account(program_id, chain_id, batch),
    );
    pt.add_account(authority.pubkey(), funded_account());
    pt.add_account(random_payer.pubkey(), funded_account());
    let mut ctx = pt.start_with_context().await;

    let open_ix = client::open_batch_ix(
        &program_id,
        &authority.pubkey(),
        chain_id,
        batch,
        n,
        &settlement_program,
    );
    send(&mut ctx, &[open_ix], &authority, &[]).await.unwrap();

    let (batch_pda, _) = client::batch_pda(&program_id, chain_id, batch);
    let target = rome_zk_layouts::batch::account_len(n);
    assert!(target > client::MAX_PERMITTED_DATA_INCREASE);

    let random_payer_lamports_before = ctx
        .banks_client
        .get_account(random_payer.pubkey())
        .await
        .unwrap()
        .unwrap()
        .lamports;

    // random_payer, never the batch/chain authority, funds and sends GrowBatch.
    let grow_ix = client::grow_batch_ix(&program_id, &random_payer.pubkey(), chain_id, batch);
    send(&mut ctx, &[grow_ix], &random_payer, &[])
        .await
        .expect("GrowBatch must be permissionless — any payer may call it");

    let grown = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        grown.data.len(),
        target,
        "must reach exactly account_len(400)"
    );

    let random_payer_lamports_after = ctx
        .banks_client
        .get_account(random_payer.pubkey())
        .await
        .unwrap()
        .unwrap()
        .lamports;
    assert!(
        random_payer_lamports_after < random_payer_lamports_before,
        "the random payer must actually have funded the rent top-up"
    );

    // A further GrowBatch (still by the random payer) is a no-op — never exceeds account_len.
    let lamports_before_noop = random_payer_lamports_after;
    let grow_again_ix = client::grow_batch_ix(&program_id, &random_payer.pubkey(), chain_id, batch);
    send(&mut ctx, &[grow_again_ix], &random_payer, &[])
        .await
        .expect("a second GrowBatch at full size must still be a no-op Ok");
    let account_after = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        account_after.data.len(),
        target,
        "must never exceed account_len(400)"
    );
    let random_payer_lamports_final = ctx
        .banks_client
        .get_account(random_payer.pubkey())
        .await
        .unwrap()
        .unwrap()
        .lamports;
    // Sending any transaction costs the base per-signature fee regardless of what the program does; a
    // no-op GrowBatch must not additionally transfer any *rent* on top of that (a real rent top-up for
    // 400 leaves would be orders of magnitude larger than one signature fee).
    let base_fee_only = lamports_before_noop - random_payer_lamports_final;
    assert!(
        base_fee_only <= 5_000,
        "a no-op GrowBatch must not charge the payer any rent, only got a lamport delta of {base_fee_only}"
    );
}

/// Chunk `Open` and `FinalizeBatch` (like `SealLeaf`, covered above) must also refuse before the batch
/// account is fully grown — `BatchNotGrown`, fail-closed.
#[tokio::test]
async fn chunk_open_and_finalize_batch_reject_before_the_batch_is_fully_grown() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let chain_id = 7;
    let batch = 0u64;
    let n = 400u32;
    let authority = Keypair::new();
    let (root, _) = client::root_pda(&settlement_program, chain_id);
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &authority.pubkey(), settlement_program),
    );
    pt.add_account(
        client::cursor_pda(&program_id, chain_id).0,
        cursor_account(program_id, chain_id, batch),
    );
    pt.add_account(authority.pubkey(), funded_account());
    let mut ctx = pt.start_with_context().await;

    let open_batch_ix = client::open_batch_ix(
        &program_id,
        &authority.pubkey(),
        chain_id,
        batch,
        n,
        &settlement_program,
    );
    send(&mut ctx, &[open_batch_ix], &authority, &[])
        .await
        .unwrap();

    let open_chunk_ix =
        client::open_chunk_ix(&program_id, &authority.pubkey(), chain_id, batch, 0, 8);
    let err = send(&mut ctx, &[open_chunk_ix], &authority, &[])
        .await
        .expect_err("chunk Open must refuse before the batch is fully grown");
    assert!(format!("{err:?}").contains(INBOX_ERR_BATCH_NOT_GROWN));

    let finalize_ix =
        client::finalize_batch_ix(&program_id, &authority.pubkey(), chain_id, batch, 0);
    let err = send(&mut ctx, &[finalize_ix], &authority, &[])
        .await
        .expect_err("FinalizeBatch must refuse before the batch is fully grown");
    assert!(format!("{err:?}").contains(INBOX_ERR_BATCH_NOT_GROWN));
}

// ---------------------------------------------------------------------------------------------
// 3. 900-leaf finalize through a real OpenBatch + GrowBatch(s) in one transaction (not by seeding
//    the account).
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn open_and_grow_batch_900_leaves_in_one_transaction_finalizes_within_cu_budget() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let chain_id = 900_900;
    let batch = 0u64;
    let n = 900u32;
    let authority = Keypair::new();
    let (root, _) = client::root_pda(&settlement_program, chain_id);
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &authority.pubkey(), settlement_program),
    );
    pt.add_account(
        client::cursor_pda(&program_id, chain_id).0,
        cursor_account(program_id, chain_id, batch),
    );
    pt.add_account(authority.pubkey(), funded_account());
    let mut ctx = pt.start_with_context().await;

    // --- OpenBatch + however many GrowBatch calls 900 leaves need, sent as ONE transaction ---
    let ixs = client::open_and_grow_batch_ixs(
        &program_id,
        &authority.pubkey(),
        chain_id,
        batch,
        n,
        &settlement_program,
    );
    assert_eq!(
        ixs.len(),
        3,
        "account_len(900) = 29,123: OpenBatch (-> 10,240) + two GrowBatch (-> 20,480 -> 29,123)"
    );
    let (total_cu, log_messages) = send(&mut ctx, &ixs, &authority, &[])
        .await
        .expect("OpenBatch + GrowBatch x2 must all succeed in one transaction");
    let per_ix_cu = per_instruction_cu(&log_messages, &program_id);
    assert_eq!(
        per_ix_cu.len(),
        3,
        "one CU figure per top-level instruction"
    );
    eprintln!(
        "OpenBatch+2xGrowBatch(900 leaves) in one tx: total {total_cu} CU; OpenBatch {} CU, GrowBatch#1 {} CU, GrowBatch#2 {} CU",
        per_ix_cu[0], per_ix_cu[1], per_ix_cu[2]
    );

    let (batch_pda, _) = client::batch_pda(&program_id, chain_id, batch);
    let target = rome_zk_layouts::batch::account_len(n);
    let account = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        account.data.len(),
        target,
        "the batch account must reach account_len(900) through the real Open+Grow path"
    );

    // --- fill in the 900 leaves' presence bitmap + hashes on the just-created (real-size) account,
    // preserving everything OpenBatch itself wrote (header fields) — this test's subject is the
    // Open+Grow+Finalize CU path, not the (separately covered) SealLeaf CU path, so this avoids 900 real
    // SealLeaf transactions while still proving the account GrowBatch produced is the right size (a
    // wrong size here would make this very write panic on an out-of-bounds slice). ---
    let bodies: Vec<[u8; 4]> = (0..n).map(|i| i.to_le_bytes()).collect();
    let chunk_hashes: Vec<[u8; 32]> = bodies.iter().map(|b| chunk_body_hash(b)).collect();
    let mut data = account.data.clone();
    let bitmap_off = rome_zk_layouts::batch::HEADER_LEN;
    let leaves_off = rome_zk_layouts::batch::leaves_offset(n);
    for i in 0..n as usize {
        data[bitmap_off + i / 8] |= 1 << (i % 8);
        let slot = leaves_off + 32 * i;
        data[slot..slot + 32].copy_from_slice(&chunk_hashes[i]);
    }
    data[rome_zk_layouts::batch::OFF_LEAVES_PRESENT
        ..rome_zk_layouts::batch::OFF_LEAVES_PRESENT + 4]
        .copy_from_slice(&n.to_le_bytes());
    let mut new_account = account.clone();
    new_account.data = data;
    ctx.set_account(&batch_pda, &AccountSharedData::from(new_account));

    // --- FinalizeBatch ---
    let finalize_ix =
        client::finalize_batch_ix(&program_id, &authority.pubkey(), chain_id, batch, 0);
    let (finalize_cu, _) = send(&mut ctx, &[finalize_ix], &authority, &[])
        .await
        .expect("FinalizeBatch must succeed on the real Open+Grow-produced account");
    eprintln!("FinalizeBatch(900 leaves, one call) consumed {finalize_cu} CU (budget: 600,000)");
    assert!(
        finalize_cu <= 600_000,
        "900-leaf finalize consumed {finalize_cu} CU, over the 0.6M budget"
    );

    let account = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .unwrap();
    let decoded = client::decode_batch_account(&account.data).unwrap();
    assert!(decoded.finalized);
    let (_, _, expected_acc) =
        client::reference_commitment(chain_id, batch, decoded.open_slot, &chunk_hashes);
    assert_eq!(decoded.acc, expected_acc);
}
