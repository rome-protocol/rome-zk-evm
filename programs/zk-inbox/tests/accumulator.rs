//! solana-program-test integration tests for the batch accumulator and the chunk lane's
//! new finality-gated `Close`. Loads the real, `cargo build-sbf`-compiled `.so` (not a native/builtin
//! shortcut) so `compute_units_consumed` reflects real BPF execution, not a host-native approximation —
//! run `cargo build-sbf --manifest-path programs/zk-inbox/Cargo.toml` before `cargo test -p zk-inbox`.

use rome_zk_layouts::batch::{account_len_for, header_len, leaves_offset_for};
use rome_zk_testkit::{
    cursor_account, cursor_account_for, funded_keypair, prefund_pda, rent_exempt,
    root_account_with_authority,
};
use solana_program::{keccak, pubkey::Pubkey};
use solana_sdk::{
    account::Account,
    instruction::InstructionError,
    signature::{Keypair, Signer},
    transaction::TransactionError,
};
use solana_system_interface::program as system_program;
use zk_inbox_client as client;

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
    Account {
        lamports: rent_exempt(d.len()),
        data: d,
        owner: program_id,
        executable: false,
        rent_epoch: 0,
    }
}

/// Builds a batch account's raw bytes directly (bypassing OpenBatch/SealLeaf) for tests that only care
/// about FinalizeBatch/CloseBatch behavior given a known leaf state.
struct BatchFixture {
    chain_id: u64,
    batch: u64,
    open_slot: u64,
    expected_count: u32,
    settlement_program: Pubkey,
    authority: Pubkey,
}

impl BatchFixture {
    fn bytes(
        &self,
        leaves_present: u32,
        leaf_hashes: &[[u8; 32]],
        sealed_idx: &[u32],
        finalized: bool,
    ) -> Vec<u8> {
        let n = self.expected_count;
        let v = zk_inbox::batch::VERSION;
        let mut d = vec![0u8; account_len_for(v, n).unwrap()];
        d[0..4].copy_from_slice(&zk_inbox::batch::MAGIC.to_le_bytes());
        d[4] = v;
        d[5..13].copy_from_slice(&self.chain_id.to_le_bytes());
        d[13..21].copy_from_slice(&self.batch.to_le_bytes());
        d[21..29].copy_from_slice(&self.open_slot.to_le_bytes());
        d[29..33].copy_from_slice(&n.to_le_bytes());
        d[33..37].copy_from_slice(&leaves_present.to_le_bytes());
        d[37] = finalized as u8;
        d[38..70].copy_from_slice(self.settlement_program.as_ref());
        d[70..102].copy_from_slice(self.authority.as_ref());
        let lo = leaves_offset_for(v, n).unwrap();
        let bitmap_off = header_len(v).unwrap();
        for &idx in sealed_idx {
            d[bitmap_off + (idx as usize) / 8] |= 1 << (idx % 8);
        }
        for (i, h) in leaf_hashes.iter().enumerate() {
            let slot = lo + 32 * i;
            d[slot..slot + 32].copy_from_slice(h);
        }
        d
    }

    fn account(
        &self,
        program_id: Pubkey,
        leaves_present: u32,
        leaf_hashes: &[[u8; 32]],
        sealed_idx: &[u32],
        finalized: bool,
    ) -> Account {
        let data = self.bytes(leaves_present, leaf_hashes, sealed_idx, finalized);
        Account {
            lamports: rent_exempt(data.len()),
            data,
            owner: program_id,
            executable: false,
            rent_epoch: 0,
        }
    }
}

fn root_account(chain_id: u64, head_final_batch: u64, owner: Pubkey) -> Account {
    // Forward-compatible layout: zk-settlement's *deployed* root account is only 88 bytes
    // today (no head_final_batch field yet), so this is a hand-built stand-in for what it will look like,
    // as documented in batch.rs's `root_view` module.
    let mut d = vec![0u8; 202];
    d[0..4].copy_from_slice(&0x5a4b_5254u32.to_le_bytes()); // 'ZKRT'
    d[4..12].copy_from_slice(&chain_id.to_le_bytes());
    d[186..194].copy_from_slice(&head_final_batch.to_le_bytes());
    Account {
        lamports: rent_exempt(d.len()),
        data: d,
        owner,
        executable: false,
        rent_epoch: 0,
    }
}

fn chunk_body_hash(body: &[u8]) -> [u8; 32] {
    keccak::hashv(&[body]).to_bytes()
}

/// Thin adapter over `rome_zk_testkit::send_measuring_cu` (the one send-and-observe helper this
/// workspace's test suites share) — this file's callers only ever want the CU figure on success.
async fn send(
    ctx: &mut solana_program_test::ProgramTestContext,
    ixs: &[solana_program::instruction::Instruction],
    payer: &Keypair,
    extra_signers: &[&Keypair],
) -> Result<u64, TransactionError> {
    let (result, cu, _logs) =
        rome_zk_testkit::send_measuring_cu(ctx, ixs, payer, extra_signers).await;
    result.map(|()| cu)
}

// ---------------------------------------------------------------------------------------------
// 1. OpenBatch
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn open_batch_creates_pda_with_clock_slot_and_correct_size() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let chain_id = 1;
    let authority = funded_keypair();
    let (root, _) = client::root_pda(&settlement_program, chain_id);
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &authority.pubkey(), settlement_program),
    );
    pt.add_account(
        client::cursor_pda(&program_id, &settlement_program, chain_id).0,
        cursor_account(program_id, chain_id, 1),
    );
    pt.add_account(
        authority.pubkey(),
        Account {
            lamports: 10_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    let mut ctx = pt.start_with_context().await;
    let batch = 1;
    let expected_count = 5;
    let ix = client::open_batch_ix(
        &program_id,
        &authority.pubkey(),
        chain_id,
        batch,
        expected_count,
        &settlement_program,
    );
    let cu = send(&mut ctx, &[ix], &authority, &[])
        .await
        .expect("OpenBatch should succeed");
    eprintln!("OpenBatch (with root-authority check) consumed {cu} CU");

    let (pda, _) = client::batch_pda(&program_id, &settlement_program, chain_id, batch);
    let acct = ctx
        .banks_client
        .get_account(pda)
        .await
        .unwrap()
        .expect("batch account must exist");
    assert_eq!(acct.owner, program_id);
    assert_eq!(
        acct.data.len(),
        account_len_for(acct.data[zk_inbox::batch::OFF_VERSION], expected_count).unwrap()
    );

    let decoded = client::decode_batch_account(&acct.data).unwrap();
    assert_eq!(decoded.chain_id, chain_id);
    assert_eq!(decoded.batch, batch);
    assert_eq!(decoded.expected_count, expected_count);
    assert_eq!(decoded.leaves_present, 0);
    assert!(!decoded.finalized);
    assert_eq!(decoded.settlement_program, settlement_program);
    assert_eq!(decoded.authority, authority.pubkey());
    assert!(
        decoded.open_slot > 0,
        "open_slot must be set from Clock, got 0"
    );
    assert!(
        decoded.open_unix_ts > 0,
        "open_unix_ts must be set from Clock, got {}",
        decoded.open_unix_ts
    );
}

/// Contract: `OpenBatch` reads `Clock` **once** for both `open_slot` and `open_unix_ts` — pin both
/// against a Clock sysvar this test sets itself (`ctx.set_sysvar`), so this is the real committed value,
/// not merely "greater than zero".
#[tokio::test]
async fn open_batch_writes_the_exact_clock_slot_and_unix_timestamp() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let chain_id = 2;
    let authority = funded_keypair();
    let (root, _) = client::root_pda(&settlement_program, chain_id);
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &authority.pubkey(), settlement_program),
    );
    pt.add_account(
        client::cursor_pda(&program_id, &settlement_program, chain_id).0,
        cursor_account(program_id, chain_id, 1),
    );
    pt.add_account(
        authority.pubkey(),
        Account {
            lamports: 10_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    let mut ctx = pt.start_with_context().await;

    const FIXED_SLOT: u64 = 123_456_789;
    const FIXED_UNIX_TS: i64 = 1_757_000_000;
    let mut clock = ctx
        .banks_client
        .get_sysvar::<solana_program::clock::Clock>()
        .await
        .expect("Clock sysvar must already exist");
    clock.slot = FIXED_SLOT;
    clock.unix_timestamp = FIXED_UNIX_TS;
    ctx.set_sysvar(&clock);

    let batch = 1;
    let expected_count = 5;
    let ix = client::open_batch_ix(
        &program_id,
        &authority.pubkey(),
        chain_id,
        batch,
        expected_count,
        &settlement_program,
    );
    send(&mut ctx, &[ix], &authority, &[])
        .await
        .expect("OpenBatch should succeed");

    let (pda, _) = client::batch_pda(&program_id, &settlement_program, chain_id, batch);
    let acct = ctx
        .banks_client
        .get_account(pda)
        .await
        .unwrap()
        .expect("batch account must exist");

    // Direct byte-offset assertions (contract): the account carries exactly T at OFF_OPEN_UNIX_TS and S
    // at OFF_OPEN_SLOT — independent of the decode path, which is exercised right after.
    let got_slot = u64::from_le_bytes(
        acct.data[rome_zk_layouts::batch::OFF_OPEN_SLOT..rome_zk_layouts::batch::OFF_OPEN_SLOT + 8]
            .try_into()
            .unwrap(),
    );
    let got_unix_ts = i64::from_le_bytes(
        acct.data[rome_zk_layouts::batch::OFF_OPEN_UNIX_TS
            ..rome_zk_layouts::batch::OFF_OPEN_UNIX_TS + 8]
            .try_into()
            .unwrap(),
    );
    assert_eq!(
        got_slot, FIXED_SLOT,
        "OFF_OPEN_SLOT must carry the exact Clock slot"
    );
    assert_eq!(
        got_unix_ts, FIXED_UNIX_TS,
        "OFF_OPEN_UNIX_TS must carry the exact Clock unix_timestamp"
    );

    let decoded = client::decode_batch_account(&acct.data).unwrap();
    assert_eq!(decoded.open_slot, FIXED_SLOT);
    assert_eq!(decoded.open_unix_ts, FIXED_UNIX_TS);
}

/// A negative `Clock::unix_timestamp` can never be constructed at the source — `OpenBatch` refuses it with
/// a named error before any write (the cursor's `next_batch` bump included), rather than letting a
/// downstream reader (`rome-zk-derive`) make sense of an impossible anchor. Real Solana validators never
/// produce a negative `unix_timestamp`; this test sets the sysvar directly (`ctx.set_sysvar`) the same way
/// the Clock-writes-through test above does, so the check is exercised without depending on that guarantee.
#[tokio::test]
async fn open_batch_rejects_a_negative_clock_unix_timestamp() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let chain_id = 3;
    let authority = funded_keypair();
    let (root, _) = client::root_pda(&settlement_program, chain_id);
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &authority.pubkey(), settlement_program),
    );
    pt.add_account(
        client::cursor_pda(&program_id, &settlement_program, chain_id).0,
        cursor_account(program_id, chain_id, 1),
    );
    pt.add_account(
        authority.pubkey(),
        Account {
            lamports: 10_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    let mut ctx = pt.start_with_context().await;

    let mut clock = ctx
        .banks_client
        .get_sysvar::<solana_program::clock::Clock>()
        .await
        .expect("Clock sysvar must already exist");
    clock.unix_timestamp = -1;
    ctx.set_sysvar(&clock);

    let batch = 1;
    let ix = client::open_batch_ix(
        &program_id,
        &authority.pubkey(),
        chain_id,
        batch,
        5,
        &settlement_program,
    );
    let err = send(&mut ctx, &[ix], &authority, &[]).await.unwrap_err();
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::Custom(14)), // BatchError::NegativeUnixTimestamp
        "a negative Clock::unix_timestamp must be refused by name at OpenBatch, before any write: {err:?}"
    );
    assert!(
        ctx.banks_client
            .get_account(client::batch_pda(&program_id, &settlement_program, chain_id, batch).0)
            .await
            .unwrap()
            .is_none(),
        "the batch PDA must not have been created"
    );
    let cursor_after = ctx
        .banks_client
        .get_account(client::cursor_pda(&program_id, &settlement_program, chain_id).0)
        .await
        .unwrap()
        .unwrap();
    let cursor = rome_zk_layouts::cursor::read(&cursor_after.data).unwrap();
    assert_eq!(
        cursor.next_batch, 1,
        "the cursor's next_batch must not have advanced — the refusal is before any write"
    );
}

/// No migration: a v1-shaped batch account (version byte 1, the old 202-byte length, no `open_unix_ts`)
/// can only ever exist post-reset under a retired program id — `OpenBatch` always writes `VERSION` (2),
/// so the only way v1 bytes reach *this* program's ownership is exactly that migration case. Every
/// mutating instruction that reads the header must refuse it, not merely the standalone decoder (already
/// covered by `rome-zk-layouts`'s own `a_v1_shaped_account_is_refused`). Dropping the version check from
/// `read_header` must turn this red.
#[tokio::test]
async fn a_v1_shaped_batch_account_is_refused_by_every_instruction() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let chain_id = 11;
    let batch = 4;
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let (batch_pda, _) = client::batch_pda(&program_id, &settlement_program, chain_id, batch);
    let authority = funded_keypair();

    // A *realistic* mid-flight v1 account — not the bare 202-byte magic+version+chain_id+batch stub
    // (that shape alone would not exercise the version check at all: v1's 202-byte header is already
    // shorter than v2's own `HEADER_LEN_V2` (210), so `read_header`'s independent `d.len() < need`
    // clause would refuse it on length alone, masking whether the version check does anything — proven
    // by hand: removing the version check (`header_len(version)`) from `read_header` and rerunning this test against
    // a bare 202-byte fixture left it green). Every field up to `finalize_cursor` (offset 198) sits at
    // the identical byte offset in v1 and v2 — `open_unix_ts` was *appended*, not inserted — so a v1
    // account with `expected_count` leaves already sealed is `202 + bitmap_len(expected_count) +
    // 32*expected_count` bytes: for 5 leaves, 363 bytes, comfortably past `HEADER_LEN_V2`, which is exactly
    // the shape a real pre-reset Tiber batch would have. This is the shape that actually isolates the
    // version check: dropping the version check from `read_header` turns GrowBatch,
    // SealLeaf and FinalizeBatch's assertions below red against this fixture (`chunk Open` and
    // `AbandonBatch` decode through `rome_zk_layouts::batch::read` instead, whose own version check comes
    // before its length check regardless of size — already covered by that crate's
    // `a_v1_shaped_account_is_refused`; included here for full instruction coverage).
    let expected_count: u32 = 5;
    let v1_account = {
        let bitmap_len = (expected_count as usize).div_ceil(8);
        let mut d = vec![0u8; 202 + bitmap_len + 32 * expected_count as usize];
        d[rome_zk_layouts::batch::OFF_MAGIC..rome_zk_layouts::batch::OFF_MAGIC + 4]
            .copy_from_slice(&rome_zk_layouts::batch::MAGIC.to_le_bytes());
        d[rome_zk_layouts::batch::OFF_VERSION] = 1;
        d[rome_zk_layouts::batch::OFF_CHAIN_ID..rome_zk_layouts::batch::OFF_CHAIN_ID + 8]
            .copy_from_slice(&chain_id.to_le_bytes());
        d[rome_zk_layouts::batch::OFF_BATCH..rome_zk_layouts::batch::OFF_BATCH + 8]
            .copy_from_slice(&batch.to_le_bytes());
        d[rome_zk_layouts::batch::OFF_EXPECTED_COUNT
            ..rome_zk_layouts::batch::OFF_EXPECTED_COUNT + 4]
            .copy_from_slice(&expected_count.to_le_bytes());
        d[rome_zk_layouts::batch::OFF_AUTHORITY..rome_zk_layouts::batch::OFF_AUTHORITY + 32]
            .copy_from_slice(authority.pubkey().as_ref());
        Account {
            lamports: rent_exempt(d.len()),
            data: d,
            owner: program_id,
            executable: false,
            rent_epoch: 0,
        }
    };

    async fn fresh_ctx(
        program_id: Pubkey,
        batch_pda: Pubkey,
        v1_account: &Account,
        authority: &Pubkey,
    ) -> solana_program_test::ProgramTestContext {
        let mut pt = rome_zk_testkit::program_test(
            &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
            true,
        );
        pt.add_account(batch_pda, v1_account.clone());
        pt.add_account(
            *authority,
            Account {
                lamports: 10_000_000_000,
                data: vec![],
                owner: system_program::id(),
                executable: false,
                rent_epoch: 0,
            },
        );
        pt.start_with_context().await
    }

    // GrowBatch: permissionless, `payer` need not be the batch authority.
    {
        let mut ctx = fresh_ctx(program_id, batch_pda, &v1_account, &authority.pubkey()).await;
        let ix = client::grow_batch_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
        );
        let err = send(&mut ctx, &[ix], &authority, &[]).await.unwrap_err();
        assert_eq!(
            err,
            TransactionError::InstructionError(0, InstructionError::InvalidAccountData),
            "GrowBatch against a v1-shaped account must fail InvalidAccountData: {err:?}"
        );
    }

    // Chunk Open: `open_chunk_check` reads the batch header before creating the chunk PDA.
    {
        let mut ctx = fresh_ctx(program_id, batch_pda, &v1_account, &authority.pubkey()).await;
        let ix = client::open_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            0,
            8,
        );
        let err = send(&mut ctx, &[ix], &authority, &[]).await.unwrap_err();
        assert_eq!(
            err,
            TransactionError::InstructionError(0, InstructionError::InvalidAccountData),
            "chunk Open against a v1-shaped batch account must fail InvalidAccountData: {err:?}"
        );
    }

    // SealLeaf: permissionless, no signer beyond the fee payer. Needs a program-owned chunk account at
    // the right PDA too — `seal_leaf_inner` checks both accounts' owners before it ever reads the batch
    // header, so a missing chunk account would fail `IncorrectProgramId` first and never exercise the
    // version check this test is about.
    {
        let mut pt = rome_zk_testkit::program_test(
            &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
            true,
        );
        pt.add_account(batch_pda, v1_account.clone());
        let (chunk_pda, _) =
            client::chunk_pda(&program_id, &settlement_program, chain_id, batch, 0);
        pt.add_account(
            chunk_pda,
            chunk_account(
                program_id,
                &authority.pubkey(),
                chain_id,
                batch,
                0,
                &[0u8; 8],
            ),
        );
        pt.add_account(
            authority.pubkey(),
            Account {
                lamports: 10_000_000_000,
                data: vec![],
                owner: system_program::id(),
                executable: false,
                rent_epoch: 0,
            },
        );
        pt.add_account(
            client::cursor_pda(&program_id, &settlement_program, chain_id).0,
            rome_zk_testkit::cursor_account_for(2, program_id, chain_id, 1_000),
        );
        let mut ctx = pt.start_with_context().await;
        let ix = client::seal_leaf_ix(&program_id, &settlement_program, chain_id, batch, 0);
        let err = send(&mut ctx, &[ix], &authority, &[]).await.unwrap_err();
        assert_eq!(
            err,
            TransactionError::InstructionError(0, InstructionError::InvalidAccountData),
            "SealLeaf against a v1-shaped batch account must fail InvalidAccountData: {err:?}"
        );
    }

    // FinalizeBatch: the version check (`read_header`) runs before the authority check, so a v1-shaped
    // account is refused the same way regardless of who signs.
    {
        let mut ctx = fresh_ctx(program_id, batch_pda, &v1_account, &authority.pubkey()).await;
        let ix = finalize_v2(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            0,
        );
        let err = send(&mut ctx, &[ix], &authority, &[]).await.unwrap_err();
        assert_eq!(
            err,
            TransactionError::InstructionError(0, InstructionError::InvalidAccountData),
            "FinalizeBatch against a v1-shaped batch account must fail InvalidAccountData: {err:?}"
        );
    }

    // AbandonBatch: authority-signed, but the version check trips before the authority is even read.
    {
        let mut ctx = fresh_ctx(program_id, batch_pda, &v1_account, &authority.pubkey()).await;
        let ix = client::abandon_batch_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
        );
        let err = send(&mut ctx, &[ix], &authority, &[&authority])
            .await
            .unwrap_err();
        assert_eq!(
            err,
            TransactionError::InstructionError(0, InstructionError::InvalidAccountData),
            "AbandonBatch against a v1-shaped batch account must fail InvalidAccountData: {err:?}"
        );
    }
}

#[tokio::test]
async fn open_batch_rejects_wrong_pda() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let chain_id = 1;
    let payer = funded_keypair();
    let (root, _) = client::root_pda(&settlement_program, chain_id);
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &payer.pubkey(), settlement_program),
    );
    let (cursor_pda, _) = client::cursor_pda(&program_id, &settlement_program, chain_id);
    pt.add_account(cursor_pda, cursor_account(program_id, chain_id, 1));
    pt.add_account(
        payer.pubkey(),
        Account {
            lamports: 10_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    let mut ctx = pt.start_with_context().await;
    let wrong_pda = Pubkey::new_unique();
    let ix = solana_program::instruction::Instruction {
        program_id,
        accounts: vec![
            solana_program::instruction::AccountMeta::new(payer.pubkey(), true),
            solana_program::instruction::AccountMeta::new(wrong_pda, false),
            solana_program::instruction::AccountMeta::new_readonly(root, false),
            solana_program::instruction::AccountMeta::new(cursor_pda, false),
            solana_program::instruction::AccountMeta::new_readonly(system_program::id(), false),
        ],
        data: borsh::to_vec(&zk_inbox::InboxIx::OpenBatch {
            chain_id,
            batch: 1,
            expected_count: 1,
            settlement_program,
        })
        .unwrap(),
    };
    let err = send(&mut ctx, &[ix], &payer, &[]).await.unwrap_err();
    assert!(matches!(err, TransactionError::InstructionError(_, _)));
}

#[tokio::test]
async fn open_batch_rejects_non_authority_signer() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let chain_id = 1;
    let real_authority = funded_keypair();
    let impostor = funded_keypair();
    let (root, _) = client::root_pda(&settlement_program, chain_id);
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &real_authority.pubkey(), settlement_program),
    );
    pt.add_account(
        client::cursor_pda(&program_id, &settlement_program, chain_id).0,
        cursor_account(program_id, chain_id, 1),
    );
    for kp in [&real_authority, &impostor] {
        pt.add_account(
            kp.pubkey(),
            Account {
                lamports: 10_000_000_000,
                data: vec![],
                owner: system_program::id(),
                executable: false,
                rent_epoch: 0,
            },
        );
    }
    let mut ctx = pt.start_with_context().await;
    let ix = client::open_batch_ix(
        &program_id,
        &impostor.pubkey(),
        chain_id,
        1,
        5,
        &settlement_program,
    );
    let err = send(&mut ctx, &[ix], &impostor, &[]).await.unwrap_err();
    assert!(
        matches!(err, TransactionError::InstructionError(_, _)),
        "a signer that is not the root's authority must not be able to open a batch: {err:?}"
    );
}

#[tokio::test]
async fn open_batch_rejects_root_owned_by_wrong_program() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let wrong_program = Pubkey::new_unique();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let chain_id = 1;
    let authority = funded_keypair();
    let (root, _) = client::root_pda(&settlement_program, chain_id);
    // Owned by the wrong program, even though its bytes claim `authority` truthfully.
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &authority.pubkey(), wrong_program),
    );
    pt.add_account(
        client::cursor_pda(&program_id, &settlement_program, chain_id).0,
        cursor_account(program_id, chain_id, 1),
    );
    pt.add_account(
        authority.pubkey(),
        Account {
            lamports: 10_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    let mut ctx = pt.start_with_context().await;
    let ix = client::open_batch_ix(
        &program_id,
        &authority.pubkey(),
        chain_id,
        1,
        5,
        &settlement_program,
    );
    let err = send(&mut ctx, &[ix], &authority, &[]).await.unwrap_err();
    assert!(matches!(err, TransactionError::InstructionError(_, _)));
}

// ---------------------------------------------------------------------------------------------
// 2. SealLeaf
// ---------------------------------------------------------------------------------------------

/// (idx, chunk-body keccak hash, chunk body bytes).
type LeafFixture = (u32, [u8; 32], Vec<u8>);

fn seal_leaf_test_setup(
    _program_id: Pubkey,
    expected_count: u32,
) -> (BatchFixture, Vec<LeafFixture>) {
    let fixture = BatchFixture {
        chain_id: 9,
        batch: 4,
        open_slot: 100,
        expected_count,
        settlement_program: Pubkey::new_unique(),
        authority: Pubkey::new_unique(),
    };
    let leaves: Vec<LeafFixture> = (0..expected_count)
        .map(|i| {
            let body = vec![i as u8; 8 + i as usize];
            (i, chunk_body_hash(&body), body)
        })
        .collect();
    (fixture, leaves)
}

#[tokio::test]
async fn seal_leaf_accepts_out_of_order_and_rejects_bad_cases() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let (fixture, leaves) = seal_leaf_test_setup(program_id, 6);
    let (batch_pda, _) = client::batch_pda(
        &program_id,
        &fixture.settlement_program,
        fixture.chain_id,
        fixture.batch,
    );
    pt.add_account(batch_pda, fixture.account(program_id, 0, &[], &[], false));
    for (idx, _hash, body) in &leaves {
        let (cpda, _) = client::chunk_pda(
            &program_id,
            &fixture.settlement_program,
            fixture.chain_id,
            fixture.batch,
            *idx,
        );
        pt.add_account(
            cpda,
            chunk_account(
                program_id,
                &fixture.authority,
                fixture.chain_id,
                fixture.batch,
                *idx,
                body,
            ),
        );
    }
    let mut ctx = pt.start_with_context().await;
    let payer = ctx.payer.insecure_clone();

    // idx 5 before idx 0: order-independent.
    let cu = send(
        &mut ctx,
        &[client::seal_leaf_ix(
            &program_id,
            &fixture.settlement_program,
            fixture.chain_id,
            fixture.batch,
            5,
        )],
        &payer,
        &[],
    )
    .await
    .expect("sealing idx 5 first must succeed");
    eprintln!("SealLeaf (unchanged, permissionless) consumed {cu} CU");
    send(
        &mut ctx,
        &[client::seal_leaf_ix(
            &program_id,
            &fixture.settlement_program,
            fixture.chain_id,
            fixture.batch,
            0,
        )],
        &payer,
        &[],
    )
    .await
    .expect("sealing idx 0 second must succeed");

    let acct = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .unwrap();
    let decoded = client::decode_batch_account(&acct.data).unwrap();
    assert_eq!(decoded.leaves_present, 2);

    // duplicate, same hash: no-op, leaves_present unchanged.
    send(
        &mut ctx,
        &[client::seal_leaf_ix(
            &program_id,
            &fixture.settlement_program,
            fixture.chain_id,
            fixture.batch,
            0,
        )],
        &payer,
        &[],
    )
    .await
    .expect("re-sealing the same idx with the same hash must be a no-op, not an error");
    let acct = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        client::decode_batch_account(&acct.data)
            .unwrap()
            .leaves_present,
        2
    );

    // idx out of range.
    let err = send(
        &mut ctx,
        &[client::seal_leaf_ix(
            &program_id,
            &fixture.settlement_program,
            fixture.chain_id,
            fixture.batch,
            6,
        )],
        &payer,
        &[],
    )
    .await
    .unwrap_err();
    assert!(matches!(err, TransactionError::InstructionError(_, _)));
}

#[tokio::test]
async fn seal_leaf_rejects_duplicate_with_different_hash() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let (fixture, leaves) = seal_leaf_test_setup(program_id, 2);
    let (batch_pda, _) = client::batch_pda(
        &program_id,
        &fixture.settlement_program,
        fixture.chain_id,
        fixture.batch,
    );
    pt.add_account(batch_pda, fixture.account(program_id, 0, &[], &[], false));
    for (idx, _hash, body) in &leaves {
        let (cpda, _) = client::chunk_pda(
            &program_id,
            &fixture.settlement_program,
            fixture.chain_id,
            fixture.batch,
            *idx,
        );
        pt.add_account(
            cpda,
            chunk_account(
                program_id,
                &fixture.authority,
                fixture.chain_id,
                fixture.batch,
                *idx,
                body,
            ),
        );
    }
    let mut ctx = pt.start_with_context().await;
    let payer = ctx.payer.insecure_clone();
    send(
        &mut ctx,
        &[client::seal_leaf_ix(
            &program_id,
            &fixture.settlement_program,
            fixture.chain_id,
            fixture.batch,
            0,
        )],
        &payer,
        &[],
    )
    .await
    .unwrap();

    // Overwrite the chunk at idx 0 with different content under the same PDA, then reseal: the chunk's
    // hash now differs from the one already recorded for idx 0. `set_account` lets the test mutate an
    // already-started ledger's account directly, which is the point here (no real Write is involved —
    // this stands in for "somehow the recorded hash and the chunk's current bytes disagree").
    let (cpda0, _) = client::chunk_pda(
        &program_id,
        &fixture.settlement_program,
        fixture.chain_id,
        fixture.batch,
        0,
    );
    let different_body = b"a completely different body".to_vec();
    let new_account = chunk_account(
        program_id,
        &fixture.authority,
        fixture.chain_id,
        fixture.batch,
        0,
        &different_body,
    );
    ctx.set_account(
        &cpda0,
        &solana_sdk::account::AccountSharedData::from(new_account),
    );

    let err = send(
        &mut ctx,
        &[client::seal_leaf_ix(
            &program_id,
            &fixture.settlement_program,
            fixture.chain_id,
            fixture.batch,
            0,
        )],
        &payer,
        &[],
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, TransactionError::InstructionError(_, _)),
        "reseal with a different hash must error: {err:?}"
    );
}

#[tokio::test]
async fn seal_leaf_rejects_an_unsealed_chunk() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let fixture = BatchFixture {
        chain_id: 9,
        batch: 4,
        open_slot: 100,
        expected_count: 1,
        settlement_program: Pubkey::new_unique(),
        authority: Pubkey::new_unique(),
    };
    let (batch_pda, _) = client::batch_pda(
        &program_id,
        &fixture.settlement_program,
        fixture.chain_id,
        fixture.batch,
    );
    pt.add_account(batch_pda, fixture.account(program_id, 0, &[], &[], false));
    let (cpda, _) = client::chunk_pda(
        &program_id,
        &fixture.settlement_program,
        fixture.chain_id,
        fixture.batch,
        0,
    );
    let mut unsealed = chunk_account(
        program_id,
        &fixture.authority,
        fixture.chain_id,
        fixture.batch,
        0,
        b"body",
    );
    unsealed.data[60] = 0; // not sealed
    pt.add_account(cpda, unsealed);
    let mut ctx = pt.start_with_context().await;
    let payer = ctx.payer.insecure_clone();

    let err = send(
        &mut ctx,
        &[client::seal_leaf_ix(
            &program_id,
            &fixture.settlement_program,
            fixture.chain_id,
            fixture.batch,
            0,
        )],
        &payer,
        &[],
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, TransactionError::InstructionError(_, _)),
        "sealing an unsealed chunk must error: {err:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// 3. FinalizeBatch correctness
// ---------------------------------------------------------------------------------------------

fn small_leaf_set(n: usize) -> Vec<[u8; 32]> {
    (0..n)
        .map(|i| chunk_body_hash(&vec![i as u8; 3 + i]))
        .collect()
}

async fn finalize_small(n: u32) {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let authority = Keypair::new();
    let fixture = BatchFixture {
        chain_id: 5,
        batch: 2,
        open_slot: 42,
        expected_count: n,
        settlement_program: Pubkey::new_unique(),
        authority: authority.pubkey(),
    };
    let leaves = small_leaf_set(n as usize);
    let (batch_pda, _) = client::batch_pda(
        &program_id,
        &fixture.settlement_program,
        fixture.chain_id,
        fixture.batch,
    );
    let sealed: Vec<u32> = (0..n).collect();
    pt.add_account(
        batch_pda,
        fixture.account(program_id, n, &leaves, &sealed, false),
    );
    pt.add_account(
        client::cursor_pda(&program_id, &fixture.settlement_program, fixture.chain_id).0,
        rome_zk_testkit::cursor_account_for(2, program_id, fixture.chain_id, 1_000),
    );
    let mut ctx = pt.start_with_context().await;
    let payer = ctx.payer.insecure_clone();

    let cu = send(
        &mut ctx,
        &[finalize_v2(
            &program_id,
            &authority.pubkey(),
            &fixture.settlement_program,
            fixture.chain_id,
            fixture.batch,
            0,
        )],
        &payer,
        &[&authority],
    )
    .await
    .expect("finalize should succeed once all leaves are present");
    eprintln!("finalize_batch({n} leaves) consumed {cu} CU");

    let acct = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .unwrap();
    let decoded = client::decode_batch_account(&acct.data).unwrap();
    assert!(decoded.finalized);

    let (expected_root, expected_forced_root, expected_acc) =
        client::reference_commitment(fixture.chain_id, fixture.batch, fixture.open_slot, &leaves);
    assert_eq!(
        decoded.root, expected_root,
        "root must match the off-chain reference"
    );
    assert_eq!(decoded.forced_root, expected_forced_root);
    assert_eq!(
        decoded.acc, expected_acc,
        "acc must match the off-chain reference formula"
    );
}

#[tokio::test]
async fn finalize_batch_matches_reference_for_small_leaf_counts() {
    for n in [1u32, 2, 3, 5] {
        finalize_small(n).await;
    }
}

#[tokio::test]
async fn finalize_batch_before_all_leaves_present_errors() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let authority = Keypair::new();
    let fixture = BatchFixture {
        chain_id: 5,
        batch: 2,
        open_slot: 42,
        expected_count: 3,
        settlement_program: Pubkey::new_unique(),
        authority: authority.pubkey(),
    };
    let leaves = small_leaf_set(2); // only 2 of 3
    let (batch_pda, _) = client::batch_pda(
        &program_id,
        &fixture.settlement_program,
        fixture.chain_id,
        fixture.batch,
    );
    pt.add_account(
        batch_pda,
        fixture.account(program_id, 2, &leaves, &[0, 1], false),
    );
    pt.add_account(
        client::cursor_pda(&program_id, &fixture.settlement_program, fixture.chain_id).0,
        rome_zk_testkit::cursor_account_for(2, program_id, fixture.chain_id, 1_000),
    );
    let mut ctx = pt.start_with_context().await;
    let payer = ctx.payer.insecure_clone();

    // Signed correctly by the batch's own authority — this must fail on `NotAllLeavesSealed`, not on
    // the (now earlier) authority check, so this test still isolates the leaf-completeness gate.
    let err = send(
        &mut ctx,
        &[finalize_v2(
            &program_id,
            &authority.pubkey(),
            &fixture.settlement_program,
            fixture.chain_id,
            fixture.batch,
            0,
        )],
        &payer,
        &[&authority],
    )
    .await
    .unwrap_err();
    assert!(matches!(err, TransactionError::InstructionError(_, _)));
}

#[tokio::test]
async fn finalize_batch_twice_errors() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let authority = Keypair::new();
    let fixture = BatchFixture {
        chain_id: 5,
        batch: 2,
        open_slot: 42,
        expected_count: 2,
        settlement_program: Pubkey::new_unique(),
        authority: authority.pubkey(),
    };
    let leaves = small_leaf_set(2);
    let (batch_pda, _) = client::batch_pda(
        &program_id,
        &fixture.settlement_program,
        fixture.chain_id,
        fixture.batch,
    );
    pt.add_account(
        batch_pda,
        fixture.account(program_id, 2, &leaves, &[0, 1], false),
    );
    pt.add_account(
        client::cursor_pda(&program_id, &fixture.settlement_program, fixture.chain_id).0,
        rome_zk_testkit::cursor_account_for(2, program_id, fixture.chain_id, 1_000),
    );
    let mut ctx = pt.start_with_context().await;
    let payer = ctx.payer.insecure_clone();

    send(
        &mut ctx,
        &[finalize_v2(
            &program_id,
            &authority.pubkey(),
            &fixture.settlement_program,
            fixture.chain_id,
            fixture.batch,
            0,
        )],
        &payer,
        &[&authority],
    )
    .await
    .unwrap();
    let err = send(
        &mut ctx,
        &[finalize_v2(
            &program_id,
            &authority.pubkey(),
            &fixture.settlement_program,
            fixture.chain_id,
            fixture.batch,
            0,
        )],
        &payer,
        &[&authority],
    )
    .await
    .unwrap_err();
    assert!(matches!(err, TransactionError::InstructionError(_, _)));
}

// ---------------------------------------------------------------------------------------------
// 4. 900-leaf CU gate
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn finalize_batch_900_leaves_within_cu_budget() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let n = 900u32;
    let authority = Keypair::new();
    let fixture = BatchFixture {
        chain_id: 200099,
        batch: 1,
        open_slot: 1000,
        expected_count: n,
        settlement_program: Pubkey::new_unique(),
        authority: authority.pubkey(),
    };
    let leaves: Vec<[u8; 32]> = (0..n as usize)
        .map(|i| chunk_body_hash(&(i as u32).to_le_bytes()))
        .collect();
    let (batch_pda, _) = client::batch_pda(
        &program_id,
        &fixture.settlement_program,
        fixture.chain_id,
        fixture.batch,
    );
    let sealed: Vec<u32> = (0..n).collect();
    pt.add_account(
        batch_pda,
        fixture.account(program_id, n, &leaves, &sealed, false),
    );
    pt.add_account(
        client::cursor_pda(&program_id, &fixture.settlement_program, fixture.chain_id).0,
        rome_zk_testkit::cursor_account_for(2, program_id, fixture.chain_id, 1_000),
    );
    let mut ctx = pt.start_with_context().await;
    let payer = ctx.payer.insecure_clone();

    let cu = send(
        &mut ctx,
        &[finalize_v2(
            &program_id,
            &authority.pubkey(),
            &fixture.settlement_program,
            fixture.chain_id,
            fixture.batch,
            0,
        )],
        &payer,
        &[&authority],
    )
    .await
    .expect("900-leaf finalize must succeed in one call");
    eprintln!("finalize_batch(900 leaves, one call) consumed {cu} CU (budget: 600,000) [authority-signer check included]");
    assert!(
        cu <= 600_000,
        "900-leaf finalize consumed {cu} CU, over the 0.6M budget"
    );

    let acct = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .unwrap();
    let decoded = client::decode_batch_account(&acct.data).unwrap();
    assert!(decoded.finalized);
    let (expected_root, _, expected_acc) =
        client::reference_commitment(fixture.chain_id, fixture.batch, fixture.open_slot, &leaves);
    assert_eq!(decoded.root, expected_root);
    assert_eq!(decoded.acc, expected_acc);
}

/// The resumable path: a `step > 0` continuation call is gated exactly like the completing
/// one. The first (signed) step advances the cursor; the second call without the signer is refused by
/// name and the cursor does not move. Pins the check's position at the top of `finalize_batch_inner`
/// against a refactor that would move it into the completing branch.
#[tokio::test]
async fn finalize_batch_step_continuation_without_the_authority_signer_is_refused_and_moves_nothing(
) {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let n = 2500u32;
    let authority = Keypair::new();
    let fixture = BatchFixture {
        chain_id: 200099,
        batch: 1,
        open_slot: 1000,
        expected_count: n,
        settlement_program: Pubkey::new_unique(),
        authority: authority.pubkey(),
    };
    let leaves: Vec<[u8; 32]> = (0..n as usize)
        .map(|i| chunk_body_hash(&(i as u32).to_le_bytes()))
        .collect();
    let (batch_pda, _) = client::batch_pda(
        &program_id,
        &fixture.settlement_program,
        fixture.chain_id,
        fixture.batch,
    );
    let sealed: Vec<u32> = (0..n).collect();
    pt.add_account(
        batch_pda,
        fixture.account(program_id, n, &leaves, &sealed, false),
    );
    pt.add_account(
        client::cursor_pda(&program_id, &fixture.settlement_program, fixture.chain_id).0,
        rome_zk_testkit::cursor_account_for(2, program_id, fixture.chain_id, 1_000),
    );
    let mut ctx = pt.start_with_context().await;
    let payer = ctx.payer.insecure_clone();

    let step = 1300u32;
    send(
        &mut ctx,
        &[finalize_v2(
            &program_id,
            &authority.pubkey(),
            &fixture.settlement_program,
            fixture.chain_id,
            fixture.batch,
            step,
        )],
        &payer,
        &[&authority],
    )
    .await
    .expect("first (signed) step must succeed");

    let mut ix = finalize_v2(
        &program_id,
        &authority.pubkey(),
        &fixture.settlement_program,
        fixture.chain_id,
        fixture.batch,
        step,
    );
    ix.accounts[1].is_signer = false;
    let err = send(&mut ctx, &[ix], &payer, &[]).await.unwrap_err();
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::MissingRequiredSignature),
        "an unsigned step continuation must be refused by name"
    );
    let acct = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .unwrap();
    let decoded = client::decode_batch_account(&acct.data).unwrap();
    assert!(!decoded.finalized, "the refused step must not finalize");
    assert_eq!(
        decoded.finalize_cursor, step,
        "the refused step must not advance the finalize cursor"
    );
}

// ---------------------------------------------------------------------------------------------
// 5. 2,500-leaf cursor resumability
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn finalize_batch_2500_leaves_resumes_across_transactions() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let n = 2500u32;
    let authority = Keypair::new();
    let fixture = BatchFixture {
        chain_id: 200099,
        batch: 1,
        open_slot: 1000,
        expected_count: n,
        settlement_program: Pubkey::new_unique(),
        authority: authority.pubkey(),
    };
    let leaves: Vec<[u8; 32]> = (0..n as usize)
        .map(|i| chunk_body_hash(&(i as u32).to_le_bytes()))
        .collect();
    let (batch_pda, _) = client::batch_pda(
        &program_id,
        &fixture.settlement_program,
        fixture.chain_id,
        fixture.batch,
    );
    let sealed: Vec<u32> = (0..n).collect();
    pt.add_account(
        batch_pda,
        fixture.account(program_id, n, &leaves, &sealed, false),
    );
    pt.add_account(
        client::cursor_pda(&program_id, &fixture.settlement_program, fixture.chain_id).0,
        rome_zk_testkit::cursor_account_for(2, program_id, fixture.chain_id, 1_000),
    );
    let mut ctx = pt.start_with_context().await;
    let payer = ctx.payer.insecure_clone();

    // Every step call needs the batch authority signer, not only the completing one.
    let step = 1300u32;
    let cu1 = send(
        &mut ctx,
        &[finalize_v2(
            &program_id,
            &authority.pubkey(),
            &fixture.settlement_program,
            fixture.chain_id,
            fixture.batch,
            step,
        )],
        &payer,
        &[&authority],
    )
    .await
    .expect("first call (transform only) must succeed");
    eprintln!("finalize_batch(2500 leaves, call 1/2, step {step}) consumed {cu1} CU");
    let acct = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .unwrap();
    let decoded = client::decode_batch_account(&acct.data).unwrap();
    assert!(
        !decoded.finalized,
        "must not be finalized after only the first call"
    );
    assert_eq!(decoded.finalize_cursor, step);

    let cu2 = send(
        &mut ctx,
        &[finalize_v2(
            &program_id,
            &authority.pubkey(),
            &fixture.settlement_program,
            fixture.chain_id,
            fixture.batch,
            step,
        )],
        &payer,
        &[&authority],
    )
    .await
    .expect("second call must finish transform and combine");
    eprintln!("finalize_batch(2500 leaves, call 2/2, step {step}) consumed {cu2} CU");
    let acct = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .unwrap();
    let decoded = client::decode_batch_account(&acct.data).unwrap();
    assert!(
        decoded.finalized,
        "must be finalized after the second call (2 calls cover 2,500 leaves at step 1,300)"
    );

    let (expected_root, _, expected_acc) =
        client::reference_commitment(fixture.chain_id, fixture.batch, fixture.open_slot, &leaves);
    assert_eq!(
        decoded.root, expected_root,
        "root after resumed finalize must match the single-shot reference"
    );
    assert_eq!(decoded.acc, expected_acc);
}

// ---------------------------------------------------------------------------------------------
// 5b. FinalizeBatch authority gate: the trailing signer must be the batch's
//     own stored `authority` (the value `OpenBatch` wrote) — refused when absent, unsigned, or a
//     signer that simply isn't that pubkey. `SealLeaf` stays permissionless throughout.
// ---------------------------------------------------------------------------------------------

/// A small (every-leaf-already-sealed) batch fixture, isolating the authority-signer gate from the
/// (separately covered) leaf-completeness gate below.
fn ready_to_finalize_fixture(
    chain_id: u64,
    batch: u64,
    authority: Pubkey,
) -> (BatchFixture, Vec<[u8; 32]>) {
    let n = 3u32;
    let fixture = BatchFixture {
        chain_id,
        batch,
        open_slot: 10,
        expected_count: n,
        settlement_program: Pubkey::new_unique(),
        authority,
    };
    (fixture, small_leaf_set(n as usize))
}

#[tokio::test]
async fn finalize_batch_signed_by_the_batch_authority_succeeds() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let authority = Keypair::new();
    let (fixture, leaves) = ready_to_finalize_fixture(300, 9, authority.pubkey());
    let (batch_pda, _) = client::batch_pda(
        &program_id,
        &fixture.settlement_program,
        fixture.chain_id,
        fixture.batch,
    );
    let sealed: Vec<u32> = (0..fixture.expected_count).collect();
    pt.add_account(
        batch_pda,
        fixture.account(program_id, fixture.expected_count, &leaves, &sealed, false),
    );
    pt.add_account(
        client::cursor_pda(&program_id, &fixture.settlement_program, fixture.chain_id).0,
        rome_zk_testkit::cursor_account_for(2, program_id, fixture.chain_id, 1_000),
    );
    let mut ctx = pt.start_with_context().await;
    let payer = ctx.payer.insecure_clone();

    send(
        &mut ctx,
        &[finalize_v2(
            &program_id,
            &authority.pubkey(),
            &fixture.settlement_program,
            fixture.chain_id,
            fixture.batch,
            0,
        )],
        &payer,
        &[&authority],
    )
    .await
    .expect("signed by the batch's own authority must succeed");
}

// `InstructionError::NotEnoughAccountKeys` is marked deprecated in the Agave 4.x line, hence the
// `allow`. It is still the right expectation: the program itself returns it. `batch.rs` reads the
// authority with `next_account_info(it)?`, which fails with `ProgramError::NotEnoughAccountKeys` when the
// list is short, and the runtime reports that as `InstructionError::NotEnoughAccountKeys`. This test
// passes on the v3 build under the 4.3.0 harness, so nothing is outstanding here.
#[allow(deprecated)]
#[tokio::test]
async fn finalize_batch_missing_the_authority_account_errors() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let authority = Keypair::new();
    let (fixture, leaves) = ready_to_finalize_fixture(301, 9, authority.pubkey());
    let (batch_pda, _) = client::batch_pda(
        &program_id,
        &fixture.settlement_program,
        fixture.chain_id,
        fixture.batch,
    );
    let sealed: Vec<u32> = (0..fixture.expected_count).collect();
    pt.add_account(
        batch_pda,
        fixture.account(program_id, fixture.expected_count, &leaves, &sealed, false),
    );
    pt.add_account(
        client::cursor_pda(&program_id, &fixture.settlement_program, fixture.chain_id).0,
        rome_zk_testkit::cursor_account_for(2, program_id, fixture.chain_id, 1_000),
    );
    let mut ctx = pt.start_with_context().await;
    let payer = ctx.payer.insecure_clone();

    // The old shape: only the batch pda, no trailing authority account at all.
    let mut ix = finalize_v2(
        &program_id,
        &authority.pubkey(),
        &fixture.settlement_program,
        fixture.chain_id,
        fixture.batch,
        0,
    );
    ix.accounts.truncate(1);
    let err = send(&mut ctx, &[ix], &payer, &[]).await.unwrap_err();
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::NotEnoughAccountKeys),
        "a FinalizeBatch with no authority account at all must be refused by name"
    );
}

#[tokio::test]
async fn finalize_batch_with_an_unsigned_authority_account_errors() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let authority = Keypair::new();
    let (fixture, leaves) = ready_to_finalize_fixture(302, 9, authority.pubkey());
    let (batch_pda, _) = client::batch_pda(
        &program_id,
        &fixture.settlement_program,
        fixture.chain_id,
        fixture.batch,
    );
    let sealed: Vec<u32> = (0..fixture.expected_count).collect();
    pt.add_account(
        batch_pda,
        fixture.account(program_id, fixture.expected_count, &leaves, &sealed, false),
    );
    pt.add_account(
        client::cursor_pda(&program_id, &fixture.settlement_program, fixture.chain_id).0,
        rome_zk_testkit::cursor_account_for(2, program_id, fixture.chain_id, 1_000),
    );
    let mut ctx = pt.start_with_context().await;
    let payer = ctx.payer.insecure_clone();

    // The right pubkey, present, but not marked as a signer in the account list — and not actually
    // signed (no extra_signers), so the compiled message never requires (or carries) its signature.
    let mut ix = finalize_v2(
        &program_id,
        &authority.pubkey(),
        &fixture.settlement_program,
        fixture.chain_id,
        fixture.batch,
        0,
    );
    ix.accounts[1].is_signer = false;
    let err = send(&mut ctx, &[ix], &payer, &[]).await.unwrap_err();
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::MissingRequiredSignature),
        "an unsigned authority account must be refused by name"
    );
}

#[tokio::test]
async fn finalize_batch_signed_by_the_wrong_key_errors() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let authority = Keypair::new();
    let wrong = Keypair::new();
    let (fixture, leaves) = ready_to_finalize_fixture(303, 9, authority.pubkey());
    let (batch_pda, _) = client::batch_pda(
        &program_id,
        &fixture.settlement_program,
        fixture.chain_id,
        fixture.batch,
    );
    let sealed: Vec<u32> = (0..fixture.expected_count).collect();
    pt.add_account(
        batch_pda,
        fixture.account(program_id, fixture.expected_count, &leaves, &sealed, false),
    );
    pt.add_account(
        client::cursor_pda(&program_id, &fixture.settlement_program, fixture.chain_id).0,
        rome_zk_testkit::cursor_account_for(2, program_id, fixture.chain_id, 1_000),
    );
    let mut ctx = pt.start_with_context().await;
    let payer = ctx.payer.insecure_clone();

    // `wrong` genuinely signs — the transaction is well-formed — but its pubkey is not what the batch
    // account stores at `OFF_AUTHORITY`.
    let ix = finalize_v2(
        &program_id,
        &wrong.pubkey(),
        &fixture.settlement_program,
        fixture.chain_id,
        fixture.batch,
        0,
    );
    let err = send(&mut ctx, &[ix], &payer, &[&wrong]).await.unwrap_err();
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::MissingRequiredSignature),
        "a genuinely-signed but wrong key must be refused by name"
    );
}

/// `SealLeaf` stays permissionless (a third party may seal every leaf), but that same third party
/// still cannot `FinalizeBatch` — only the batch's own `authority` (set by `OpenBatch`, which is
/// itself authority-gated) can. Drives the real `OpenBatch`/chunk/`SealLeaf` pipeline (not the
/// `BatchFixture` shortcut) so the third party's `SealLeaf` calls are genuinely permissionless
/// on-chain, not merely asserted.
#[tokio::test]
async fn finalize_batch_by_a_third_party_after_it_permissionlessly_sealed_every_leaf_still_errors()
{
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let chain_id = 88_001;
    let batch = 1;
    let n = 2u32;

    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let authority = funded_keypair();
    let third_party = funded_keypair();
    let (root, _) = client::root_pda(&settlement_program, chain_id);
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &authority.pubkey(), settlement_program),
    );
    pt.add_account(
        client::cursor_pda(&program_id, &settlement_program, chain_id).0,
        cursor_account_for(2, program_id, chain_id, batch),
    );
    for kp in [&authority, &third_party] {
        pt.add_account(
            kp.pubkey(),
            Account {
                lamports: 10_000_000_000,
                data: vec![],
                owner: system_program::id(),
                executable: false,
                rent_epoch: 0,
            },
        );
    }
    let mut ctx = pt.start_with_context().await;

    send(
        &mut ctx,
        &[client::open_batch_ix(
            &program_id,
            &authority.pubkey(),
            chain_id,
            batch,
            n,
            &settlement_program,
        )],
        &authority,
        &[],
    )
    .await
    .expect("OpenBatch must succeed for the root's authority");

    // Chunk `Open` is itself authority-gated — only `SealLeaf` is permissionless — so
    // the authority creates and seals every chunk; the third party never touches the chunk lane.
    for idx in 0..n {
        let body = format!("chunk {idx}").into_bytes();
        send(
            &mut ctx,
            &[
                client::open_chunk_ix(
                    &program_id,
                    &authority.pubkey(),
                    &settlement_program,
                    chain_id,
                    batch,
                    idx,
                    body.len() as u32,
                ),
                client::write_chunk_ix(
                    &program_id,
                    &authority.pubkey(),
                    &settlement_program,
                    chain_id,
                    batch,
                    idx,
                    0,
                    body.clone(),
                ),
                client::seal_chunk_ix(
                    &program_id,
                    &authority.pubkey(),
                    &settlement_program,
                    chain_id,
                    batch,
                    idx,
                    body.len() as u32,
                    client::chunk_body_hash(&body),
                ),
            ],
            &authority,
            &[],
        )
        .await
        .expect("chunk open+write+seal must succeed");
    }

    // The third party seals every leaf itself — permissionless, must succeed even though it never
    // touched OpenBatch or any chunk Open.
    for idx in 0..n {
        send(
            &mut ctx,
            &[client::seal_leaf_ix(
                &program_id,
                &settlement_program,
                chain_id,
                batch,
                idx,
            )],
            &third_party,
            &[],
        )
        .await
        .expect("SealLeaf is permissionless — a third party may call it");
    }

    // That same third party now tries to FinalizeBatch itself: every leaf is sealed and it paid for
    // every SealLeaf call, but it is not the batch's `authority` — must be refused.
    let err = send(
        &mut ctx,
        &[finalize_v2(
            &program_id,
            &third_party.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            0,
        )],
        &third_party,
        &[],
    )
    .await
    .unwrap_err();
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::MissingRequiredSignature),
        "a non-authority finalizer must be refused by name even after it sealed every leaf"
    );

    // The real authority can still finalize.
    send(
        &mut ctx,
        &[finalize_v2(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            0,
        )],
        &authority,
        &[],
    )
    .await
    .expect("the real batch authority must still be able to finalize");
}

// ---------------------------------------------------------------------------------------------
// 6. Close / CloseBatch finality gate
// ---------------------------------------------------------------------------------------------

fn finalized_batch_fixture(
    _program_id: Pubkey,
    n: u32,
    chain_id: u64,
    batch: u64,
    settlement_program: Pubkey,
    authority: Pubkey,
) -> (BatchFixture, Vec<[u8; 32]>) {
    let fixture = BatchFixture {
        chain_id,
        batch,
        open_slot: 7,
        expected_count: n,
        settlement_program,
        authority,
    };
    let leaves = small_leaf_set(n as usize);
    (fixture, leaves)
}

#[tokio::test]
async fn close_batch_before_final_root_errors() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let authority = Keypair::new();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let (fixture, leaves) =
        finalized_batch_fixture(program_id, 2, 11, 6, settlement_program, authority.pubkey());
    let (batch_pda, _) = client::batch_pda(
        &program_id,
        &settlement_program,
        fixture.chain_id,
        fixture.batch,
    );
    pt.add_account(
        batch_pda,
        fixture.account(program_id, 2, &leaves, &[0, 1], true),
    );
    let (root, _) = client::root_pda(&settlement_program, fixture.chain_id);
    pt.add_account(
        root,
        root_account(fixture.chain_id, fixture.batch - 1, settlement_program),
    ); // not yet final
    pt.add_account(
        client::cursor_pda(&program_id, &settlement_program, fixture.chain_id).0,
        cursor_account_for(2, program_id, fixture.chain_id, fixture.batch + 1),
    );
    pt.add_account(
        authority.pubkey(),
        Account {
            lamports: 10_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    let mut ctx = pt.start_with_context().await;
    let payer = ctx.payer.insecure_clone();

    let ix = client::close_batch_ix(
        &program_id,
        &authority.pubkey(),
        &settlement_program,
        fixture.chain_id,
        fixture.batch,
    );
    let err = send(&mut ctx, &[ix], &payer, &[&authority])
        .await
        .unwrap_err();
    assert!(matches!(err, TransactionError::InstructionError(_, _)));
}

#[tokio::test]
async fn close_batch_with_root_owned_by_wrong_program_errors() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let authority = Keypair::new();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let wrong_program = Pubkey::new_unique();
    let (fixture, leaves) =
        finalized_batch_fixture(program_id, 2, 11, 6, settlement_program, authority.pubkey());
    let (batch_pda, _) = client::batch_pda(
        &program_id,
        &settlement_program,
        fixture.chain_id,
        fixture.batch,
    );
    pt.add_account(
        batch_pda,
        fixture.account(program_id, 2, &leaves, &[0, 1], true),
    );
    let (root, _) = client::root_pda(&settlement_program, fixture.chain_id);
    // Owned by the wrong program, even though its bytes claim finality.
    pt.add_account(
        root,
        root_account(fixture.chain_id, fixture.batch, wrong_program),
    );
    pt.add_account(
        client::cursor_pda(&program_id, &settlement_program, fixture.chain_id).0,
        cursor_account_for(2, program_id, fixture.chain_id, fixture.batch + 1),
    );
    pt.add_account(
        authority.pubkey(),
        Account {
            lamports: 10_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    let mut ctx = pt.start_with_context().await;
    let payer = ctx.payer.insecure_clone();

    let ix = client::close_batch_ix(
        &program_id,
        &authority.pubkey(),
        &settlement_program,
        fixture.chain_id,
        fixture.batch,
    );
    let err = send(&mut ctx, &[ix], &payer, &[&authority])
        .await
        .unwrap_err();
    assert!(matches!(err, TransactionError::InstructionError(_, _)));
}

#[tokio::test]
async fn close_batch_succeeds_once_root_is_final_and_returns_rent() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let authority = Keypair::new();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let (fixture, leaves) =
        finalized_batch_fixture(program_id, 2, 11, 6, settlement_program, authority.pubkey());
    let (batch_pda, _) = client::batch_pda(
        &program_id,
        &settlement_program,
        fixture.chain_id,
        fixture.batch,
    );
    let batch_account = fixture.account(program_id, 2, &leaves, &[0, 1], true);
    let expected_rent = batch_account.lamports;
    pt.add_account(batch_pda, batch_account);
    let (root, _) = client::root_pda(&settlement_program, fixture.chain_id);
    pt.add_account(
        root,
        root_account(fixture.chain_id, fixture.batch, settlement_program),
    ); // final
    pt.add_account(
        client::cursor_pda(&program_id, &settlement_program, fixture.chain_id).0,
        cursor_account_for(2, program_id, fixture.chain_id, fixture.batch + 1),
    );
    pt.add_account(
        authority.pubkey(),
        Account {
            lamports: 10_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    let mut ctx = pt.start_with_context().await;
    let payer = ctx.payer.insecure_clone();

    let before = ctx
        .banks_client
        .get_account(authority.pubkey())
        .await
        .unwrap()
        .unwrap()
        .lamports;
    let ix = client::close_batch_ix(
        &program_id,
        &authority.pubkey(),
        &settlement_program,
        fixture.chain_id,
        fixture.batch,
    );
    send(&mut ctx, &[ix], &payer, &[&authority])
        .await
        .expect("close should succeed once the root is final");

    assert!(
        ctx.banks_client
            .get_account(batch_pda)
            .await
            .unwrap()
            .is_none(),
        "batch account must be closed"
    );
    let after = ctx
        .banks_client
        .get_account(authority.pubkey())
        .await
        .unwrap()
        .unwrap()
        .lamports;
    assert_eq!(
        after,
        before + expected_rent,
        "rent must be returned to the authority"
    );
}

// ---------------------------------------------------------------------------------------------
// 7. Chunk lane end to end: OpenBatch -> chunk Open/Write/Seal -> SealLeaf -> FinalizeBatch ->
//    Close before final root fails -> Close after final root succeeds and reclaims rent.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn chunk_lane_end_to_end_open_write_seal_close_requires_final_root() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let chain_id = 42;
    let batch = 1;
    let idx = 0;
    let body = b"hello rome-zk".to_vec();

    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let authority = funded_keypair();
    let (root, _) = client::root_pda(&settlement_program, chain_id);
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &authority.pubkey(), settlement_program),
    );
    pt.add_account(
        client::cursor_pda(&program_id, &settlement_program, chain_id).0,
        cursor_account_for(2, program_id, chain_id, batch),
    );
    pt.add_account(
        authority.pubkey(),
        Account {
            lamports: 10_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    let mut ctx = pt.start_with_context().await;

    send(
        &mut ctx,
        &[client::open_batch_ix(
            &program_id,
            &authority.pubkey(),
            chain_id,
            batch,
            1,
            &settlement_program,
        )],
        &authority,
        &[],
    )
    .await
    .expect("OpenBatch must succeed for the root's authority");

    send(
        &mut ctx,
        &[client::open_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            body.len() as u32,
        )],
        &authority,
        &[],
    )
    .await
    .expect("Open must succeed once the batch exists and the signer is its authority");
    send(
        &mut ctx,
        &[client::write_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            0,
            body.clone(),
        )],
        &authority,
        &[],
    )
    .await
    .expect("Write must still work");
    send(
        &mut ctx,
        &[client::seal_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            body.len() as u32,
            client::chunk_body_hash(&body),
        )],
        &authority,
        &[],
    )
    .await
    .expect("Seal must still work");

    let (cpda, _) = client::chunk_pda(&program_id, &settlement_program, chain_id, batch, idx);
    let acct = ctx
        .banks_client
        .get_account(cpda)
        .await
        .unwrap()
        .expect("chunk account must exist");
    assert_eq!(
        &acct.data[client::CHUNK_HEADER_LEN..client::CHUNK_HEADER_LEN + body.len()],
        body.as_slice(),
        "Write must still place bytes at the right offset"
    );
    assert_eq!(acct.data[60], 1, "Seal must still mark the chunk sealed");

    // Close while the batch exists but is not yet finalized: must fail (a live batch
    // is neither "final" nor "abandoned").
    let close_ix = client::close_chunk_ix(
        &program_id,
        &authority.pubkey(),
        &settlement_program,
        chain_id,
        batch,
        idx,
    );
    let err = send(&mut ctx, std::slice::from_ref(&close_ix), &authority, &[])
        .await
        .unwrap_err();
    assert!(
        matches!(err, TransactionError::InstructionError(_, _)),
        "Close must fail while the batch is live and not finalized: {err:?}"
    );

    // SealLeaf + FinalizeBatch, then advance the root to final: now Close must succeed.
    send(
        &mut ctx,
        &[client::seal_leaf_ix(
            &program_id,
            &settlement_program,
            chain_id,
            batch,
            idx,
        )],
        &authority,
        &[],
    )
    .await
    .expect("SealLeaf must succeed");
    send(
        &mut ctx,
        &[finalize_v2(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            0,
        )],
        &authority,
        &[],
    )
    .await
    .expect("FinalizeBatch must succeed");
    ctx.set_account(
        &root,
        &solana_sdk::account::AccountSharedData::from(root_account(
            chain_id,
            batch,
            settlement_program,
        )),
    );

    let before = ctx
        .banks_client
        .get_account(authority.pubkey())
        .await
        .unwrap()
        .unwrap()
        .lamports;
    send(&mut ctx, std::slice::from_ref(&close_ix), &authority, &[])
        .await
        .expect("Close must succeed once the covering root is final");
    assert!(
        ctx.banks_client.get_account(cpda).await.unwrap().is_none(),
        "chunk account must be closed"
    );
    let after = ctx
        .banks_client
        .get_account(authority.pubkey())
        .await
        .unwrap()
        .unwrap()
        .lamports;
    assert!(
        after > before,
        "rent must be returned to the chunk authority"
    );
}

// ---------------------------------------------------------------------------------------------
// 7b. Seal enforces body_hash == keccak(body[..len]): a short-seal — a hole
//     `Write` never covered — is unconstructable at the core, not merely a client-side bound.
// ---------------------------------------------------------------------------------------------

/// Common scaffolding for the `body_hash` tests below: a funded chain authority, its root account
/// (with a real `authority` for `OpenBatch`'s authority check), a fresh batch cursor, and one
/// `OpenBatch(expected_count: 1)` already sent — mirrors `chunk_lane_end_to_end_*`'s setup exactly, so
/// these tests exercise the identical real Open/Write/Seal path, not a shortcut.
async fn open_one_chunk_batch(
    program_id: Pubkey,
    settlement_program: Pubkey,
    chain_id: u64,
    batch: u64,
) -> (solana_program_test::ProgramTestContext, Keypair) {
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let authority = funded_keypair();
    let (root, _) = client::root_pda(&settlement_program, chain_id);
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &authority.pubkey(), settlement_program),
    );
    pt.add_account(
        client::cursor_pda(&program_id, &settlement_program, chain_id).0,
        cursor_account_for(2, program_id, chain_id, batch),
    );
    pt.add_account(
        authority.pubkey(),
        Account {
            lamports: 10_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    let mut ctx = pt.start_with_context().await;
    send(
        &mut ctx,
        &[client::open_batch_ix(
            &program_id,
            &authority.pubkey(),
            chain_id,
            batch,
            1,
            &settlement_program,
        )],
        &authority,
        &[],
    )
    .await
    .expect("OpenBatch must succeed for the root's authority");
    (ctx, authority)
}

/// Test 1: a chunk with one 900-B hole — `Open`ed at the full intended size, `Write`
/// covers only the first and last parts, the 900 B in between is never written (stays zero) — then
/// `Seal` is sent with the hash of the FULL intended body (what a client that forgot to write the hole,
/// or hashed its source buffer instead of what it actually sent, would compute). Pre-fix this sealed
/// unconditionally (`Seal { len }` never looked at the bytes); post-fix the program recomputes
/// `keccak256(body[..len])` from the account itself and must reject the mismatch.
#[tokio::test]
async fn seal_rejects_a_short_seal_with_an_unwritten_hole() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let chain_id = 900;
    let batch = 1;
    let idx = 0;

    // Deterministic non-zero content throughout, so the unwritten hole (left as zero by the runtime)
    // is byte-for-byte distinguishable from the honest content — a hole of all-zero bytes could
    // otherwise coincidentally match a mostly-zero intended body.
    let full_body: Vec<u8> = (0..2_000u32).map(|i| ((i % 251) + 1) as u8).collect();
    let hole_start = 550usize;
    let hole_len = 900usize;
    let hole_end = hole_start + hole_len;

    let (mut ctx, authority) =
        open_one_chunk_batch(program_id, settlement_program, chain_id, batch).await;

    send(
        &mut ctx,
        &[client::open_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            full_body.len() as u32,
        )],
        &authority,
        &[],
    )
    .await
    .expect("Open must succeed");
    send(
        &mut ctx,
        &[client::write_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            0,
            full_body[..hole_start].to_vec(),
        )],
        &authority,
        &[],
    )
    .await
    .expect("Write the first part (before the hole)");
    send(
        &mut ctx,
        &[client::write_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            hole_end as u32,
            full_body[hole_end..].to_vec(),
        )],
        &authority,
        &[],
    )
    .await
    .expect("Write the last part (after the hole) — [hole_start, hole_end) is never written");

    // Seal claims the FULL intended body's hash — but the account only has the first+last parts; the
    // 900-B hole is still zero. This must be rejected.
    let claimed_hash = client::chunk_body_hash(&full_body);
    let seal_ix = client::seal_chunk_ix(
        &program_id,
        &authority.pubkey(),
        &settlement_program,
        chain_id,
        batch,
        idx,
        full_body.len() as u32,
        claimed_hash,
    );
    let err = send(&mut ctx, &[seal_ix], &authority, &[])
        .await
        .expect_err(
            "Seal must reject a short-seal hole: the on-chain bytes don't hash to body_hash",
        );
    let msg = format!("{err:?}");
    assert!(
        msg.contains("Custom(100)"),
        "expected SealHashMismatch (Custom(100)), got {msg}"
    );

    // Fail-closed: the chunk must not be left sealed after a rejected Seal.
    let (cpda, _) = client::chunk_pda(&program_id, &settlement_program, chain_id, batch, idx);
    let acct = ctx
        .banks_client
        .get_account(cpda)
        .await
        .unwrap()
        .expect("chunk account must still exist");
    assert_eq!(
        acct.data[60], // OFF_SEALED
        0,
        "a rejected Seal must not mark the chunk sealed"
    );
}

/// Test 2: the honest path — `Write` covers the whole body, `Seal` carries the correct
/// `body_hash` — is accepted, and `SealLeaf` + `FinalizeBatch` produce an `acc` that matches the
/// off-chain `reference_commitment` exactly (existing chunk-lane assertion, now driven through the new
/// `Seal{len, body_hash}` shape).
#[tokio::test]
async fn seal_accepts_the_correct_hash_and_acc_matches_the_reference() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let chain_id = 901;
    let batch = 1;
    let idx = 0;
    let body = b"rome-zk chunk body: the quick brown fox jumps over the lazy dog".to_vec();

    let (mut ctx, authority) =
        open_one_chunk_batch(program_id, settlement_program, chain_id, batch).await;

    send(
        &mut ctx,
        &[client::open_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            body.len() as u32,
        )],
        &authority,
        &[],
    )
    .await
    .expect("Open must succeed");
    send(
        &mut ctx,
        &[client::write_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            0,
            body.clone(),
        )],
        &authority,
        &[],
    )
    .await
    .expect("Write the whole body");

    let hash = client::chunk_body_hash(&body);
    send(
        &mut ctx,
        &[client::seal_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            body.len() as u32,
            hash,
        )],
        &authority,
        &[],
    )
    .await
    .expect("Seal with the correct body_hash must be accepted");

    send(
        &mut ctx,
        &[client::seal_leaf_ix(
            &program_id,
            &settlement_program,
            chain_id,
            batch,
            idx,
        )],
        &authority,
        &[],
    )
    .await
    .expect("SealLeaf");
    send(
        &mut ctx,
        &[finalize_v2(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            0,
        )],
        &authority,
        &[],
    )
    .await
    .expect("FinalizeBatch");

    let (batch_pda, _) = client::batch_pda(&program_id, &settlement_program, chain_id, batch);
    let acct = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .unwrap();
    let decoded = client::decode_batch_account(&acct.data).unwrap();
    assert!(decoded.finalized);
    let (_root, _forced_root, expected_acc) =
        client::reference_commitment(chain_id, batch, decoded.open_slot, &[hash]);
    assert_eq!(
        decoded.acc, expected_acc,
        "acc must match the off-chain reference once Seal has verified the real body bytes"
    );
}

/// Test 3: the honest short-seal case — the client's own `body_hash` matches whatever bytes it
/// actually wrote (here, a hole it never filled), so `Seal` accepts it (nothing on chain can
/// distinguish "the client wrote a hole on purpose" from "the client wrote a hole by bug" — both hash
/// the same bytes it put there). `SealLeaf`'s leaf is then `keccak(hole-y bytes)`, not
/// `keccak(intended full body)`. This is expected and is the honest case: the client sealed exactly
/// what it wrote; catching a client that *silently intended* different bytes is the batcher's own
/// re-derive-before-send check against its own frame (crate `rome-zk-batcher`, out of scope here),
/// not something the chunk program's `Seal` can or should judge.
#[tokio::test]
async fn seal_accepts_the_hash_of_whatever_was_actually_written_hole_included() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let chain_id = 902;
    let batch = 1;
    let idx = 0;

    let full_body: Vec<u8> = (0..2_000u32).map(|i| ((i % 251) + 1) as u8).collect();
    let hole_start = 550usize;
    let hole_len = 900usize;
    let hole_end = hole_start + hole_len;

    let (mut ctx, authority) =
        open_one_chunk_batch(program_id, settlement_program, chain_id, batch).await;

    send(
        &mut ctx,
        &[client::open_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            full_body.len() as u32,
        )],
        &authority,
        &[],
    )
    .await
    .expect("Open must succeed");
    send(
        &mut ctx,
        &[client::write_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            0,
            full_body[..hole_start].to_vec(),
        )],
        &authority,
        &[],
    )
    .await
    .expect("Write the first part (before the hole)");
    send(
        &mut ctx,
        &[client::write_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            hole_end as u32,
            full_body[hole_end..].to_vec(),
        )],
        &authority,
        &[],
    )
    .await
    .expect("Write the last part (after the hole)");

    // What is actually on chain: the intended body with the hole left as zero.
    let mut hole_y_bytes = full_body.clone();
    hole_y_bytes[hole_start..hole_end].fill(0);
    let honest_hash = client::chunk_body_hash(&hole_y_bytes);

    send(
        &mut ctx,
        &[client::seal_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            full_body.len() as u32,
            honest_hash,
        )],
        &authority,
        &[],
    )
    .await
    .expect("Seal must accept a body_hash that matches exactly what is on chain, hole included");

    send(
        &mut ctx,
        &[client::seal_leaf_ix(
            &program_id,
            &settlement_program,
            chain_id,
            batch,
            idx,
        )],
        &authority,
        &[],
    )
    .await
    .expect("SealLeaf");

    let (batch_pda, _) = client::batch_pda(&program_id, &settlement_program, chain_id, batch);
    let acct = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .unwrap();
    // leaves_offset/header layout mirrors batch.rs — read the one leaf hash directly.
    let lo = leaves_offset_for(acct.data[zk_inbox::batch::OFF_VERSION], 1).unwrap();
    let leaf_hash: [u8; 32] = acct.data[lo..lo + 32].try_into().unwrap();
    assert_eq!(
        leaf_hash, honest_hash,
        "SealLeaf's leaf must be keccak(hole-y bytes) — exactly what was written, not the clean intent"
    );
    assert_ne!(
        leaf_hash,
        client::chunk_body_hash(&full_body),
        "sanity: the hole-y hash must differ from the clean intended body's hash"
    );
}

/// Test 4: Seal's CU cost on the design's own max frame body (3,681 B) — the added cost
/// is one `keccak::hashv` syscall over up to that many bytes.
/// Baseline measured on origin/main (before `body_hash`, same 3,681-B body, same setup): **831 CU**.
#[tokio::test]
async fn seal_cu_on_the_max_frame_body_3681_bytes() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let chain_id = 903;
    let batch = 1;
    let idx = 0;
    let body = vec![0x5au8; 3681];

    let (mut ctx, authority) =
        open_one_chunk_batch(program_id, settlement_program, chain_id, batch).await;

    send(
        &mut ctx,
        &[client::open_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            body.len() as u32,
        )],
        &authority,
        &[],
    )
    .await
    .expect("Open");
    send(
        &mut ctx,
        &[client::write_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            0,
            body.clone(),
        )],
        &authority,
        &[],
    )
    .await
    .expect("Write");

    let cu = send(
        &mut ctx,
        &[client::seal_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            body.len() as u32,
            client::chunk_body_hash(&body),
        )],
        &authority,
        &[],
    )
    .await
    .expect("Seal (with body_hash verification) must succeed on the correct hash");
    eprintln!(
        "Seal(3,681-B body) consumed {cu} CU (post-body_hash; the baseline before \
         the body hash was 831 CU)"
    );
}

// ---------------------------------------------------------------------------------------------
// 7c. A sealed chunk is immutable in bytes AND length:
//     `Seal` used to ignore the `sealed` flag entirely, so an authority could re-Seal a shorter `len`
//     after `SealLeaf` — the header would then say a smaller length than the leaf hash `SealLeaf`
//     already committed to the batch account, so honest readers re-deriving from Solana DA get an `acc`
//     mismatch (operator-side data withholding, undetectable at `PostRoot`). Bytes were already
//     protected (Write-after-Seal rejected, tested in 7d below) — only `len` was left open.
// ---------------------------------------------------------------------------------------------

/// Test: Open → Write(1,000 B) → Seal(1000, hash) → SealLeaf → a re-Seal with a shorter `len` and the
/// *correct* prefix hash (i.e. not even a hash mismatch — this re-Seal is "honest" about the bytes it
/// names, it just claims a different length) must be rejected with the new
/// `ChunkError::AlreadySealed`, and the header must come out byte-for-byte unchanged.
#[tokio::test]
async fn reseal_with_a_shorter_len_after_seal_leaf_is_rejected_and_header_unchanged() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let chain_id = 904;
    let batch = 1;
    let idx = 0;
    let full_body: Vec<u8> = (0..1_000u32).map(|i| ((i % 251) + 1) as u8).collect();

    let (mut ctx, authority) =
        open_one_chunk_batch(program_id, settlement_program, chain_id, batch).await;

    send(
        &mut ctx,
        &[client::open_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            full_body.len() as u32,
        )],
        &authority,
        &[],
    )
    .await
    .expect("Open must succeed");
    send(
        &mut ctx,
        &[client::write_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            0,
            full_body.clone(),
        )],
        &authority,
        &[],
    )
    .await
    .expect("Write the whole body");

    let full_hash = client::chunk_body_hash(&full_body);
    send(
        &mut ctx,
        &[client::seal_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            full_body.len() as u32,
            full_hash,
        )],
        &authority,
        &[],
    )
    .await
    .expect("Seal(1,000, full_hash) must succeed");
    send(
        &mut ctx,
        &[client::seal_leaf_ix(
            &program_id,
            &settlement_program,
            chain_id,
            batch,
            idx,
        )],
        &authority,
        &[],
    )
    .await
    .expect("SealLeaf must commit the 1,000-byte leaf");

    let (cpda, _) = client::chunk_pda(&program_id, &settlement_program, chain_id, batch, idx);
    let before = ctx
        .banks_client
        .get_account(cpda)
        .await
        .unwrap()
        .expect("chunk account exists before the re-Seal attempt");

    // A shorter re-Seal, honest about the (shorter) bytes it names — the correct prefix hash, not a
    // hash mismatch. Pre-fix this was accepted outright: `len` would drop to 500 while the batch
    // account's committed leaf still hashes the original 1,000-byte body.
    let short_len = 500u32;
    let short_hash = client::chunk_body_hash(&full_body[..short_len as usize]);
    let err = send(
        &mut ctx,
        &[client::seal_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            short_len,
            short_hash,
        )],
        &authority,
        &[],
    )
    .await
    .expect_err(
        "a shorter re-Seal after SealLeaf must be rejected: a sealed chunk is immutable in bytes AND \
         length",
    );
    let msg = format!("{err:?}");
    assert!(
        msg.contains("Custom(101)"),
        "expected AlreadySealed (Custom(101)), got {msg}"
    );

    let after = ctx
        .banks_client
        .get_account(cpda)
        .await
        .unwrap()
        .expect("chunk account still exists after the rejected re-Seal");
    assert_eq!(
        before.data, after.data,
        "a rejected re-Seal must leave the chunk account (header included) byte-for-byte unchanged"
    );
}

/// Test: the batcher's resubmit path re-sends the exact same `Seal` after a dropped confirmation — an
/// identical re-Seal (same `len`, same `body_hash`) after `SealLeaf` must stay an idempotent `Ok`, not
/// start rejecting a client that never changed anything.
#[tokio::test]
async fn identical_reseal_after_seal_leaf_stays_idempotent_ok() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let chain_id = 905;
    let batch = 1;
    let idx = 0;
    let body: Vec<u8> = (0..600u32).map(|i| ((i % 251) + 1) as u8).collect();

    let (mut ctx, authority) =
        open_one_chunk_batch(program_id, settlement_program, chain_id, batch).await;

    send(
        &mut ctx,
        &[client::open_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            body.len() as u32,
        )],
        &authority,
        &[],
    )
    .await
    .expect("Open must succeed");
    send(
        &mut ctx,
        &[client::write_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            0,
            body.clone(),
        )],
        &authority,
        &[],
    )
    .await
    .expect("Write the whole body");

    let hash = client::chunk_body_hash(&body);
    let seal_ix = client::seal_chunk_ix(
        &program_id,
        &authority.pubkey(),
        &settlement_program,
        chain_id,
        batch,
        idx,
        body.len() as u32,
        hash,
    );
    send(&mut ctx, std::slice::from_ref(&seal_ix), &authority, &[])
        .await
        .expect("first Seal must succeed");
    send(
        &mut ctx,
        &[client::seal_leaf_ix(
            &program_id,
            &settlement_program,
            chain_id,
            batch,
            idx,
        )],
        &authority,
        &[],
    )
    .await
    .expect("SealLeaf");

    // The batcher's resubmit path: exactly the same Seal instruction, sent again after SealLeaf.
    send(&mut ctx, &[seal_ix], &authority, &[])
        .await
        .expect("an identical re-Seal (same len, same body_hash) must stay an idempotent Ok");

    let (cpda, _) = client::chunk_pda(&program_id, &settlement_program, chain_id, batch, idx);
    let acct = ctx
        .banks_client
        .get_account(cpda)
        .await
        .unwrap()
        .expect("chunk account exists");
    assert_eq!(
        acct.data[client::CHUNK_HEADER_LEN..],
        body[..],
        "body bytes unchanged"
    );
    assert_eq!(acct.data[60], 1, "still sealed"); // OFF_SEALED
}

// ---------------------------------------------------------------------------------------------
// 7d. Write-after-Seal guard: a `Write` on an already sealed chunk must be rejected, pinned here by a
//     real-BPF regression test — the body-hash contract in 7b/7c depends on it (a body a client could
//     still mutate after sealing would make `body_hash` prove nothing).
// ---------------------------------------------------------------------------------------------

/// Test: Open → Write → Seal → Write(different bytes) must fail; the body bytes and header must be
/// unchanged after the rejected Write; `SealLeaf` afterward still yields the hash of the *original*
/// body, not the attempted overwrite.
#[tokio::test]
async fn write_after_seal_is_rejected_and_bytes_unchanged() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let chain_id = 906;
    let batch = 1;
    let idx = 0;
    let original_body: Vec<u8> = (0..800u32).map(|i| ((i % 251) + 1) as u8).collect();

    let (mut ctx, authority) =
        open_one_chunk_batch(program_id, settlement_program, chain_id, batch).await;

    send(
        &mut ctx,
        &[client::open_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            original_body.len() as u32,
        )],
        &authority,
        &[],
    )
    .await
    .expect("Open must succeed");
    send(
        &mut ctx,
        &[client::write_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            0,
            original_body.clone(),
        )],
        &authority,
        &[],
    )
    .await
    .expect("Write the original body");

    let original_hash = client::chunk_body_hash(&original_body);
    send(
        &mut ctx,
        &[client::seal_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            original_body.len() as u32,
            original_hash,
        )],
        &authority,
        &[],
    )
    .await
    .expect("Seal must succeed");

    let (cpda, _) = client::chunk_pda(&program_id, &settlement_program, chain_id, batch, idx);
    let before = ctx
        .banks_client
        .get_account(cpda)
        .await
        .unwrap()
        .expect("chunk account exists before the attempted overwrite");

    // Different bytes, same offset/length — a client trying to mutate a sealed chunk's body.
    let different_bytes: Vec<u8> = original_body.iter().map(|b| b.wrapping_add(1)).collect();
    send(
        &mut ctx,
        &[client::write_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            0,
            different_bytes,
        )],
        &authority,
        &[],
    )
    .await
    .expect_err("Write on an already-sealed chunk must be rejected");

    let after = ctx
        .banks_client
        .get_account(cpda)
        .await
        .unwrap()
        .expect("chunk account still exists after the rejected Write");
    assert_eq!(
        before.data, after.data,
        "a rejected Write-after-Seal must leave the chunk account (body and header) byte-for-byte unchanged"
    );

    // SealLeaf still yields the original hash — proves the account's bytes were never actually
    // overwritten, not merely that the second Write returned an error.
    send(
        &mut ctx,
        &[client::seal_leaf_ix(
            &program_id,
            &settlement_program,
            chain_id,
            batch,
            idx,
        )],
        &authority,
        &[],
    )
    .await
    .expect("SealLeaf");
    let (batch_pda, _) = client::batch_pda(&program_id, &settlement_program, chain_id, batch);
    let batch_acct = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .unwrap();
    let lo = leaves_offset_for(batch_acct.data[zk_inbox::batch::OFF_VERSION], 1).unwrap();
    let leaf_hash: [u8; 32] = batch_acct.data[lo..lo + 32].try_into().unwrap();
    assert_eq!(
        leaf_hash, original_hash,
        "SealLeaf must commit the ORIGINAL body's hash — the rejected Write never took effect"
    );
}

// ---------------------------------------------------------------------------------------------
// 8. Chunk Open binding: batch must exist, signer must be its authority.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn open_chunk_rejects_without_a_batch_account() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let mut ctx = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    )
    .start_with_context()
    .await;
    let payer = ctx.payer.insecure_clone();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let err = send(
        &mut ctx,
        &[client::open_chunk_ix(
            &program_id,
            &payer.pubkey(),
            &settlement_program,
            7,
            1,
            0,
            8,
        )],
        &payer,
        &[],
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, TransactionError::InstructionError(_, _)),
        "chunk Open must fail when the batch account does not exist: {err:?}"
    );
}

/// The chunk-lane instance of the pre-funding attack on batch PDAs: an attacker prefunds a chunk PDA before the
/// real batch authority's `Open` lands. Must adopt the donated lamports (`create_or_adopt_pda`), never
/// permanently fail `AccountAlreadyInUse` — with sequential batch ids the batcher can no longer just skip
/// the touched index either, so this is exactly as core a fix as the batch/cursor PDA sites.
#[tokio::test]
async fn open_chunk_succeeds_even_when_an_attacker_prefunds_its_pda() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let authority = funded_keypair();
    let fixture = BatchFixture {
        chain_id: 7,
        batch: 1,
        open_slot: 5,
        expected_count: 2,
        settlement_program: Pubkey::new_unique(),
        authority: authority.pubkey(),
    };
    let (batch_pda, _) = client::batch_pda(
        &program_id,
        &fixture.settlement_program,
        fixture.chain_id,
        fixture.batch,
    );
    pt.add_account(batch_pda, fixture.account(program_id, 0, &[], &[], false));
    pt.add_account(
        authority.pubkey(),
        Account {
            lamports: 10_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    let mut ctx = pt.start_with_context().await;

    let idx = 0u32;
    let size = 8u32;
    let (chunk_pda, _) = client::chunk_pda(
        &program_id,
        &fixture.settlement_program,
        fixture.chain_id,
        fixture.batch,
        idx,
    );
    prefund_pda(&mut ctx, chunk_pda).await;
    let donated_lamports = ctx
        .banks_client
        .get_account(chunk_pda)
        .await
        .unwrap()
        .expect("the prefund transfer must have created a system-owned account")
        .lamports;
    assert!(donated_lamports > 0);

    send(
        &mut ctx,
        &[client::open_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &fixture.settlement_program,
            fixture.chain_id,
            fixture.batch,
            idx,
            size,
        )],
        &authority,
        &[],
    )
    .await
    .expect("chunk Open must adopt a pre-funded PDA rather than fail AccountAlreadyInUse");

    let acct = ctx
        .banks_client
        .get_account(chunk_pda)
        .await
        .unwrap()
        .expect("chunk account must exist");
    assert_eq!(acct.owner, program_id);
    assert_eq!(acct.data.len(), zk_inbox::HEADER_LEN + size as usize);
    assert!(
        acct.lamports >= donated_lamports,
        "the attacker's donated lamports must still be part of the account"
    );
}

#[tokio::test]
async fn open_chunk_rejects_non_batch_authority_signer() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let fixture = BatchFixture {
        chain_id: 7,
        batch: 1,
        open_slot: 5,
        expected_count: 2,
        settlement_program: Pubkey::new_unique(),
        authority: Pubkey::new_unique(), // not the payer below
    };
    let (batch_pda, _) = client::batch_pda(
        &program_id,
        &fixture.settlement_program,
        fixture.chain_id,
        fixture.batch,
    );
    pt.add_account(batch_pda, fixture.account(program_id, 0, &[], &[], false));
    let mut ctx = pt.start_with_context().await;
    let payer = ctx.payer.insecure_clone();
    let err = send(
        &mut ctx,
        &[client::open_chunk_ix(
            &program_id,
            &payer.pubkey(),
            &fixture.settlement_program,
            fixture.chain_id,
            fixture.batch,
            0,
            8,
        )],
        &payer,
        &[],
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, TransactionError::InstructionError(_, _)),
        "chunk Open must fail when the signer is not the batch's authority: {err:?}"
    );
}

#[tokio::test]
async fn open_chunk_rejects_idx_beyond_expected_count() {
    // `open_chunk_check` orders its checks finalized -> idx -> authority, so idx-out-of-range is hit
    // regardless of who signs; the batch's `authority` here is deliberately unrelated to the signer.
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let fixture = BatchFixture {
        chain_id: 7,
        batch: 1,
        open_slot: 5,
        expected_count: 2,
        settlement_program: Pubkey::new_unique(),
        authority: Pubkey::new_unique(),
    };
    let (batch_pda, _) = client::batch_pda(
        &program_id,
        &fixture.settlement_program,
        fixture.chain_id,
        fixture.batch,
    );
    pt.add_account(batch_pda, fixture.account(program_id, 0, &[], &[], false));
    let mut ctx = pt.start_with_context().await;
    let payer = ctx.payer.insecure_clone();
    let err = send(
        &mut ctx,
        &[client::open_chunk_ix(
            &program_id,
            &payer.pubkey(),
            &fixture.settlement_program,
            fixture.chain_id,
            fixture.batch,
            2, // == expected_count, out of range
            8,
        )],
        &payer,
        &[],
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, TransactionError::InstructionError(_, _)),
        "chunk Open must reject idx >= expected_count: {err:?}"
    );
}

#[tokio::test]
async fn open_chunk_rejects_finalized_batch() {
    // Same ordering note as above: finalized is checked before authority.
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let fixture = BatchFixture {
        chain_id: 7,
        batch: 1,
        open_slot: 5,
        expected_count: 1,
        settlement_program: Pubkey::new_unique(),
        authority: Pubkey::new_unique(),
    };
    let leaves = small_leaf_set(1);
    let (batch_pda, _) = client::batch_pda(
        &program_id,
        &fixture.settlement_program,
        fixture.chain_id,
        fixture.batch,
    );
    pt.add_account(
        batch_pda,
        fixture.account(program_id, 1, &leaves, &[0], true),
    );
    let mut ctx = pt.start_with_context().await;
    let payer = ctx.payer.insecure_clone();
    let err = send(
        &mut ctx,
        &[client::open_chunk_ix(
            &program_id,
            &payer.pubkey(),
            &fixture.settlement_program,
            fixture.chain_id,
            fixture.batch,
            0,
            8,
        )],
        &payer,
        &[],
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, TransactionError::InstructionError(_, _)),
        "chunk Open must reject a finalized batch: {err:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// 9. AbandonBatch: authority-only, only while not finalized; returns rent, and
//    the abandoned batch's chunks then close for the chunk authority alone.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn abandon_batch_returns_rent_when_not_finalized() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let authority = Keypair::new();
    let fixture = BatchFixture {
        chain_id: 3,
        batch: 9,
        open_slot: 1,
        expected_count: 4,
        settlement_program: Pubkey::new_unique(),
        authority: authority.pubkey(),
    };
    let (batch_pda, _) = client::batch_pda(
        &program_id,
        &fixture.settlement_program,
        fixture.chain_id,
        fixture.batch,
    );
    let batch_account = fixture.account(program_id, 0, &[], &[], false);
    let expected_rent = batch_account.lamports;
    pt.add_account(batch_pda, batch_account);
    pt.add_account(
        authority.pubkey(),
        Account {
            lamports: 10_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    let mut ctx = pt.start_with_context().await;
    let payer = ctx.payer.insecure_clone();

    let before = ctx
        .banks_client
        .get_account(authority.pubkey())
        .await
        .unwrap()
        .unwrap()
        .lamports;
    send(
        &mut ctx,
        &[client::abandon_batch_ix(
            &program_id,
            &authority.pubkey(),
            &fixture.settlement_program,
            fixture.chain_id,
            fixture.batch,
        )],
        &payer,
        &[&authority],
    )
    .await
    .expect("AbandonBatch should succeed for the batch authority while not finalized");
    assert!(
        ctx.banks_client
            .get_account(batch_pda)
            .await
            .unwrap()
            .is_none(),
        "batch account must be closed"
    );
    let after = ctx
        .banks_client
        .get_account(authority.pubkey())
        .await
        .unwrap()
        .unwrap()
        .lamports;
    assert_eq!(
        after,
        before + expected_rent,
        "rent must return to the authority"
    );
}

#[tokio::test]
async fn abandon_batch_rejects_non_authority_signer() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let authority = Pubkey::new_unique();
    let impostor = Keypair::new();
    let fixture = BatchFixture {
        chain_id: 3,
        batch: 9,
        open_slot: 1,
        expected_count: 4,
        settlement_program: Pubkey::new_unique(),
        authority,
    };
    let (batch_pda, _) = client::batch_pda(
        &program_id,
        &fixture.settlement_program,
        fixture.chain_id,
        fixture.batch,
    );
    pt.add_account(batch_pda, fixture.account(program_id, 0, &[], &[], false));
    pt.add_account(
        impostor.pubkey(),
        Account {
            lamports: 10_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    let mut ctx = pt.start_with_context().await;
    let payer = ctx.payer.insecure_clone();
    let err = send(
        &mut ctx,
        &[client::abandon_batch_ix(
            &program_id,
            &impostor.pubkey(),
            &fixture.settlement_program,
            fixture.chain_id,
            fixture.batch,
        )],
        &payer,
        &[&impostor],
    )
    .await
    .unwrap_err();
    assert!(matches!(err, TransactionError::InstructionError(_, _)));
}

#[tokio::test]
async fn abandon_batch_rejects_a_finalized_batch() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let authority = Keypair::new();
    let fixture = BatchFixture {
        chain_id: 3,
        batch: 9,
        open_slot: 1,
        expected_count: 1,
        settlement_program: Pubkey::new_unique(),
        authority: authority.pubkey(),
    };
    let leaves = small_leaf_set(1);
    let (batch_pda, _) = client::batch_pda(
        &program_id,
        &fixture.settlement_program,
        fixture.chain_id,
        fixture.batch,
    );
    pt.add_account(
        batch_pda,
        fixture.account(program_id, 1, &leaves, &[0], true),
    );
    pt.add_account(
        authority.pubkey(),
        Account {
            lamports: 10_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    let mut ctx = pt.start_with_context().await;
    let payer = ctx.payer.insecure_clone();
    let err = send(
        &mut ctx,
        &[client::abandon_batch_ix(
            &program_id,
            &authority.pubkey(),
            &fixture.settlement_program,
            fixture.chain_id,
            fixture.batch,
        )],
        &payer,
        &[&authority],
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, TransactionError::InstructionError(_, _)),
        "a finalized batch must not be abandonable: {err:?}"
    );
}

#[tokio::test]
async fn chunk_close_succeeds_for_an_abandoned_batch_via_chunk_authority_alone() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let chain_id = 55;
    let batch = 2;
    let idx = 0;
    let body = b"never posted".to_vec();

    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let authority = funded_keypair();
    let (root, _) = client::root_pda(&settlement_program, chain_id);
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &authority.pubkey(), settlement_program),
    );
    pt.add_account(
        client::cursor_pda(&program_id, &settlement_program, chain_id).0,
        cursor_account(program_id, chain_id, batch),
    );
    pt.add_account(
        authority.pubkey(),
        Account {
            lamports: 10_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    let mut ctx = pt.start_with_context().await;
    let payer = authority;

    // Open a batch, then open+write+seal one chunk under it.
    send(
        &mut ctx,
        &[client::open_batch_ix(
            &program_id,
            &payer.pubkey(),
            chain_id,
            batch,
            1,
            &settlement_program,
        )],
        &payer,
        &[],
    )
    .await
    .expect("OpenBatch must succeed for the root's authority");
    send(
        &mut ctx,
        &[client::open_chunk_ix(
            &program_id,
            &payer.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            body.len() as u32,
        )],
        &payer,
        &[],
    )
    .await
    .expect("Open must succeed once the batch exists");
    send(
        &mut ctx,
        &[client::write_chunk_ix(
            &program_id,
            &payer.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            0,
            body.clone(),
        )],
        &payer,
        &[],
    )
    .await
    .expect("Write");
    send(
        &mut ctx,
        &[client::seal_chunk_ix(
            &program_id,
            &payer.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            body.len() as u32,
            client::chunk_body_hash(&body),
        )],
        &payer,
        &[],
    )
    .await
    .expect("Seal");

    // Abandon the batch before it is ever posted.
    send(
        &mut ctx,
        &[client::abandon_batch_ix(
            &program_id,
            &payer.pubkey(),
            &settlement_program,
            chain_id,
            batch,
        )],
        &payer,
        &[],
    )
    .await
    .expect("AbandonBatch must succeed while not finalized");

    // Now the chunk must be closable for its own authority alone — no batch/root check applies.
    let close_ix = client::close_chunk_ix(
        &program_id,
        &payer.pubkey(),
        &settlement_program,
        chain_id,
        batch,
        idx,
    );
    let (cpda, _) = client::chunk_pda(&program_id, &settlement_program, chain_id, batch, idx);
    send(&mut ctx, &[close_ix], &payer, &[])
        .await
        .expect("Close must succeed for the chunk authority once the batch has been abandoned");
    assert!(
        ctx.banks_client.get_account(cpda).await.unwrap().is_none(),
        "chunk account must be closed"
    );
}

#[tokio::test]
async fn chunk_close_rejects_a_foreign_system_account_posing_as_an_absent_batch() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let chain_id = 55;
    let batch = 2;
    let idx = 0;
    let body = b"live batch".to_vec();

    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let authority = funded_keypair();
    let (root, _) = client::root_pda(&settlement_program, chain_id);
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &authority.pubkey(), settlement_program),
    );
    pt.add_account(
        client::cursor_pda(&program_id, &settlement_program, chain_id).0,
        cursor_account(program_id, chain_id, batch),
    );
    pt.add_account(
        authority.pubkey(),
        Account {
            lamports: 10_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    let mut ctx = pt.start_with_context().await;
    let payer = authority;

    // Open a batch, then open+write+seal one chunk under it.
    send(
        &mut ctx,
        &[client::open_batch_ix(
            &program_id,
            &payer.pubkey(),
            chain_id,
            batch,
            1,
            &settlement_program,
        )],
        &payer,
        &[],
    )
    .await
    .expect("OpenBatch must succeed for the root's authority");
    send(
        &mut ctx,
        &[client::open_chunk_ix(
            &program_id,
            &payer.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            body.len() as u32,
        )],
        &payer,
        &[],
    )
    .await
    .expect("Open must succeed once the batch exists");
    send(
        &mut ctx,
        &[client::write_chunk_ix(
            &program_id,
            &payer.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            0,
            body.clone(),
        )],
        &payer,
        &[],
    )
    .await
    .expect("Write");
    send(
        &mut ctx,
        &[client::seal_chunk_ix(
            &program_id,
            &payer.pubkey(),
            &settlement_program,
            chain_id,
            batch,
            idx,
            body.len() as u32,
            client::chunk_body_hash(&body),
        )],
        &payer,
        &[],
    )
    .await
    .expect("Seal");

    // The batch is LIVE (open, not abandoned). A closer who swaps the batch account for an arbitrary
    // system-owned account must not be able to pass the "batch absent → chunk authority alone" rule:
    // that would let the chain authority delete DA of a pending batch.
    let impostor_batch = Pubkey::new_unique();
    let mut close_ix = client::close_chunk_ix(
        &program_id,
        &payer.pubkey(),
        &settlement_program,
        chain_id,
        batch,
        idx,
    );
    close_ix.accounts[2] =
        solana_program::instruction::AccountMeta::new_readonly(impostor_batch, false);
    let err = send(&mut ctx, &[close_ix], &payer, &[]).await.expect_err(
        "Close with a foreign account in the batch slot must be rejected while the batch is live",
    );
    let msg = format!("{err:?}");
    assert!(
        msg.contains("Custom(8)") || msg.contains("InvalidSeeds"),
        "expected WrongBatchAccount/InvalidSeeds, got {msg}"
    );
}

/// `FinalizeBatchV2` over an empty deposit range (the chain has no deposits, the cursor's `deposit_next` is 0).
fn finalize_v2(
    program_id: &Pubkey,
    authority: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
    batch: u64,
    step: u32,
) -> solana_program::instruction::Instruction {
    client::finalize_batch_v2_ix(
        program_id,
        authority,
        settlement_program,
        chain_id,
        batch,
        step,
        0,
        None,
    )
}
