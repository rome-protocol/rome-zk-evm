//! solana-program-test integration tests for the set-once bridge rule in `ProposeExitConfig` and
//! `ActivateExitConfig`, plus the `PostRoot` checks that must not move with the batch header's
//! version. They load the real `cargo build-sbf` `.so`, so the CU figures printed are real BPF
//! numbers.
//!
//! The rule: once a chain's `exit_config.bridge_program` is non-zero, no proposal may name a bridge
//! (refused by name, the same bridge included), and an older pending proposal that still carries a
//! bridge part has that part dropped on activation, the portal, cap and bond parts applied, and the
//! pending slot cleared. Activation never refuses over it: a pending proposal cannot be cancelled,
//! so a refusal would freeze the chain's exit config for good.

use rome_zk_layouts::batch::{self, BatchDeposit, BatchFields};
use rome_zk_layouts::exit::exit_config;
use rome_zk_layouts::registry as reg_layout;
use rome_zk_testkit::{funded_keypair, rent_exempt};
use solana_program::{keccak, pubkey::Pubkey};
use solana_program_test::ProgramTestContext;
use solana_sdk::{
    account::Account,
    instruction::InstructionError,
    signature::{Keypair, Signer},
    transaction::TransactionError,
};
use zk_settlement::errors::SettleError;
use zk_settlement_client as sclient;

fn funded_account() -> Account {
    Account {
        lamports: 50_000_000_000,
        data: vec![],
        owner: solana_system_interface::program::id(),
        executable: false,
        rent_epoch: 0,
    }
}

async fn send(
    ctx: &mut ProgramTestContext,
    ixs: &[solana_program::instruction::Instruction],
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

struct Chain {
    settlement_program: Pubkey,
    inbox_program: Pubkey,
    chain_id: u64,
    authority: Keypair,
    registry_authority: Keypair,
    upgrade_authority: Keypair,
    treasury: Pubkey,
    genesis_state_root: [u8; 32],
    challenge_window_slots: u32,
}

fn chain(settlement_program: Pubkey) -> Chain {
    Chain {
        settlement_program,
        inbox_program: Pubkey::new_unique(),
        chain_id: 1,
        authority: funded_keypair(),
        registry_authority: funded_keypair(),
        upgrade_authority: funded_keypair(),
        treasury: Pubkey::new_unique(),
        genesis_state_root: keccak::hashv(&[b"genesis"]).to_bytes(),
        challenge_window_slots: 5,
    }
}

const GENESIS_BLOCK_HASH: [u8; 32] = [0x11u8; 32];

/// Registers a chain on the reserved path, bootstrapping the global config and the allowlist first.
async fn init_chain(ctx: &mut ProgramTestContext, payer: &Keypair, c: &Chain) {
    let (global_pda, _) = sclient::global_config_pda(&c.settlement_program);
    assert!(ctx
        .banks_client
        .get_account(global_pda)
        .await
        .unwrap()
        .is_none());

    // The `ProgramData` account is seeded with a default upgrade authority; rewrite only its data
    // (same lamports) so a later warp stays consistent.
    let program_data = sclient::program_data_pda(&c.settlement_program);
    let mut pd = ctx
        .banks_client
        .get_account(program_data)
        .await
        .unwrap()
        .expect("ProgramData account exists");
    pd.data[13..45].copy_from_slice(c.upgrade_authority.pubkey().as_ref());
    ctx.set_account(&program_data, &pd.into());

    let transfer = solana_system_interface::instruction::transfer(
        &ctx.payer.pubkey(),
        &c.treasury,
        rent_exempt(0),
    );
    let funder = ctx.payer.insecure_clone();
    send(ctx, &[transfer], &funder, &[])
        .await
        .expect("fund the treasury to the rent-exempt minimum");

    let fields = sclient::GlobalConfigFields {
        registry_authority: c.registry_authority.pubkey(),
        treasury: c.treasury,
        permissionless_init_enabled: true,
        reclaim_window_slots: zk_settlement::governance::MIN_RECLAIM_WINDOW_SLOTS,
        deposit_lamports: 2_000_000_000,
        default_fee_base_lamports: 1_000_000,
        default_fee_bps: 0,
    };
    let ix = sclient::init_global_config_ix(
        &c.settlement_program,
        &payer.pubkey(),
        &c.upgrade_authority.pubkey(),
        fields.clone(),
    );
    send(ctx, &[ix], payer, &[&c.upgrade_authority])
        .await
        .expect("InitGlobalConfig");
    let set = sclient::set_global_config_ix(
        &c.settlement_program,
        &c.registry_authority.pubkey(),
        sclient::GlobalConfigUpdate {
            permissionless_init_enabled: fields.permissionless_init_enabled,
            reclaim_window_slots: fields.reclaim_window_slots,
            deposit_lamports: fields.deposit_lamports,
            default_fee_base_lamports: fields.default_fee_base_lamports,
            default_fee_bps: fields.default_fee_bps,
        },
    );
    send(ctx, &[set], payer, &[&c.registry_authority])
        .await
        .expect("SetGlobalConfig");

    let allow = sclient::allow_reserved_id_ix(
        &c.settlement_program,
        &payer.pubkey(),
        &c.registry_authority.pubkey(),
        c.chain_id,
    );
    send(ctx, &[allow], payer, &[&c.registry_authority])
        .await
        .expect("AllowReservedId");

    let entries = vec![sclient::RegistryEntry {
        curve: reg_layout::CURVE_BN254,
        scheme: reg_layout::SCHEME_GROTH16,
        vkey_hash: [0u8; 32],
        layout_id: reg_layout::LAYOUT_HEADER_FALLBACK,
    }];
    let init = sclient::init_chain_reserved_ix(
        &c.settlement_program,
        &payer.pubkey(),
        &c.authority.pubkey(),
        &c.registry_authority.pubkey(),
        c.chain_id,
        sclient::InitChainFields {
            number: 0,
            parent_hash: [0u8; 32],
            state_root: c.genesis_state_root,
            block_hash: GENESIS_BLOCK_HASH,
            profile: 0,
            challenge_window_slots: c.challenge_window_slots,
            prove_window_slots: 1000,
            proving_policy: 1,
            poster_bond: 0,
            exit_cap_per_window: 0,
            max_pending: 16,
            inbox_program: c.inbox_program,
            registry_entries: entries,
            max_drift_secs: 60,
        },
    );
    send(ctx, &[init], payer, &[&c.authority, &c.registry_authority])
        .await
        .expect("InitChain");
}

/// A started chain with a pinned program id (the exit-config PDAs' bump search depends on it).
async fn rig() -> (ProgramTestContext, Pubkey, Keypair, Chain) {
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let payer = funded_keypair();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::upgradeable(
            "zk_settlement",
            settlement_program,
        )],
        true,
    );
    pt.add_account(payer.pubkey(), funded_account());
    let c = chain(settlement_program);
    pt.add_account(c.authority.pubkey(), funded_account());
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;
    (ctx, settlement_program, payer, c)
}

#[allow(clippy::too_many_arguments)]
async fn propose(
    ctx: &mut ProgramTestContext,
    payer: &Keypair,
    c: &Chain,
    portal: Option<[u8; 20]>,
    bridge: Option<Pubkey>,
    cap: Option<u64>,
    bond: Option<u64>,
) -> (Result<u64, TransactionError>, u64) {
    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let activation_slot = now + c.challenge_window_slots as u64 + 10;
    let ix = sclient::propose_exit_config_ix(
        &c.settlement_program,
        &c.authority.pubkey(),
        &payer.pubkey(),
        c.chain_id,
        portal,
        bridge,
        cap,
        bond,
        activation_slot,
    );
    (
        send(ctx, &[ix], payer, &[&c.authority]).await,
        activation_slot,
    )
}

async fn activate_at(
    ctx: &mut ProgramTestContext,
    payer: &Keypair,
    c: &Chain,
    activation_slot: u64,
) -> Result<u64, TransactionError> {
    ctx.warp_to_slot(activation_slot).unwrap();
    let ix = sclient::activate_exit_config_ix(&c.settlement_program, c.chain_id);
    send(ctx, &[ix], payer, &[]).await
}

async fn exit_config_of(ctx: &mut ProgramTestContext, c: &Chain) -> exit_config::ExitConfigFields {
    let (pda, _) = sclient::exit_config_pda(&c.settlement_program, c.chain_id);
    let a = ctx.banks_client.get_account(pda).await.unwrap().unwrap();
    exit_config::read(&a.data).unwrap()
}

async fn root_of(ctx: &mut ProgramTestContext, c: &Chain) -> rome_zk_layouts::root::RootFields {
    let (pda, _) = sclient::root_pda(&c.settlement_program, c.chain_id);
    let a = ctx.banks_client.get_account(pda).await.unwrap().unwrap();
    rome_zk_layouts::root::read(&a.data).unwrap()
}

/// Sets the chain's first bridge through the real propose and activate path.
async fn set_first_bridge(
    ctx: &mut ProgramTestContext,
    payer: &Keypair,
    c: &Chain,
    bridge: Pubkey,
) {
    let (res, slot) = propose(ctx, payer, c, Some([1u8; 20]), Some(bridge), None, None).await;
    res.expect("the first bridge proposal passes");
    activate_at(ctx, payer, c, slot)
        .await
        .expect("the first bridge activation passes");
    assert_eq!(
        exit_config_of(ctx, c).await.bridge_program,
        bridge.to_bytes()
    );
}

// ---------------------------------------------------------------------------------------------
// ProposeExitConfig
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn first_bridge_proposal_and_activation_still_pass() {
    let (mut ctx, _sp, payer, c) = rig().await;
    let bridge = Pubkey::new_from_array([0xb1u8; 32]);
    let (res, slot) = propose(&mut ctx, &payer, &c, None, Some(bridge), None, None).await;
    res.expect("the first bridge proposal must pass");
    let pending = exit_config_of(&mut ctx, &c).await;
    assert_eq!(pending.bridge_program, [0u8; 32]);
    assert_eq!(pending.pending_bridge_program, bridge.to_bytes());
    assert_eq!(pending.pending_mask, exit_config::PENDING_MASK_BRIDGE);

    let cu = activate_at(&mut ctx, &payer, &c, slot)
        .await
        .expect("the first bridge activation must pass");
    eprintln!("ActivateExitConfig (first bridge set) consumed {cu} CU");
    let f = exit_config_of(&mut ctx, &c).await;
    assert_eq!(f.bridge_program, bridge.to_bytes());
    assert_eq!(f.pending_bridge_program, [0u8; 32]);
    assert_eq!(f.pending_mask, 0);
    assert_eq!(f.activation_slot, 0);
}

#[tokio::test]
async fn a_later_proposal_naming_a_different_bridge_is_refused_by_name() {
    let (mut ctx, _sp, payer, c) = rig().await;
    let first = Pubkey::new_from_array([0xb1u8; 32]);
    set_first_bridge(&mut ctx, &payer, &c, first).await;
    let before = exit_config_of(&mut ctx, &c).await;

    let other = Pubkey::new_from_array([0xb2u8; 32]);
    let (res, _) = propose(&mut ctx, &payer, &c, None, Some(other), None, None).await;
    let err = res.expect_err("a different bridge must be refused");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::BridgeProgramSetOnce as u32)
    );

    // A bridge named next to a portal, cap and bond part is refused as a whole.
    let (res, _) = propose(
        &mut ctx,
        &payer,
        &c,
        Some([9u8; 20]),
        Some(other),
        Some(5),
        Some(6),
    )
    .await;
    let err = res.expect_err("a bridge among other parts must be refused");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::BridgeProgramSetOnce as u32)
    );

    let after = exit_config_of(&mut ctx, &c).await;
    assert_eq!(after, before, "a refused proposal must write nothing");
}

#[tokio::test]
async fn a_later_proposal_naming_the_same_bridge_is_refused_by_name() {
    let (mut ctx, _sp, payer, c) = rig().await;
    let first = Pubkey::new_from_array([0xb1u8; 32]);
    set_first_bridge(&mut ctx, &payer, &c, first).await;

    let (res, _) = propose(&mut ctx, &payer, &c, None, Some(first), None, None).await;
    let err = res.expect_err("the same bridge again must be refused");
    assert_eq!(
        custom_error(&err),
        Some(SettleError::BridgeProgramSetOnce as u32)
    );
    assert_eq!(exit_config_of(&mut ctx, &c).await.pending_mask, 0);
}

#[tokio::test]
async fn portal_cap_and_bond_proposals_still_pass_after_the_bridge_is_set() {
    let (mut ctx, _sp, payer, c) = rig().await;
    let first = Pubkey::new_from_array([0xb1u8; 32]);
    set_first_bridge(&mut ctx, &payer, &c, first).await;

    // Each part alone, then all three together, each activated before the next is proposed.
    let cases = [
        (Some([0x71u8; 20]), None, None),
        (None, Some(1_000), None),
        (None, None, Some(2_000)),
        (Some([0x72u8; 20]), Some(3_000), Some(4_000)),
    ];
    for (portal, cap, bond) in cases {
        let (res, slot) = propose(&mut ctx, &payer, &c, portal, None, cap, bond).await;
        res.expect("a proposal without a bridge must pass once the bridge is set");
        activate_at(&mut ctx, &payer, &c, slot)
            .await
            .expect("its activation must pass");
        let f = exit_config_of(&mut ctx, &c).await;
        assert_eq!(f.bridge_program, first.to_bytes(), "the bridge never moves");
        assert_eq!(f.pending_mask, 0);
        let root = root_of(&mut ctx, &c).await;
        if let Some(p) = portal {
            assert_eq!(f.exit_portal, p);
        }
        if let Some(v) = cap {
            assert_eq!(root.exit_cap_per_window, v);
        }
        if let Some(v) = bond {
            assert_eq!(root.poster_bond, v);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// ActivateExitConfig over a proposal that was made before the bridge became set-once
// ---------------------------------------------------------------------------------------------

/// Rewrites the `exit_config` account's data (same lamports, so a later warp stays consistent) into
/// the state an older program version could leave behind: the bridge set, and a pending proposal that
/// still names another bridge.
async fn leave_old_pending_proposal(
    ctx: &mut ProgramTestContext,
    c: &Chain,
    mask: u8,
    activation_slot: u64,
    pending_bridge: Pubkey,
) {
    let (pda, _) = sclient::exit_config_pda(&c.settlement_program, c.chain_id);
    let mut acct = ctx.banks_client.get_account(pda).await.unwrap().unwrap();
    let mut f = exit_config::read(&acct.data).unwrap();
    f.pending_exit_portal = [0x33u8; 20];
    f.pending_bridge_program = pending_bridge.to_bytes();
    f.pending_exit_cap = 7_000;
    f.pending_poster_bond = 8_000;
    f.activation_slot = activation_slot;
    f.pending_mask = mask;
    acct.data = exit_config::write(&f).to_vec();
    ctx.set_account(&pda, &acct.into());
}

#[tokio::test]
async fn an_old_pending_bridge_proposal_cannot_replace_a_set_bridge_but_its_other_parts_apply() {
    let (mut ctx, _sp, payer, c) = rig().await;
    let first = Pubkey::new_from_array([0xb1u8; 32]);
    set_first_bridge(&mut ctx, &payer, &c, first).await;

    let slot = ctx.banks_client.get_root_slot().await.unwrap() + 20;
    let mask = exit_config::PENDING_MASK_PORTAL
        | exit_config::PENDING_MASK_BRIDGE
        | exit_config::PENDING_MASK_CAP
        | exit_config::PENDING_MASK_BOND;
    let old = Pubkey::new_from_array([0xb2u8; 32]);
    leave_old_pending_proposal(&mut ctx, &c, mask, slot, old).await;

    let cu = activate_at(&mut ctx, &payer, &c, slot)
        .await
        .expect("activation must not refuse over a dropped bridge part");
    eprintln!("ActivateExitConfig (bridge part dropped, three parts applied) consumed {cu} CU");

    let f = exit_config_of(&mut ctx, &c).await;
    assert_eq!(f.bridge_program, first.to_bytes(), "the set bridge stays");
    assert_eq!(f.exit_portal, [0x33u8; 20], "the portal part applies");
    assert_eq!(f.pending_exit_portal, [0u8; 20]);
    assert_eq!(
        f.pending_bridge_program, [0u8; 32],
        "the dropped bridge is cleared too"
    );
    assert_eq!(f.pending_exit_cap, 0);
    assert_eq!(f.pending_poster_bond, 0);
    assert_eq!(f.activation_slot, 0);
    assert_eq!(f.pending_mask, 0, "the pending slot is clear");
    let root = root_of(&mut ctx, &c).await;
    assert_eq!(root.exit_cap_per_window, 7_000, "the cap part applies");
    assert_eq!(root.poster_bond, 8_000, "the bond part applies");

    // The slot is free again: a portal proposal goes through, and a bridge one is still refused.
    let (res, _) = propose(&mut ctx, &payer, &c, Some([0x44u8; 20]), None, None, None).await;
    res.expect("the cleared slot takes a new portal proposal");
}

#[tokio::test]
async fn an_old_bridge_only_pending_proposal_is_cleared_without_refusing() {
    let (mut ctx, _sp, payer, c) = rig().await;
    let first = Pubkey::new_from_array([0xb1u8; 32]);
    set_first_bridge(&mut ctx, &payer, &c, first).await;
    let before = exit_config_of(&mut ctx, &c).await;
    let root_before = root_of(&mut ctx, &c).await;

    let slot = ctx.banks_client.get_root_slot().await.unwrap() + 20;
    leave_old_pending_proposal(
        &mut ctx,
        &c,
        exit_config::PENDING_MASK_BRIDGE,
        slot,
        Pubkey::new_from_array([0xb2u8; 32]),
    )
    .await;
    activate_at(&mut ctx, &payer, &c, slot)
        .await
        .expect("a bridge-only leftover must clear, not freeze the exit config");

    assert_eq!(exit_config_of(&mut ctx, &c).await, before);
    let root = root_of(&mut ctx, &c).await;
    assert_eq!(root.exit_cap_per_window, root_before.exit_cap_per_window);
    assert_eq!(root.poster_bond, root_before.poster_bond);
}

// ---------------------------------------------------------------------------------------------
// PostRoot does not care which header version the batch carries
// ---------------------------------------------------------------------------------------------

const OPEN_SLOT: u64 = 100;
const EXPECTED_COUNT: u32 = 3;

/// A finalized inbox batch account, as a v2 header (no deposit range) or a v3 header (a bound range).
fn inbox_batch(c: &Chain, batch_no: u64, acc: [u8; 32], deposit: Option<BatchDeposit>) -> Account {
    let f = BatchFields {
        chain_id: c.chain_id,
        batch: batch_no,
        open_slot: OPEN_SLOT,
        expected_count: EXPECTED_COUNT,
        leaves_present: EXPECTED_COUNT,
        finalized: true,
        settlement_program: c.settlement_program.to_bytes(),
        authority: c.authority.pubkey().to_bytes(),
        root: [0u8; 32],
        forced_root: [0u8; 32],
        acc,
        finalize_cursor: 0,
        open_unix_ts: 0,
        deposit,
    };
    let (version, header): (u8, Vec<u8>) = match deposit {
        None => (batch::VERSION, batch::write_header(&f).to_vec()),
        Some(_) => (
            batch::VERSION_V3,
            batch::write_header_v3(&f).unwrap().to_vec(),
        ),
    };
    let mut d = vec![0u8; batch::account_len_for(version, EXPECTED_COUNT).unwrap()];
    d[..header.len()].copy_from_slice(&header);
    Account {
        lamports: rent_exempt(d.len()),
        data: d,
        owner: c.inbox_program,
        executable: false,
        rent_epoch: 0,
    }
}

fn deposit_range() -> BatchDeposit {
    BatchDeposit {
        from: 0,
        to: 2,
        hash_from: [0xd1u8; 32],
        hash_to: [0xd2u8; 32],
    }
}

fn post_args(c: &Chain, batch_no: u64, inbox_commitment: [u8; 32]) -> sclient::PostRootFields {
    sclient::PostRootFields {
        chain_id: c.chain_id,
        batch: batch_no,
        prev_batch: 0,
        pre_state_root: c.genesis_state_root,
        first_block: 1,
        last_block: 1,
        state_root: keccak::hashv(&[b"state root"]).to_bytes(),
        block_roots_merkle: keccak::hashv(&[b"block roots"]).to_bytes(),
        inbox_commitment,
        forced_outcome_commitment: rome_zk_layouts::forced_empty_root(&rome_zk_merkle::keccak256),
        parent_hash: keccak::hashv(&[b"parent hash"]).to_bytes(),
        last_block_hash: keccak::hashv(&[b"block hash"]).to_bytes(),
        gas_in_batch: 0,
    }
}

/// A started chain with the inbox batch account for batch 1 in place from genesis.
async fn post_root_rig(
    batch_account: impl Fn(&Chain) -> Account,
) -> (ProgramTestContext, Keypair, Chain) {
    let settlement_program = Pubkey::new_unique();
    let payer = funded_keypair();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::upgradeable(
            "zk_settlement",
            settlement_program,
        )],
        true,
    );
    pt.add_account(payer.pubkey(), funded_account());
    let c = chain(settlement_program);
    pt.add_account(c.authority.pubkey(), funded_account());
    pt.add_account(
        sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 1),
        batch_account(&c),
    );
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;
    (ctx, payer, c)
}

async fn post_root(
    ctx: &mut ProgramTestContext,
    c: &Chain,
    args: sclient::PostRootFields,
) -> Result<u64, TransactionError> {
    let ix = sclient::post_root_ix(
        &c.settlement_program,
        &c.authority.pubkey(),
        &c.inbox_program,
        &c.treasury,
        args,
    )
    .expect("reserved chain");
    send(ctx, &[ix], &c.authority, &[]).await
}

#[tokio::test]
async fn post_root_on_a_reserved_chain_passes_over_a_v2_header() {
    let acc = keccak::hashv(&[b"inbox acc v2"]).to_bytes();
    let (mut ctx, _payer, c) = post_root_rig(|c| inbox_batch(c, 1, acc, None)).await;
    let cu = post_root(&mut ctx, &c, post_args(&c, 1, acc))
        .await
        .expect("PostRoot over a v2 header must pass");
    eprintln!("PostRoot (reserved chain, v2 header) consumed {cu} CU");
}

#[tokio::test]
async fn post_root_on_a_reserved_chain_passes_over_a_v3_header() {
    let acc = keccak::hashv(&[b"inbox acc v3"]).to_bytes();
    let (mut ctx, _payer, c) =
        post_root_rig(|c| inbox_batch(c, 1, acc, Some(deposit_range()))).await;
    let cu = post_root(&mut ctx, &c, post_args(&c, 1, acc))
        .await
        .expect("PostRoot over a v3 header must pass");
    eprintln!("PostRoot (reserved chain, v3 header) consumed {cu} CU");
}

/// A batch that bound a deposit range has an `acc` built over a forced root that carries the
/// deposits. A post that brings the deposit-free `acc` (the one over the empty forced lane) for that
/// batch must be refused, not matched.
#[tokio::test]
async fn post_root_carrying_the_deposit_free_acc_for_a_batch_that_bound_a_range_is_refused() {
    let h = rome_zk_merkle::keccak256;
    let root = [0u8; 32];
    let with_deposits = rome_zk_layouts::acc(
        &h,
        1,
        1,
        OPEN_SLOT,
        EXPECTED_COUNT,
        &root,
        &keccak::hashv(&[b"forced root with two deposit credits"]).to_bytes(),
    );
    let deposit_free = rome_zk_layouts::acc(
        &h,
        1,
        1,
        OPEN_SLOT,
        EXPECTED_COUNT,
        &root,
        &rome_zk_layouts::forced_empty_root(&h),
    );
    assert_ne!(with_deposits, deposit_free);

    let (mut ctx, _payer, c) =
        post_root_rig(|c| inbox_batch(c, 1, with_deposits, Some(deposit_range()))).await;
    let err = post_root(&mut ctx, &c, post_args(&c, 1, deposit_free))
        .await
        .expect_err("the deposit-free acc must not match a batch that bound a range");
    assert_eq!(custom_error(&err), Some(SettleError::AccMismatch as u32));

    // And the batch's own acc is the one that passes.
    post_root(&mut ctx, &c, post_args(&c, 1, with_deposits))
        .await
        .expect("the acc the batch carries must pass");
}
