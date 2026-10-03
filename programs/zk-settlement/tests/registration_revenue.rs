//! solana-program-test integration tests for chain registration and revenue: the chain-id namespace split,
//! the deposit lifecycle, and the protocol fee. Loads the real, `cargo build-sbf`-compiled `.so` — run
//! `cargo build-sbf --manifest-path programs/zk-settlement/Cargo.toml` before `cargo test -p zk-settlement`.
//!
//! Kept as its own file (not folded into `settlement.rs`) — these tests share almost no rig
//! with the existing PostRoot/FinalizeBatch suite beyond the smallest primitives, which are duplicated
//! below rather than factored into a shared `tests/common` module for a file this size.

use rome_zk_testkit::{funded_keypair, rent_exempt};
use solana_program::{instruction::AccountMeta, keccak, pubkey::Pubkey};
// `bpf_loader_upgradeable`/`system_program` moved out of `solana_program`'s
// root re-export in the Agave 4.x line (API fallout).
mod bpf_loader_upgradeable {
    pub use solana_sdk_ids::bpf_loader_upgradeable::id;
}
use solana_program_test::ProgramTest;
use solana_sdk::{
    account::Account,
    instruction::{Instruction, InstructionError},
    signature::{Keypair, Signer},
    transaction::TransactionError,
};
use solana_system_interface::program as system_program;
use zk_settlement::errors::SettleError;
use zk_settlement_client as sclient;

fn funded_account_with(lamports: u64) -> Account {
    Account {
        lamports,
        data: vec![],
        owner: system_program::id(),
        executable: false,
        rent_epoch: 0,
    }
}
fn funded_account() -> Account {
    funded_account_with(50_000_000_000)
}

/// Thin adapter over `rome_zk_testkit::send_measuring_cu` — this file's callers only ever want the CU
/// figure on success.
async fn send(
    ctx: &mut solana_program_test::ProgramTestContext,
    ixs: &[Instruction],
    payer: &Keypair,
    extra_signers: &[&Keypair],
) -> Result<u64, TransactionError> {
    let (result, cu, _logs) =
        rome_zk_testkit::send_measuring_cu(ctx, ixs, payer, extra_signers).await;
    result.map(|()| cu)
}

fn custom_error(err: &TransactionError) -> Option<u32> {
    match err {
        TransactionError::InstructionError(_, InstructionError::Custom(c)) => Some(*c),
        _ => None,
    }
}

async fn lamports_of(ctx: &mut solana_program_test::ProgramTestContext, key: Pubkey) -> u64 {
    ctx.banks_client
        .get_account(key)
        .await
        .unwrap()
        .map(|a| a.lamports)
        .unwrap_or(0)
}

/// One posted+finalized batch's worth of inbox account bytes (the inbox layout), independent of the
/// real zk-inbox program — `PostRoot`/`PostRootProved` only read this account.
fn inbox_batch_account(
    owner: Pubkey,
    chain_id: u64,
    batch: u64,
    settlement_program: Pubkey,
    authority: Pubkey,
    acc: [u8; 32],
) -> Account {
    let expected_count: u32 = 1;
    let mut d = vec![0u8; rome_zk_layouts::batch::account_len(expected_count)];
    d[0..4].copy_from_slice(&rome_zk_layouts::batch::MAGIC.to_le_bytes());
    d[4] = rome_zk_layouts::batch::VERSION;
    d[5..13].copy_from_slice(&chain_id.to_le_bytes());
    d[13..21].copy_from_slice(&batch.to_le_bytes());
    d[29..33].copy_from_slice(&expected_count.to_le_bytes());
    d[33..37].copy_from_slice(&expected_count.to_le_bytes());
    d[37] = 1; // finalized
    d[38..70].copy_from_slice(settlement_program.as_ref());
    d[70..102].copy_from_slice(authority.as_ref());
    d[166..198].copy_from_slice(&acc);
    Account {
        lamports: rent_exempt(d.len()),
        data: d,
        owner,
        executable: false,
        rent_epoch: 0,
    }
}

/// `reclaim_window_slots` has a 1-day-of-slots floor, enforced at both `InitGlobalConfig` and
/// `SetGlobalConfig` — every fixture and warp below now targets this window instead of an earlier
/// fixture's below-floor `20`.
const RECLAIM_WINDOW_SLOTS: u64 = zk_settlement::governance::MIN_RECLAIM_WINDOW_SLOTS;

fn default_global_config_fields(
    registry_authority: Pubkey,
    treasury: Pubkey,
    permissionless_init_enabled: bool,
) -> sclient::GlobalConfigFields {
    sclient::GlobalConfigFields {
        registry_authority,
        treasury,
        permissionless_init_enabled,
        reclaim_window_slots: RECLAIM_WINDOW_SLOTS,
        deposit_lamports: 2_000_000_000,
        default_fee_base_lamports: 1_000_000,
        default_fee_bps: 0,
    }
}

fn empty_init_chain_fields(
    genesis_state_root: [u8; 32],
    challenge_window_slots: u32,
    max_pending: u32,
    inbox_program: Pubkey,
) -> sclient::InitChainFields {
    sclient::InitChainFields {
        number: 0,
        parent_hash: [0u8; 32],
        state_root: genesis_state_root,
        block_hash: [0u8; 32],
        profile: 0,
        challenge_window_slots,
        prove_window_slots: 1000,
        proving_policy: 1,
        poster_bond: 0,
        exit_cap_per_window: 0,
        max_pending,
        inbox_program,
        registry_entries: vec![],
        max_drift_secs: 60,
    }
}

struct Rig {
    settlement_program: Pubkey,
    payer: Keypair,
    authority: Keypair,
    registry_authority: Keypair,
    treasury: Pubkey,
    inbox_program: Pubkey,
    genesis_state_root: [u8; 32],
    challenge_window_slots: u32,
    /// The program's real upgrade authority — `program_test` seeds the `ProgramData` account via
    /// `add_upgradeable_program_to_genesis` (authority defaults to `Pubkey::default()`);
    /// `start_and_init_global_config` patches it to this key before signing `InitGlobalConfig`
    /// with it.
    upgrade_authority: Keypair,
}

/// Builds (but does not start) the `ProgramTest` + `Rig` fields — split out from [`rig`] so a test that
/// needs a pre-start account (anything injected via `pt.add_account`, not a post-start `ctx.set_account`)
/// ahead of a later `warp_to_slot` can add it before calling [`start_and_init_global_config`]. A
/// post-start `set_account`'s lamports are not part of the bank's genesis supply, and `warp_to_slot`'s
/// accounts-hash verification panics on that mismatch (`settlement.rs`'s `post_root_rig` doc explains the
/// same gotcha) — every account a warping test needs must go in before `start_with_context`.
fn rig_program_test() -> (ProgramTest, Rig) {
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let payer = funded_keypair();
    // Fixed, not `funded_keypair()`: pinning only `settlement_program` stabilizes the reserved-path CU
    // figures (their `chain_id` is a small literal already), but every permissionless-path measurement
    // below still derives `chain_id` — and so the bump seed for
    // `root`/`registry`/`chain_config`/`perm_nonce` — from `authority`'s random pubkey, and `PostRoot`/
    // `PostRootProved` independently re-derive the inbox batch PDA on-chain from `inbox_program`.
    // Verified by direct re-measurement (not asserted from architecture): with only the program id
    // pinned, `cu_refund_deposit` alone still swung ~17.4k -> ~26.1k between two otherwise-identical
    // runs. Pinning these two closes it.
    let authority = solana_sdk::signature::keypair_from_seed(&[0x11u8; 32]).unwrap();
    let registry_authority = funded_keypair();
    let treasury = Pubkey::new_unique();
    let inbox_program = Pubkey::new_from_array([0x22u8; 32]);
    let upgrade_authority = funded_keypair();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::upgradeable(
            "zk_settlement",
            settlement_program,
        )],
        true,
    );
    pt.add_account(payer.pubkey(), funded_account());
    pt.add_account(authority.pubkey(), funded_account());
    // A brand-new, never-before-seen account that receives a fee smaller than the rent-exempt minimum
    // (a real risk for a small base fee against a fresh treasury) hits the runtime's "no new rent-paying
    // accounts" rule (`InsufficientFundsForRent`) — a real treasury is funded ahead of time, so this test
    // treasury is too, above the 0-byte rent-exempt minimum. It also satisfies the `InitGlobal
    // Config`-time rent-exemption check for the same reason.
    pt.add_account(treasury, funded_account_with(10_000_000));
    let r = Rig {
        settlement_program,
        payer,
        authority,
        registry_authority,
        treasury,
        inbox_program,
        genesis_state_root: keccak::hashv(&[b"registration-revenue genesis"]).to_bytes(),
        challenge_window_slots: 5,
        upgrade_authority,
    };
    (pt, r)
}

async fn start_and_init_global_config(
    pt: ProgramTest,
    r: &Rig,
    permissionless_init_enabled: bool,
) -> solana_program_test::ProgramTestContext {
    let mut ctx = pt.start_with_context().await;
    // Patch the `ProgramData` account's stored authority to `r.upgrade_authority` — a
    // data-only rewrite (same lamports), safe under this rig's later `warp_to_slot` calls.
    let program_data = sclient::program_data_pda(&r.settlement_program);
    let mut pd_account = ctx
        .banks_client
        .get_account(program_data)
        .await
        .unwrap()
        .expect("ProgramData account must exist (added via add_upgradeable_program_to_genesis)");
    pd_account.data[13..45].copy_from_slice(r.upgrade_authority.pubkey().as_ref());
    ctx.set_account(&program_data, &pd_account.into());

    let fields = default_global_config_fields(
        r.registry_authority.pubkey(),
        r.treasury,
        permissionless_init_enabled,
    );
    let ix = sclient::init_global_config_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.upgrade_authority.pubkey(),
        fields.clone(),
    );
    send(&mut ctx, &[ix], &r.payer, &[&r.upgrade_authority])
        .await
        .expect("InitGlobalConfig should succeed");

    // `InitGlobalConfig` forces `permissionless_init_enabled = false` regardless of what was
    // passed above — flip every field to what the caller actually asked for via the registry-authority-
    // only `SetGlobalConfig`.
    let set_ix = sclient::set_global_config_ix(
        &r.settlement_program,
        &r.registry_authority.pubkey(),
        sclient::GlobalConfigUpdate {
            permissionless_init_enabled: fields.permissionless_init_enabled,
            reclaim_window_slots: fields.reclaim_window_slots,
            deposit_lamports: fields.deposit_lamports,
            default_fee_base_lamports: fields.default_fee_base_lamports,
            default_fee_bps: fields.default_fee_bps,
        },
    );
    send(&mut ctx, &[set_ix], &r.payer, &[&r.registry_authority])
        .await
        .expect("SetGlobalConfig should succeed");
    ctx
}

/// A fresh throwaway program with `InitGlobalConfig` already sent — `permissionless_init_enabled` and
/// `reclaim_window_slots` are the two knobs most tests below vary.
async fn rig(permissionless_init_enabled: bool) -> (solana_program_test::ProgramTestContext, Rig) {
    let (pt, r) = rig_program_test();
    let ctx = start_and_init_global_config(pt, &r, permissionless_init_enabled).await;
    (ctx, r)
}

async fn allow_reserved(ctx: &mut solana_program_test::ProgramTestContext, r: &Rig, chain_id: u64) {
    let ix = sclient::allow_reserved_id_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.registry_authority.pubkey(),
        chain_id,
    );
    send(ctx, &[ix], &r.payer, &[&r.registry_authority])
        .await
        .expect("AllowReservedId should succeed");
}

/// Patches the pre-genesis `ProgramData` account (seeded by `add_upgradeable_program_to_genesis` with
/// `Some(Pubkey::default())`) so its stored upgrade authority is `authority` — mirrors
/// `start_and_init_global_config`'s own patch, exposed separately here so the upgrade-authority tests can drive
/// `InitGlobalConfig` themselves instead of going through that helper (which always signs with the real
/// authority and always succeeds).
async fn set_program_data_authority(
    ctx: &mut solana_program_test::ProgramTestContext,
    program_id: &Pubkey,
    authority: &Pubkey,
) {
    let program_data = sclient::program_data_pda(program_id);
    let mut pd_account = ctx
        .banks_client
        .get_account(program_data)
        .await
        .unwrap()
        .expect("ProgramData account must exist (added via add_upgradeable_program_to_genesis)");
    pd_account.data[13..45].copy_from_slice(authority.as_ref());
    ctx.set_account(&program_data, &pd_account.into());
}

// ---------------------------------------------------------------------------------------------
// (0) InitGlobalConfig upgrade-authority gate
// ---------------------------------------------------------------------------------------------

/// The positive control every other rig-based test in this file already exercises implicitly (`rig()`
/// always signs `InitGlobalConfig` with the real upgrade authority and always succeeds) — spelled out here
/// once, directly.
#[tokio::test]
async fn init_global_config_accepted_with_the_real_upgrade_authority() {
    let (_ctx, _r) = rig(false).await; // panics via .expect(...) inside if this ever regresses
}

#[tokio::test]
async fn init_global_config_rejects_a_signer_that_is_not_the_upgrade_authority() {
    let (pt, r) = rig_program_test();
    let mut ctx = pt.start_with_context().await;
    set_program_data_authority(
        &mut ctx,
        &r.settlement_program,
        &r.upgrade_authority.pubkey(),
    )
    .await;

    let decoy = funded_keypair();
    ctx.set_account(&decoy.pubkey(), &funded_account().into());
    let ix = sclient::init_global_config_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &decoy.pubkey(),
        default_global_config_fields(r.registry_authority.pubkey(), r.treasury, false),
    );
    let err = send(&mut ctx, &[ix], &r.payer, &[&decoy])
        .await
        .expect_err("a signer that is not the program's real upgrade authority must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::NotUpgradeAuthority as u32)
    );
    let (global_pda, _) = sclient::global_config_pda(&r.settlement_program);
    assert!(
        ctx.banks_client
            .get_account(global_pda)
            .await
            .unwrap()
            .is_none(),
        "a rejected InitGlobalConfig must not create the global_config pda"
    );
}

/// `UpgradeableLoaderState::ProgramData { upgrade_authority_address: None }` (byte 12 = 0, the immutable-
/// program case) — no signer can ever satisfy this, including the one that used to be the authority.
///
/// Keep the FORMER authority's bytes live at `13..45` and flip only the `Option` tag
/// at byte 12 — the realistic post-`SetAuthority(None)` state (the loader's `set_state` leaves the tail
/// alone; it does not zero it), and it isolates this test from a tag-check regression that a coincidental
/// zeroed tail would mask.
#[tokio::test]
async fn init_global_config_rejects_an_immutable_program() {
    let (pt, r) = rig_program_test();
    let mut ctx = pt.start_with_context().await;
    let program_data = sclient::program_data_pda(&r.settlement_program);
    let mut pd_account = ctx
        .banks_client
        .get_account(program_data)
        .await
        .unwrap()
        .expect("ProgramData account must exist");
    pd_account.data[13..45].copy_from_slice(r.upgrade_authority.pubkey().as_ref());
    pd_account.data[12] = 0; // Option::None — the tail at 13..45 stays live, untouched
    ctx.set_account(&program_data, &pd_account.into());

    let ix = sclient::init_global_config_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.upgrade_authority.pubkey(),
        default_global_config_fields(r.registry_authority.pubkey(), r.treasury, false),
    );
    let err = send(&mut ctx, &[ix], &r.payer, &[&r.upgrade_authority])
        .await
        .expect_err("an immutable program (None upgrade authority) must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::NotUpgradeAuthority as u32)
    );
}

/// A well-formed 45-byte `UpgradeableLoaderState::ProgramData` blob: discriminant `3` (LE u32), an
/// arbitrary slot, `Option` tag `1` (Some), then `authority`.
fn program_data_bytes(authority: &Pubkey) -> Vec<u8> {
    let mut d = vec![0u8; 45];
    d[0..4].copy_from_slice(&3u32.to_le_bytes());
    d[4..12].copy_from_slice(&1u64.to_le_bytes());
    d[12] = 1;
    d[13..45].copy_from_slice(authority.as_ref());
    d
}

/// Builds `InitGlobalConfig` by hand (not `sclient::init_global_config_ix`, which always derives the
/// REAL `program_data` PDA) so the three probes below can substitute a forged `program_data` account and
/// an unsigned `authority`.
fn init_global_config_ix_raw(
    program_id: &Pubkey,
    payer: &Pubkey,
    authority: &Pubkey,
    authority_is_signer: bool,
    program_data: &Pubkey,
    fields: sclient::GlobalConfigFields,
) -> Instruction {
    let (global_config, _) = sclient::global_config_pda(program_id);
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(*authority, authority_is_signer),
            AccountMeta::new(global_config, false),
            AccountMeta::new_readonly(*program_data, false),
            AccountMeta::new_readonly(fields.treasury, false),
            AccountMeta::new_readonly(system_program::id(), false),
        ],
        data: borsh::to_vec(&zk_settlement::SettleIx::InitGlobalConfig(fields)).unwrap(),
    }
}

/// Probe 1 (address check): a forged, otherwise-perfectly-valid `ProgramData` account — real owner,
/// well-formed data, the real upgrade authority stored and signing — but living at a random address
/// instead of the program's actual `["<program_id>"]` PDA. Isolates the address compare: removing it
/// (leaving only the owner check) would let this forged account through.
#[tokio::test]
async fn init_global_config_rejects_program_data_at_the_wrong_address() {
    let (pt, r) = rig_program_test();
    let mut ctx = pt.start_with_context().await;
    let forged_addr = Pubkey::new_unique();
    ctx.set_account(
        &forged_addr,
        &Account {
            lamports: rent_exempt(45),
            data: program_data_bytes(&r.upgrade_authority.pubkey()),
            owner: bpf_loader_upgradeable::id(),
            executable: false,
            rent_epoch: 0,
        }
        .into(),
    );

    let fields = default_global_config_fields(r.registry_authority.pubkey(), r.treasury, false);
    let ix = init_global_config_ix_raw(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.upgrade_authority.pubkey(),
        true,
        &forged_addr,
        fields,
    );
    let err = send(&mut ctx, &[ix], &r.payer, &[&r.upgrade_authority])
        .await
        .expect_err("a ProgramData account at the wrong address must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::NotUpgradeAuthority as u32)
    );
    let (global_pda, _) = sclient::global_config_pda(&r.settlement_program);
    assert!(
        ctx.banks_client
            .get_account(global_pda)
            .await
            .unwrap()
            .is_none(),
        "a rejected InitGlobalConfig must not create the global_config pda"
    );
}

/// Probe 2 (owner check): the same forged, otherwise well-formed `ProgramData` content as probe 1 (real
/// authority stored and signing) — owned by the system program instead of `bpf_loader_upgradeable`.
/// `expect != *program_data_acc.key` under the real, currently-deployed `program_data` PDA is not
/// constructible here without also breaking the runtime's own ability to load `zk_settlement`'s
/// executable (that PDA's owner is a runtime invariant, not something this test can safely override) —
/// this probe instead confirms the owner comparison itself rejects a mismatch, independent of address.
#[tokio::test]
async fn init_global_config_rejects_program_data_owned_by_the_wrong_program() {
    let (pt, r) = rig_program_test();
    let mut ctx = pt.start_with_context().await;
    let forged_addr = Pubkey::new_unique();
    ctx.set_account(
        &forged_addr,
        &Account {
            lamports: rent_exempt(45),
            data: program_data_bytes(&r.upgrade_authority.pubkey()),
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        }
        .into(),
    );

    let fields = default_global_config_fields(r.registry_authority.pubkey(), r.treasury, false);
    let ix = init_global_config_ix_raw(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.upgrade_authority.pubkey(),
        true,
        &forged_addr,
        fields,
    );
    let err = send(&mut ctx, &[ix], &r.payer, &[&r.upgrade_authority])
        .await
        .expect_err("a ProgramData account not owned by bpf_loader_upgradeable must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::NotUpgradeAuthority as u32)
    );
    let (global_pda, _) = sclient::global_config_pda(&r.settlement_program);
    assert!(
        ctx.banks_client
            .get_account(global_pda)
            .await
            .unwrap()
            .is_none(),
        "a rejected InitGlobalConfig must not create the global_config pda"
    );
}

/// Probe 3 (signer check): the REAL `program_data` account, address and owner both
/// correct, the real authority pubkey named in the `authority` account slot — but that account is not
/// marked as a transaction signer. Isolates the `is_signer` compare: removing it (leaving only the
/// stored-pubkey compare, which matches here) would let this through.
#[tokio::test]
async fn init_global_config_rejects_the_real_authority_when_not_signing() {
    let (pt, r) = rig_program_test();
    let mut ctx = pt.start_with_context().await;
    set_program_data_authority(
        &mut ctx,
        &r.settlement_program,
        &r.upgrade_authority.pubkey(),
    )
    .await;
    let program_data = sclient::program_data_pda(&r.settlement_program);

    let fields = default_global_config_fields(r.registry_authority.pubkey(), r.treasury, false);
    let ix = init_global_config_ix_raw(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.upgrade_authority.pubkey(),
        false, // real authority pubkey, but not marked as a signer
        &program_data,
        fields,
    );
    // `r.upgrade_authority` deliberately not in the signer list — the AccountMeta above already marks it
    // not-a-signer, and it must sign nothing for this to be a faithful probe.
    let err = send(&mut ctx, &[ix], &r.payer, &[])
        .await
        .expect_err("the real upgrade authority, not signing, must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::NotUpgradeAuthority as u32)
    );
    let (global_pda, _) = sclient::global_config_pda(&r.settlement_program);
    assert!(
        ctx.banks_client
            .get_account(global_pda)
            .await
            .unwrap()
            .is_none(),
        "a rejected InitGlobalConfig must not create the global_config pda"
    );
}

// ---------------------------------------------------------------------------------------------
// (1) namespace + nonce
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn reserved_init_chain_rejects_a_signer_that_is_not_the_registry_authority() {
    let (mut ctx, r) = rig(false).await;
    let chain_id = 1u64;
    allow_reserved(&mut ctx, &r, chain_id).await;

    let decoy_registry_authority = funded_keypair();
    let ix = sclient::init_chain_reserved_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        &decoy_registry_authority.pubkey(),
        chain_id,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    let err = send(
        &mut ctx,
        &[ix],
        &r.payer,
        &[&r.authority, &decoy_registry_authority],
    )
    .await
    .expect_err("a co-signer that is not the registry authority must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::NotRegistryAuthority as u32)
    );

    let (root_pda, _) = sclient::root_pda(&r.settlement_program, chain_id);
    assert!(
        ctx.banks_client
            .get_account(root_pda)
            .await
            .unwrap()
            .is_none(),
        "a rejected InitChain must not create the root pda"
    );
}

#[tokio::test]
async fn reserved_init_chain_rejects_an_id_with_no_allowlist_marker() {
    let (mut ctx, r) = rig(false).await;
    let chain_id = 2u64;
    // deliberately skip allow_reserved(...)

    let ix = sclient::init_chain_reserved_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        &r.registry_authority.pubkey(),
        chain_id,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    let err = send(
        &mut ctx,
        &[ix],
        &r.payer,
        &[&r.authority, &r.registry_authority],
    )
    .await
    .expect_err("an id with no live AllowReservedId marker must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::ReservedIdNotAllowed as u32)
    );
}

#[tokio::test]
async fn permissionless_init_chain_rejects_a_chain_id_that_does_not_match_the_derived_id() {
    let (mut ctx, r) = rig(true).await;
    // A well-formed permissionless id (>= 2^32) that is not the one the program will derive for
    // (authority, nonce=0) — astronomically unlikely to collide with the real derivation.
    let wrong_id = rome_zk_layouts::chainid::PERMISSIONLESS_BASE + 123_456_789;
    let real_id = sclient::derive_permissionless_chain_id(&r.authority.pubkey(), 0);
    assert_ne!(
        wrong_id, real_id,
        "test setup: wrong_id must not accidentally equal the real one"
    );

    let ix = sclient::init_chain_permissionless_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        wrong_id,
        0,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    let err = send(&mut ctx, &[ix], &r.payer, &[&r.authority])
        .await
        .expect_err("a chain_id argument that disagrees with the derived id must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::BadPermissionlessChainId as u32)
    );
}

#[tokio::test]
async fn permissionless_init_chain_rejected_while_globally_disabled() {
    let (mut ctx, r) = rig(false).await; // permissionless_init_enabled = false
    let chain_id = sclient::derive_permissionless_chain_id(&r.authority.pubkey(), 0);
    let ix = sclient::init_chain_permissionless_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        chain_id,
        0,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    let err = send(&mut ctx, &[ix], &r.payer, &[&r.authority])
        .await
        .expect_err("permissionless InitChain must be rejected while the global gate is off");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::PermissionlessInitDisabled as u32)
    );
}

#[tokio::test]
async fn two_permissionless_registrations_by_the_same_authority_get_nonce_0_then_1_and_distinct_ids(
) {
    let (mut ctx, r) = rig(true).await;

    let (nonce_pda, _) = sclient::perm_nonce_pda(&r.settlement_program, &r.authority.pubkey());
    assert!(
        ctx.banks_client
            .get_account(nonce_pda)
            .await
            .unwrap()
            .is_none(),
        "the nonce account must not exist before the first permissionless InitChain"
    );

    let id0 = sclient::derive_permissionless_chain_id(&r.authority.pubkey(), 0);
    let ix0 = sclient::init_chain_permissionless_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        id0,
        0,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    send(&mut ctx, &[ix0], &r.payer, &[&r.authority])
        .await
        .expect("first permissionless InitChain should succeed");

    let nonce_after_first = rome_zk_layouts::perm_nonce::read(
        &ctx.banks_client
            .get_account(nonce_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap()
    .nonce;
    assert_eq!(nonce_after_first, 1, "nonce must advance from 0 to 1");

    let id1 = sclient::derive_permissionless_chain_id(&r.authority.pubkey(), 1);
    assert_ne!(
        id0, id1,
        "the second registration must derive a distinct id"
    );
    let ix1 = sclient::init_chain_permissionless_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        id1,
        1,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    send(&mut ctx, &[ix1], &r.payer, &[&r.authority])
        .await
        .expect("second permissionless InitChain (nonce=1) should succeed");

    let nonce_after_second = rome_zk_layouts::perm_nonce::read(
        &ctx.banks_client
            .get_account(nonce_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap()
    .nonce;
    assert_eq!(nonce_after_second, 2);

    let (root0, _) = sclient::root_pda(&r.settlement_program, id0);
    let (root1, _) = sclient::root_pda(&r.settlement_program, id1);
    assert!(ctx.banks_client.get_account(root0).await.unwrap().is_some());
    assert!(ctx.banks_client.get_account(root1).await.unwrap().is_some());
}

/// An authority whose next permissionless id was already claimed (a grinding attack: someone else lands
/// a `chain_id` that collides with what nonce 0 for this authority would derive to) must be able to
/// skip past it, rather than being wedged forever at a nonce it can never successfully use. Simulated
/// here by directly pre-populating nonce 0's `chain_config` PDA as already program-owned with real data
/// — from `InitChain`'s perspective this is indistinguishable from "someone else's chain already lives
/// at this id" (the grinding attack's actual end state), and it is far cheaper to set up than an actual
/// ~2^53 preimage search.
#[tokio::test]
async fn permissionless_init_chain_skips_a_nonce_whose_id_was_already_claimed() {
    let (mut ctx, r) = rig(true).await;
    let burned_id = sclient::derive_permissionless_chain_id(&r.authority.pubkey(), 0);
    let (cc_pda, _) = sclient::chain_config_pda(&r.settlement_program, burned_id);
    ctx.set_account(
        &cc_pda,
        &Account {
            lamports: rent_exempt(rome_zk_layouts::chain_config::LEN_V2),
            data: vec![0xAAu8; rome_zk_layouts::chain_config::LEN_V2],
            owner: r.settlement_program,
            executable: false,
            rent_epoch: 0,
        }
        .into(),
    );

    // The naive nonce-0 attempt fails — the id's chain_config PDA is already someone else's.
    let ix0 = sclient::init_chain_permissionless_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        burned_id,
        0,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    send(&mut ctx, &[ix0], &r.payer, &[&r.authority])
        .await
        .expect_err("InitChain against an already-claimed chain_config PDA must fail");

    // The stored nonce must be untouched by the failed attempt (a failed instruction has no partial
    // commit) — still 0, so the client can compute nonce 1's id off-chain exactly as it would for a
    // fresh authority.
    let (nonce_pda, _) = sclient::perm_nonce_pda(&r.settlement_program, &r.authority.pubkey());
    assert!(
        ctx.banks_client
            .get_account(nonce_pda)
            .await
            .unwrap()
            .is_none(),
        "a failed InitChain must not create the nonce pda"
    );

    // Skip ahead to nonce 1 — must be accepted even though the stored nonce is still 0
    // (`nonce >= stored_nonce`, not `==`).
    let id1 = sclient::derive_permissionless_chain_id(&r.authority.pubkey(), 1);
    let ix1 = sclient::init_chain_permissionless_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        id1,
        1,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    send(&mut ctx, &[ix1], &r.payer, &[&r.authority])
        .await
        .expect("skipping ahead to nonce 1 should succeed");

    // The stored nonce is now 2 (nonce + 1), not 1 — the skip is recorded, so nonce 0 is never retried.
    let nonce_after = rome_zk_layouts::perm_nonce::read(
        &ctx.banks_client
            .get_account(nonce_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap()
    .nonce;
    assert_eq!(
        nonce_after, 2,
        "the stored nonce must reflect the skip (nonce_used + 1), not a plain increment from 0"
    );
}

// ---------------------------------------------------------------------------------------------
// (2) deposit lifecycle
// ---------------------------------------------------------------------------------------------

/// A permissionless `InitChainV2` may not carry its own verifier keys. Whoever calls it
/// picks nothing about what the chain can later finalize under: the registry starts empty and the
/// registry authority adds the layout-1 key afterwards with `SetRegistryEntry`.
///
/// Solana rolls a failed transaction back, so "the nonce did not move" and "no account was created" hold
/// whether or not the refusal comes first. What shows that the refusal really sits in front of the nonce
/// bookkeeping is the compute it spends: the nonce account is created and a chain id is hashed on the way
/// past `require_permissionless_id`, which costs thousands of units. The refusal must cost far less.
#[tokio::test]
async fn permissionless_init_chain_refuses_caller_supplied_registry_entries() {
    assert_eq!(SettleError::RegistryEntriesNotAllowed as u32, 82);
    let (mut ctx, r) = rig(true).await;
    let chain_id = sclient::derive_permissionless_chain_id(&r.authority.pubkey(), 0);
    let mut fields = empty_init_chain_fields(
        r.genesis_state_root,
        r.challenge_window_slots,
        16,
        r.inbox_program,
    );
    fields.registry_entries = vec![sclient::RegistryEntry {
        curve: rome_zk_layouts::registry::CURVE_BN254,
        scheme: rome_zk_layouts::registry::SCHEME_PLONK,
        vkey_hash: [0x42u8; 32],
        layout_id: rome_zk_layouts::registry::LAYOUT_ZISK_V1,
    }];
    let ix = sclient::init_chain_permissionless_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        chain_id,
        0,
        fields,
    );
    let payer_before = lamports_of(&mut ctx, r.payer.pubkey()).await;
    let (result, cu, _logs) =
        rome_zk_testkit::send_measuring_cu(&mut ctx, &[ix], &r.payer, &[&r.authority]).await;
    let err =
        result.expect_err("a permissionless InitChain with its own registry entries must refuse");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::RegistryEntriesNotAllowed as u32)
    );

    // Nothing was created, nothing was locked, the nonce did not move.
    let (root_pda, _) = sclient::root_pda(&r.settlement_program, chain_id);
    let (registry_pda, _) = sclient::registry_pda(&r.settlement_program, chain_id);
    let (cc_pda, _) = sclient::chain_config_pda(&r.settlement_program, chain_id);
    let (nonce_pda, _) = sclient::perm_nonce_pda(&r.settlement_program, &r.authority.pubkey());
    for (name, key) in [
        ("root", root_pda),
        ("registry", registry_pda),
        ("chain_config", cc_pda),
        ("perm_nonce", nonce_pda),
    ] {
        assert!(
            ctx.banks_client.get_account(key).await.unwrap().is_none(),
            "{name} must not exist after the refusal"
        );
    }
    let payer_after = lamports_of(&mut ctx, r.payer.pubkey()).await;
    assert!(
        payer_before - payer_after <= 10_000,
        "the payer may lose the two signature fees and nothing else (no deposit): lost {}",
        payer_before - payer_after
    );
    eprintln!("CU InitChainV2 (permissionless, refused: RegistryEntriesNotAllowed): {cu}");
    assert!(
        cu < REFUSAL_CU_CEILING,
        "the refusal must come before the nonce account is created and the chain id derived: spent {cu} CU"
    );
}

/// Ceiling for a refusal that sits in front of `require_permissionless_id`. Measured on SBPF v3: 5,287 CU
/// with the check in front, 11,469 CU with it moved behind the nonce bookkeeping.
const REFUSAL_CU_CEILING: u64 = 8_000;

/// The reserved path is untouched: the registry authority co-signs there, so caller-supplied entries stay.
#[tokio::test]
async fn reserved_init_chain_still_accepts_registry_entries() {
    let (mut ctx, r) = rig(false).await;
    let chain_id = 7u64;
    allow_reserved(&mut ctx, &r, chain_id).await;
    let entry = sclient::RegistryEntry {
        curve: rome_zk_layouts::registry::CURVE_BN254,
        scheme: rome_zk_layouts::registry::SCHEME_PLONK,
        vkey_hash: [0x42u8; 32],
        layout_id: rome_zk_layouts::registry::LAYOUT_ZISK_V1,
    };
    let mut fields = empty_init_chain_fields(
        r.genesis_state_root,
        r.challenge_window_slots,
        16,
        r.inbox_program,
    );
    fields.registry_entries = vec![entry];
    let ix = sclient::init_chain_reserved_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        &r.registry_authority.pubkey(),
        chain_id,
        fields,
    );
    send(
        &mut ctx,
        &[ix],
        &r.payer,
        &[&r.authority, &r.registry_authority],
    )
    .await
    .expect("a reserved InitChain with registry entries should succeed");
    let (registry_pda, _) = sclient::registry_pda(&r.settlement_program, chain_id);
    let reg = sclient::decode_registry_account(
        &ctx.banks_client
            .get_account(registry_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap();
    assert_eq!(reg.count, 1);
    assert_eq!(reg.entries[0].vkey_hash, [0x42u8; 32]);
}

/// One `SetRegistryEntry` for `chain_id`, signed by the registry authority, activating now.
fn set_entry_ix(r: &Rig, chain_id: u64, vkey: [u8; 32], layout_id: u8, now: u64) -> Instruction {
    sclient::set_registry_entry_ix(
        &r.settlement_program,
        &r.registry_authority.pubkey(),
        &r.payer.pubkey(),
        chain_id,
        sclient::RegistryEntry {
            curve: rome_zk_layouts::registry::CURVE_BN254,
            scheme: rome_zk_layouts::registry::SCHEME_PLONK,
            vkey_hash: vkey,
            layout_id,
        },
        now,
    )
}

/// Layout 2 (the header fallback) binds neither the chain id nor the inbox commitment, so a
/// permissionless chain's registry only ever holds layout-1 keys. `SetRegistryEntry` refuses layout 2
/// there by name, and the registry is left empty; the same call with layout 1 is the control that
/// shows nothing else was wrong with it.
#[tokio::test]
async fn set_registry_entry_refuses_layout_2_on_a_permissionless_chain() {
    assert_eq!(SettleError::HeaderFallbackNotAllowed as u32, 85);
    let (mut ctx, r) = rig(true).await;
    let chain_id = sclient::derive_permissionless_chain_id(&r.authority.pubkey(), 0);
    let init_ix = sclient::init_chain_permissionless_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        chain_id,
        0,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    send(&mut ctx, &[init_ix], &r.payer, &[&r.authority])
        .await
        .expect("permissionless InitChain should succeed");
    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let (registry_pda, _) = sclient::registry_pda(&r.settlement_program, chain_id);
    let count = |data: &[u8]| sclient::decode_registry_account(data).unwrap().count;

    let vkey = [0x42u8; 32];
    let ix = set_entry_ix(
        &r,
        chain_id,
        vkey,
        rome_zk_layouts::registry::LAYOUT_HEADER_FALLBACK,
        now,
    );
    let err = send(&mut ctx, &[ix], &r.payer, &[&r.registry_authority])
        .await
        .expect_err("layout 2 on a permissionless chain must refuse");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::HeaderFallbackNotAllowed as u32)
    );
    let data = ctx
        .banks_client
        .get_account(registry_pda)
        .await
        .unwrap()
        .unwrap()
        .data;
    assert_eq!(count(&data), 0, "the refused entry must not be written");

    // Control: the same vkey under layout 1 registers.
    let ix = set_entry_ix(
        &r,
        chain_id,
        vkey,
        rome_zk_layouts::registry::LAYOUT_ZISK_V1,
        now,
    );
    send(&mut ctx, &[ix], &r.payer, &[&r.registry_authority])
        .await
        .expect("layout 1 on a permissionless chain must register");
    let data = ctx
        .banks_client
        .get_account(registry_pda)
        .await
        .unwrap()
        .unwrap()
        .data;
    assert_eq!(count(&data), 1);
}

/// The reserved range keeps layout 2: Rome's own chains still register a header-fallback key.
#[tokio::test]
async fn set_registry_entry_still_accepts_layout_2_on_a_reserved_chain() {
    let (mut ctx, r) = rig(false).await;
    let chain_id = 7u64;
    allow_reserved(&mut ctx, &r, chain_id).await;
    let init_ix = sclient::init_chain_reserved_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        &r.registry_authority.pubkey(),
        chain_id,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    send(
        &mut ctx,
        &[init_ix],
        &r.payer,
        &[&r.authority, &r.registry_authority],
    )
    .await
    .expect("reserved InitChain should succeed");
    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let ix = set_entry_ix(
        &r,
        chain_id,
        [0x43u8; 32],
        rome_zk_layouts::registry::LAYOUT_HEADER_FALLBACK,
        now,
    );
    send(&mut ctx, &[ix], &r.payer, &[&r.registry_authority])
        .await
        .expect("layout 2 on a reserved chain must register");
    let (registry_pda, _) = sclient::registry_pda(&r.settlement_program, chain_id);
    let reg = sclient::decode_registry_account(
        &ctx.banks_client
            .get_account(registry_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap();
    assert_eq!(reg.count, 1);
    assert_eq!(
        reg.entries[0].layout_id,
        rome_zk_layouts::registry::LAYOUT_HEADER_FALLBACK
    );
}

/// Puts a chain's progress counters where a run of posted batches would have left them, by rewriting the
/// two accounts that hold them (`root`: `head_pending_batch`, `head_final_batch`; `chain_config`:
/// `posted_batches`). Used by the permissionless-chain tests whose subject is the refund or reclaim
/// trigger: a permissionless chain is proved-only, and no proof exists in this repository
/// for a permissionless chain id, so the unproved `PostRoot` that used to set this up is refused.
async fn set_chain_progress(
    ctx: &mut solana_program_test::ProgramTestContext,
    r: &Rig,
    chain_id: u64,
    head_pending_batch: u64,
    head_final_batch: u64,
    posted_batches: u64,
) {
    let (root_pda, _) = sclient::root_pda(&r.settlement_program, chain_id);
    let mut root_acc = ctx
        .banks_client
        .get_account(root_pda)
        .await
        .unwrap()
        .unwrap();
    let mut root = rome_zk_layouts::root::read(&root_acc.data).unwrap();
    root.head_pending_batch = head_pending_batch;
    root.head_final_batch = head_final_batch;
    let n = rome_zk_layouts::root::MIN_LEN;
    root_acc.data[..n].copy_from_slice(&rome_zk_layouts::root::write(&root));
    ctx.set_account(&root_pda, &root_acc.into());

    let (cc_pda, _) = sclient::chain_config_pda(&r.settlement_program, chain_id);
    let mut cc_acc = ctx.banks_client.get_account(cc_pda).await.unwrap().unwrap();
    let mut cc = rome_zk_layouts::chain_config::read(&cc_acc.data).unwrap();
    cc.posted_batches = posted_batches as u32;
    rome_zk_layouts::chain_config::write(&mut cc_acc.data, &cc);
    ctx.set_account(&cc_pda, &cc_acc.into());
}

/// Proved-only: a permissionless chain may only ever finalize a root a ZisK proof backs,
/// against a verifier key Rome registered. The unproved `PostRoot` is therefore closed to it, by name.
///
/// A failed transaction rolls back, so "nothing changed" holds wherever the refusal sits. What pins its
/// position is the compute it spends: the refusal must come before any account is read (a PDA derivation
/// alone costs over a thousand units), so it has to cost a small fraction of an accepted `PostRoot`.
#[tokio::test]
async fn permissionless_post_root_unproved_refuses_by_name() {
    assert_eq!(SettleError::UnprovedRootNotAllowed as u32, 83);
    let (mut ctx, r) = rig(true).await;
    let chain_id = sclient::derive_permissionless_chain_id(&r.authority.pubkey(), 0);
    let ix = sclient::init_chain_permissionless_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        chain_id,
        0,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    send(&mut ctx, &[ix], &r.payer, &[&r.authority])
        .await
        .expect("permissionless InitChain should succeed");

    // Everything else is valid: the inbox batch is finalized and matches, the sequence is right. Only the
    // missing proof makes this refuse.
    let acc = keccak::hashv(&[b"proved-only acc"]).to_bytes();
    let inbox_pda = sclient::inbox_batch_pda(&r.inbox_program, &r.settlement_program, chain_id, 1);
    ctx.set_account(
        &inbox_pda,
        &inbox_batch_account(
            r.inbox_program,
            chain_id,
            1,
            r.settlement_program,
            r.authority.pubkey(),
            acc,
        )
        .into(),
    );
    // The client builder refuses this for a permissionless chain, so go around it to show the PROGRAM does too.
    let post_ix = sclient::post_root_ix_unchecked(
        &r.settlement_program,
        &r.authority.pubkey(),
        &r.inbox_program,
        &r.treasury,
        post_root_args_for(&r, chain_id, 1, acc, 0),
    );

    let (root_pda, _) = sclient::root_pda(&r.settlement_program, chain_id);
    let (registry_pda, _) = sclient::registry_pda(&r.settlement_program, chain_id);
    let (cc_pda, _) = sclient::chain_config_pda(&r.settlement_program, chain_id);
    let (global_pda, _) = sclient::global_config_pda(&r.settlement_program);
    let (pending_pda, _) = sclient::pending_pda(&r.settlement_program, chain_id, 1);
    let watched = [
        r.authority.pubkey(),
        r.treasury,
        root_pda,
        registry_pda,
        cc_pda,
        global_pda,
        inbox_pda,
    ];
    let mut before = Vec::new();
    for k in watched {
        before.push(ctx.banks_client.get_account(k).await.unwrap());
    }

    // The payer pays the signature fees, so the authority's balance must not move at all.
    let (result, cu, _logs) =
        rome_zk_testkit::send_measuring_cu(&mut ctx, &[post_ix], &r.payer, &[&r.authority]).await;
    let err = result.expect_err("an unproved PostRoot on a permissionless chain must refuse");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::UnprovedRootNotAllowed as u32)
    );

    for (k, b) in watched.iter().zip(before) {
        let after = ctx.banks_client.get_account(*k).await.unwrap();
        assert_eq!(after, b, "{k} must be untouched by the refusal");
    }
    assert!(
        ctx.banks_client
            .get_account(pending_pda)
            .await
            .unwrap()
            .is_none(),
        "no pending batch may exist after the refusal"
    );
    eprintln!("CU PostRoot (permissionless, refused: UnprovedRootNotAllowed): {cu}");
    assert!(
        cu < UNPROVED_REFUSAL_CU_CEILING,
        "the refusal must come before any account is read: spent {cu} CU"
    );
}

/// Genesis: a permissionless chain registered with an EMPTY registry has no final root. Batch 0 is the
/// genesis the chain authority wrote at `InitChainV2`, and nobody has proved it, so a CPI caller (the
/// bridge) must not read it as final: `RootView(0)` refuses `NotFinal`. The chain keeps refusing until
/// its first proved batch advances `head_final_batch`.
#[tokio::test]
async fn permissionless_chain_genesis_is_not_a_final_root() {
    let (mut ctx, r) = rig(true).await;
    let chain_id = sclient::derive_permissionless_chain_id(&r.authority.pubkey(), 0);
    let ix = sclient::init_chain_permissionless_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        chain_id,
        0,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    send(&mut ctx, &[ix], &r.payer, &[&r.authority])
        .await
        .expect("permissionless InitChain should succeed");

    let err = send(
        &mut ctx,
        &[sclient::root_view_ix(&r.settlement_program, chain_id, 0)],
        &r.payer,
        &[],
    )
    .await
    .expect_err("RootView(0) on a permissionless chain with no proved batch must refuse");
    assert_eq!(custom_error(&err), Some(SettleError::NotFinal as u32));
}

/// Ceiling for the proved-only refusal at the top of `PostRoot`. 4,096 CU measured on SBPF v3, where an accepted unproved PostRoot costs 26,311.
const UNPROVED_REFUSAL_CU_CEILING: u64 = 6_000;

/// Rome's reserved-range chains keep the unproved, window-based path exactly as before.
#[tokio::test]
async fn reserved_post_root_unproved_still_accepted() {
    let (mut ctx, r) = rig(false).await;
    let chain_id = 9u64;
    reserved_chain_ready_for_post_root(&mut ctx, &r, chain_id, 16).await;
    let acc = keccak::hashv(&[b"reserved unproved acc"]).to_bytes();
    let inbox_pda = sclient::inbox_batch_pda(&r.inbox_program, &r.settlement_program, chain_id, 1);
    ctx.set_account(
        &inbox_pda,
        &inbox_batch_account(
            r.inbox_program,
            chain_id,
            1,
            r.settlement_program,
            r.authority.pubkey(),
            acc,
        )
        .into(),
    );
    let post_ix = sclient::post_root_ix(
        &r.settlement_program,
        &r.authority.pubkey(),
        &r.inbox_program,
        &r.treasury,
        post_root_args_for(&r, chain_id, 1, acc, 0),
    )
    .expect("reserved chain");
    let cu = send(&mut ctx, &[post_ix], &r.authority, &[])
        .await
        .expect("an unproved PostRoot on a reserved chain must still be accepted");
    eprintln!("CU PostRoot (reserved, unproved, accepted): {cu}");
    let (pending_pda, _) = sclient::pending_pda(&r.settlement_program, chain_id, 1);
    assert!(
        ctx.banks_client
            .get_account(pending_pda)
            .await
            .unwrap()
            .is_some(),
        "the pending batch must exist"
    );
    let (root_pda, _) = sclient::root_pda(&r.settlement_program, chain_id);
    let root = sclient::decode_root_account(
        &ctx.banks_client
            .get_account(root_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap();
    assert_eq!(root.head_pending_batch, 1);
}

#[tokio::test]
async fn permissionless_init_chain_locks_the_configured_deposit_in_chain_config() {
    let (mut ctx, r) = rig(true).await;
    let chain_id = sclient::derive_permissionless_chain_id(&r.authority.pubkey(), 0);
    let payer_before = lamports_of(&mut ctx, r.payer.pubkey()).await;
    let ix = sclient::init_chain_permissionless_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        chain_id,
        0,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    send(&mut ctx, &[ix], &r.payer, &[&r.authority])
        .await
        .expect("permissionless InitChain should succeed");

    let (cc_pda, _) = sclient::chain_config_pda(&r.settlement_program, chain_id);
    let cc = sclient::decode_chain_config_account(
        &ctx.banks_client
            .get_account(cc_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap();
    assert!(!cc.reserved);
    assert_eq!(cc.deposit_lamports, 2_000_000_000);
    assert!(!cc.deposit_refunded);
    let cc_actual_lamports = lamports_of(&mut ctx, cc_pda).await;
    assert!(
        cc_actual_lamports >= 2_000_000_000,
        "the deposit must actually sit in chain_config's own balance, not just the recorded field"
    );
    let payer_after = lamports_of(&mut ctx, r.payer.pubkey()).await;
    assert!(
        payer_before - payer_after >= 2_000_000_000,
        "the deposit must be debited from the payer"
    );
}

#[tokio::test]
async fn reserved_init_chain_locks_no_deposit() {
    let (mut ctx, r) = rig(false).await;
    let chain_id = 3u64;
    allow_reserved(&mut ctx, &r, chain_id).await;
    let ix = sclient::init_chain_reserved_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        &r.registry_authority.pubkey(),
        chain_id,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    send(
        &mut ctx,
        &[ix],
        &r.payer,
        &[&r.authority, &r.registry_authority],
    )
    .await
    .expect("reserved InitChain should succeed");
    let (cc_pda, _) = sclient::chain_config_pda(&r.settlement_program, chain_id);
    let cc = sclient::decode_chain_config_account(
        &ctx.banks_client
            .get_account(cc_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap();
    assert!(cc.reserved);
    assert_eq!(cc.deposit_lamports, 0);
    assert!(cc.deposit_refunded, "nothing to refund on a reserved chain");
}

#[tokio::test]
async fn refund_deposit_rejected_before_either_trigger() {
    let (mut ctx, r) = rig(true).await;
    let chain_id = sclient::derive_permissionless_chain_id(&r.authority.pubkey(), 0);
    let ix = sclient::init_chain_permissionless_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        chain_id,
        0,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    send(&mut ctx, &[ix], &r.payer, &[&r.authority])
        .await
        .expect("permissionless InitChain should succeed");

    let refund_ix =
        sclient::refund_deposit_ix(&r.settlement_program, chain_id, &r.authority.pubkey());
    let err = send(&mut ctx, &[refund_ix], &r.payer, &[])
        .await
        .expect_err("RefundDeposit must be rejected before either trigger");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::RefundNotYetEligible as u32)
    );
}

/// Drives one PostRoot + a window-elapsed FinalizeBatch, then asserts `RefundDeposit` succeeds off the
/// "1 final root" trigger — a fresh, real head-of-chain post (not the 10-posted-batches trigger, exercised
/// separately below).
#[tokio::test]
async fn refund_deposit_accepted_after_one_final_root() {
    let (mut ctx, r) = rig(true).await;
    let chain_id = sclient::derive_permissionless_chain_id(&r.authority.pubkey(), 0);
    let ix = sclient::init_chain_permissionless_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        chain_id,
        0,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    send(&mut ctx, &[ix], &r.payer, &[&r.authority])
        .await
        .expect("permissionless InitChain should succeed");

    // A permissionless chain is proved-only, so it cannot reach a final root through the
    // unproved PostRoot + FinalizeBatch this test used to use, and no proof exists in this repository for a
    // permissionless chain id. This test is about the refund trigger, so leave the chain exactly where
    // one posted and finalized batch would have left it.
    set_chain_progress(&mut ctx, &r, chain_id, 1, 1, 1).await;

    let authority_before = lamports_of(&mut ctx, r.authority.pubkey()).await;
    let refund_ix =
        sclient::refund_deposit_ix(&r.settlement_program, chain_id, &r.authority.pubkey());
    send(&mut ctx, &[refund_ix], &r.payer, &[])
        .await
        .expect("RefundDeposit should succeed once head_final_batch >= 1");
    let authority_after = lamports_of(&mut ctx, r.authority.pubkey()).await;
    assert_eq!(authority_after - authority_before, 2_000_000_000);

    let (cc_pda, _) = sclient::chain_config_pda(&r.settlement_program, chain_id);
    let cc = sclient::decode_chain_config_account(
        &ctx.banks_client
            .get_account(cc_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap();
    assert_eq!(cc.deposit_lamports, 0);
    assert!(cc.deposit_refunded);
}

/// The other trigger: 10 posted batches, none of them ever finalized (`head_final_batch` stays 0).
#[tokio::test]
async fn refund_deposit_accepted_after_ten_posted_batches_with_none_final() {
    let (mut ctx, r) = rig(true).await;
    let chain_id = sclient::derive_permissionless_chain_id(&r.authority.pubkey(), 0);
    let ix = sclient::init_chain_permissionless_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        chain_id,
        0,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            100,
            r.inbox_program,
        ),
    );
    send(&mut ctx, &[ix], &r.payer, &[&r.authority])
        .await
        .expect("permissionless InitChain should succeed");

    // Each batch is exactly one block, numbered the same as its batch id — `first_block`/`last_block`
    // for batch N is simply N, so no separate running counter is needed alongside the loop variable.
    // A permissionless chain is proved-only, so the unproved PostRoot that used to bring it
    // here is refused. This test is about the refund trigger, not about posting: leave the chain's
    // counters exactly where 10 posted batches would have left them.
    set_chain_progress(&mut ctx, &r, chain_id, 10, 0, 10).await;

    let (root_pda, _) = sclient::root_pda(&r.settlement_program, chain_id);
    let root = sclient::decode_root_account(
        &ctx.banks_client
            .get_account(root_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap();
    assert_eq!(
        root.head_final_batch, 0,
        "none of the 10 batches were ever finalized"
    );

    let refund_ix =
        sclient::refund_deposit_ix(&r.settlement_program, chain_id, &r.authority.pubkey());
    send(&mut ctx, &[refund_ix], &r.payer, &[])
        .await
        .expect("RefundDeposit should succeed once posted_batches >= 10, even with 0 final roots");
}

#[tokio::test]
async fn reclaim_chain_rejected_before_the_window_elapses() {
    let (mut ctx, r) = rig(true).await;
    let chain_id = sclient::derive_permissionless_chain_id(&r.authority.pubkey(), 0);
    let ix = sclient::init_chain_permissionless_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        chain_id,
        0,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    send(&mut ctx, &[ix], &r.payer, &[&r.authority])
        .await
        .expect("permissionless InitChain should succeed");

    let reclaim_ix = sclient::reclaim_chain_ix(&r.settlement_program, chain_id, &r.treasury);
    let err = send(&mut ctx, &[reclaim_ix], &r.payer, &[])
        .await
        .expect_err("ReclaimChain must be rejected before the reclaim window elapses");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::ChainNotReclaimable as u32)
    );
}

#[tokio::test]
async fn reclaim_chain_permissionless_sweeps_deposit_and_rent_to_treasury_after_the_window() {
    let (mut ctx, r) = rig(true).await;
    let chain_id = sclient::derive_permissionless_chain_id(&r.authority.pubkey(), 0);
    let ix = sclient::init_chain_permissionless_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        chain_id,
        0,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    send(&mut ctx, &[ix], &r.payer, &[&r.authority])
        .await
        .expect("permissionless InitChain should succeed");

    let (root_pda, _) = sclient::root_pda(&r.settlement_program, chain_id);
    let (registry_pda, _) = sclient::registry_pda(&r.settlement_program, chain_id);
    let (cc_pda, _) = sclient::chain_config_pda(&r.settlement_program, chain_id);
    let root_lamports = lamports_of(&mut ctx, root_pda).await;
    let registry_lamports = lamports_of(&mut ctx, registry_pda).await;
    let cc_lamports = lamports_of(&mut ctx, cc_pda).await;
    let expect_swept = root_lamports + registry_lamports + cc_lamports;
    let treasury_before = lamports_of(&mut ctx, r.treasury).await;

    ctx.warp_to_slot(ctx.banks_client.get_root_slot().await.unwrap() + RECLAIM_WINDOW_SLOTS + 10)
        .unwrap();
    let reclaim_ix = sclient::reclaim_chain_ix(&r.settlement_program, chain_id, &r.treasury);
    send(&mut ctx, &[reclaim_ix], &r.payer, &[])
        .await
        .expect("ReclaimChain should succeed after the window elapses");

    let treasury_after = lamports_of(&mut ctx, r.treasury).await;
    assert_eq!(
        treasury_after - treasury_before,
        expect_swept,
        "deposit + every closed account's rent must land on the treasury"
    );
    assert!(ctx
        .banks_client
        .get_account(root_pda)
        .await
        .unwrap()
        .is_none());
    assert!(ctx
        .banks_client
        .get_account(registry_pda)
        .await
        .unwrap()
        .is_none());
    assert!(ctx
        .banks_client
        .get_account(cc_pda)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn reclaim_chain_reserved_has_no_deposit_to_sweep_and_frees_the_id() {
    let (mut ctx, r) = rig(false).await;
    let chain_id = 4u64;
    allow_reserved(&mut ctx, &r, chain_id).await;
    let ix = sclient::init_chain_reserved_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        &r.registry_authority.pubkey(),
        chain_id,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    send(
        &mut ctx,
        &[ix],
        &r.payer,
        &[&r.authority, &r.registry_authority],
    )
    .await
    .expect("reserved InitChain should succeed");

    let (root_pda, _) = sclient::root_pda(&r.settlement_program, chain_id);
    let (registry_pda, _) = sclient::registry_pda(&r.settlement_program, chain_id);
    let (cc_pda, _) = sclient::chain_config_pda(&r.settlement_program, chain_id);
    let expect_swept = lamports_of(&mut ctx, root_pda).await
        + lamports_of(&mut ctx, registry_pda).await
        + lamports_of(&mut ctx, cc_pda).await;
    let treasury_before = lamports_of(&mut ctx, r.treasury).await;

    ctx.warp_to_slot(ctx.banks_client.get_root_slot().await.unwrap() + RECLAIM_WINDOW_SLOTS + 10)
        .unwrap();

    // A reserved chain is NOT reclaimable while its `AllowReservedId`
    // marker is still live — past the reclaim window is not enough on its own.
    let reclaim_while_allowed_ix =
        sclient::reclaim_chain_ix(&r.settlement_program, chain_id, &r.treasury);
    let err = send(&mut ctx, &[reclaim_while_allowed_ix], &r.payer, &[])
        .await
        .expect_err(
            "ReclaimChain must be rejected for a reserved chain whose allowlist marker is still live",
        );
    assert_eq!(
        custom_error(&err),
        Some(SettleError::ChainNotReclaimable as u32)
    );

    // The registry authority revokes the marker — only then does the normal reclaim apply.
    let revoke_ix = sclient::revoke_reserved_id_ix(
        &r.settlement_program,
        &r.registry_authority.pubkey(),
        chain_id,
    );
    send(&mut ctx, &[revoke_ix], &r.payer, &[&r.registry_authority])
        .await
        .expect("RevokeReservedId should succeed");
    let reclaim_ix = sclient::reclaim_chain_ix(&r.settlement_program, chain_id, &r.treasury);
    send(&mut ctx, &[reclaim_ix], &r.payer, &[])
        .await
        .expect("ReclaimChain should succeed for a reserved chain once the marker is revoked");
    let treasury_after = lamports_of(&mut ctx, r.treasury).await;
    assert_eq!(
        treasury_after - treasury_before,
        expect_swept,
        "a reserved chain locks no deposit — only rent moves to the treasury"
    );

    // id free: the same chain_id can be registered again — but the marker was just revoked above,
    // so it must be allowed again first (same as any other never-before-used reserved id).
    allow_reserved(&mut ctx, &r, chain_id).await;
    let ix2 = sclient::init_chain_reserved_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        &r.registry_authority.pubkey(),
        chain_id,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    send(
        &mut ctx,
        &[ix2],
        &r.payer,
        &[&r.authority, &r.registry_authority],
    )
    .await
    .expect("the reclaimed chain_id must be re-registrable");
}

// ---------------------------------------------------------------------------------------------
// (2b) MigrateChain, revoke-then-reserved-InitChain, and the mutations that used to survive the suite
// (the MigrateChain replay guard among them, each with a dedicated test; these were previously exercised
// only incidentally, if at all)
// ---------------------------------------------------------------------------------------------

/// A root account exactly as `InitChain` would have written it, built by hand — standing in for a chain
/// that predates registration (Tiber) and so has a real root/registry pair but no `chain_config` yet.
fn hand_built_root_account(chain_id: u64, authority: Pubkey) -> Account {
    let mut d = vec![0u8; rome_zk_layouts::root::MIN_LEN];
    d[rome_zk_layouts::root::OFF_MAGIC..rome_zk_layouts::root::OFF_MAGIC + 4]
        .copy_from_slice(&rome_zk_layouts::root::MAGIC.to_le_bytes());
    d[rome_zk_layouts::root::OFF_CHAIN_ID..rome_zk_layouts::root::OFF_CHAIN_ID + 8]
        .copy_from_slice(&chain_id.to_le_bytes());
    d[rome_zk_layouts::root::OFF_AUTHORITY..rome_zk_layouts::root::OFF_AUTHORITY + 32]
        .copy_from_slice(authority.as_ref());
    Account {
        lamports: rent_exempt(d.len()),
        data: d,
        owner: Pubkey::default(), // overwritten by the caller to the real settlement_program
        executable: false,
        rent_epoch: 0,
    }
}

#[tokio::test]
async fn migrate_chain_creates_chain_config_for_a_pre_existing_root() {
    let (mut ctx, r) = rig(false).await;
    let chain_id = 200_101u64; // reserved, Tiber-shaped
    let (root_pda, _) = sclient::root_pda(&r.settlement_program, chain_id);
    let mut root_acc = hand_built_root_account(chain_id, r.authority.pubkey());
    root_acc.owner = r.settlement_program;
    ctx.set_account(&root_pda, &root_acc.into());

    let before_slot = ctx.banks_client.get_root_slot().await.unwrap();
    let migrate_ix = sclient::migrate_chain_ix(
        &r.settlement_program,
        &r.registry_authority.pubkey(),
        &r.payer.pubkey(),
        chain_id,
        60,
    );
    send(&mut ctx, &[migrate_ix], &r.payer, &[&r.registry_authority])
        .await
        .expect("MigrateChainV2 should succeed for a pre-existing root");

    let (cc_pda, _) = sclient::chain_config_pda(&r.settlement_program, chain_id);
    let cc = sclient::decode_chain_config_account(
        &ctx.banks_client
            .get_account(cc_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap();
    assert_eq!(cc.chain_id, chain_id);
    assert!(cc.reserved, "chain_id < 2^32 must be recorded as reserved");
    assert_eq!(
        cc.deposit_lamports, 0,
        "a migrated chain never locked a deposit"
    );
    assert!(
        cc.deposit_refunded,
        "nothing to refund for a migrated chain — must start true"
    );
    assert!(
        cc.registered_slot >= before_slot,
        "registered_slot must be the migration slot, not 0 or stale"
    );
    assert_eq!(cc.posted_batches, 0);
    assert_eq!(
        cc.fee_base_lamports, 1_000_000,
        "must inherit the global default base fee"
    );
    assert_eq!(cc.fee_bps, 0, "must inherit the global default bps");
    assert_eq!(
        cc.max_drift_secs,
        Some(60),
        "MigrateChainV2's max_drift_secs argument must land in chain_config v2"
    );
}

/// A shipped instruction's body never changes at its discriminant — Tiber's recorded
/// `MigrateChain { chain_id }` (8-byte body, discriminant 15) must decode forever in the settlement
/// watcher. The v1→v2 bring-forward with `max_drift_secs` is therefore `MigrateChainV2` (discriminant
/// 23); the original `MigrateChain` still decodes and is refused by name, touching nothing. Mutation:
/// dispatch 15 to `governance::migrate_chain` again → the `RetiredInstruction` assertion goes red.
#[tokio::test]
async fn the_original_migrate_chain_body_is_refused_by_name_and_never_migrates() {
    let (mut ctx, r) = rig(false).await;
    let chain_id = 200_101u64;
    let (root_pda, _) = sclient::root_pda(&r.settlement_program, chain_id);
    let mut root_acc = hand_built_root_account(chain_id, r.authority.pubkey());
    root_acc.owner = r.settlement_program;
    ctx.set_account(&root_pda, &root_acc.into());

    let (global_config, _) = sclient::global_config_pda(&r.settlement_program);
    let (cc_pda, _) = sclient::chain_config_pda(&r.settlement_program, chain_id);
    // Exactly the bytes Tiber's recorded call carries: discriminant 15 + chain_id LE (no drift field).
    let mut data = vec![15u8];
    data.extend_from_slice(&chain_id.to_le_bytes());
    assert_eq!(
        data.len(),
        9,
        "the retired body is discriminant + chain_id only"
    );
    let legacy_ix = solana_program::instruction::Instruction {
        program_id: r.settlement_program,
        accounts: vec![
            AccountMeta::new_readonly(r.registry_authority.pubkey(), true),
            AccountMeta::new(r.payer.pubkey(), true),
            AccountMeta::new_readonly(global_config, false),
            AccountMeta::new_readonly(root_pda, false),
            AccountMeta::new(cc_pda, false),
            AccountMeta::new_readonly(solana_system_interface::program::id(), false),
        ],
        data,
    };
    let err = send(&mut ctx, &[legacy_ix], &r.payer, &[&r.registry_authority])
        .await
        .expect_err("the retired MigrateChain body must be refused");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::RetiredInstruction as u32)
    );
    assert!(
        ctx.banks_client
            .get_account(cc_pda)
            .await
            .unwrap()
            .is_none(),
        "a refused instruction must not create chain_config"
    );
}

/// The same rule applied to discriminant 3: the recorded devnet `InitChain` body (fixture, 289 bytes,
/// slot 2554) still decodes and is refused by name — it never creates anything. New chains use
/// `InitChainV2` (24). Mutation: dispatch 3 to `chain::init_chain` again → red.
#[tokio::test]
async fn the_recorded_init_chain_body_is_refused_by_name_and_creates_nothing() {
    let (mut ctx, r) = rig(false).await;
    let data: &[u8] =
        include_bytes!("../../../fixtures/settlement-program/txv1-dev-initchain-slot2554.bin");
    let chain_id = 200_101u64;
    let (root_pda, _) = sclient::root_pda(&r.settlement_program, chain_id);
    let (registry_pda, _) = sclient::registry_pda(&r.settlement_program, chain_id);
    let (cc_pda, _) = sclient::chain_config_pda(&r.settlement_program, chain_id);
    let (global_config, _) = sclient::global_config_pda(&r.settlement_program);
    let ix = solana_program::instruction::Instruction {
        program_id: r.settlement_program,
        accounts: vec![
            AccountMeta::new(r.payer.pubkey(), true),
            AccountMeta::new_readonly(r.authority.pubkey(), true),
            AccountMeta::new_readonly(r.registry_authority.pubkey(), true),
            AccountMeta::new(root_pda, false),
            AccountMeta::new(registry_pda, false),
            AccountMeta::new(cc_pda, false),
            AccountMeta::new_readonly(global_config, false),
            AccountMeta::new_readonly(
                sclient::reserved_allow_pda(&r.settlement_program, chain_id).0,
                false,
            ),
            AccountMeta::new_readonly(system_program::id(), false),
        ],
        data: data.to_vec(),
    };
    let err = send(
        &mut ctx,
        &[ix],
        &r.payer,
        &[&r.authority, &r.registry_authority],
    )
    .await
    .expect_err("the retired InitChain body must be refused");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::RetiredInstruction as u32)
    );
    for pda in [root_pda, registry_pda, cc_pda] {
        assert!(
            ctx.banks_client.get_account(pda).await.unwrap().is_none(),
            "a refused instruction must create nothing"
        );
    }
}

/// The v1→v2 realloc must top up rent against the NON-deposit balance.
/// A permissionless chain's `chain_config` holds its deposit escrow as lamports above rent; if the
/// top-up compares the new minimum against the whole balance, the deposit silently covers the 8-byte
/// delta and the later `RefundDeposit` (which moves the deposit out) is refused by the runtime
/// (`InsufficientFundsForRent`) — the deposit becomes unrefundable. Mutation: compare against `have`
/// instead of `have - deposit` → the refund step goes red.
#[tokio::test]
async fn migrate_chain_v2_keeps_a_live_deposit_refundable() {
    let (mut ctx, r) = rig(false).await;
    let chain_id = 200_106u64;
    let (root_pda, _) = sclient::root_pda(&r.settlement_program, chain_id);
    let mut root_acc = hand_built_root_account(chain_id, r.authority.pubkey());
    root_acc.owner = r.settlement_program;
    ctx.set_account(&root_pda, &root_acc.into());

    let deposit = 25_000_000_000u64;
    let (cc_pda, _) = sclient::chain_config_pda(&r.settlement_program, chain_id);
    let v1_fields = rome_zk_layouts::chain_config::ChainConfigFields {
        chain_id,
        reserved: false,
        deposit_lamports: deposit,
        deposit_refunded: false,
        registered_slot: 42,
        posted_batches: 10, // refund-eligible (>= 10 posted batches)
        fee_base_lamports: 1_000_000,
        fee_bps: 0,
        max_drift_secs: None,
    };
    let mut v1_acc = hand_built_v1_chain_config(v1_fields);
    v1_acc.lamports += deposit; // the escrow sits above the v1 rent minimum, exactly as InitChain leaves it
    v1_acc.owner = r.settlement_program;
    ctx.set_account(&cc_pda, &v1_acc.into());

    let migrate_ix = sclient::migrate_chain_ix(
        &r.settlement_program,
        &r.registry_authority.pubkey(),
        &r.payer.pubkey(),
        chain_id,
        60,
    );
    send(&mut ctx, &[migrate_ix], &r.payer, &[&r.registry_authority])
        .await
        .expect("MigrateChainV2 must realloc a v1 chain_config carrying a deposit");
    let after = ctx.banks_client.get_account(cc_pda).await.unwrap().unwrap();
    let rent_v2 = rent_exempt(rome_zk_layouts::chain_config::LEN_V2);
    assert!(
        after.lamports >= rent_v2 + deposit,
        "the non-deposit balance must cover v2 rent: lamports {} < rent {} + deposit {}",
        after.lamports,
        rent_v2,
        deposit
    );

    let refund_ix =
        sclient::refund_deposit_ix(&r.settlement_program, chain_id, &r.authority.pubkey());
    send(&mut ctx, &[refund_ix], &r.payer, &[])
        .await
        .expect("RefundDeposit must still succeed after the migration");
    let final_acc = ctx.banks_client.get_account(cc_pda).await.unwrap().unwrap();
    assert!(
        final_acc.lamports >= rent_v2,
        "the account must stay rent-exempt after the refund"
    );
    let cc = sclient::decode_chain_config_account(&final_acc.data).unwrap();
    assert!(cc.deposit_refunded);
}

/// A v1-shaped `chain_config` (47 bytes, `VERSION_V1`) built by hand — standing in for a chain that
/// migrated before `max_drift_secs` was added. `MigrateChainV2` must bring it to v2 in place: realloc to
/// 55 bytes and preserve every existing field, only adding `max_drift_secs`.
fn hand_built_v1_chain_config(fields: rome_zk_layouts::chain_config::ChainConfigFields) -> Account {
    assert_eq!(
        fields.max_drift_secs, None,
        "a v1 chain_config carries no drift bound"
    );
    let mut d = vec![0u8; rome_zk_layouts::chain_config::LEN_V1];
    rome_zk_layouts::chain_config::write(&mut d, &fields);
    assert_eq!(d.len(), rome_zk_layouts::chain_config::LEN_V1);
    Account {
        lamports: rent_exempt(d.len()),
        data: d,
        owner: Pubkey::default(), // overwritten by the caller to the real settlement_program
        executable: false,
        rent_epoch: 0,
    }
}

#[tokio::test]
async fn migrate_chain_reallocs_a_v1_chain_config_to_v2_preserving_its_fields() {
    let (mut ctx, r) = rig(false).await;
    let chain_id = 200_104u64;
    let (root_pda, _) = sclient::root_pda(&r.settlement_program, chain_id);
    let mut root_acc = hand_built_root_account(chain_id, r.authority.pubkey());
    root_acc.owner = r.settlement_program;
    ctx.set_account(&root_pda, &root_acc.into());

    let (cc_pda, _) = sclient::chain_config_pda(&r.settlement_program, chain_id);
    let v1_fields = rome_zk_layouts::chain_config::ChainConfigFields {
        chain_id,
        reserved: true,
        deposit_lamports: 0,
        deposit_refunded: true,
        registered_slot: 42,
        posted_batches: 7,
        fee_base_lamports: 2_500_000,
        fee_bps: 3,
        max_drift_secs: None,
    };
    let mut v1_acc = hand_built_v1_chain_config(v1_fields);
    v1_acc.owner = r.settlement_program;
    ctx.set_account(&cc_pda, &v1_acc.into());

    let migrate_ix = sclient::migrate_chain_ix(
        &r.settlement_program,
        &r.registry_authority.pubkey(),
        &r.payer.pubkey(),
        chain_id,
        60,
    );
    send(&mut ctx, &[migrate_ix], &r.payer, &[&r.registry_authority])
        .await
        .expect("MigrateChainV2 must realloc a v1 chain_config to v2");

    let acc = ctx.banks_client.get_account(cc_pda).await.unwrap().unwrap();
    assert_eq!(
        acc.data.len(),
        rome_zk_layouts::chain_config::LEN_V2,
        "the account must be reallocated to the v2 length"
    );
    let cc = sclient::decode_chain_config_account(&acc.data).unwrap();
    assert_eq!(cc.max_drift_secs, Some(60), "the new field must be set");
    // Every pre-existing field must survive the migration untouched.
    assert_eq!(cc.reserved, v1_fields.reserved);
    assert_eq!(cc.deposit_lamports, v1_fields.deposit_lamports);
    assert_eq!(cc.deposit_refunded, v1_fields.deposit_refunded);
    assert_eq!(cc.registered_slot, v1_fields.registered_slot);
    assert_eq!(cc.posted_batches, v1_fields.posted_batches);
    assert_eq!(cc.fee_base_lamports, v1_fields.fee_base_lamports);
    assert_eq!(cc.fee_bps, v1_fields.fee_bps);
}

#[tokio::test]
async fn migrate_chain_rejects_a_zero_max_drift_secs() {
    let (mut ctx, r) = rig(false).await;
    let chain_id = 200_105u64;
    let (root_pda, _) = sclient::root_pda(&r.settlement_program, chain_id);
    let mut root_acc = hand_built_root_account(chain_id, r.authority.pubkey());
    root_acc.owner = r.settlement_program;
    ctx.set_account(&root_pda, &root_acc.into());

    let migrate_ix = sclient::migrate_chain_ix(
        &r.settlement_program,
        &r.registry_authority.pubkey(),
        &r.payer.pubkey(),
        chain_id,
        0,
    );
    let err = send(&mut ctx, &[migrate_ix], &r.payer, &[&r.registry_authority])
        .await
        .expect_err("MigrateChainV2 with max_drift_secs == 0 must be rejected");
    assert_eq!(custom_error(&err), Some(SettleError::DriftBoundZero as u32));
}

// ---------------------------------------------------------------------------------------------
// SetDriftBound
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn set_drift_bound_updates_an_already_v2_chain_config() {
    let (mut ctx, r) = rig(false).await;
    let chain_id = 200_106u64;
    let (root_pda, _) = sclient::root_pda(&r.settlement_program, chain_id);
    let mut root_acc = hand_built_root_account(chain_id, r.authority.pubkey());
    root_acc.owner = r.settlement_program;
    ctx.set_account(&root_pda, &root_acc.into());
    let migrate_ix = sclient::migrate_chain_ix(
        &r.settlement_program,
        &r.registry_authority.pubkey(),
        &r.payer.pubkey(),
        chain_id,
        60,
    );
    send(&mut ctx, &[migrate_ix], &r.payer, &[&r.registry_authority])
        .await
        .expect("MigrateChainV2 should succeed");

    let set_ix = sclient::set_drift_bound_ix(
        &r.settlement_program,
        &r.registry_authority.pubkey(),
        chain_id,
        90,
    );
    send(&mut ctx, &[set_ix], &r.payer, &[&r.registry_authority])
        .await
        .expect("SetDriftBound should succeed");

    let (cc_pda, _) = sclient::chain_config_pda(&r.settlement_program, chain_id);
    let cc = sclient::decode_chain_config_account(
        &ctx.banks_client
            .get_account(cc_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap();
    assert_eq!(cc.max_drift_secs, Some(90));
}

#[tokio::test]
async fn set_drift_bound_rejects_a_non_registry_authority_signer() {
    let (mut ctx, r) = rig(false).await;
    let chain_id = 200_107u64;
    let (root_pda, _) = sclient::root_pda(&r.settlement_program, chain_id);
    let mut root_acc = hand_built_root_account(chain_id, r.authority.pubkey());
    root_acc.owner = r.settlement_program;
    ctx.set_account(&root_pda, &root_acc.into());
    let migrate_ix = sclient::migrate_chain_ix(
        &r.settlement_program,
        &r.registry_authority.pubkey(),
        &r.payer.pubkey(),
        chain_id,
        60,
    );
    send(&mut ctx, &[migrate_ix], &r.payer, &[&r.registry_authority])
        .await
        .expect("MigrateChainV2 should succeed");

    let decoy = funded_keypair();
    let set_ix = sclient::set_drift_bound_ix(&r.settlement_program, &decoy.pubkey(), chain_id, 90);
    let err = send(&mut ctx, &[set_ix], &r.payer, &[&decoy])
        .await
        .expect_err("SetDriftBound by a non-registry-authority signer must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::NotRegistryAuthority as u32)
    );
}

#[tokio::test]
async fn set_drift_bound_rejects_zero() {
    let (mut ctx, r) = rig(false).await;
    let chain_id = 200_108u64;
    let (root_pda, _) = sclient::root_pda(&r.settlement_program, chain_id);
    let mut root_acc = hand_built_root_account(chain_id, r.authority.pubkey());
    root_acc.owner = r.settlement_program;
    ctx.set_account(&root_pda, &root_acc.into());
    let migrate_ix = sclient::migrate_chain_ix(
        &r.settlement_program,
        &r.registry_authority.pubkey(),
        &r.payer.pubkey(),
        chain_id,
        60,
    );
    send(&mut ctx, &[migrate_ix], &r.payer, &[&r.registry_authority])
        .await
        .expect("MigrateChainV2 should succeed");

    let set_ix = sclient::set_drift_bound_ix(
        &r.settlement_program,
        &r.registry_authority.pubkey(),
        chain_id,
        0,
    );
    let err = send(&mut ctx, &[set_ix], &r.payer, &[&r.registry_authority])
        .await
        .expect_err("SetDriftBound with max_drift_secs == 0 must be rejected");
    assert_eq!(custom_error(&err), Some(SettleError::DriftBoundZero as u32));
}

// ---------------------------------------------------------------------------------------------
// InitChain max_drift_secs
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn init_chain_writes_max_drift_secs_into_chain_config_v2() {
    let (mut ctx, r) = rig(false).await;
    allow_reserved(&mut ctx, &r, 200_109).await;
    let ix = sclient::init_chain_reserved_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        &r.registry_authority.pubkey(),
        200_109,
        empty_init_chain_fields(
            keccak::hashv(&[b"genesis"]).to_bytes(),
            5,
            16,
            Pubkey::new_unique(),
        ),
    );
    send(
        &mut ctx,
        &[ix],
        &r.payer,
        &[&r.authority, &r.registry_authority],
    )
    .await
    .expect("InitChain should succeed");
    let (cc_pda, _) = sclient::chain_config_pda(&r.settlement_program, 200_109);
    let cc = sclient::decode_chain_config_account(
        &ctx.banks_client
            .get_account(cc_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap();
    assert_eq!(
        cc.max_drift_secs,
        Some(60),
        "empty_init_chain_fields sets 60"
    );
}

#[tokio::test]
async fn init_chain_rejects_a_zero_max_drift_secs() {
    let (mut ctx, r) = rig(false).await;
    allow_reserved(&mut ctx, &r, 200_110).await;
    let mut fields = empty_init_chain_fields(
        keccak::hashv(&[b"genesis"]).to_bytes(),
        5,
        16,
        Pubkey::new_unique(),
    );
    fields.max_drift_secs = 0;
    let ix = sclient::init_chain_reserved_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        &r.registry_authority.pubkey(),
        200_110,
        fields,
    );
    let err = send(
        &mut ctx,
        &[ix],
        &r.payer,
        &[&r.authority, &r.registry_authority],
    )
    .await
    .expect_err("InitChain with max_drift_secs == 0 must be rejected");
    assert_eq!(custom_error(&err), Some(SettleError::DriftBoundZero as u32));
}

#[tokio::test]
async fn migrate_chain_rejected_twice() {
    let (mut ctx, r) = rig(false).await;
    let chain_id = 200_102u64;
    let (root_pda, _) = sclient::root_pda(&r.settlement_program, chain_id);
    let mut root_acc = hand_built_root_account(chain_id, r.authority.pubkey());
    root_acc.owner = r.settlement_program;
    ctx.set_account(&root_pda, &root_acc.into());

    let migrate_ix = sclient::migrate_chain_ix(
        &r.settlement_program,
        &r.registry_authority.pubkey(),
        &r.payer.pubkey(),
        chain_id,
        60,
    );
    send(
        &mut ctx,
        std::slice::from_ref(&migrate_ix),
        &r.payer,
        &[&r.registry_authority],
    )
    .await
    .expect("first MigrateChainV2 should succeed");

    let err = send(&mut ctx, &[migrate_ix], &r.payer, &[&r.registry_authority])
        .await
        .expect_err("a second MigrateChainV2 for the same chain_id must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::ChainAlreadyMigrated as u32)
    );
}

#[tokio::test]
async fn migrate_chain_rejected_by_non_registry_authority() {
    let (mut ctx, r) = rig(false).await;
    let chain_id = 200_103u64;
    let (root_pda, _) = sclient::root_pda(&r.settlement_program, chain_id);
    let mut root_acc = hand_built_root_account(chain_id, r.authority.pubkey());
    root_acc.owner = r.settlement_program;
    ctx.set_account(&root_pda, &root_acc.into());

    let decoy = funded_keypair();
    let migrate_ix = sclient::migrate_chain_ix(
        &r.settlement_program,
        &decoy.pubkey(),
        &r.payer.pubkey(),
        chain_id,
        60,
    );
    let err = send(&mut ctx, &[migrate_ix], &r.payer, &[&decoy])
        .await
        .expect_err("MigrateChainV2 by a non-registry-authority signer must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::NotRegistryAuthority as u32)
    );
    let (cc_pda, _) = sclient::chain_config_pda(&r.settlement_program, chain_id);
    assert!(
        ctx.banks_client
            .get_account(cc_pda)
            .await
            .unwrap()
            .is_none(),
        "a rejected MigrateChainV2 must not create the chain_config pda"
    );
}

/// The "revoke-then-reserved-InitChain" scenario (named alongside MigrateChain/RevokeReservedId):
/// once a marker is revoked, the same id is exactly as unregistrable as one that was never allowed.
#[tokio::test]
async fn revoke_reserved_id_then_reserved_init_chain_rejected() {
    let (mut ctx, r) = rig(false).await;
    let chain_id = 70u64;
    allow_reserved(&mut ctx, &r, chain_id).await;

    let revoke_ix = sclient::revoke_reserved_id_ix(
        &r.settlement_program,
        &r.registry_authority.pubkey(),
        chain_id,
    );
    send(&mut ctx, &[revoke_ix], &r.payer, &[&r.registry_authority])
        .await
        .expect("RevokeReservedId should succeed");

    let init_ix = sclient::init_chain_reserved_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        &r.registry_authority.pubkey(),
        chain_id,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    let err = send(
        &mut ctx,
        &[init_ix],
        &r.payer,
        &[&r.authority, &r.registry_authority],
    )
    .await
    .expect_err("InitChain for a chain_id whose marker was revoked must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::ReservedIdNotAllowed as u32)
    );
}

#[tokio::test]
async fn init_global_config_rejected_twice() {
    let (pt, r) = rig_program_test();
    let mut ctx = pt.start_with_context().await;
    set_program_data_authority(
        &mut ctx,
        &r.settlement_program,
        &r.upgrade_authority.pubkey(),
    )
    .await;
    let fields = default_global_config_fields(r.registry_authority.pubkey(), r.treasury, false);
    let ix = sclient::init_global_config_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.upgrade_authority.pubkey(),
        fields.clone(),
    );
    send(
        &mut ctx,
        std::slice::from_ref(&ix),
        &r.payer,
        &[&r.upgrade_authority],
    )
    .await
    .expect("first InitGlobalConfig should succeed");

    let err = send(&mut ctx, &[ix], &r.payer, &[&r.upgrade_authority])
        .await
        .expect_err("a second InitGlobalConfig must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::GlobalConfigAlreadyInitialized as u32)
    );
}

#[tokio::test]
async fn refund_deposit_rejected_twice() {
    let (mut ctx, r) = rig(true).await;
    let chain_id = sclient::derive_permissionless_chain_id(&r.authority.pubkey(), 0);
    let ix = sclient::init_chain_permissionless_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        chain_id,
        0,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    send(&mut ctx, &[ix], &r.payer, &[&r.authority])
        .await
        .expect("permissionless InitChain should succeed");

    // 10 posted batches is the cheapest eligibility trigger to set up for this test.
    // A permissionless chain is proved-only, so the unproved PostRoot that used to bring it
    // here is refused. This test is about the refund trigger, not about posting: leave the chain's
    // counters exactly where 10 posted batches would have left them.
    set_chain_progress(&mut ctx, &r, chain_id, 10, 0, 10).await;

    let refund_ix =
        sclient::refund_deposit_ix(&r.settlement_program, chain_id, &r.authority.pubkey());
    send(&mut ctx, std::slice::from_ref(&refund_ix), &r.payer, &[])
        .await
        .expect("first RefundDeposit should succeed");

    let err = send(&mut ctx, &[refund_ix], &r.payer, &[])
        .await
        .expect_err("a second RefundDeposit must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::NoDepositToRefund as u32)
    );
}

/// 9 posted batches is one short of the 10-posted-batches trigger and there is no
/// final root either — refund must still be refused.
#[tokio::test]
async fn refund_rejected_at_nine_posted_batches() {
    let (mut ctx, r) = rig(true).await;
    let chain_id = sclient::derive_permissionless_chain_id(&r.authority.pubkey(), 0);
    let ix = sclient::init_chain_permissionless_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        chain_id,
        0,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    send(&mut ctx, &[ix], &r.payer, &[&r.authority])
        .await
        .expect("permissionless InitChain should succeed");

    // A permissionless chain is proved-only, so the unproved PostRoot that used to bring it
    // here is refused. This test is about the refund trigger, not about posting: leave the chain's
    // counters exactly where 9 posted batches would have left them.
    set_chain_progress(&mut ctx, &r, chain_id, 9, 0, 9).await;

    let refund_ix =
        sclient::refund_deposit_ix(&r.settlement_program, chain_id, &r.authority.pubkey());
    let err = send(&mut ctx, &[refund_ix], &r.payer, &[])
        .await
        .expect_err("RefundDeposit at 9 posted batches (one short) must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::RefundNotYetEligible as u32)
    );
}

#[tokio::test]
async fn refund_deposit_rejects_a_recipient_that_is_not_the_chain_authority() {
    let (mut ctx, r) = rig(true).await;
    let chain_id = sclient::derive_permissionless_chain_id(&r.authority.pubkey(), 0);
    let ix = sclient::init_chain_permissionless_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        chain_id,
        0,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    send(&mut ctx, &[ix], &r.payer, &[&r.authority])
        .await
        .expect("permissionless InitChain should succeed");

    // A permissionless chain is proved-only, so the unproved PostRoot that used to bring it
    // here is refused. This test is about the refund trigger, not about posting: leave the chain's
    // counters exactly where 10 posted batches would have left them.
    set_chain_progress(&mut ctx, &r, chain_id, 10, 0, 10).await;

    let (cc_pda, _) = sclient::chain_config_pda(&r.settlement_program, chain_id);
    let cc_before = sclient::decode_chain_config_account(
        &ctx.banks_client
            .get_account(cc_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap();
    assert!(!cc_before.deposit_refunded && cc_before.deposit_lamports > 0);

    let decoy = Pubkey::new_unique();
    let refund_ix = sclient::refund_deposit_ix(&r.settlement_program, chain_id, &decoy);
    let err = send(&mut ctx, &[refund_ix], &r.payer, &[])
        .await
        .expect_err("RefundDeposit to an account that is not root.authority must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::WrongChainAuthority as u32)
    );

    let cc_after = sclient::decode_chain_config_account(
        &ctx.banks_client
            .get_account(cc_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap();
    assert_eq!(
        cc_after, cc_before,
        "a rejected RefundDeposit must leave the deposit untouched"
    );
}

#[tokio::test]
async fn post_root_rejects_a_treasury_that_is_not_global_config_treasury() {
    let (mut ctx, r) = rig(false).await;
    let chain_id = 80u64;
    reserved_chain_ready_for_post_root(&mut ctx, &r, chain_id, 16).await;

    let acc = keccak::hashv(&[b"wrong treasury acc"]).to_bytes();
    let inbox_pda = sclient::inbox_batch_pda(&r.inbox_program, &r.settlement_program, chain_id, 1);
    ctx.set_account(
        &inbox_pda,
        &inbox_batch_account(
            r.inbox_program,
            chain_id,
            1,
            r.settlement_program,
            r.authority.pubkey(),
            acc,
        )
        .into(),
    );
    let wrong_treasury = Pubkey::new_unique();
    let args = post_root_args_for(&r, chain_id, 1, acc, 0);
    let post_ix = sclient::post_root_ix(
        &r.settlement_program,
        &r.authority.pubkey(),
        &r.inbox_program,
        &wrong_treasury,
        args,
    )
    .expect("reserved chain");
    // The REAL treasury's balance is the meaningful invariant here — `r.authority` (also this
    // transaction's fee payer) legitimately loses the ~5000-lamport tx fee even on a failed
    // instruction, so its balance is not a useful "nothing moved" signal.
    let real_treasury_before = lamports_of(&mut ctx, r.treasury).await;
    let err = send(&mut ctx, &[post_ix], &r.authority, &[])
        .await
        .expect_err(
            "PostRoot against a treasury that is not global_config.treasury must be rejected",
        );
    assert_eq!(custom_error(&err), Some(SettleError::WrongTreasury as u32));
    let real_treasury_after = lamports_of(&mut ctx, r.treasury).await;
    assert_eq!(
        real_treasury_before, real_treasury_after,
        "no fee may move on a rejected PostRoot"
    );
}

#[tokio::test]
async fn reclaim_chain_rejects_a_wrong_treasury() {
    let (mut ctx, r) = rig(true).await;
    let chain_id = sclient::derive_permissionless_chain_id(&r.authority.pubkey(), 0);
    let ix = sclient::init_chain_permissionless_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        chain_id,
        0,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    send(&mut ctx, &[ix], &r.payer, &[&r.authority])
        .await
        .expect("permissionless InitChain should succeed");

    ctx.warp_to_slot(ctx.banks_client.get_root_slot().await.unwrap() + RECLAIM_WINDOW_SLOTS + 10)
        .unwrap();
    let wrong_treasury = Pubkey::new_unique();
    let reclaim_ix = sclient::reclaim_chain_ix(&r.settlement_program, chain_id, &wrong_treasury);
    let err = send(&mut ctx, &[reclaim_ix], &r.payer, &[])
        .await
        .expect_err(
            "ReclaimChain against a treasury that is not global_config.treasury must be rejected",
        );
    assert_eq!(custom_error(&err), Some(SettleError::WrongTreasury as u32));

    let (root_pda, _) = sclient::root_pda(&r.settlement_program, chain_id);
    assert!(
        ctx.banks_client
            .get_account(root_pda)
            .await
            .unwrap()
            .is_some(),
        "a rejected ReclaimChain must not close any account"
    );
}

/// A root that HAS had a batch posted (even one still `Pending`) must never be
/// reclaimable, regardless of how far past the reclaim window the caller waits.
#[tokio::test]
async fn reclaim_chain_rejected_after_a_root_was_posted() {
    // PERMISSIONLESS (not reserved) deliberately — a reserved chain here would also be rejected by
    // the separate "reserved + live marker" guard, which would mask a mutation to THIS check
    // (the one this test exists to isolate) behind the same `ChainNotReclaimable` error.
    let (pt, r) = rig_program_test();
    let chain_id = sclient::derive_permissionless_chain_id(&r.authority.pubkey(), 0);
    let mut ctx = start_and_init_global_config(pt, &r, true).await;
    let init_ix = sclient::init_chain_permissionless_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        chain_id,
        0,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    send(&mut ctx, &[init_ix], &r.payer, &[&r.authority])
        .await
        .expect("permissionless InitChain should succeed");

    // A permissionless chain is proved-only, so the unproved PostRoot this test used to post
    // with is refused. What the test isolates is ReclaimChain's "a batch was ever posted" check, so leave
    // the root exactly as one posted, still-pending batch would have left it.
    set_chain_progress(&mut ctx, &r, chain_id, 1, 0, 1).await;

    ctx.warp_to_slot(ctx.banks_client.get_root_slot().await.unwrap() + RECLAIM_WINDOW_SLOTS + 10)
        .unwrap();
    let reclaim_ix = sclient::reclaim_chain_ix(&r.settlement_program, chain_id, &r.treasury);
    let err = send(&mut ctx, &[reclaim_ix], &r.payer, &[])
        .await
        .expect_err("ReclaimChain must be rejected once any batch has ever been posted");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::ChainNotReclaimable as u32)
    );
}

// ---------------------------------------------------------------------------------------------
// (3) protocol fee
// ---------------------------------------------------------------------------------------------

async fn reserved_chain_ready_for_post_root(
    ctx: &mut solana_program_test::ProgramTestContext,
    r: &Rig,
    chain_id: u64,
    max_pending: u32,
) {
    allow_reserved(ctx, r, chain_id).await;
    let ix = sclient::init_chain_reserved_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        &r.registry_authority.pubkey(),
        chain_id,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            max_pending,
            r.inbox_program,
        ),
    );
    send(ctx, &[ix], &r.payer, &[&r.authority, &r.registry_authority])
        .await
        .expect("reserved InitChain should succeed");
}

fn post_root_args_for(
    r: &Rig,
    chain_id: u64,
    batch: u64,
    acc: [u8; 32],
    gas_in_batch: u64,
) -> sclient::PostRootFields {
    sclient::PostRootFields {
        chain_id,
        batch,
        prev_batch: batch - 1,
        pre_state_root: r.genesis_state_root,
        first_block: batch,
        last_block: batch,
        state_root: keccak::hashv(&[b"fee test state root", &batch.to_le_bytes()]).to_bytes(),
        block_roots_merkle: [0u8; 32],
        inbox_commitment: acc,
        forced_outcome_commitment: rome_zk_layouts::forced_empty_root(
            &(|p: &[&[u8]]| keccak::hashv(p).to_bytes()),
        ),
        parent_hash: [0u8; 32],
        last_block_hash: [0u8; 32],
        gas_in_batch,
    }
}

#[tokio::test]
async fn post_root_charges_the_base_fee_and_ignores_bps() {
    let (mut ctx, r) = rig(false).await;
    let chain_id = 5u64;
    reserved_chain_ready_for_post_root(&mut ctx, &r, chain_id, 16).await;

    // The unproved `PostRoot` path has no proof-bound value to check `gas_in_batch` against, so it
    // charges the base fee only — the bps term below (200 bps against a declared 1_000_000 gas, which
    // would be +20_000 if it were honored) must NOT show up in what the treasury actually receives.
    let set_fee_ix = sclient::set_fee_ix(
        &r.settlement_program,
        &r.registry_authority.pubkey(),
        chain_id,
        750_000,
        200,
    );
    send(&mut ctx, &[set_fee_ix], &r.payer, &[&r.registry_authority])
        .await
        .expect("SetFee should succeed");

    let acc = keccak::hashv(&[b"fee test acc"]).to_bytes();
    let inbox_pda = sclient::inbox_batch_pda(&r.inbox_program, &r.settlement_program, chain_id, 1);
    ctx.set_account(
        &inbox_pda,
        &inbox_batch_account(
            r.inbox_program,
            chain_id,
            1,
            r.settlement_program,
            r.authority.pubkey(),
            acc,
        )
        .into(),
    );
    let args = post_root_args_for(&r, chain_id, 1, acc, 1_000_000);
    let treasury_before = lamports_of(&mut ctx, r.treasury).await;
    let authority_before = lamports_of(&mut ctx, r.authority.pubkey()).await;
    let post_ix = sclient::post_root_ix(
        &r.settlement_program,
        &r.authority.pubkey(),
        &r.inbox_program,
        &r.treasury,
        args,
    )
    .expect("reserved chain");
    send(&mut ctx, &[post_ix], &r.authority, &[])
        .await
        .expect("PostRoot should succeed");
    let treasury_after = lamports_of(&mut ctx, r.treasury).await;
    let authority_after = lamports_of(&mut ctx, r.authority.pubkey()).await;

    assert_eq!(
        treasury_after - treasury_before,
        750_000,
        "PostRoot must charge the base fee only (750_000) — bps must never apply on the unproved path"
    );
    assert!(
        authority_before - authority_after >= 750_000,
        "the fee must be debited from the poster"
    );

    let (cc_pda, _) = sclient::chain_config_pda(&r.settlement_program, chain_id);
    let cc = sclient::decode_chain_config_account(
        &ctx.banks_client
            .get_account(cc_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap();
    assert_eq!(cc.posted_batches, 1);
}

#[tokio::test]
async fn post_root_fails_when_the_poster_cannot_cover_the_fee() {
    let (mut ctx, r) = rig(false).await;
    let chain_id = 6u64;
    reserved_chain_ready_for_post_root(&mut ctx, &r, chain_id, 16).await;

    // Enough to sign and pay the tx fee, but nowhere near the pending PDA's rent-exempt minimum
    // (~2.8M for PENDING_LEN) plus the 1_000_000 base fee.
    let poor_authority = funded_keypair();
    ctx.set_account(
        &poor_authority.pubkey(),
        &funded_account_with(2_000_000).into(),
    );
    // Re-register the chain under the poor authority so PostRoot's signer check passes; simplest is a
    // second reserved chain owned by poor_authority.
    let chain_id2 = 7u64;
    let ix = sclient::init_chain_reserved_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &poor_authority.pubkey(),
        &r.registry_authority.pubkey(),
        chain_id2,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    allow_reserved(&mut ctx, &r, chain_id2).await;
    send(
        &mut ctx,
        &[ix],
        &r.payer,
        &[&poor_authority, &r.registry_authority],
    )
    .await
    .expect("reserved InitChain should succeed");

    let acc = keccak::hashv(&[b"poor poster acc"]).to_bytes();
    let inbox_pda = sclient::inbox_batch_pda(&r.inbox_program, &r.settlement_program, chain_id2, 1);
    ctx.set_account(
        &inbox_pda,
        &inbox_batch_account(
            r.inbox_program,
            chain_id2,
            1,
            r.settlement_program,
            poor_authority.pubkey(),
            acc,
        )
        .into(),
    );
    let args = post_root_args_for(&r, chain_id2, 1, acc, 0);
    let post_ix = sclient::post_root_ix(
        &r.settlement_program,
        &poor_authority.pubkey(),
        &r.inbox_program,
        &r.treasury,
        args,
    )
    .expect("reserved chain");
    send(&mut ctx, &[post_ix], &poor_authority, &[])
        .await
        .expect_err("PostRoot must fail when the poster cannot cover the protocol fee");
}

#[tokio::test]
async fn set_fee_and_set_treasury_reject_a_non_authority_signer() {
    let (mut ctx, r) = rig(false).await;
    let chain_id = 8u64;
    reserved_chain_ready_for_post_root(&mut ctx, &r, chain_id, 16).await;

    let decoy = funded_keypair();
    let set_fee_ix = sclient::set_fee_ix(&r.settlement_program, &decoy.pubkey(), chain_id, 1, 1);
    let err = send(&mut ctx, &[set_fee_ix], &r.payer, &[&decoy])
        .await
        .expect_err("SetFee by a non-registry-authority signer must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::NotRegistryAuthority as u32)
    );

    let set_treasury_ix =
        sclient::set_treasury_ix(&r.settlement_program, &decoy.pubkey(), Pubkey::new_unique());
    let err = send(&mut ctx, &[set_treasury_ix], &r.payer, &[&decoy])
        .await
        .expect_err("SetTreasury by a non-registry-authority signer must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::NotRegistryAuthority as u32)
    );
}

// ---------------------------------------------------------------------------------------------
// (3b) treasury rent-exemption, global-config mutability, reserved-only allowlisting
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn init_global_config_rejects_a_treasury_with_zero_lamports() {
    let (pt, r) = rig_program_test();
    let mut ctx = pt.start_with_context().await;
    set_program_data_authority(
        &mut ctx,
        &r.settlement_program,
        &r.upgrade_authority.pubkey(),
    )
    .await;

    let unfunded_treasury = Pubkey::new_unique();
    let ix = sclient::init_global_config_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.upgrade_authority.pubkey(),
        default_global_config_fields(r.registry_authority.pubkey(), unfunded_treasury, false),
    );
    let err = send(&mut ctx, &[ix], &r.payer, &[&r.upgrade_authority])
        .await
        .expect_err("a 0-lamport treasury must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::TreasuryNotRentExempt as u32)
    );
    let (global_pda, _) = sclient::global_config_pda(&r.settlement_program);
    assert!(
        ctx.banks_client
            .get_account(global_pda)
            .await
            .unwrap()
            .is_none(),
        "a rejected InitGlobalConfig must not create the global_config pda"
    );
}

#[tokio::test]
async fn set_treasury_rejects_a_treasury_with_zero_lamports() {
    let (mut ctx, r) = rig(false).await;
    let unfunded_treasury = Pubkey::new_unique();
    let set_treasury_ix = sclient::set_treasury_ix(
        &r.settlement_program,
        &r.registry_authority.pubkey(),
        unfunded_treasury,
    );
    let err = send(
        &mut ctx,
        &[set_treasury_ix],
        &r.payer,
        &[&r.registry_authority],
    )
    .await
    .expect_err("a 0-lamport treasury must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::TreasuryNotRentExempt as u32)
    );
}

#[tokio::test]
async fn set_global_config_rejects_a_non_registry_authority_signer() {
    let (mut ctx, r) = rig(false).await;
    let decoy = funded_keypair();
    let set_ix = sclient::set_global_config_ix(
        &r.settlement_program,
        &decoy.pubkey(),
        sclient::GlobalConfigUpdate {
            permissionless_init_enabled: true,
            reclaim_window_slots: RECLAIM_WINDOW_SLOTS,
            deposit_lamports: 2_000_000_000,
            default_fee_base_lamports: 1_000_000,
            default_fee_bps: 0,
        },
    );
    let err = send(&mut ctx, &[set_ix], &r.payer, &[&decoy])
        .await
        .expect_err("SetGlobalConfig by a non-registry-authority signer must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::NotRegistryAuthority as u32)
    );
}

/// A `reclaim_window_slots` below the 1-day-of-slots floor would make a never-posted
/// permissionless chain reclaimable in (or near) its own registration slot.
#[tokio::test]
async fn set_global_config_rejects_reclaim_window_below_the_floor() {
    let (mut ctx, r) = rig(false).await;
    let set_ix = sclient::set_global_config_ix(
        &r.settlement_program,
        &r.registry_authority.pubkey(),
        sclient::GlobalConfigUpdate {
            permissionless_init_enabled: false,
            reclaim_window_slots: RECLAIM_WINDOW_SLOTS - 1,
            deposit_lamports: 2_000_000_000,
            default_fee_base_lamports: 1_000_000,
            default_fee_bps: 0,
        },
    );
    let err = send(&mut ctx, &[set_ix], &r.payer, &[&r.registry_authority])
        .await
        .expect_err("SetGlobalConfig must reject a reclaim_window_slots below the floor");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::ReclaimWindowTooShort as u32)
    );
}

/// `deposit_lamports == 0` while `permissionless_init_enabled` is true would make
/// permissionless chain ids free to mint while the gate that mints them is on.
#[tokio::test]
async fn set_global_config_rejects_zero_deposit_while_permissionless_enabled() {
    let (mut ctx, r) = rig(false).await;
    let set_ix = sclient::set_global_config_ix(
        &r.settlement_program,
        &r.registry_authority.pubkey(),
        sclient::GlobalConfigUpdate {
            permissionless_init_enabled: true,
            reclaim_window_slots: RECLAIM_WINDOW_SLOTS,
            deposit_lamports: 0,
            default_fee_base_lamports: 1_000_000,
            default_fee_bps: 0,
        },
    );
    let err = send(&mut ctx, &[set_ix], &r.payer, &[&r.registry_authority])
        .await
        .expect_err(
            "SetGlobalConfig must reject deposit_lamports == 0 while permissionless_init_enabled",
        );
    assert_eq!(
        custom_error(&err),
        Some(SettleError::DepositRequiredForPermissionless as u32)
    );
}

/// The same floor applies at `InitGlobalConfig` — there is no other instruction that
/// ever writes `reclaim_window_slots` for the first time.
#[tokio::test]
async fn init_global_config_rejects_reclaim_window_below_the_floor() {
    let (pt, r) = rig_program_test();
    let mut ctx = pt.start_with_context().await;
    set_program_data_authority(
        &mut ctx,
        &r.settlement_program,
        &r.upgrade_authority.pubkey(),
    )
    .await;
    let mut fields = default_global_config_fields(r.registry_authority.pubkey(), r.treasury, false);
    fields.reclaim_window_slots = RECLAIM_WINDOW_SLOTS - 1;
    let ix = sclient::init_global_config_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.upgrade_authority.pubkey(),
        fields,
    );
    let err = send(&mut ctx, &[ix], &r.payer, &[&r.upgrade_authority])
        .await
        .expect_err("InitGlobalConfig must reject a reclaim_window_slots below the floor");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::ReclaimWindowTooShort as u32)
    );
    let (global_pda, _) = sclient::global_config_pda(&r.settlement_program);
    assert!(
        ctx.banks_client
            .get_account(global_pda)
            .await
            .unwrap()
            .is_none(),
        "a rejected InitGlobalConfig must not create the global_config pda"
    );
}

/// `permissionless_init_enabled` is forced `false` at `InitGlobalConfig` regardless of what
/// is passed — confirmed directly here (not just inferred from the rig's own flip-after-init dance).
#[tokio::test]
async fn init_global_config_forces_permissionless_init_enabled_false_even_if_true_was_requested() {
    let (pt, r) = rig_program_test();
    let mut ctx = pt.start_with_context().await;
    set_program_data_authority(
        &mut ctx,
        &r.settlement_program,
        &r.upgrade_authority.pubkey(),
    )
    .await;

    let ix = sclient::init_global_config_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.upgrade_authority.pubkey(),
        default_global_config_fields(r.registry_authority.pubkey(), r.treasury, true),
    );
    send(&mut ctx, &[ix], &r.payer, &[&r.upgrade_authority])
        .await
        .expect("InitGlobalConfig should succeed");

    let (global_pda, _) = sclient::global_config_pda(&r.settlement_program);
    let cfg = sclient::decode_global_config_account(
        &ctx.banks_client
            .get_account(global_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap();
    assert!(
        !cfg.permissionless_init_enabled,
        "InitGlobalConfig must force permissionless_init_enabled false regardless of the caller's args"
    );
}

/// The flip must actually change program behavior, not just the stored bit — a permissionless
/// `InitChain` that would have failed before the flip must succeed after it.
#[tokio::test]
async fn set_global_config_flip_takes_effect_on_the_next_init_chain() {
    let (mut ctx, r) = rig(false).await;
    let chain_id = sclient::derive_permissionless_chain_id(&r.authority.pubkey(), 0);
    let ix_before = sclient::init_chain_permissionless_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        chain_id,
        0,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    let err = send(&mut ctx, &[ix_before], &r.payer, &[&r.authority])
        .await
        .expect_err("permissionless InitChain must still be rejected before the flip");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::PermissionlessInitDisabled as u32)
    );

    let set_ix = sclient::set_global_config_ix(
        &r.settlement_program,
        &r.registry_authority.pubkey(),
        sclient::GlobalConfigUpdate {
            permissionless_init_enabled: true,
            reclaim_window_slots: RECLAIM_WINDOW_SLOTS,
            deposit_lamports: 2_000_000_000,
            default_fee_base_lamports: 1_000_000,
            default_fee_bps: 0,
        },
    );
    send(&mut ctx, &[set_ix], &r.payer, &[&r.registry_authority])
        .await
        .expect("SetGlobalConfig should succeed");

    let ix_after = sclient::init_chain_permissionless_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        chain_id,
        0,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    send(&mut ctx, &[ix_after], &r.payer, &[&r.authority])
        .await
        .expect("permissionless InitChain must succeed once SetGlobalConfig flips the gate on");
}

// ---------------------------------------------------------------------------------------------
// Two-step registry-authority rotation (`ProposeRegistryAuthority` +
// `AcceptRegistryAuthority`, replacing the disabled single-step `SetRegistryAuthority`).
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn propose_registry_authority_rejects_a_non_registry_authority_signer() {
    let (mut ctx, r) = rig(false).await;
    let decoy = funded_keypair();
    let propose_ix = sclient::propose_registry_authority_ix(
        &r.settlement_program,
        &decoy.pubkey(),
        Pubkey::new_unique(),
    );
    let err = send(&mut ctx, &[propose_ix], &r.payer, &[&decoy])
        .await
        .expect_err("ProposeRegistryAuthority by a non-registry-authority signer must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::NotRegistryAuthority as u32)
    );
}

#[tokio::test]
async fn propose_registry_authority_rejects_the_default_pubkey() {
    let (mut ctx, r) = rig(false).await;
    let propose_ix = sclient::propose_registry_authority_ix(
        &r.settlement_program,
        &r.registry_authority.pubkey(),
        Pubkey::default(),
    );
    let err = send(&mut ctx, &[propose_ix], &r.payer, &[&r.registry_authority])
        .await
        .expect_err("ProposeRegistryAuthority must reject Pubkey::default() as `new`");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::InvalidRegistryAuthority as u32)
    );
}

#[tokio::test]
async fn accept_registry_authority_rejects_a_third_key() {
    let (mut ctx, r) = rig(false).await;
    let new_authority = funded_keypair();
    let propose_ix = sclient::propose_registry_authority_ix(
        &r.settlement_program,
        &r.registry_authority.pubkey(),
        new_authority.pubkey(),
    );
    send(&mut ctx, &[propose_ix], &r.payer, &[&r.registry_authority])
        .await
        .expect("ProposeRegistryAuthority should succeed");

    let third = funded_keypair();
    let accept_ix = sclient::accept_registry_authority_ix(&r.settlement_program, &third.pubkey());
    let err = send(&mut ctx, &[accept_ix], &r.payer, &[&third])
        .await
        .expect_err(
            "AcceptRegistryAuthority by a key other than the proposed one must be rejected",
        );
    assert_eq!(
        custom_error(&err),
        Some(SettleError::NotPendingRegistryAuthority as u32)
    );
}

#[tokio::test]
async fn old_registry_authority_still_works_between_propose_and_accept() {
    let (mut ctx, r) = rig(false).await;
    let new_authority = funded_keypair();
    let propose_ix = sclient::propose_registry_authority_ix(
        &r.settlement_program,
        &r.registry_authority.pubkey(),
        new_authority.pubkey(),
    );
    send(&mut ctx, &[propose_ix], &r.payer, &[&r.registry_authority])
        .await
        .expect("ProposeRegistryAuthority should succeed");

    // A proposal alone must not de-authorize the OLD authority — only `AcceptRegistryAuthority` does.
    let chain_id = 41u64;
    let allow_ix = sclient::allow_reserved_id_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.registry_authority.pubkey(),
        chain_id,
    );
    send(&mut ctx, &[allow_ix], &r.payer, &[&r.registry_authority])
        .await
        .expect("the OLD registry authority must still work before the proposal is accepted");
}

#[tokio::test]
async fn accept_registry_authority_rotates_and_the_old_authority_is_then_rejected() {
    let (mut ctx, r) = rig(false).await;
    let new_authority = funded_keypair();
    let propose_ix = sclient::propose_registry_authority_ix(
        &r.settlement_program,
        &r.registry_authority.pubkey(),
        new_authority.pubkey(),
    );
    send(&mut ctx, &[propose_ix], &r.payer, &[&r.registry_authority])
        .await
        .expect("ProposeRegistryAuthority should succeed");

    let accept_ix =
        sclient::accept_registry_authority_ix(&r.settlement_program, &new_authority.pubkey());
    send(&mut ctx, &[accept_ix], &r.payer, &[&new_authority])
        .await
        .expect("AcceptRegistryAuthority should succeed");

    // The OLD authority must now be rejected...
    let chain_id = 42u64;
    let allow_ix_old = sclient::allow_reserved_id_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.registry_authority.pubkey(),
        chain_id,
    );
    let err = send(
        &mut ctx,
        &[allow_ix_old],
        &r.payer,
        &[&r.registry_authority],
    )
    .await
    .expect_err("the OLD registry authority must be rejected after rotation");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::NotRegistryAuthority as u32)
    );

    // ...and the NEW one must work.
    let allow_ix_new = sclient::allow_reserved_id_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &new_authority.pubkey(),
        chain_id,
    );
    send(&mut ctx, &[allow_ix_new], &r.payer, &[&new_authority])
        .await
        .expect("the NEW registry authority must be accepted after rotation");
}

#[tokio::test]
async fn allow_reserved_id_rejects_a_permissionless_chain_id() {
    let (mut ctx, r) = rig(false).await;
    let permissionless_id = rome_zk_layouts::chainid::PERMISSIONLESS_BASE + 7; // >= 2^32, not reserved
    let ix = sclient::allow_reserved_id_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.registry_authority.pubkey(),
        permissionless_id,
    );
    let err = send(&mut ctx, &[ix], &r.payer, &[&r.registry_authority])
        .await
        .expect_err("AllowReservedId must reject a chain_id that is not reserved");
    assert!(
        matches!(
            err,
            TransactionError::InstructionError(_, InstructionError::InvalidArgument)
        ),
        "expected InvalidArgument, got {err:?}"
    );
}

#[tokio::test]
async fn a_fee_change_applies_to_the_next_post_not_a_retroactive_one() {
    let (mut ctx, r) = rig(false).await;
    let chain_id = 9u64;
    reserved_chain_ready_for_post_root(&mut ctx, &r, chain_id, 16).await;

    // Batch 1 at the default fee (base 1_000_000, bps 0).
    let acc1 = keccak::hashv(&[b"fee change acc 1"]).to_bytes();
    let inbox1 = sclient::inbox_batch_pda(&r.inbox_program, &r.settlement_program, chain_id, 1);
    ctx.set_account(
        &inbox1,
        &inbox_batch_account(
            r.inbox_program,
            chain_id,
            1,
            r.settlement_program,
            r.authority.pubkey(),
            acc1,
        )
        .into(),
    );
    let treasury_before_1 = lamports_of(&mut ctx, r.treasury).await;
    let args1 = post_root_args_for(&r, chain_id, 1, acc1, 0);
    let ix1 = sclient::post_root_ix(
        &r.settlement_program,
        &r.authority.pubkey(),
        &r.inbox_program,
        &r.treasury,
        args1,
    )
    .expect("reserved chain");
    send(&mut ctx, &[ix1], &r.authority, &[])
        .await
        .expect("PostRoot batch 1 should succeed");
    let treasury_after_1 = lamports_of(&mut ctx, r.treasury).await;
    assert_eq!(treasury_after_1 - treasury_before_1, 1_000_000);

    // Raise the fee, then post batch 2 — must use the NEW fee, not the one batch 1 paid.
    let set_fee_ix = sclient::set_fee_ix(
        &r.settlement_program,
        &r.registry_authority.pubkey(),
        chain_id,
        3_000_000,
        0,
    );
    send(&mut ctx, &[set_fee_ix], &r.payer, &[&r.registry_authority])
        .await
        .expect("SetFee should succeed");

    let acc2 = keccak::hashv(&[b"fee change acc 2"]).to_bytes();
    let inbox2 = sclient::inbox_batch_pda(&r.inbox_program, &r.settlement_program, chain_id, 2);
    ctx.set_account(
        &inbox2,
        &inbox_batch_account(
            r.inbox_program,
            chain_id,
            2,
            r.settlement_program,
            r.authority.pubkey(),
            acc2,
        )
        .into(),
    );
    let mut args2 = post_root_args_for(&r, chain_id, 2, acc2, 0);
    args2.pre_state_root = keccak::hashv(&[b"fee test state root", &1u64.to_le_bytes()]).to_bytes();
    let treasury_before_2 = lamports_of(&mut ctx, r.treasury).await;
    let ix2 = sclient::post_root_ix(
        &r.settlement_program,
        &r.authority.pubkey(),
        &r.inbox_program,
        &r.treasury,
        args2,
    )
    .expect("reserved chain");
    send(&mut ctx, &[ix2], &r.authority, &[])
        .await
        .expect("PostRoot batch 2 should succeed");
    let treasury_after_2 = lamports_of(&mut ctx, r.treasury).await;
    assert_eq!(
        treasury_after_2 - treasury_before_2,
        3_000_000,
        "batch 2 must pay the fee in force at post time, not the one batch 1 paid"
    );
}

/// `PostRootProved`'s bps fee component binds to the proved header's own `gasUsed` — a poster-claimed
/// `gas_in_batch` that disagrees must be rejected BEFORE the expensive pairing (`veritas::verify_zisk`,
/// ~442k CU), same cheapest-first discipline as the registry/vkey checks above it. This is provable without
/// a real ZisK proof (the mismatch check runs before the proof is ever verified) — a synthetic RLP header +
/// a garbage proof blob sized/keyed correctly enough to clear the length and vkey-lookup checks is exactly
/// what reaches it. The complementary "matching gas charges base + bps" success-path assertion needs a REAL
/// proof over a REAL header: no fixture here pairs a real layout-2 proof with its header bytes (the
/// `fixtures/s10/...` proofs commit only the header hash), so it is not implemented here.
#[tokio::test]
async fn post_root_proved_rejects_a_gas_in_batch_that_disagrees_with_the_proved_headers_gas_used() {
    use alloy_consensus::Header;
    use alloy_primitives::{B256, U256};

    let (mut ctx, r) = rig(false).await;
    let chain_id = 60u64;
    allow_reserved(&mut ctx, &r, chain_id).await;

    // An arbitrary "registered" vkey — never cryptographically checked, since the gas mismatch rejects
    // before `verify_zisk` runs.
    let vk = [0x37u8; 32];
    let mut fields = empty_init_chain_fields(
        r.genesis_state_root,
        r.challenge_window_slots,
        16,
        r.inbox_program,
    );
    fields.registry_entries = vec![sclient::RegistryEntry {
        curve: rome_zk_layouts::registry::CURVE_BN254,
        scheme: rome_zk_layouts::registry::SCHEME_PLONK,
        vkey_hash: vk,
        layout_id: rome_zk_layouts::registry::LAYOUT_HEADER_FALLBACK,
    }];
    let init_ix = sclient::init_chain_reserved_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        &r.registry_authority.pubkey(),
        chain_id,
        fields,
    );
    send(
        &mut ctx,
        &[init_ix],
        &r.payer,
        &[&r.authority, &r.registry_authority],
    )
    .await
    .expect("reserved InitChain should succeed");

    let acc = keccak::hashv(&[b"gas mismatch test acc"]).to_bytes();
    let inbox_pda = sclient::inbox_batch_pda(&r.inbox_program, &r.settlement_program, chain_id, 1);
    ctx.set_account(
        &inbox_pda,
        &inbox_batch_account(
            r.inbox_program,
            chain_id,
            1,
            r.settlement_program,
            r.authority.pubkey(),
            acc,
        )
        .into(),
    );

    // A synthetic single-block header (like header.rs's own unit test) with a real, decodable
    // `gasUsed` (the RLP field at index 10, zero-based) — deliberately different from what the poster will
    // claim below.
    let header = Header {
        number: 1,
        parent_hash: B256::ZERO,
        state_root: B256::repeat_byte(0x55),
        gas_limit: 30_000_000,
        gas_used: 500_000,
        timestamp: 1_700_000_000,
        base_fee_per_gas: Some(7),
        difficulty: U256::ZERO,
        ..Default::default()
    };
    let header_rlp = alloy_rlp::encode(&header);
    let header_hash = solana_program::keccak::hash(&header_rlp).to_bytes();

    let mut proof_abi = vec![0u8; 768 + 32 + 32 + 512];
    proof_abi[768..800].copy_from_slice(&vk);

    let args = sclient::PostRootFields {
        chain_id,
        batch: 1,
        prev_batch: 0,
        pre_state_root: r.genesis_state_root,
        first_block: 1,
        last_block: 1,
        state_root: header.state_root.0,
        block_roots_merkle: [0u8; 32],
        inbox_commitment: acc,
        forced_outcome_commitment: rome_zk_layouts::forced_empty_root(
            &(|p: &[&[u8]]| keccak::hashv(p).to_bytes()),
        ),
        parent_hash: header.parent_hash.0,
        last_block_hash: header_hash,
        gas_in_batch: header.gas_used + 1, // disagrees with the header's real gasUsed
    };
    let ix = sclient::post_root_proved_ix(
        &r.settlement_program,
        &r.authority.pubkey(),
        &r.inbox_program,
        &r.treasury,
        args,
        proof_abi,
        header_rlp,
    );
    let err = send(&mut ctx, &[ix], &r.authority, &[]).await.expect_err(
        "a gas_in_batch that disagrees with the proved header's gasUsed must be rejected",
    );
    assert_eq!(
        custom_error(&err),
        Some(SettleError::GasInBatchMismatch as u32)
    );
}

// ---------------------------------------------------------------------------------------------
// (4) CU measurements — one instruction per test, real BPF, CU read
// straight off `process_transaction_with_metadata`'s own metadata (never replayed/estimated).
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn cu_init_chain_reserved() {
    let (mut ctx, r) = rig(false).await;
    let chain_id = 100u64;
    allow_reserved(&mut ctx, &r, chain_id).await;
    let ix = sclient::init_chain_reserved_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        &r.registry_authority.pubkey(),
        chain_id,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    let cu = send(
        &mut ctx,
        &[ix],
        &r.payer,
        &[&r.authority, &r.registry_authority],
    )
    .await
    .expect("reserved InitChain should succeed");
    eprintln!("CU InitChain (reserved): {cu}");
}

#[tokio::test]
async fn cu_init_chain_permissionless_first_call_creates_nonce_pda() {
    let (mut ctx, r) = rig(true).await;
    let chain_id = sclient::derive_permissionless_chain_id(&r.authority.pubkey(), 0);
    let ix = sclient::init_chain_permissionless_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        chain_id,
        0,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    let cu = send(&mut ctx, &[ix], &r.payer, &[&r.authority])
        .await
        .expect("permissionless InitChain should succeed");
    eprintln!("CU InitChain (permissionless, nonce PDA created this call): {cu}");
}

#[tokio::test]
async fn cu_init_chain_permissionless_second_call_nonce_pda_already_exists() {
    let (mut ctx, r) = rig(true).await;
    let id0 = sclient::derive_permissionless_chain_id(&r.authority.pubkey(), 0);
    let ix0 = sclient::init_chain_permissionless_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        id0,
        0,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    send(&mut ctx, &[ix0], &r.payer, &[&r.authority])
        .await
        .expect("first permissionless InitChain should succeed");

    let id1 = sclient::derive_permissionless_chain_id(&r.authority.pubkey(), 1);
    let ix1 = sclient::init_chain_permissionless_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        id1,
        1,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    let cu = send(&mut ctx, &[ix1], &r.payer, &[&r.authority])
        .await
        .expect("second permissionless InitChain should succeed");
    eprintln!("CU InitChain (permissionless, nonce PDA already existed): {cu}");
}

#[tokio::test]
async fn cu_post_root_with_fee() {
    let (mut ctx, r) = rig(false).await;
    let chain_id = 101u64;
    reserved_chain_ready_for_post_root(&mut ctx, &r, chain_id, 16).await;
    let acc = keccak::hashv(&[b"cu measurement acc"]).to_bytes();
    let inbox_pda = sclient::inbox_batch_pda(&r.inbox_program, &r.settlement_program, chain_id, 1);
    ctx.set_account(
        &inbox_pda,
        &inbox_batch_account(
            r.inbox_program,
            chain_id,
            1,
            r.settlement_program,
            r.authority.pubkey(),
            acc,
        )
        .into(),
    );
    let args = post_root_args_for(&r, chain_id, 1, acc, 500_000);
    let post_ix = sclient::post_root_ix(
        &r.settlement_program,
        &r.authority.pubkey(),
        &r.inbox_program,
        &r.treasury,
        args,
    )
    .expect("reserved chain");
    let cu = send(&mut ctx, &[post_ix], &r.authority, &[])
        .await
        .expect("PostRoot should succeed");
    eprintln!("CU PostRoot (with protocol fee charge): {cu}");
}

#[tokio::test]
async fn cu_refund_deposit() {
    let (mut ctx, r) = rig(true).await;
    let chain_id = sclient::derive_permissionless_chain_id(&r.authority.pubkey(), 0);
    let ix = sclient::init_chain_permissionless_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        chain_id,
        0,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    send(&mut ctx, &[ix], &r.payer, &[&r.authority])
        .await
        .expect("permissionless InitChain should succeed");
    // Trigger via 10 posted batches (cheaper to set up than a real finality wait for a CU-only
    // measurement) — RefundDeposit's own cost does not depend on which trigger fired.
    // A permissionless chain is proved-only, so the unproved PostRoot that used to bring it
    // here is refused. This test is about the refund trigger, not about posting: leave the chain's
    // counters exactly where 10 posted batches would have left them.
    set_chain_progress(&mut ctx, &r, chain_id, 10, 0, 10).await;
    let refund_ix =
        sclient::refund_deposit_ix(&r.settlement_program, chain_id, &r.authority.pubkey());
    let cu = send(&mut ctx, &[refund_ix], &r.payer, &[])
        .await
        .expect("RefundDeposit should succeed");
    eprintln!("CU RefundDeposit: {cu}");
}

/// `ReclaimChain` never has a pending PDA to sweep in this design — it requires
/// `head_pending_batch == 0` (no root ever posted), and that is exactly the condition under which no
/// pending PDA has ever been created (see `chain_config.rs`'s module doc). This measures the actual,
/// only-reachable shape: root + registry + chain_config closed, no pending accounts involved.
#[tokio::test]
async fn cu_reclaim_chain_no_pending_pdas_possible() {
    let (mut ctx, r) = rig(true).await;
    let chain_id = sclient::derive_permissionless_chain_id(&r.authority.pubkey(), 0);
    let ix = sclient::init_chain_permissionless_ix(
        &r.settlement_program,
        &r.payer.pubkey(),
        &r.authority.pubkey(),
        chain_id,
        0,
        empty_init_chain_fields(
            r.genesis_state_root,
            r.challenge_window_slots,
            16,
            r.inbox_program,
        ),
    );
    send(&mut ctx, &[ix], &r.payer, &[&r.authority])
        .await
        .expect("permissionless InitChain should succeed");
    ctx.warp_to_slot(ctx.banks_client.get_root_slot().await.unwrap() + RECLAIM_WINDOW_SLOTS + 10)
        .unwrap();
    let reclaim_ix = sclient::reclaim_chain_ix(&r.settlement_program, chain_id, &r.treasury);
    let cu = send(&mut ctx, &[reclaim_ix], &r.payer, &[])
        .await
        .expect("ReclaimChain should succeed");
    eprintln!("CU ReclaimChain (root+registry+chain_config closed, 0 pending PDAs — the only reachable shape): {cu}");
}
