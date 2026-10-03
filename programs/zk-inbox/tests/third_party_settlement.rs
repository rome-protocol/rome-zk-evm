//! INBOX-THIRD-PARTY-HALT.
//!
//! Before the fix the inbox keyed a chain's batch cursor (`["batch_cursor", chain_id]`) and its batch accounts
//! (`["batch", chain_id, batch]`) by chain id alone, under the inbox program. `InitBatchCursor` and
//! `OpenBatch` take the chain's authority from a root account that lives under whatever
//! `settlement_program` the CALLER names (`load_root`: owner == the named program, address == `["root",
//! chain_id]` under it, `authority` field == signer). So a third party who controls any program can make a
//! root-shaped account for somebody else's chain id under that program and then drive the shared cursor /
//! batch ids of the victim's chain.
//!
//! The accounts are now keyed by the settlement program as well
//! (`["batch_cursor", settlement_program, chain_id]`, `["batch", settlement_program, chain_id, batch]`,
//! `["inbox", settlement_program, chain_id, batch, idx]`), so whatever a third party creates through a
//! program of his own lives at addresses the real chain never reads. Every test here asserts the SAFE
//! behaviour (the third party is refused, or the real chain's own slots are untouched and still usable);
//! before the fix the first kind were red and proved the attack. A test marked "regression guard" holds on
//! both sides of the fix.
//!
//! The "attacker's settlement program" is a plain program id here: the inbox never checks that the owner
//! of a root account is an executable program, so `add_account` of a root-shaped account owned by an id
//! the attacker picks is exactly what a program the attacker deploys could create on a real cluster. The
//! settlement-level tests (programs/zk-settlement/tests/settlement.rs, `third_party_*`) go further and use
//! a genuine second deployment of zk-settlement with a genuine `InitChain` as the attacker's program.
//!
//! Loads the real, `cargo build-sbf`-compiled `zk_inbox.so` (`prefer_bpf`).

use rome_zk_layouts::chainid::PERMISSIONLESS_BASE;
use rome_zk_testkit::{cursor_account, root_account_with_authority};
use solana_program::pubkey::Pubkey;
use solana_program_test::ProgramTestContext;
use solana_sdk::{
    account::Account,
    instruction::InstructionError,
    signature::{Keypair, Signer},
    transaction::TransactionError,
};
use solana_system_interface::program as system_program;
use zk_inbox_client as client;

const ERR_NOT_CHAIN_AUTHORITY: u32 = 9;
const ERR_CURSOR_MISMATCH: u32 = 12;
const ERR_CURSOR_ALREADY_INITIALIZED: u32 = 13;

/// A permissionless chain id is computable by anyone in advance (`rome_zk_layouts::chainid`), so a victim
/// chain id is simply a number in that range here.
const VICTIM_CHAIN: u64 = PERMISSIONLESS_BASE + 12_345;

fn funded_account() -> Account {
    Account {
        lamports: 10_000_000_000,
        data: vec![],
        owner: system_program::id(),
        executable: false,
        rent_epoch: 0,
    }
}

fn custom(err: &TransactionError) -> Option<u32> {
    match err {
        TransactionError::InstructionError(_, InstructionError::Custom(c)) => Some(*c),
        _ => None,
    }
}

async fn send(
    ctx: &mut ProgramTestContext,
    ixs: &[solana_program::instruction::Instruction],
    payer: &Keypair,
) -> Result<(), TransactionError> {
    let (result, _cu, _logs) = rome_zk_testkit::send_measuring_cu(ctx, ixs, payer, &[]).await;
    result
}

async fn cursor_next_batch(
    ctx: &mut ProgramTestContext,
    inbox: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
) -> u64 {
    let acct = ctx
        .banks_client
        .get_account(client::cursor_pda(inbox, settlement_program, chain_id).0)
        .await
        .unwrap()
        .expect("cursor account must exist");
    client::decode_batch_cursor(&acct.data).unwrap().next_batch
}

/// Distinct ids for the attacker's settlement program (any id the attacker controls) and the chain's real
/// settlement program.
fn attacker_settlement_program() -> Pubkey {
    Pubkey::new_from_array([0xA7u8; 32])
}

struct World {
    inbox: Pubkey,
    real_settlement: Pubkey,
    attacker_settlement: Pubkey,
    victim: Keypair,
    attacker: Keypair,
}

impl World {
    fn new() -> Self {
        Self {
            inbox: rome_zk_testkit::fixed_inbox_program_id(),
            real_settlement: rome_zk_testkit::fixed_settlement_program_id(),
            attacker_settlement: attacker_settlement_program(),
            victim: Keypair::new(),
            attacker: Keypair::new(),
        }
    }

    /// `with_real_root`: the victim chain is registered under the real settlement program (its root exists).
    /// `with_victim_cursor_at`: the victim already initialised its cursor at that id.
    fn program_test(
        &self,
        with_real_root: bool,
        with_victim_cursor_at: Option<u64>,
    ) -> solana_program_test::ProgramTest {
        let mut pt = rome_zk_testkit::program_test(
            &[rome_zk_testkit::ProgramSpec::new("zk_inbox", self.inbox)],
            true,
        );
        // The attacker's root-shaped account: owner = the attacker's program, address = ["root", chain_id]
        // under it, authority = the attacker. This is the whole setup the attack needs.
        pt.add_account(
            client::root_pda(&self.attacker_settlement, VICTIM_CHAIN).0,
            root_account_with_authority(
                VICTIM_CHAIN,
                &self.attacker.pubkey(),
                self.attacker_settlement,
            ),
        );
        if with_real_root {
            pt.add_account(
                client::root_pda(&self.real_settlement, VICTIM_CHAIN).0,
                root_account_with_authority(
                    VICTIM_CHAIN,
                    &self.victim.pubkey(),
                    self.real_settlement,
                ),
            );
        }
        if let Some(n) = with_victim_cursor_at {
            pt.add_account(
                client::cursor_pda(&self.inbox, &self.real_settlement, VICTIM_CHAIN).0,
                cursor_account(self.inbox, VICTIM_CHAIN, n),
            );
        }
        pt.add_account(self.victim.pubkey(), funded_account());
        pt.add_account(self.attacker.pubkey(), funded_account());
        pt
    }
}

// ---------------------------------------------------------------------------------------------
// (a) InitBatchCursor through a settlement program the caller controls
// ---------------------------------------------------------------------------------------------

/// A chain id nobody has registered yet (permissionless ids are computable in advance): the attacker names
/// his own settlement program and initialises the victim chain id's cursor, at an id no real chain can use.
/// The attacker's own InitBatchCursor is allowed to succeed: it creates a cursor at the address keyed by
/// HIS settlement program, which is a different address from the real chain's. SAFE behaviour: the real
/// chain's cursor slot (keyed by the real settlement program) is left empty.
#[tokio::test]
async fn a_third_partys_cursor_for_a_chain_id_lands_at_a_different_address_than_the_real_chains() {
    let w = World::new();
    let mut ctx = w.program_test(false, None).start_with_context().await;

    let attack = client::init_batch_cursor_ix(
        &w.inbox,
        &w.attacker.pubkey(),
        VICTIM_CHAIN,
        u64::MAX,
        &w.attacker_settlement,
    );
    // Through a program of his own the attacker can create a cursor, but only at the address keyed by that
    // program: the slot the real chain's settlement program would use stays empty.
    let _ = send(&mut ctx, &[attack], &w.attacker).await;
    let real_slot = ctx
        .banks_client
        .get_account(client::cursor_pda(&w.inbox, &w.real_settlement, VICTIM_CHAIN).0)
        .await
        .unwrap();
    assert!(
        real_slot.is_none(),
        "a third party's settlement program set the cursor of chain {VICTIM_CHAIN} under the real \
         settlement program's key; nothing on the real chain's path could ever open a batch there"
    );
}

/// The victim chain exists under the real settlement program. The attacker gets to InitBatchCursor first,
/// through his own program (the attack outcome is not asserted here, only what it does to the victim).
/// SAFE behaviour: the victim can still initialise its own cursor and open its first batch.
#[tokio::test]
async fn real_chain_can_still_initialise_its_cursor_after_a_third_party_tried_first() {
    let w = World::new();
    let mut ctx = w.program_test(true, None).start_with_context().await;

    let attack = client::init_batch_cursor_ix(
        &w.inbox,
        &w.attacker.pubkey(),
        VICTIM_CHAIN,
        u64::MAX,
        &w.attacker_settlement,
    );
    let _ = send(&mut ctx, &[attack], &w.attacker).await;

    let init = client::init_batch_cursor_ix(
        &w.inbox,
        &w.victim.pubkey(),
        VICTIM_CHAIN,
        1,
        &w.real_settlement,
    );
    let res = send(&mut ctx, &[init], &w.victim).await;
    assert!(
        res.is_ok(),
        "the real chain's own InitBatchCursor (real settlement program, real authority) was refused: {res:?}"
    );
    assert_eq!(
        cursor_next_batch(&mut ctx, &w.inbox, &w.real_settlement, VICTIM_CHAIN).await,
        1,
        "the real chain's cursor must start where the real chain put it"
    );

    let open = client::open_batch_ix(
        &w.inbox,
        &w.victim.pubkey(),
        VICTIM_CHAIN,
        1,
        3,
        &w.real_settlement,
    );
    let res = send(&mut ctx, &[open], &w.victim).await;
    assert!(
        res.is_ok(),
        "the real chain could not open its first batch: {res:?}"
    );
}

/// Same race, but the attacker sets the cursor to a harmless-looking id. Either way the cursor must be the
/// real chain's to set. SAFE behaviour: the third party is refused and the cursor is still absent.
#[tokio::test]
async fn a_third_party_cannot_leave_a_cursor_behind_on_the_real_chains_id() {
    let w = World::new();
    let mut ctx = w.program_test(true, None).start_with_context().await;

    let attack = client::init_batch_cursor_ix(
        &w.inbox,
        &w.attacker.pubkey(),
        VICTIM_CHAIN,
        2,
        &w.attacker_settlement,
    );
    let _ = send(&mut ctx, &[attack], &w.attacker).await;

    let cursor = ctx
        .banks_client
        .get_account(client::cursor_pda(&w.inbox, &w.real_settlement, VICTIM_CHAIN).0)
        .await
        .unwrap();
    assert!(
        cursor.is_none() || cursor.unwrap().data.is_empty(),
        "a cursor now exists for chain {VICTIM_CHAIN} and the real authority did not create it"
    );
}

// ---------------------------------------------------------------------------------------------
// (b) OpenBatch through a settlement program the caller controls, then AbandonBatch
// ---------------------------------------------------------------------------------------------

/// The victim chain is registered and its cursor sits at batch 1. The attacker names his own settlement
/// program and opens batch 1. SAFE behaviour: refused.
#[tokio::test]
async fn open_batch_for_the_real_chains_next_id_through_a_third_partys_program_is_refused() {
    let w = World::new();
    let mut ctx = w.program_test(true, Some(1)).start_with_context().await;

    let attack = client::open_batch_ix(
        &w.inbox,
        &w.attacker.pubkey(),
        VICTIM_CHAIN,
        1,
        1,
        &w.attacker_settlement,
    );
    let res = send(&mut ctx, &[attack], &w.attacker).await;
    // The attacker's own cursor (keyed by his program) does not exist, so the cursor slot he must pass is
    // an empty system account.
    assert_eq!(
        res.expect_err(&format!(
            "a third party's settlement program opened batch 1 of chain {VICTIM_CHAIN}"
        )),
        TransactionError::InstructionError(0, InstructionError::IncorrectProgramId),
    );
    assert_eq!(
        cursor_next_batch(&mut ctx, &w.inbox, &w.real_settlement, VICTIM_CHAIN).await,
        1,
        "a third party moved the real chain's cursor"
    );
}

/// The attacker opens the victim's next batch id and abandons it (refunding his rent). The attack outcome is
/// not asserted; what is asserted is the consequence. SAFE behaviour: the real chain can still open the next
/// batch it expects, with its own settlement program and authority recorded in it.
#[tokio::test]
async fn real_chain_can_still_open_its_next_batch_after_a_third_party_opened_and_abandoned_it() {
    let w = World::new();
    let mut ctx = w.program_test(true, Some(1)).start_with_context().await;

    // The attacker bootstraps a cursor of his own for the victim's chain id through his own program.
    let own_cursor = client::init_batch_cursor_ix(
        &w.inbox,
        &w.attacker.pubkey(),
        VICTIM_CHAIN,
        1,
        &w.attacker_settlement,
    );
    let _ = send(&mut ctx, &[own_cursor], &w.attacker).await;

    let open = client::open_batch_ix(
        &w.inbox,
        &w.attacker.pubkey(),
        VICTIM_CHAIN,
        1,
        1,
        &w.attacker_settlement,
    );
    let _ = send(&mut ctx, &[open], &w.attacker).await;
    let abandon = client::abandon_batch_ix(
        &w.inbox,
        &w.attacker.pubkey(),
        &w.attacker_settlement,
        VICTIM_CHAIN,
        1,
    );
    let _ = send(&mut ctx, &[abandon], &w.attacker).await;

    let next = cursor_next_batch(&mut ctx, &w.inbox, &w.real_settlement, VICTIM_CHAIN).await;
    let real_open = client::open_batch_ix(
        &w.inbox,
        &w.victim.pubkey(),
        VICTIM_CHAIN,
        next,
        1,
        &w.real_settlement,
    );
    let res = send(&mut ctx, &[real_open], &w.victim).await;
    assert!(
        res.is_ok(),
        "the real authority cannot open the cursor's next batch ({next}) of its own chain: {res:?}"
    );
    // Settlement only ever posts head_pending + 1 = 1 for a chain that has not posted yet, so the id the
    // real chain just opened has to be 1, not a later one.
    assert_eq!(
        next, 1,
        "the real chain's first batch id was consumed by someone else; settlement posts only batch 1 first"
    );
}

/// The attacker opens the victim's next batch id and does NOT abandon it (no refund needed; the rent is
/// small). The real chain's batch id 1 is now an attacker-authored account the real authority cannot
/// finalise or abandon. SAFE behaviour: the real chain's batch 1 is the real chain's.
#[tokio::test]
async fn batch_one_of_the_real_chain_is_never_authored_by_a_third_party() {
    let w = World::new();
    let mut ctx = w.program_test(true, Some(1)).start_with_context().await;

    // The attacker bootstraps a cursor of his own for the victim's chain id through his own program.
    let own_cursor = client::init_batch_cursor_ix(
        &w.inbox,
        &w.attacker.pubkey(),
        VICTIM_CHAIN,
        1,
        &w.attacker_settlement,
    );
    let _ = send(&mut ctx, &[own_cursor], &w.attacker).await;

    let open = client::open_batch_ix(
        &w.inbox,
        &w.attacker.pubkey(),
        VICTIM_CHAIN,
        1,
        1,
        &w.attacker_settlement,
    );
    let _ = send(&mut ctx, &[open], &w.attacker).await;

    if let Some(acct) = ctx
        .banks_client
        .get_account(client::batch_pda(&w.inbox, &w.real_settlement, VICTIM_CHAIN, 1).0)
        .await
        .unwrap()
    {
        let b = client::decode_batch_account(&acct.data).unwrap();
        assert_eq!(
            b.authority,
            w.victim.pubkey(),
            "batch 1 of chain {VICTIM_CHAIN} is owned by a third party, not the chain's authority"
        );
    }

    // And the real chain opens its own batch 1 at its own address, with its own authority recorded.
    let real_open = client::open_batch_ix(
        &w.inbox,
        &w.victim.pubkey(),
        VICTIM_CHAIN,
        1,
        1,
        &w.real_settlement,
    );
    send(&mut ctx, &[real_open], &w.victim)
        .await
        .expect("the real chain could not open its batch 1");
    let acct = ctx
        .banks_client
        .get_account(client::batch_pda(&w.inbox, &w.real_settlement, VICTIM_CHAIN, 1).0)
        .await
        .unwrap()
        .expect("the real chain's batch 1 exists");
    let b = client::decode_batch_account(&acct.data).unwrap();
    assert_eq!(b.authority, w.victim.pubkey());
    assert_eq!(b.settlement_program, w.real_settlement);
}

// ---------------------------------------------------------------------------------------------
// (c) variants and controls
// ---------------------------------------------------------------------------------------------

/// Regression guard. A third party who names the REAL settlement program cannot use the real
/// chain's root: the signer is not the real root's authority. This is the check the fix must keep.
#[tokio::test]
async fn naming_the_real_settlement_program_still_requires_the_real_authority() {
    let w = World::new();
    let mut ctx = w.program_test(true, Some(1)).start_with_context().await;

    let init = client::init_batch_cursor_ix(
        &w.inbox,
        &w.attacker.pubkey(),
        VICTIM_CHAIN,
        7,
        &w.real_settlement,
    );
    let err = send(&mut ctx, &[init], &w.attacker).await.unwrap_err();
    // Either the already-initialised refusal or the authority refusal is acceptable; both refuse.
    assert!(
        matches!(
            custom(&err),
            Some(ERR_NOT_CHAIN_AUTHORITY) | Some(ERR_CURSOR_ALREADY_INITIALIZED)
        ),
        "unexpected refusal: {err:?}"
    );

    let open = client::open_batch_ix(
        &w.inbox,
        &w.attacker.pubkey(),
        VICTIM_CHAIN,
        1,
        1,
        &w.real_settlement,
    );
    let err = send(&mut ctx, &[open], &w.attacker).await.unwrap_err();
    assert_eq!(
        custom(&err),
        Some(ERR_NOT_CHAIN_AUTHORITY),
        "expected NotChainAuthority, got {err:?}"
    );
    assert_eq!(
        cursor_next_batch(&mut ctx, &w.inbox, &w.real_settlement, VICTIM_CHAIN).await,
        1
    );
}

/// Regression guard. The cursor is still strictly sequential for the real chain: the real
/// authority cannot skip ahead, so the only way to move the cursor past 1 without batch 1 existing is
/// somebody else's OpenBatch (which the red tests above show).
#[tokio::test]
async fn real_chain_cannot_skip_a_batch_id() {
    let w = World::new();
    let mut ctx = w.program_test(true, Some(1)).start_with_context().await;

    let skip = client::open_batch_ix(
        &w.inbox,
        &w.victim.pubkey(),
        VICTIM_CHAIN,
        2,
        1,
        &w.real_settlement,
    );
    let err = send(&mut ctx, &[skip], &w.victim).await.unwrap_err();
    assert_eq!(custom(&err), Some(ERR_CURSOR_MISMATCH));
}

// ---------------------------------------------------------------------------------------------
// (d) chunk Close: the settlement program is taken from the root account the caller passes
// ---------------------------------------------------------------------------------------------

/// The real chain opens batch 1 and one chunk under it (all through its own settlement program).
async fn real_chain_with_one_open_chunk(w: &World) -> ProgramTestContext {
    let mut ctx = w.program_test(true, Some(1)).start_with_context().await;
    send(
        &mut ctx,
        &[client::open_batch_ix(
            &w.inbox,
            &w.victim.pubkey(),
            VICTIM_CHAIN,
            1,
            1,
            &w.real_settlement,
        )],
        &w.victim,
    )
    .await
    .expect("the real chain opens its batch 1");
    send(
        &mut ctx,
        &[client::open_chunk_ix(
            &w.inbox,
            &w.victim.pubkey(),
            &w.real_settlement,
            VICTIM_CHAIN,
            1,
            0,
            16,
        )],
        &w.victim,
    )
    .await
    .expect("the real chain opens its chunk");
    ctx
}

/// Writes, seals and finalizes the one chunk of the real chain's batch 1, so a later refusal cannot come
/// from `NotFinalized`.
async fn finalize_real_batch_one(w: &World, ctx: &mut ProgramTestContext) {
    let body = [7u8; 16];
    let steps = [
        client::write_chunk_ix(
            &w.inbox,
            &w.victim.pubkey(),
            &w.real_settlement,
            VICTIM_CHAIN,
            1,
            0,
            0,
            body.to_vec(),
        ),
        client::seal_chunk_ix(
            &w.inbox,
            &w.victim.pubkey(),
            &w.real_settlement,
            VICTIM_CHAIN,
            1,
            0,
            body.len() as u32,
            client::chunk_body_hash(&body),
        ),
        client::seal_leaf_ix(&w.inbox, &w.real_settlement, VICTIM_CHAIN, 1, 0),
        client::finalize_batch_ix(
            &w.inbox,
            &w.victim.pubkey(),
            &w.real_settlement,
            VICTIM_CHAIN,
            1,
            0,
        ),
    ];
    for (i, step) in steps.into_iter().enumerate() {
        send(ctx, &[step], &w.victim)
            .await
            .unwrap_or_else(|e| panic!("finalizing the real chain's batch 1, step {i}: {e:?}"));
    }
    let batch = ctx
        .banks_client
        .get_account(client::batch_pda(&w.inbox, &w.real_settlement, VICTIM_CHAIN, 1).0)
        .await
        .unwrap()
        .expect("the real chain's batch 1");
    assert!(
        client::decode_batch_account(&batch.data).unwrap().finalized,
        "the real chain's batch 1 must be finalized for this test"
    );
}

async fn account_exists(ctx: &mut ProgramTestContext, key: &Pubkey) -> bool {
    ctx.banks_client
        .get_account(*key)
        .await
        .unwrap()
        .map(|a| a.lamports > 0)
        .unwrap_or(false)
}

/// A fake root under another program cannot close a live chunk. The caller (the chunk's own authority)
/// passes a root-shaped account owned by a program of his choosing: the chunk, batch and root addresses
/// are all derived from that program, none of which is the real chain's chunk, so Close is refused whether
/// the batch slot holds the real (live) batch or the address the batch would have under the other program.
#[tokio::test]
async fn a_fake_root_under_another_program_cannot_close_a_live_chunk() {
    let w = World::new();
    let mut ctx = real_chain_with_one_open_chunk(&w).await;
    let real_chunk = client::chunk_pda(&w.inbox, &w.real_settlement, VICTIM_CHAIN, 1, 0).0;
    assert!(account_exists(&mut ctx, &real_chunk).await);
    // Finalized first: with the batch final the only thing left to refuse the close is the fake root.
    finalize_real_batch_one(&w, &mut ctx).await;

    // (1) real chunk + real (live) batch, root swapped for the attacker-program root.
    let mut ix = client::close_chunk_ix(
        &w.inbox,
        &w.victim.pubkey(),
        &w.real_settlement,
        VICTIM_CHAIN,
        1,
        0,
    );
    ix.accounts[3] = solana_program::instruction::AccountMeta::new_readonly(
        client::root_pda(&w.attacker_settlement, VICTIM_CHAIN).0,
        false,
    );
    // The chunk address is derived from the settlement program that owns the root passed, so it is not
    // the real chunk's address.
    assert_eq!(
        send(&mut ctx, &[ix], &w.victim)
            .await
            .expect_err("a fake root must not close the real chain's live chunk"),
        TransactionError::InstructionError(0, InstructionError::InvalidSeeds),
    );

    // (2) the batch slot also swapped for the (absent, system-owned) batch address under the attacker's
    // program, which is what makes the chunk look "abandoned".
    let mut ix = client::close_chunk_ix(
        &w.inbox,
        &w.victim.pubkey(),
        &w.real_settlement,
        VICTIM_CHAIN,
        1,
        0,
    );
    ix.accounts[2] = solana_program::instruction::AccountMeta::new_readonly(
        client::batch_pda(&w.inbox, &w.attacker_settlement, VICTIM_CHAIN, 1).0,
        false,
    );
    ix.accounts[3] = solana_program::instruction::AccountMeta::new_readonly(
        client::root_pda(&w.attacker_settlement, VICTIM_CHAIN).0,
        false,
    );
    assert_eq!(
        send(&mut ctx, &[ix], &w.victim).await.expect_err(
            "an attacker-program batch slot must not make the real chunk look abandoned"
        ),
        TransactionError::InstructionError(0, InstructionError::InvalidSeeds),
    );

    assert!(
        account_exists(&mut ctx, &real_chunk).await,
        "the real chain's chunk must survive both attempts"
    );
}

/// An abandoned batch's chunks still close for the chunk authority alone, through the real root; a root
/// under another program is refused for the same chunk.
#[tokio::test]
async fn an_abandoned_batchs_chunk_closes_only_against_its_own_settlement_programs_root() {
    let w = World::new();
    let mut ctx = real_chain_with_one_open_chunk(&w).await;
    let real_chunk = client::chunk_pda(&w.inbox, &w.real_settlement, VICTIM_CHAIN, 1, 0).0;

    send(
        &mut ctx,
        &[client::abandon_batch_ix(
            &w.inbox,
            &w.victim.pubkey(),
            &w.real_settlement,
            VICTIM_CHAIN,
            1,
        )],
        &w.victim,
    )
    .await
    .expect("the real chain abandons its batch 1");

    let mut fake = client::close_chunk_ix(
        &w.inbox,
        &w.victim.pubkey(),
        &w.real_settlement,
        VICTIM_CHAIN,
        1,
        0,
    );
    fake.accounts[3] = solana_program::instruction::AccountMeta::new_readonly(
        client::root_pda(&w.attacker_settlement, VICTIM_CHAIN).0,
        false,
    );
    assert_eq!(
        send(&mut ctx, &[fake], &w.victim)
            .await
            .expect_err("a root under another program must not close the abandoned batch's chunk"),
        TransactionError::InstructionError(0, InstructionError::InvalidSeeds),
    );
    assert!(account_exists(&mut ctx, &real_chunk).await);

    send(
        &mut ctx,
        &[client::close_chunk_ix(
            &w.inbox,
            &w.victim.pubkey(),
            &w.real_settlement,
            VICTIM_CHAIN,
            1,
            0,
        )],
        &w.victim,
    )
    .await
    .expect("the abandoned batch's chunk closes against the real settlement program's root");
    assert!(!account_exists(&mut ctx, &real_chunk).await);
}
