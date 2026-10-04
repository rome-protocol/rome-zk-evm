//! solana-program-test integration tests for programs/zk-settlement.
//! Loads the real, `cargo build-sbf`-compiled `.so` (not a native/builtin shortcut) so
//! `compute_units_consumed` reflects real BPF execution — run
//! `cargo build-sbf --manifest-path programs/zk-settlement/Cargo.toml` (and, for the cross-program
//! tests, `--manifest-path programs/zk-inbox/Cargo.toml`) before `cargo test -p zk-settlement`.

use rome_zk_layouts::registry as reg_layout;
use rome_zk_testkit::{cursor_account, funded_keypair, prefund_pda, rent_exempt};
use solana_program::{
    instruction::{AccountMeta, Instruction},
    keccak,
    pubkey::Pubkey,
};
use solana_program_test::ProgramTest;
use solana_sdk::{
    account::Account,
    instruction::InstructionError,
    signature::{Keypair, Signer},
    transaction::{Transaction, TransactionError},
};
use solana_system_interface::program as system_program;
use zk_settlement_client as sclient;

fn funded_account() -> Account {
    Account {
        lamports: 50_000_000_000,
        data: vec![],
        owner: system_program::id(),
        executable: false,
        rent_epoch: 0,
    }
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

/// Builds a finalized inbox batch account's raw bytes directly (the inbox layout), independent of
/// whether the real zk-inbox program is loaded — `PostRoot`/`PostRootProved` only read this account.
#[allow(clippy::too_many_arguments)]
fn inbox_batch_account(
    owner: Pubkey,
    chain_id: u64,
    batch: u64,
    settlement_program: Pubkey,
    authority: Pubkey,
    finalized: bool,
    acc: [u8; 32],
) -> Account {
    let expected_count: u32 = 3;
    let mut d = vec![
        0u8;
        rome_zk_layouts::batch::account_len_for(
            rome_zk_layouts::batch::VERSION,
            expected_count
        )
        .unwrap()
    ];
    d[0..4].copy_from_slice(&rome_zk_layouts::batch::MAGIC.to_le_bytes());
    d[4] = rome_zk_layouts::batch::VERSION;
    d[5..13].copy_from_slice(&chain_id.to_le_bytes());
    d[13..21].copy_from_slice(&batch.to_le_bytes());
    d[29..33].copy_from_slice(&expected_count.to_le_bytes());
    d[33..37].copy_from_slice(&expected_count.to_le_bytes()); // leaves_present == expected_count
    d[37] = finalized as u8;
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

/// Same as [`inbox_batch_account`], plus an explicit `open_unix_ts` (`PostRootProved`'s
/// layout-1 binding checks the guest's committed `open_unix_ts` against exactly this field) — every other
/// existing call site keeps using [`inbox_batch_account`], which leaves this at 0.
#[allow(clippy::too_many_arguments)]
fn inbox_batch_account_with_open_ts(
    owner: Pubkey,
    chain_id: u64,
    batch: u64,
    settlement_program: Pubkey,
    authority: Pubkey,
    finalized: bool,
    acc: [u8; 32],
    open_unix_ts: i64,
) -> Account {
    let mut acct = inbox_batch_account(
        owner,
        chain_id,
        batch,
        settlement_program,
        authority,
        finalized,
        acc,
    );
    acct.data
        [rome_zk_layouts::batch::OFF_OPEN_UNIX_TS..rome_zk_layouts::batch::OFF_OPEN_UNIX_TS + 8]
        .copy_from_slice(&open_unix_ts.to_le_bytes());
    acct
}

struct Chain {
    settlement_program: Pubkey,
    inbox_program: Pubkey,
    chain_id: u64,
    authority: Keypair,
    genesis_state_root: [u8; 32],
    challenge_window_slots: u32,
    /// Registration-and-revenue proposal: the registry authority that must co-sign a reserved
    /// `InitChain` and gates every other governance instruction. Not pre-funded on-chain — every call
    /// that needs it to pay rent takes a separate funded `payer` instead (see `governance.rs`'s module
    /// doc split).
    registry_authority: Keypair,
    /// Where `PostRoot`/`PostRootProved`'s protocol fee and `ReclaimChain`'s sweep land. Funded to
    /// the rent-exempt minimum by `ensure_global_config` (`InitGlobalConfig` requires this
    /// account to already exist and be rent-exempt) — no longer a bare, never-materialized address.
    treasury: Pubkey,
    /// The program's real upgrade authority — `ensure_global_config` patches the
    /// `ProgramData` account (seeded by `add_upgradeable_program_to_genesis` with
    /// `Pubkey::default()`) to this key and signs `InitGlobalConfig` with it.
    upgrade_authority: Keypair,
}

fn default_chain(settlement_program: Pubkey) -> Chain {
    Chain {
        settlement_program,
        inbox_program: Pubkey::new_unique(),
        chain_id: 1,
        authority: funded_keypair(),
        genesis_state_root: keccak::hashv(&[b"genesis"]).to_bytes(),
        challenge_window_slots: 5,
        registry_authority: funded_keypair(),
        treasury: Pubkey::new_unique(),
        upgrade_authority: funded_keypair(),
    }
}

/// Default global config fields this test suite registers under (registration-and-revenue proposal):
/// permissionless registration enabled (so the permissionless tests can exercise it directly), a short reclaim
/// window (in slots, not real time — `warp_to_slot` drives it), a deposit small enough that
/// `funded_account()`'s balance comfortably covers it, and a nonzero base fee so `charge_fee`'s transfer
/// is always observable.
fn default_global_config_fields(c: &Chain) -> sclient::GlobalConfigFields {
    sclient::GlobalConfigFields {
        registry_authority: c.registry_authority.pubkey(),
        treasury: c.treasury,
        permissionless_init_enabled: true,
        // `InitGlobalConfig`/`SetGlobalConfig` floor this at
        // `governance::MIN_RECLAIM_WINDOW_SLOTS` (1 day of slots) — this suite never exercises
        // `ReclaimChain`, so the exact value doesn't matter beyond clearing the floor.
        reclaim_window_slots: zk_settlement::governance::MIN_RECLAIM_WINDOW_SLOTS,
        deposit_lamports: 2_000_000_000,
        default_fee_base_lamports: 1_000_000,
        default_fee_bps: 0,
    }
}

/// Idempotent: sends `InitGlobalConfig` for `c.settlement_program` unless it already exists — most tests
/// build a fresh throwaway program per test (one `InitGlobalConfig` needed), but a few chain a second
/// `Chain` onto the same program (only the first call should actually send the instruction).
async fn ensure_global_config(
    ctx: &mut solana_program_test::ProgramTestContext,
    payer: &Keypair,
    c: &Chain,
) {
    let (global_pda, _) = sclient::global_config_pda(&c.settlement_program);
    if ctx
        .banks_client
        .get_account(global_pda)
        .await
        .unwrap()
        .is_some()
    {
        return;
    }

    // Patch the `ProgramData` account (seeded by `add_upgradeable_program_to_genesis` with
    // `Some(Pubkey::default())`) to `c.upgrade_authority` — a DATA-only rewrite (same lamports), so it
    // stays safe under a later `warp_to_slot` (unlike a lamport-changing post-start `set_account` —
    // `program_test`'s doc / `post_root_rig`'s doc explain that gotcha).
    let program_data = sclient::program_data_pda(&c.settlement_program);
    let mut pd_account = ctx
        .banks_client
        .get_account(program_data)
        .await
        .unwrap()
        .expect("ProgramData account must exist (added via add_upgradeable_program_to_genesis)");
    pd_account.data[13..45].copy_from_slice(c.upgrade_authority.pubkey().as_ref());
    ctx.set_account(&program_data, &pd_account.into());

    // The treasury account must already be rent-exempt when `InitGlobalConfig` runs. Funded
    // via a real system transfer (not `ctx.set_account`) — a transfer moves lamports between accounts the
    // bank already tracks, so it never desyncs the capitalization counter a later `warp_to_slot` checks
    // (the same reason `pt_add_extra_funding` above uses a transfer, not a raw account injection).
    let transfer_ix = solana_system_interface::instruction::transfer(
        &ctx.payer.pubkey(),
        &c.treasury,
        rent_exempt(0),
    );
    let recent = ctx.get_new_latest_blockhash().await.unwrap();
    let funder = ctx.payer.insecure_clone();
    rome_zk_testkit::send_checked(
        &mut ctx.banks_client,
        Transaction::new_signed_with_payer(
            &[transfer_ix],
            Some(&funder.pubkey()),
            &[&funder],
            recent,
        ),
    )
    .await
    .expect("fund treasury to the rent-exempt minimum");

    let fields = default_global_config_fields(c);
    let ix = sclient::init_global_config_ix(
        &c.settlement_program,
        &payer.pubkey(),
        &c.upgrade_authority.pubkey(),
        fields.clone(),
    );
    send(ctx, &[ix], payer, &[&c.upgrade_authority])
        .await
        .expect("InitGlobalConfig should succeed");

    // `permissionless_init_enabled` is forced `false` at `InitGlobalConfig` regardless of what
    // was passed above — flip every field to what this suite actually wants via the registry-authority-
    // only `SetGlobalConfig`.
    let set_ix = sclient::set_global_config_ix(
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
    send(ctx, &[set_ix], payer, &[&c.registry_authority])
        .await
        .expect("SetGlobalConfig should succeed");
}

/// Idempotent: allows `c.chain_id` on the reserved allowlist unless it already is.
async fn ensure_reserved_allowed(
    ctx: &mut solana_program_test::ProgramTestContext,
    payer: &Keypair,
    c: &Chain,
) {
    let (allow_pda, _) = sclient::reserved_allow_pda(&c.settlement_program, c.chain_id);
    if ctx
        .banks_client
        .get_account(allow_pda)
        .await
        .unwrap()
        .is_some()
    {
        return;
    }
    let ix = sclient::allow_reserved_id_ix(
        &c.settlement_program,
        &payer.pubkey(),
        &c.registry_authority.pubkey(),
        c.chain_id,
    );
    send(ctx, &[ix], payer, &[&c.registry_authority])
        .await
        .expect("AllowReservedId should succeed");
}

/// Hand-built `SettleIx::Init` (discriminant 0) — no client builder exists for it (the legacy path
/// predates `zk-settlement-client` and is disabled here); accounts mirror what the legacy handler used
/// to expect: `[payer (signer, writable), root pda (writable), system_program]`. `root_pda` is the same
/// `["root", chain_id]` derivation `InitChain` uses (`zk_settlement::root_pda`, re-exported from
/// `chain.rs`) — the legacy path never had its own seeds.
fn legacy_init_ix(
    program_id: &Pubkey,
    payer: &Pubkey,
    chain_id: u64,
    number: u64,
    state_root: [u8; 32],
) -> Instruction {
    let (pda, _bump) = zk_settlement::root_pda(program_id, chain_id);
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(pda, false),
            AccountMeta::new_readonly(system_program::id(), false),
        ],
        data: borsh::to_vec(&zk_settlement::SettleIx::Init {
            chain_id,
            number,
            state_root,
        })
        .unwrap(),
    }
}

// ---------------------------------------------------------------------------------------------
// (0) create_or_adopt_pda: a pre-funded PDA must never permanently block creation, on zk-settlement's
// own creation sites: the root + registry and the per-batch pending PDA. The legacy root's own
// prefund test is gone — the legacy path is disabled below, so it has no creation site left to
// prefund.
// ---------------------------------------------------------------------------------------------

/// `legacy::init` shared `InitChain`'s `["root", chain_id]` seeds and was permissionless, so anyone
/// could pre-create an 88-byte legacy root for any future chain id and brick `InitChain` for that chain
/// forever (`InvalidAccountData`, owner already == program). Tiber's programs are fresh deployments —
/// no already-deployed 88-byte legacy root exists anywhere for this to stay backward-compatible with —
/// so `legacy.rs` is removed outright and `SettleIx::Init` (discriminant 0) is disabled: a
/// properly-encoded `Init` instruction must be rejected before it ever touches an account, regardless
/// of what rides along in the accounts list.
#[tokio::test]
async fn legacy_init_is_disabled() {
    let program_id = Pubkey::new_unique();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::upgradeable(
            "zk_settlement",
            program_id,
        )],
        true,
    );
    let payer = funded_keypair();
    pt.add_account(payer.pubkey(), funded_account());
    let mut ctx = pt.start_with_context().await;

    let chain_id = 900_100u64;
    let state_root = keccak::hashv(&[b"legacy genesis"]).to_bytes();
    let err = send(
        &mut ctx,
        &[legacy_init_ix(
            &program_id,
            &payer.pubkey(),
            chain_id,
            0,
            state_root,
        )],
        &payer,
        &[],
    )
    .await
    .expect_err("SettleIx::Init (discriminant 0) must be rejected, not executed");
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::InvalidInstructionData)
    );

    // And it must not have created anything at the would-be root PDA either.
    let (pda, _bump) = zk_settlement::root_pda(&program_id, chain_id);
    assert!(
        ctx.banks_client.get_account(pda).await.unwrap().is_none(),
        "a disabled Init must not create the root pda"
    );
}

fn registry_entries() -> Vec<sclient::RegistryEntry> {
    vec![
        sclient::RegistryEntry {
            curve: reg_layout::CURVE_BN254,
            scheme: reg_layout::SCHEME_PLONK,
            vkey_hash: zisk_program_vk(14),
            layout_id: reg_layout::LAYOUT_HEADER_FALLBACK,
        },
        sclient::RegistryEntry {
            curve: reg_layout::CURVE_BN254,
            scheme: reg_layout::SCHEME_GROTH16,
            vkey_hash: [0u8; 32],
            layout_id: reg_layout::LAYOUT_HEADER_FALLBACK,
        },
    ]
}

/// The genesis block hash every test chain is registered with. A proved root must chain to its
/// predecessor, so a layout-1 test that wants to reach the pairing posts a proof whose parent hash is
/// this value.
const GENESIS_BLOCK_HASH: [u8; 32] = [0x11u8; 32];

fn default_init_chain_fields(c: &Chain, max_pending: u32) -> sclient::InitChainFields {
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
        max_pending,
        inbox_program: c.inbox_program,
        registry_entries: registry_entries(),
        max_drift_secs: 60,
    }
}

/// The shared rig for every existing (pre-registration-and-revenue) test: registers `c.chain_id` on the
/// RESERVED path (idempotently bootstrapping `global_config` + the allowlist marker first) with
/// `max_pending = 16`. `payer` both pays every instruction's rent and signs as the chain authority's
/// counterpart fee payer; `c.authority` and `c.registry_authority` co-sign as the reserved path requires.
async fn init_chain(
    ctx: &mut solana_program_test::ProgramTestContext,
    payer: &Keypair,
    c: &Chain,
) -> u64 {
    init_chain_with_max_pending(ctx, payer, c, 16).await
}

async fn init_chain_with_max_pending(
    ctx: &mut solana_program_test::ProgramTestContext,
    payer: &Keypair,
    c: &Chain,
    max_pending: u32,
) -> u64 {
    ensure_global_config(ctx, payer, c).await;
    ensure_reserved_allowed(ctx, payer, c).await;
    let ix = sclient::init_chain_reserved_ix(
        &c.settlement_program,
        &payer.pubkey(),
        &c.authority.pubkey(),
        &c.registry_authority.pubkey(),
        c.chain_id,
        default_init_chain_fields(c, max_pending),
    );
    send(ctx, &[ix], payer, &[&c.authority, &c.registry_authority])
        .await
        .expect("InitChain should succeed")
}

fn calldata_fixture(block: u32) -> serde_json::Value {
    serde_json::from_str(
        &std::fs::read_to_string(format!(
            "{}/../../fixtures/s10/block{block}.calldata.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap(),
    )
    .unwrap()
}
fn hx(v: &serde_json::Value) -> Vec<u8> {
    hex::decode(v.as_str().unwrap().trim_start_matches("0x")).unwrap()
}
fn zisk_program_vk(block: u32) -> [u8; 32] {
    hx(&calldata_fixture(block)["programVK"])
        .try_into()
        .unwrap()
}
fn zisk_proof_abi(block: u32) -> Vec<u8> {
    let j = calldata_fixture(block);
    let mut d = hx(&j["proofBytes"]);
    d.extend(hx(&j["programVK"]));
    d.extend(hx(&j["rootCVadcopFinal"]));
    d.extend(hx(&j["publicValues"]));
    assert_eq!(d.len(), 768 + 32 + 32 + 512);
    d
}

// ---------------------------------------------------------------------------------------------
// (1) InitChain
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn init_chain_writes_the_exact_zkrt_layout() {
    // Fixed, not `Pubkey::new_unique()`: the root/registry PDA bump search depends on the program id, so
    // the CU printed below would otherwise move from run to run.
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::upgradeable(
            "zk_settlement",
            settlement_program,
        )],
        true,
    );
    let payer = funded_keypair();
    pt.add_account(payer.pubkey(), funded_account());
    let mut ctx = pt.start_with_context().await;
    let c = default_chain(settlement_program);
    let cu = init_chain(&mut ctx, &payer, &c).await;
    eprintln!("InitChain consumed {cu} CU");

    let (root_pda, _) = sclient::root_pda(&settlement_program, c.chain_id);
    let root_acc = ctx
        .banks_client
        .get_account(root_pda)
        .await
        .unwrap()
        .expect("root account must exist");
    assert_eq!(root_acc.data.len(), rome_zk_layouts::root::MIN_LEN);
    let f = rome_zk_layouts::root::read(&root_acc.data).expect("root account must decode");
    assert_eq!(f.chain_id, c.chain_id);
    assert_eq!(f.number, 0);
    assert_eq!(f.state_root, c.genesis_state_root);
    assert_eq!(f.authority, c.authority.pubkey().to_bytes());
    assert_eq!(f.head_pending_batch, 0);
    assert_eq!(f.head_final_batch, 0);
    assert_eq!(f.pending_count, 0);
    assert_eq!(f.max_pending, 16);
    assert_eq!(f.challenge_window_slots, c.challenge_window_slots);

    let (registry_pda, _) = sclient::registry_pda(&settlement_program, c.chain_id);
    let registry_acc = ctx
        .banks_client
        .get_account(registry_pda)
        .await
        .unwrap()
        .expect("registry account must exist");
    assert_eq!(registry_acc.data.len(), reg_layout::REGISTRY_LEN);
    let hdr = reg_layout::read_header(&registry_acc.data).unwrap();
    assert_eq!(hdr.chain_id, c.chain_id);
    assert_eq!(hdr.count, 2);
    assert_eq!(Pubkey::new_from_array(hdr.inbox_program), c.inbox_program);
    let (e0, activation0) = reg_layout::entry_at(&registry_acc.data, 0).unwrap();
    assert_eq!(e0.curve, reg_layout::CURVE_BN254);
    assert_eq!(e0.scheme, reg_layout::SCHEME_PLONK);
    assert_eq!(e0.layout_id, reg_layout::LAYOUT_HEADER_FALLBACK);
    assert_eq!(
        activation0, 0,
        "InitChainV2's entries are active since genesis (v1-length account)"
    );
}

/// Repro on `InitChain`'s two creation sites: an attacker prefunds both the root PDA and the registry PDA
/// before `InitChain` ever runs. Must adopt both, not permanently block chain bring-up.
#[tokio::test]
async fn init_chain_succeeds_even_when_an_attacker_prefunds_the_root_or_registry_pda() {
    // Fixed, not `Pubkey::new_unique()`: the root/registry PDA bump search depends on the program id, so
    // the CU printed below would otherwise move from run to run.
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::upgradeable(
            "zk_settlement",
            settlement_program,
        )],
        true,
    );
    let payer = funded_keypair();
    pt.add_account(payer.pubkey(), funded_account());
    let mut ctx = pt.start_with_context().await;
    let c = default_chain(settlement_program);

    let (root_pda, _) = sclient::root_pda(&settlement_program, c.chain_id);
    let (registry_pda, _) = sclient::registry_pda(&settlement_program, c.chain_id);
    prefund_pda(&mut ctx, root_pda).await;
    prefund_pda(&mut ctx, registry_pda).await;
    let donated_root = ctx
        .banks_client
        .get_account(root_pda)
        .await
        .unwrap()
        .expect("prefund must have created a system-owned root account")
        .lamports;
    let donated_registry = ctx
        .banks_client
        .get_account(registry_pda)
        .await
        .unwrap()
        .expect("prefund must have created a system-owned registry account")
        .lamports;
    assert!(donated_root > 0 && donated_registry > 0);

    let cu = init_chain(&mut ctx, &payer, &c).await;
    eprintln!("InitChain (against pre-funded root+registry PDAs) consumed {cu} CU");

    let root_acc = ctx
        .banks_client
        .get_account(root_pda)
        .await
        .unwrap()
        .expect("root account must exist");
    assert_eq!(root_acc.owner, settlement_program);
    assert_eq!(root_acc.data.len(), rome_zk_layouts::root::MIN_LEN);
    assert!(root_acc.lamports >= donated_root);
    let f = rome_zk_layouts::root::read(&root_acc.data).expect("root account must decode");
    assert_eq!(f.chain_id, c.chain_id);
    assert_eq!(f.authority, c.authority.pubkey().to_bytes());

    let registry_acc = ctx
        .banks_client
        .get_account(registry_pda)
        .await
        .unwrap()
        .expect("registry account must exist");
    assert_eq!(registry_acc.owner, settlement_program);
    assert_eq!(registry_acc.data.len(), reg_layout::REGISTRY_LEN);
    assert!(registry_acc.lamports >= donated_registry);
    let hdr = reg_layout::read_header(&registry_acc.data).unwrap();
    assert_eq!(hdr.chain_id, c.chain_id);
    assert_eq!(hdr.count, 2);
}

/// The zk-inbox program's `OpenBatch`/`Close` checks read this same root account — this
/// proves they now PASS against a REAL root account produced by `InitChain`, using the built
/// `zk_inbox.so` (cross-program test, both real programs loaded in one `ProgramTest`).
#[tokio::test]
async fn inbox_open_batch_and_close_pass_against_a_real_initchain_root_account() {
    let settlement_program = Pubkey::new_unique();
    let inbox_program = Pubkey::new_unique();
    let mut pt = rome_zk_testkit::program_test(
        &[
            rome_zk_testkit::ProgramSpec::upgradeable("zk_settlement", settlement_program),
            rome_zk_testkit::ProgramSpec::new("zk_inbox", inbox_program),
        ],
        true,
    );
    let payer = funded_keypair();
    pt.add_account(payer.pubkey(), funded_account());
    // `default_chain`'s `chain_id` is always the constant 1 (see its own definition) — safe to hardcode
    // here since the cursor account must be seeded before `start_with_context`, ahead of `default_chain`
    // actually being called.
    let chain_id = 1u64;
    let batch = 1u64;
    pt.add_account(
        zk_inbox_client::cursor_pda(&inbox_program, &settlement_program, chain_id).0,
        cursor_account(inbox_program, chain_id, batch),
    );
    let mut ctx = pt.start_with_context().await;

    let mut c = default_chain(settlement_program);
    c.inbox_program = inbox_program;
    c.authority = funded_keypair();
    assert_eq!(c.chain_id, chain_id);
    pt_add_extra_funding(&mut ctx, &c.authority.pubkey()).await;
    init_chain(&mut ctx, &payer, &c).await;

    // --- inbox OpenBatch, signed by the real root account's authority ---
    let n = 3u32;
    let open_ix = zk_inbox_client::open_batch_ix(
        &inbox_program,
        &c.authority.pubkey(),
        c.chain_id,
        batch,
        n,
        &settlement_program,
    );
    let cu = send(&mut ctx, &[open_ix], &c.authority, &[])
        .await
        .expect("inbox OpenBatch must succeed against the real InitChain root account");
    eprintln!("inbox OpenBatch (against real ZKRT root) consumed {cu} CU");

    // --- write + seal 3 chunks, finalize the batch ---
    let bodies: Vec<Vec<u8>> = (0..n).map(|i| format!("chunk {i}").into_bytes()).collect();
    for (idx, body) in bodies.iter().enumerate() {
        let idx = idx as u32;
        let ixs = vec![
            zk_inbox_client::open_chunk_ix(
                &inbox_program,
                &c.authority.pubkey(),
                &settlement_program,
                c.chain_id,
                batch,
                idx,
                body.len() as u32,
            ),
            zk_inbox_client::write_chunk_ix(
                &inbox_program,
                &c.authority.pubkey(),
                &settlement_program,
                c.chain_id,
                batch,
                idx,
                0,
                body.clone(),
            ),
            zk_inbox_client::seal_chunk_ix(
                &inbox_program,
                &c.authority.pubkey(),
                &settlement_program,
                c.chain_id,
                batch,
                idx,
                body.len() as u32,
                zk_inbox_client::chunk_body_hash(body),
            ),
            zk_inbox_client::seal_leaf_ix(
                &inbox_program,
                &settlement_program,
                c.chain_id,
                batch,
                idx,
            ),
        ];
        send(&mut ctx, &ixs, &c.authority, &[])
            .await
            .expect("chunk open+write+seal must succeed");
    }
    send(
        &mut ctx,
        &[zk_inbox_client::finalize_batch_ix(
            &inbox_program,
            &c.authority.pubkey(),
            &settlement_program,
            c.chain_id,
            batch,
            0,
        )],
        &c.authority,
        &[],
    )
    .await
    .expect("inbox FinalizeBatch must succeed");

    // --- close a chunk: requires the covering root to be final; not yet, so it must fail with RootNotFinal ---
    let close_ix = zk_inbox_client::close_chunk_ix(
        &inbox_program,
        &c.authority.pubkey(),
        &settlement_program,
        c.chain_id,
        batch,
        0,
    );
    let err = send(&mut ctx, std::slice::from_ref(&close_ix), &c.authority, &[])
        .await
        .expect_err("Close must fail before the root is final");
    assert_eq!(
        custom_error(&err),
        Some(zk_inbox::batch::BatchError::RootNotFinal as u32)
    );

    // --- now settle the batch to Final via PostRoot + FinalizeBatch (window elapsed) ---
    let (inbox_batch_pda, _) =
        zk_inbox_client::batch_pda(&inbox_program, &settlement_program, c.chain_id, batch);
    let inbox_acct = ctx
        .banks_client
        .get_account(inbox_batch_pda)
        .await
        .unwrap()
        .unwrap();
    let inbox_decoded = zk_inbox_client::decode_batch_account(&inbox_acct.data).unwrap();

    let args = sclient::PostRootFields {
        chain_id: c.chain_id,
        batch,
        prev_batch: 0,
        pre_state_root: c.genesis_state_root,
        first_block: 1,
        last_block: 1,
        state_root: keccak::hashv(&[b"batch1 state root"]).to_bytes(),
        block_roots_merkle: [0u8; 32],
        inbox_commitment: inbox_decoded.acc,
        forced_outcome_commitment: rome_zk_layouts::forced_empty_root(&rome_zk_merkle::keccak256),
        parent_hash: keccak::hashv(&[b"batch1 parent hash"]).to_bytes(),
        last_block_hash: keccak::hashv(&[b"batch1 block hash"]).to_bytes(),
        gas_in_batch: 0,
    };
    send(
        &mut ctx,
        &[sclient::post_root_ix(
            &settlement_program,
            &c.authority.pubkey(),
            &inbox_program,
            &c.treasury,
            args.clone(),
        )
        .expect("reserved chain")],
        &c.authority,
        &[],
    )
    .await
    .expect("PostRoot should succeed");
    ctx.warp_to_slot(
        ctx.banks_client.get_root_slot().await.unwrap() + c.challenge_window_slots as u64 + 2,
    )
    .unwrap();
    send(
        &mut ctx,
        &[sclient::finalize_batch_ix(
            &settlement_program,
            c.chain_id,
            batch,
            &[],
        )],
        &payer,
        &[],
    )
    .await
    .expect("settlement FinalizeBatch should succeed");

    // --- Close now succeeds against the final root ---
    let cu = send(&mut ctx, &[close_ix], &c.authority, &[])
        .await
        .expect("Close should succeed once the covering root is final");
    eprintln!("inbox chunk Close (against final root) consumed {cu} CU");
}

async fn pt_add_extra_funding(ctx: &mut solana_program_test::ProgramTestContext, who: &Pubkey) {
    // banks_client doesn't expose add_account post-start; fund via a system transfer from the context payer.
    let ix =
        solana_system_interface::instruction::transfer(&ctx.payer.pubkey(), who, 20_000_000_000);
    let recent = ctx.get_new_latest_blockhash().await.unwrap();
    let payer = ctx.payer.insecure_clone();
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer], recent);
    rome_zk_testkit::send_checked(&mut ctx.banks_client, tx)
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------------------------
// (2)/(3) PostRoot happy path + rejects
// ---------------------------------------------------------------------------------------------

async fn happy_post_root_args(c: &Chain, inbox_acc_bytes_acc: [u8; 32]) -> sclient::PostRootFields {
    sclient::PostRootFields {
        chain_id: c.chain_id,
        batch: 1,
        prev_batch: 0,
        pre_state_root: c.genesis_state_root,
        first_block: 1,
        last_block: 1,
        state_root: keccak::hashv(&[b"state root 1"]).to_bytes(),
        block_roots_merkle: keccak::hashv(&[b"block roots 1"]).to_bytes(),
        inbox_commitment: inbox_acc_bytes_acc,
        forced_outcome_commitment: rome_zk_layouts::forced_empty_root(&rome_zk_merkle::keccak256),
        parent_hash: keccak::hashv(&[b"parent hash 1"]).to_bytes(),
        last_block_hash: keccak::hashv(&[b"block hash 1"]).to_bytes(),
        gas_in_batch: 0,
    }
}

#[tokio::test]
async fn post_root_happy_path_creates_pending_pda_and_advances_head() {
    let settlement_program = Pubkey::new_unique();
    let payer = funded_keypair();
    let c = default_chain(settlement_program);
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::upgradeable(
            "zk_settlement",
            settlement_program,
        )],
        true,
    );
    pt.add_account(payer.pubkey(), funded_account());
    pt.add_account(c.authority.pubkey(), funded_account());
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;

    let acc = keccak::hashv(&[b"inbox acc 1"]).to_bytes();
    let inbox_pda = sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 1);
    ctx.set_account(
        &inbox_pda,
        &inbox_batch_account(
            c.inbox_program,
            c.chain_id,
            1,
            settlement_program,
            c.authority.pubkey(),
            true,
            acc,
        )
        .into(),
    );

    let args = happy_post_root_args(&c, acc).await;
    let cu = send(
        &mut ctx,
        &[sclient::post_root_ix(
            &settlement_program,
            &c.authority.pubkey(),
            &c.inbox_program,
            &c.treasury,
            args.clone(),
        )
        .expect("reserved chain")],
        &c.authority,
        &[],
    )
    .await
    .expect("PostRoot happy path should succeed");
    eprintln!("PostRoot consumed {cu} CU");

    let (root_pda, _) = sclient::root_pda(&settlement_program, c.chain_id);
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
    assert_eq!(root.pending_count, 1);
    assert_eq!(root.head_final_batch, 0);

    let (pending_pda, _) = sclient::pending_pda(&settlement_program, c.chain_id, 1);
    let pending = sclient::decode_pending_account(
        &ctx.banks_client
            .get_account(pending_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap();
    assert_eq!(pending.batch, 1);
    assert_eq!(pending.status, rome_zk_layouts::pending::STATUS_PENDING);
    assert_eq!(pending.state_root, args.state_root);
    assert_eq!(
        pending.deadline_slot,
        pending.posted_slot + c.challenge_window_slots as u64
    );
}

/// Repro on `PostRoot`'s pending-PDA creation site: an attacker prefunds the batch's pending PDA before
/// the real authority's `PostRoot` lands. With in-order finality (`FinalizeBatch` requires
/// `batch == head_final_batch + 1`), a batch id that can never be posted halts settlement for the whole
/// chain from that point on — must adopt, not permanently block.
#[tokio::test]
async fn post_root_creates_pending_pda_even_when_an_attacker_prefunds_it() {
    let settlement_program = Pubkey::new_unique();
    let payer = funded_keypair();
    let c = default_chain(settlement_program);
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::upgradeable(
            "zk_settlement",
            settlement_program,
        )],
        true,
    );
    pt.add_account(payer.pubkey(), funded_account());
    pt.add_account(c.authority.pubkey(), funded_account());
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;

    let acc = keccak::hashv(&[b"inbox acc 1"]).to_bytes();
    let inbox_pda = sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 1);
    ctx.set_account(
        &inbox_pda,
        &inbox_batch_account(
            c.inbox_program,
            c.chain_id,
            1,
            settlement_program,
            c.authority.pubkey(),
            true,
            acc,
        )
        .into(),
    );

    let (pending_pda, _) = sclient::pending_pda(&settlement_program, c.chain_id, 1);
    prefund_pda(&mut ctx, pending_pda).await;
    let donated_lamports = ctx
        .banks_client
        .get_account(pending_pda)
        .await
        .unwrap()
        .expect("prefund must have created a system-owned pending account")
        .lamports;
    assert!(donated_lamports > 0);

    let args = happy_post_root_args(&c, acc).await;
    send(
        &mut ctx,
        &[sclient::post_root_ix(
            &settlement_program,
            &c.authority.pubkey(),
            &c.inbox_program,
            &c.treasury,
            args.clone(),
        )
        .expect("reserved chain")],
        &c.authority,
        &[],
    )
    .await
    .expect("PostRoot must adopt a pre-funded pending PDA rather than fail AccountAlreadyInUse");

    let pending_acc = ctx
        .banks_client
        .get_account(pending_pda)
        .await
        .unwrap()
        .expect("pending account must exist");
    assert_eq!(pending_acc.owner, settlement_program);
    assert!(pending_acc.lamports >= donated_lamports);
    let pending = sclient::decode_pending_account(&pending_acc.data).unwrap();
    assert_eq!(pending.batch, 1);
    assert_eq!(pending.status, rome_zk_layouts::pending::STATUS_PENDING);
    assert_eq!(pending.state_root, args.state_root);
}

/// Shared rig for the `PostRoot` negative cases: `InitChain` + a valid finalized inbox batch account, so
/// each test can mutate exactly one field of the args/accounts and assert the specific rejection. The
/// inbox batch account is registered on `pt` **before** `start_with_context` (not injected later via
/// `ctx.set_account`) — a post-start `set_account` account's lamports aren't part of the bank's genesis
/// supply, and `warp_to_slot`'s accounts-hash verification panics on that mismatch (`solana-program-test`
/// gotcha, hit while writing the `FinalizeBatch`/`ClosePending`/`RootView` tests below, all of which
/// warp).
async fn post_root_rig() -> (ProgramTest, Pubkey, Keypair, Chain, [u8; 32]) {
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
    let c = default_chain(settlement_program);
    pt.add_account(c.authority.pubkey(), funded_account());
    let acc = keccak::hashv(&[b"inbox acc 1"]).to_bytes();
    let inbox_pda = sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 1);
    pt.add_account(
        inbox_pda,
        inbox_batch_account(
            c.inbox_program,
            c.chain_id,
            1,
            settlement_program,
            c.authority.pubkey(),
            true,
            acc,
        ),
    );
    (pt, settlement_program, payer, c, acc)
}

/// Registers batch 2's inbox account too (seeds are per-batch) — for tests that post a
/// second batch, added the same pre-start way as batch 1's in [`post_root_rig`].
fn add_batch2_inbox_account(
    pt: &mut ProgramTest,
    c: &Chain,
    settlement_program: Pubkey,
    acc: [u8; 32],
) {
    let inbox_pda = sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 2);
    pt.add_account(
        inbox_pda,
        inbox_batch_account(
            c.inbox_program,
            c.chain_id,
            2,
            settlement_program,
            c.authority.pubkey(),
            true,
            acc,
        ),
    );
}

#[tokio::test]
async fn post_root_rejects_non_authority_signer() {
    let (pt, settlement_program, payer, c, acc) = post_root_rig().await;
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;
    let inbox_pda = sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 1);
    ctx.set_account(
        &inbox_pda,
        &inbox_batch_account(
            c.inbox_program,
            c.chain_id,
            1,
            settlement_program,
            c.authority.pubkey(),
            true,
            acc,
        )
        .into(),
    );

    let not_authority = funded_keypair();
    ctx.set_account(&not_authority.pubkey(), &funded_account().into());
    let args = happy_post_root_args(&c, acc).await;
    let err = send(
        &mut ctx,
        &[sclient::post_root_ix(
            &settlement_program,
            &not_authority.pubkey(),
            &c.inbox_program,
            &c.treasury,
            args,
        )
        .expect("reserved chain")],
        &not_authority,
        &[],
    )
    .await
    .expect_err("non-authority signer must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::NotChainAuthority as u32)
    );
}

#[tokio::test]
async fn post_root_rejects_wrong_prev_batch() {
    let (pt, settlement_program, payer, c, acc) = post_root_rig().await;
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;
    let inbox_pda = sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 1);
    ctx.set_account(
        &inbox_pda,
        &inbox_batch_account(
            c.inbox_program,
            c.chain_id,
            1,
            settlement_program,
            c.authority.pubkey(),
            true,
            acc,
        )
        .into(),
    );

    let mut args = happy_post_root_args(&c, acc).await;
    args.prev_batch = 5;
    let err = send(
        &mut ctx,
        &[sclient::post_root_ix(
            &settlement_program,
            &c.authority.pubkey(),
            &c.inbox_program,
            &c.treasury,
            args,
        )
        .expect("reserved chain")],
        &c.authority,
        &[],
    )
    .await
    .expect_err("wrong prev_batch must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::BadPrevBatch as u32)
    );
}

#[tokio::test]
async fn post_root_rejects_wrong_pre_state_root() {
    let (pt, settlement_program, payer, c, acc) = post_root_rig().await;
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;
    let inbox_pda = sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 1);
    ctx.set_account(
        &inbox_pda,
        &inbox_batch_account(
            c.inbox_program,
            c.chain_id,
            1,
            settlement_program,
            c.authority.pubkey(),
            true,
            acc,
        )
        .into(),
    );

    let mut args = happy_post_root_args(&c, acc).await;
    args.pre_state_root = [0xffu8; 32];
    let err = send(
        &mut ctx,
        &[sclient::post_root_ix(
            &settlement_program,
            &c.authority.pubkey(),
            &c.inbox_program,
            &c.treasury,
            args,
        )
        .expect("reserved chain")],
        &c.authority,
        &[],
    )
    .await
    .expect_err("wrong pre_state_root must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::BadPreStateRoot as u32)
    );
}

#[tokio::test]
async fn post_root_rejects_first_block_gap() {
    let (pt, settlement_program, payer, c, acc) = post_root_rig().await;
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;
    let inbox_pda = sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 1);
    ctx.set_account(
        &inbox_pda,
        &inbox_batch_account(
            c.inbox_program,
            c.chain_id,
            1,
            settlement_program,
            c.authority.pubkey(),
            true,
            acc,
        )
        .into(),
    );

    let mut args = happy_post_root_args(&c, acc).await;
    args.first_block = 5;
    let err = send(
        &mut ctx,
        &[sclient::post_root_ix(
            &settlement_program,
            &c.authority.pubkey(),
            &c.inbox_program,
            &c.treasury,
            args,
        )
        .expect("reserved chain")],
        &c.authority,
        &[],
    )
    .await
    .expect_err("first_block gap must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::BadFirstBlock as u32)
    );
}

#[tokio::test]
async fn post_root_rejects_inbox_batch_not_finalized() {
    let (pt, settlement_program, payer, c, acc) = post_root_rig().await;
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;
    let inbox_pda = sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 1);
    ctx.set_account(
        &inbox_pda,
        &inbox_batch_account(
            c.inbox_program,
            c.chain_id,
            1,
            settlement_program,
            c.authority.pubkey(),
            false,
            acc,
        )
        .into(),
    );

    let args = happy_post_root_args(&c, acc).await;
    let err = send(
        &mut ctx,
        &[sclient::post_root_ix(
            &settlement_program,
            &c.authority.pubkey(),
            &c.inbox_program,
            &c.treasury,
            args,
        )
        .expect("reserved chain")],
        &c.authority,
        &[],
    )
    .await
    .expect_err("unfinalized inbox batch must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::InboxNotFinalized as u32)
    );
}

#[tokio::test]
async fn post_root_rejects_acc_mismatch() {
    let (pt, settlement_program, payer, c, acc) = post_root_rig().await;
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;
    let inbox_pda = sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 1);
    ctx.set_account(
        &inbox_pda,
        &inbox_batch_account(
            c.inbox_program,
            c.chain_id,
            1,
            settlement_program,
            c.authority.pubkey(),
            true,
            acc,
        )
        .into(),
    );

    let mut args = happy_post_root_args(&c, acc).await;
    args.inbox_commitment = [0xabu8; 32];
    let err = send(
        &mut ctx,
        &[sclient::post_root_ix(
            &settlement_program,
            &c.authority.pubkey(),
            &c.inbox_program,
            &c.treasury,
            args,
        )
        .expect("reserved chain")],
        &c.authority,
        &[],
    )
    .await
    .expect_err("acc mismatch must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::AccMismatch as u32)
    );
}

#[tokio::test]
async fn post_root_rejects_wrong_inbox_owner() {
    let (pt, settlement_program, payer, c, acc) = post_root_rig().await;
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;
    let inbox_pda = sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 1);
    // wrong owner: not the registered inbox program
    ctx.set_account(
        &inbox_pda,
        &inbox_batch_account(
            Pubkey::new_unique(),
            c.chain_id,
            1,
            settlement_program,
            c.authority.pubkey(),
            true,
            acc,
        )
        .into(),
    );

    let args = happy_post_root_args(&c, acc).await;
    let err = send(
        &mut ctx,
        &[sclient::post_root_ix(
            &settlement_program,
            &c.authority.pubkey(),
            &c.inbox_program,
            &c.treasury,
            args,
        )
        .expect("reserved chain")],
        &c.authority,
        &[],
    )
    .await
    .expect_err("wrong-owner inbox account must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::WrongInboxAccount as u32)
    );
}

/// There is no migration of v1 inbox accounts: `PostRoot` reads the inbox batch account's header
/// (`settle.rs`'s `post_root`, `rome_zk_layouts::batch::read`) to check `chain_id`/`batch` — a v1-shaped
/// account must still be refused, and by the same `SettleError::WrongInboxAccount` the decode-failure branch
/// already names (not merely by owner or seeds, which
/// `post_root_rejects_wrong_inbox_owner`/`post_root_rejects_wrong_inbox_seeds` already cover). Built from the
/// same `inbox_batch_account` fixture the happy path uses — chain_id, batch, `finalized` and `acc` all
/// otherwise valid and matching — with only the version byte flipped to 1, so a decode that silently accepted
/// version 1 would let this batch through as if it had genuinely settled, not merely fail differently. Making
/// `rome-zk-layouts::batch::read` accept version 1 turns this red: the account is long enough (built at the
/// real `account_len_for`) that removing the version check does not merely swap which decode error fires, it
/// makes decode succeed outright.
#[tokio::test]
async fn post_root_rejects_a_v1_shaped_inbox_batch_account() {
    let (pt, settlement_program, payer, c, acc) = post_root_rig().await;
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;
    let inbox_pda = sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 1);
    let mut v1_account = inbox_batch_account(
        c.inbox_program,
        c.chain_id,
        1,
        settlement_program,
        c.authority.pubkey(),
        true,
        acc,
    );
    v1_account.data[4] = 1; // version 1, everything else (still the real account_len_for) unchanged
    ctx.set_account(&inbox_pda, &v1_account.into());

    let args = happy_post_root_args(&c, acc).await;
    let err = send(
        &mut ctx,
        &[sclient::post_root_ix(
            &settlement_program,
            &c.authority.pubkey(),
            &c.inbox_program,
            &c.treasury,
            args,
        )
        .expect("reserved chain")],
        &c.authority,
        &[],
    )
    .await
    .expect_err("a v1-shaped inbox batch account must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::WrongInboxAccount as u32),
        "must be refused by the same WrongInboxAccount the decode-failure branch (settle.rs:231) names"
    );
}

#[tokio::test]
async fn post_root_rejects_wrong_inbox_seeds() {
    // A fresh chain, not `post_root_rig`'s (which pre-registers a valid batch-1 inbox account at the
    // CORRECT pda) — here the correct pda for batch 1 has no account at all; only the WRONG one (batch
    // 2's pda) does, so this specifically exercises the seeds mismatch this test names.
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
    let mut c = default_chain(settlement_program);
    c.chain_id = 99;
    pt.add_account(c.authority.pubkey(), funded_account());
    let acc = keccak::hashv(&[b"inbox acc 1"]).to_bytes();
    let wrong_pda = sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 2);
    pt.add_account(
        wrong_pda,
        inbox_batch_account(
            c.inbox_program,
            c.chain_id,
            1,
            settlement_program,
            c.authority.pubkey(),
            true,
            acc,
        ),
    );
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;

    let args = happy_post_root_args(&c, acc).await;
    let ix = sclient::post_root_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &c.inbox_program,
        &c.treasury,
        args,
    )
    .expect("reserved chain");
    let err = send(&mut ctx, &[ix], &c.authority, &[])
        .await
        .expect_err("missing inbox account at the derived seeds must be rejected");
    // account doesn't exist at the derived PDA at all -> the runtime rejects it before our program runs
    assert!(matches!(err, TransactionError::InstructionError(_, _)));
}

#[tokio::test]
async fn post_root_rejects_max_pending_reached() {
    let (pt, settlement_program, payer, mut c, _acc) = post_root_rig().await;
    c.chain_id = 2;
    let mut ctx = pt.start_with_context().await;
    // InitChain with max_pending = 0 so the very first PostRoot already hits the cap.
    init_chain_with_max_pending(&mut ctx, &payer, &c, 0).await;

    let acc = keccak::hashv(&[b"acc"]).to_bytes();
    let inbox_pda = sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 1);
    ctx.set_account(
        &inbox_pda,
        &inbox_batch_account(
            c.inbox_program,
            c.chain_id,
            1,
            settlement_program,
            c.authority.pubkey(),
            true,
            acc,
        )
        .into(),
    );
    let args = happy_post_root_args(&c, acc).await;
    let err = send(
        &mut ctx,
        &[sclient::post_root_ix(
            &settlement_program,
            &c.authority.pubkey(),
            &c.inbox_program,
            &c.treasury,
            args,
        )
        .expect("reserved chain")],
        &c.authority,
        &[],
    )
    .await
    .expect_err("max_pending == 0 must reject the first PostRoot");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::MaxPendingReached as u32)
    );
}

// ---------------------------------------------------------------------------------------------
// (4) FinalizeBatch
// ---------------------------------------------------------------------------------------------

/// Posts batch 1 against the inbox account `post_root_rig` already registered on `pt` before
/// `start_with_context` (see that function's doc: injecting it here instead, via `ctx.set_account`,
/// breaks a later `warp_to_slot` in the caller).
async fn post_first_batch(
    ctx: &mut solana_program_test::ProgramTestContext,
    settlement_program: Pubkey,
    c: &Chain,
    acc: [u8; 32],
) -> [u8; 32] {
    let args = happy_post_root_args(c, acc).await;
    send(
        ctx,
        &[sclient::post_root_ix(
            &settlement_program,
            &c.authority.pubkey(),
            &c.inbox_program,
            &c.treasury,
            args.clone(),
        )
        .expect("reserved chain")],
        &c.authority,
        &[],
    )
    .await
    .expect("PostRoot should succeed");
    args.state_root
}

#[tokio::test]
async fn finalize_batch_before_deadline_is_rejected() {
    let (pt, settlement_program, payer, c, acc) = post_root_rig().await;
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;
    post_first_batch(&mut ctx, settlement_program, &c, acc).await;

    let err = send(
        &mut ctx,
        &[sclient::finalize_batch_ix(
            &settlement_program,
            c.chain_id,
            1,
            &[],
        )],
        &payer,
        &[],
    )
    .await
    .expect_err("FinalizeBatch before the deadline must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::BeforeDeadline as u32)
    );
}

#[tokio::test]
async fn finalize_batch_after_deadline_advances_root() {
    let (pt, settlement_program, payer, c, acc) = post_root_rig().await;
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;
    let state_root = post_first_batch(&mut ctx, settlement_program, &c, acc).await;

    let now = ctx.banks_client.get_root_slot().await.unwrap();
    ctx.warp_to_slot(now + c.challenge_window_slots as u64 + 2)
        .unwrap();
    let cu = send(
        &mut ctx,
        &[sclient::finalize_batch_ix(
            &settlement_program,
            c.chain_id,
            1,
            &[],
        )],
        &payer,
        &[],
    )
    .await
    .expect("FinalizeBatch after the deadline should succeed");
    eprintln!("FinalizeBatch consumed {cu} CU");

    let (root_pda, _) = sclient::root_pda(&settlement_program, c.chain_id);
    let root = sclient::decode_root_account(
        &ctx.banks_client
            .get_account(root_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap();
    assert_eq!(root.head_final_batch, 1);
    assert_eq!(root.number, 1);
    assert_eq!(root.state_root, state_root);
    // The window-elapsed path must write a real, batch-consistent parent_hash/block_hash pair (from
    // the pending PDA's own claimed fields), not leave them stale.
    assert_eq!(
        root.parent_hash,
        keccak::hashv(&[b"parent hash 1"]).to_bytes()
    );
    assert_eq!(
        root.block_hash,
        keccak::hashv(&[b"block hash 1"]).to_bytes()
    );

    let (pending_pda, _) = sclient::pending_pda(&settlement_program, c.chain_id, 1);
    let pending = sclient::decode_pending_account(
        &ctx.banks_client
            .get_account(pending_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap();
    assert_eq!(pending.status, rome_zk_layouts::pending::STATUS_FINAL);
}

/// A batch created `Final` immediately (`PostRootProved`) must never increment `pending_count` — it was
/// never `Pending`. Uses `PostRoot` (unproved) for batch 1 so the chain has a real predecessor, then a
/// synthetic write of batch 2 directly as `Final` (bypassing the expensive real proof — this test is
/// about the counter, not the proof path) to check the invariant in isolation.
#[tokio::test]
async fn pending_count_does_not_increase_for_a_batch_created_final() {
    let (pt, settlement_program, payer, c, acc) = post_root_rig().await;
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;
    post_first_batch(&mut ctx, settlement_program, &c, acc).await;

    let (root_pda, _) = sclient::root_pda(&settlement_program, c.chain_id);
    let after_one_pending = sclient::decode_root_account(
        &ctx.banks_client
            .get_account(root_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap();
    assert_eq!(
        after_one_pending.pending_count, 1,
        "one PostRoot -> pending_count == 1"
    );

    // Fabricate a batch-2 pending PDA directly with STATUS_FINAL (as PostRootProved would leave it)
    // without touching pending_count, then assert the counter is unaffected by its mere existence.
    let (pending2_pda, _) = sclient::pending_pda(&settlement_program, c.chain_id, 2);
    let mut d = vec![0u8; rome_zk_layouts::pending::PENDING_LEN];
    d[rome_zk_layouts::pending::OFF_BATCH..rome_zk_layouts::pending::OFF_BATCH + 8]
        .copy_from_slice(&2u64.to_le_bytes());
    d[rome_zk_layouts::pending::OFF_STATUS] = rome_zk_layouts::pending::STATUS_FINAL;
    ctx.set_account(
        &pending2_pda,
        &Account {
            lamports: rent_exempt(d.len()),
            data: d,
            owner: settlement_program,
            executable: false,
            rent_epoch: 0,
        }
        .into(),
    );
    let still = sclient::decode_root_account(
        &ctx.banks_client
            .get_account(root_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap();
    assert_eq!(
        still.pending_count, 1,
        "a batch that exists as Final from birth must never have been counted as pending"
    );
}

/// End to end. Batch 1 is posted via `PostRoot` (window-based, still `Pending`); batch 2 is fabricated
/// directly as already `Final` (standing in for a batch that was posted `Final` immediately via
/// `PostRootProved` while batch 1 was still pending — allowed because "continuity binds to the immediate
/// predecessor batch, final or not"; a hand-built account keeps this test about the finality
/// bookkeeping, not the proof path). Before this fix, `finalize_batch(1)` alone would leave
/// `head_final_batch` stuck at 1 forever, since `finalize_batch(2)` itself fails `NotPending` (batch 2
/// was never `Pending`). Finalizing batch 1 with batch 2's pending PDA passed as a trailing "walk"
/// account must advance `head_final_batch` straight to 2, write batch 2's own tuple into the root, and
/// leave `pending_count == 0`. `RootView` for batch 1 (now behind the head) must still return batch 1's
/// own tuple, and `RootView(3)` must fail (nothing posted that far).
#[tokio::test]
async fn finalize_batch_walks_past_an_already_final_successor_and_root_view_is_per_batch() {
    let (mut pt, settlement_program, payer, c, acc) = post_root_rig().await;

    // Batch 2's pending PDA, fabricated already `Final` — registered on `pt` *before*
    // `start_with_context` (post_root_rig's own doc: a post-start `ctx.set_account`'s lamports aren't
    // part of genesis supply, and this test's `warp_to_slot` would panic on the accounts-hash mismatch).
    let batch2_last_block = 2u64;
    let batch2_state_root = keccak::hashv(&[b"state root 2"]).to_bytes();
    let batch2_parent_hash = keccak::hashv(&[b"parent hash 2"]).to_bytes();
    let batch2_block_hash = keccak::hashv(&[b"block hash 2"]).to_bytes();
    let (pending2_pda, _) = sclient::pending_pda(&settlement_program, c.chain_id, 2);
    let mut d = vec![0u8; rome_zk_layouts::pending::PENDING_LEN];
    d[rome_zk_layouts::pending::OFF_BATCH..rome_zk_layouts::pending::OFF_BATCH + 8]
        .copy_from_slice(&2u64.to_le_bytes());
    d[rome_zk_layouts::pending::OFF_LAST_BLOCK..rome_zk_layouts::pending::OFF_LAST_BLOCK + 8]
        .copy_from_slice(&batch2_last_block.to_le_bytes());
    d[rome_zk_layouts::pending::OFF_STATE_ROOT..rome_zk_layouts::pending::OFF_STATE_ROOT + 32]
        .copy_from_slice(&batch2_state_root);
    d[rome_zk_layouts::pending::OFF_PARENT_HASH..rome_zk_layouts::pending::OFF_PARENT_HASH + 32]
        .copy_from_slice(&batch2_parent_hash);
    d[rome_zk_layouts::pending::OFF_LAST_BLOCK_HASH
        ..rome_zk_layouts::pending::OFF_LAST_BLOCK_HASH + 32]
        .copy_from_slice(&batch2_block_hash);
    d[rome_zk_layouts::pending::OFF_STATUS] = rome_zk_layouts::pending::STATUS_FINAL;
    pt.add_account(
        pending2_pda,
        Account {
            lamports: rent_exempt(d.len()),
            data: d,
            owner: settlement_program,
            executable: false,
            rent_epoch: 0,
        },
    );

    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;
    post_first_batch(&mut ctx, settlement_program, &c, acc).await;

    let now = ctx.banks_client.get_root_slot().await.unwrap();
    ctx.warp_to_slot(now + c.challenge_window_slots as u64 + 2)
        .unwrap();
    send(
        &mut ctx,
        &[sclient::finalize_batch_ix(
            &settlement_program,
            c.chain_id,
            1,
            &[2],
        )],
        &payer,
        &[],
    )
    .await
    .expect("FinalizeBatch(1) with a walk past the already-final batch 2 should succeed");

    let (root_pda, _) = sclient::root_pda(&settlement_program, c.chain_id);
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
        root.head_final_batch, 2,
        "the walk must advance past the already-final batch 2, not stop at batch 1"
    );
    assert_eq!(root.number, batch2_last_block);
    assert_eq!(root.state_root, batch2_state_root);
    assert_eq!(root.parent_hash, batch2_parent_hash);
    assert_eq!(root.block_hash, batch2_block_hash);
    assert_eq!(
        root.pending_count, 0,
        "batch 1's PENDING -> FINAL transition decrements once; batch 2 was never counted"
    );

    // RootView(1): behind the head now (head_final_batch == 2) — must return batch 1's OWN tuple, not
    // the root account's current (batch-2) fields.
    let ix1 = sclient::root_view_ix(&settlement_program, c.chain_id, 1);
    let recent = ctx.get_new_latest_blockhash().await.unwrap();
    let tx1 = Transaction::new_signed_with_payer(&[ix1], Some(&payer.pubkey()), &[&payer], recent);
    let sim1 = ctx.banks_client.simulate_transaction(tx1).await.unwrap();
    assert!(sim1.result.unwrap().is_ok(), "RootView(1) must succeed");
    let rd1 = sim1.simulation_details.unwrap().return_data.unwrap();
    let got1: sclient::RootViewData = borsh::from_slice(&rd1.data).unwrap();
    assert_eq!(got1.number, 1, "batch 1's own last_block, not batch 2's");
    assert_eq!(
        got1.state_root,
        keccak::hashv(&[b"state root 1"]).to_bytes()
    );
    assert_eq!(
        got1.parent_hash,
        keccak::hashv(&[b"parent hash 1"]).to_bytes()
    );
    assert_eq!(
        got1.block_hash,
        keccak::hashv(&[b"block hash 1"]).to_bytes()
    );

    // RootView(3): nothing posted that far.
    let ix3 = sclient::root_view_ix(&settlement_program, c.chain_id, 3);
    let err = send(&mut ctx, &[ix3], &payer, &[])
        .await
        .expect_err("RootView(3) must fail — nothing posted that far");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::NotFinal as u32)
    );
}

#[tokio::test]
async fn finalize_batch_out_of_order_is_rejected() {
    let (mut pt, settlement_program, payer, c, acc) = post_root_rig().await;
    let acc2 = keccak::hashv(&[b"inbox acc 2"]).to_bytes();
    add_batch2_inbox_account(&mut pt, &c, settlement_program, acc2);
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;
    post_first_batch(&mut ctx, settlement_program, &c, acc).await;

    // post batch 2 as well, then try to finalize batch 2 before batch 1
    let args2 = sclient::PostRootFields {
        chain_id: c.chain_id,
        batch: 2,
        prev_batch: 1,
        pre_state_root: keccak::hashv(&[b"state root 1"]).to_bytes(),
        first_block: 2,
        last_block: 2,
        state_root: keccak::hashv(&[b"state root 2"]).to_bytes(),
        block_roots_merkle: [0u8; 32],
        inbox_commitment: acc2,
        forced_outcome_commitment: rome_zk_layouts::forced_empty_root(&rome_zk_merkle::keccak256),
        parent_hash: keccak::hashv(&[b"parent hash 2"]).to_bytes(),
        last_block_hash: keccak::hashv(&[b"block hash 2"]).to_bytes(),
        gas_in_batch: 0,
    };
    send(
        &mut ctx,
        &[sclient::post_root_ix(
            &settlement_program,
            &c.authority.pubkey(),
            &c.inbox_program,
            &c.treasury,
            args2,
        )
        .expect("reserved chain")],
        &c.authority,
        &[],
    )
    .await
    .expect("PostRoot batch 2 should succeed");

    let now = ctx.banks_client.get_root_slot().await.unwrap();
    ctx.warp_to_slot(now + c.challenge_window_slots as u64 + 2)
        .unwrap();
    let err = send(
        &mut ctx,
        &[sclient::finalize_batch_ix(
            &settlement_program,
            c.chain_id,
            2,
            &[],
        )],
        &payer,
        &[],
    )
    .await
    .expect_err("finalizing batch 2 before batch 1 must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::OutOfOrderFinality as u32)
    );
}

// ---------------------------------------------------------------------------------------------
// (5) RootView
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn root_view_fails_on_a_pending_batch_and_succeeds_on_a_final_one() {
    let (pt, settlement_program, payer, c, acc) = post_root_rig().await;
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;
    let state_root = post_first_batch(&mut ctx, settlement_program, &c, acc).await;

    let pending_err = send(
        &mut ctx,
        &[sclient::root_view_ix(&settlement_program, c.chain_id, 1)],
        &payer,
        &[],
    )
    .await
    .expect_err("RootView on a pending batch must fail");
    assert_eq!(
        custom_error(&pending_err),
        Some(zk_settlement::errors::SettleError::NotFinal as u32)
    );

    let now = ctx.banks_client.get_root_slot().await.unwrap();
    ctx.warp_to_slot(now + c.challenge_window_slots as u64 + 2)
        .unwrap();
    send(
        &mut ctx,
        &[sclient::finalize_batch_ix(
            &settlement_program,
            c.chain_id,
            1,
            &[],
        )],
        &payer,
        &[],
    )
    .await
    .unwrap();

    let recent = ctx.get_new_latest_blockhash().await.unwrap();
    let ix = sclient::root_view_ix(&settlement_program, c.chain_id, 1);
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer], recent);
    let sim = ctx.banks_client.simulate_transaction(tx).await.unwrap();
    assert!(
        sim.result.unwrap().is_ok(),
        "RootView on a final batch must succeed"
    );
    let return_data = sim.simulation_details.unwrap().return_data.unwrap();
    let got: sclient::RootViewData = borsh::from_slice(&return_data.data).unwrap();
    assert_eq!(got.chain_id, c.chain_id);
    assert_eq!(got.number, 1);
    assert_eq!(got.state_root, state_root);
}

/// Control for the permissionless-genesis refusal (`registration_revenue.rs`,
/// `permissionless_chain_genesis_is_not_a_final_root`): a RESERVED chain keeps its genesis as the final
/// root, so `RootView(0)` on a freshly registered reserved chain succeeds and returns the genesis.
#[tokio::test]
async fn root_view_of_the_genesis_batch_succeeds_on_a_reserved_chain() {
    let (pt, settlement_program, payer, c, _acc) = post_root_rig().await;
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;
    assert!(rome_zk_layouts::chainid::is_reserved(c.chain_id));

    let recent = ctx.get_new_latest_blockhash().await.unwrap();
    let ix = sclient::root_view_ix(&settlement_program, c.chain_id, 0);
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer], recent);
    let sim = ctx.banks_client.simulate_transaction(tx).await.unwrap();
    assert!(
        sim.result.unwrap().is_ok(),
        "a reserved chain's genesis is its final root until a batch is posted"
    );
    let return_data = sim.simulation_details.unwrap().return_data.unwrap();
    let got: sclient::RootViewData = borsh::from_slice(&return_data.data).unwrap();
    assert_eq!(got.number, 0);
    assert_eq!(got.state_root, c.genesis_state_root);
}

// ---------------------------------------------------------------------------------------------
// (5b) PostRootProved binds the proof to its predecessor (continuity)
// ---------------------------------------------------------------------------------------------

/// Posts a layout-1 `PostRootProved` whose proof-bound `parent_hash` is `proof_parent_hash`, onto a
/// chain whose predecessor's last block hash is `predecessor_hash`. Every other binding matches, so the
/// only thing that can refuse it before the pairing is the predecessor check.
///
/// `prior_batch == false`: the predecessor is the chain's genesis (`head_pending_batch == 0`), whose
/// block hash lives in the root account (`InitChainV2`'s `block_hash`). `prior_batch == true`: the
/// predecessor is batch 1's own pending account (`head_pending_batch == 1`), whose `last_block_hash` is
/// `predecessor_hash` — and the root account's own `block_hash` is set to something ELSE, so a check
/// that read the wrong place would show.
async fn layout1_post_over_predecessor(
    predecessor_hash: [u8; 32],
    proof_parent_hash: [u8; 32],
    prior_batch: bool,
) -> (Result<(), TransactionError>, u64) {
    let (pt, settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = 22;
    let mut ctx = pt.start_with_context().await;

    let mut fields = default_init_chain_fields(&c, 16); // max_drift_secs: 60
    fields.registry_entries = layout1_registry_entries();
    fields.block_hash = if prior_batch {
        [0xEEu8; 32]
    } else {
        predecessor_hash
    };
    ensure_global_config(&mut ctx, &payer, &c).await;
    ensure_reserved_allowed(&mut ctx, &payer, &c).await;
    let init_ix = sclient::init_chain_reserved_ix(
        &settlement_program,
        &payer.pubkey(),
        &c.authority.pubkey(),
        &c.registry_authority.pubkey(),
        c.chain_id,
        fields,
    );
    send(
        &mut ctx,
        &[init_ix],
        &payer,
        &[&c.authority, &c.registry_authority],
    )
    .await
    .expect("InitChain should succeed");

    let (batch, pred_last_block, pred_state_root) = if prior_batch {
        (2u64, 10u64, [0x44u8; 32])
    } else {
        (1u64, 0u64, c.genesis_state_root)
    };
    if prior_batch {
        // Batch 1 already posted: the root points at it, and its own pending account carries the
        // predecessor's tuple.
        let (root_pda, _) = sclient::root_pda(&settlement_program, c.chain_id);
        let mut root_acc = ctx
            .banks_client
            .get_account(root_pda)
            .await
            .unwrap()
            .unwrap();
        let mut root = rome_zk_layouts::root::read(&root_acc.data).unwrap();
        root.head_pending_batch = 1;
        root.head_final_batch = 1;
        root.number = pred_last_block;
        let n = rome_zk_layouts::root::MIN_LEN;
        root_acc.data[..n].copy_from_slice(&rome_zk_layouts::root::write(&root));
        ctx.set_account(&root_pda, &root_acc.into());

        use rome_zk_layouts::pending as p;
        let mut d = vec![0u8; p::PENDING_LEN];
        d[p::OFF_BATCH..p::OFF_BATCH + 8].copy_from_slice(&1u64.to_le_bytes());
        d[p::OFF_LAST_BLOCK..p::OFF_LAST_BLOCK + 8].copy_from_slice(&pred_last_block.to_le_bytes());
        d[p::OFF_STATE_ROOT..p::OFF_STATE_ROOT + 32].copy_from_slice(&pred_state_root);
        d[p::OFF_STATUS] = p::STATUS_FINAL;
        d[p::OFF_LAST_BLOCK_HASH..p::OFF_LAST_BLOCK_HASH + 32].copy_from_slice(&predecessor_hash);
        let (pending_pda, _) = sclient::pending_pda(&settlement_program, c.chain_id, 1);
        ctx.set_account(
            &pending_pda,
            &Account {
                lamports: rent_exempt(d.len()),
                data: d,
                owner: settlement_program,
                executable: false,
                rent_epoch: 0,
            }
            .into(),
        );
    }

    let open_unix_ts: i64 = 1_700_000_000;
    let acc = keccak::hashv(&[b"layout1 predecessor acc"]).to_bytes();
    let inbox_pda =
        sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, batch);
    ctx.set_account(
        &inbox_pda,
        &inbox_batch_account_with_open_ts(
            c.inbox_program,
            c.chain_id,
            batch,
            settlement_program,
            c.authority.pubkey(),
            true,
            acc,
            open_unix_ts,
        )
        .into(),
    );

    let first_block = pred_last_block + 1;
    let last_block_hash = [0x22u8; 32];
    let state_root = [0x33u8; 32];
    let forced_outcome_commitment = rome_zk_layouts::forced_empty_root(&rome_zk_merkle::keccak256);
    let gas_used = 21_000u64;
    let pv_bytes = build_public_values_v2(
        c.chain_id,
        first_block,
        first_block,
        open_unix_ts as u64,
        60,
        gas_used,
        proof_parent_hash,
        last_block_hash,
        state_root,
        acc,
        forced_outcome_commitment,
    );
    let proof_abi = synthetic_layout1_proof_abi(14, &pv_bytes);
    let args = sclient::PostRootFields {
        chain_id: c.chain_id,
        batch,
        prev_batch: batch - 1,
        pre_state_root: pred_state_root,
        first_block,
        last_block: first_block,
        state_root,
        block_roots_merkle: [0u8; 32],
        inbox_commitment: acc,
        forced_outcome_commitment,
        parent_hash: proof_parent_hash,
        last_block_hash,
        gas_in_batch: gas_used,
    };
    let ix = sclient::post_root_proved_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &c.inbox_program,
        &c.treasury,
        args,
        proof_abi,
        vec![],
    );
    send_capturing_cu(&mut ctx, &[ix], &c.authority).await
}

/// Continuity, first post: a proof whose bound parent hash is not the chain's genesis block
/// hash chains onto some other history, and is refused by name BEFORE the pairing (cheap) — while the
/// same proof over the matching genesis reaches the pairing (so the refusal is that check and nothing
/// else).
#[tokio::test]
async fn post_root_proved_first_batch_must_chain_to_the_genesis_block_hash() {
    let genesis = [0x77u8; 32];

    let (err, cu) = layout1_post_over_predecessor(genesis, [0x78u8; 32], false).await;
    let err = err.expect_err("a parent hash that is not the genesis block hash must refuse");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::PredecessorHashMismatch as u32)
    );
    assert!(
        cu < 60_000,
        "the continuity refusal must come long before the ~456k CU pairing: got {cu} CU"
    );

    let (err, cu) = layout1_post_over_predecessor(genesis, genesis, false).await;
    let err = err.expect_err("a synthetic proof must still fail the pairing itself");
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::InvalidInstructionData),
        "a chained proof passes the continuity check and reaches the pairing"
    );
    assert!(cu > 400_000, "must reach the pairing: got {cu} CU");
}

/// Continuity, later posts: the predecessor is the previous batch's own pending account,
/// not the root account (which is set to a different block hash here).
#[tokio::test]
async fn post_root_proved_later_batch_must_chain_to_the_predecessors_last_block_hash() {
    let predecessor = [0x55u8; 32];

    let (err, cu) = layout1_post_over_predecessor(predecessor, [0xEEu8; 32], true).await;
    let err = err.expect_err(
        "a parent hash equal to the ROOT's block hash but not the predecessor's must refuse",
    );
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::PredecessorHashMismatch as u32)
    );
    assert!(cu < 60_000, "refused before the pairing: got {cu} CU");

    let (err, cu) = layout1_post_over_predecessor(predecessor, predecessor, true).await;
    let err = err.expect_err("a synthetic proof must still fail the pairing itself");
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::InvalidInstructionData),
        "a chained proof passes the continuity check and reaches the pairing"
    );
    assert!(cu > 400_000, "must reach the pairing: got {cu} CU");
}

/// A layout-2 (header fallback) proof ABI whose committed block hash is `keccak(header)`: the real
/// fixture's proof bytes and vkey, with the publics replaced by the guest's `bincode` commit of that hash
/// (0x20 length prefix, then the 32 hash bytes, one u32 per u64 word). The proof was never generated for
/// these publics, so the pairing itself must still fail — the point is to get a well-formed layout-2 blob
/// past every binding check.
fn synthetic_layout2_proof_abi(header: &[u8]) -> Vec<u8> {
    let j = calldata_fixture(14);
    let mut d = hx(&j["proofBytes"]);
    d.extend(hx(&j["programVK"]));
    d.extend(hx(&j["rootCVadcopFinal"]));
    let mut out = [0u8; 36];
    out[0] = 0x20;
    out[1..33].copy_from_slice(&keccak::hash(header).to_bytes());
    let mut publics = [0u8; 512];
    for i in 0..9 {
        let w = u32::from_le_bytes(out[i * 4..i * 4 + 4].try_into().unwrap()) as u64;
        publics[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
    }
    d.extend_from_slice(&publics);
    assert_eq!(d.len(), 768 + 32 + 32 + 512);
    d
}

/// Posts a layout-2 `PostRootProved` over the chain's genesis (a RESERVED chain, whose registry carries
/// a header-fallback entry for the fixture's vkey). The synthetic header's `parent_hash` is
/// `header_parent_hash`; the chain's genesis block hash is `genesis_hash`. Every other binding matches
/// (number, state root, gas, committed hash == keccak(header) == `last_block_hash`), so the only thing
/// that can refuse it before the pairing is the continuity check.
async fn layout2_post_over_genesis(
    genesis_hash: [u8; 32],
    header_parent_hash: [u8; 32],
) -> (Result<(), TransactionError>, u64) {
    let (pt, settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = 23;
    let mut ctx = pt.start_with_context().await;

    let mut fields = default_init_chain_fields(&c, 16);
    fields.registry_entries = vec![sclient::RegistryEntry {
        curve: reg_layout::CURVE_BN254,
        scheme: reg_layout::SCHEME_PLONK,
        vkey_hash: zisk_program_vk(14),
        layout_id: reg_layout::LAYOUT_HEADER_FALLBACK,
    }];
    fields.block_hash = genesis_hash;
    ensure_global_config(&mut ctx, &payer, &c).await;
    ensure_reserved_allowed(&mut ctx, &payer, &c).await;
    let init_ix = sclient::init_chain_reserved_ix(
        &settlement_program,
        &payer.pubkey(),
        &c.authority.pubkey(),
        &c.registry_authority.pubkey(),
        c.chain_id,
        fields,
    );
    send(
        &mut ctx,
        &[init_ix],
        &payer,
        &[&c.authority, &c.registry_authority],
    )
    .await
    .expect("InitChain should succeed");

    let acc = keccak::hashv(&[b"layout2 predecessor acc"]).to_bytes();
    let inbox_pda = sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 1);
    ctx.set_account(
        &inbox_pda,
        &inbox_batch_account(
            c.inbox_program,
            c.chain_id,
            1,
            settlement_program,
            c.authority.pubkey(),
            true,
            acc,
        )
        .into(),
    );

    let state_root = [0x22u8; 32];
    let header = build_synthetic_header(1, header_parent_hash, state_root);
    let header_hash = keccak::hash(&header).to_bytes();
    let args = sclient::PostRootFields {
        chain_id: c.chain_id,
        batch: 1,
        prev_batch: 0,
        pre_state_root: c.genesis_state_root,
        first_block: 1,
        last_block: 1,
        state_root,
        block_roots_merkle: [0u8; 32],
        inbox_commitment: acc,
        forced_outcome_commitment: rome_zk_layouts::forced_empty_root(&rome_zk_merkle::keccak256),
        parent_hash: header_parent_hash,
        last_block_hash: header_hash,
        gas_in_batch: 0,
    };
    let ix = sclient::post_root_proved_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &c.inbox_program,
        &c.treasury,
        args,
        synthetic_layout2_proof_abi(&header),
        header,
    );
    send_capturing_cu(&mut ctx, &[ix], &c.authority).await
}

/// Continuity, layout 2: the same predecessor check guards the header-fallback layout. A
/// header whose `parent_hash` is not the predecessor's block hash is refused by name before the pairing,
/// while a header that chains reaches the pairing — so the refusal is that check and nothing else.
#[tokio::test]
async fn layout2_post_must_chain_to_the_predecessors_block_hash() {
    let genesis = [0x77u8; 32];

    let (err, cu) = layout2_post_over_genesis(genesis, [0x78u8; 32]).await;
    let err = err.expect_err("a header whose parent is not the genesis block hash must refuse");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::PredecessorHashMismatch as u32)
    );
    assert!(
        cu < 60_000,
        "the continuity refusal must come long before the ~456k CU pairing: got {cu} CU"
    );

    let (err, cu) = layout2_post_over_genesis(genesis, genesis).await;
    let err = err.expect_err("a synthetic proof must still fail the pairing itself");
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::InvalidInstructionData),
        "a chained header passes the continuity check and reaches the pairing"
    );
    assert!(cu > 400_000, "must reach the pairing: got {cu} CU");
}

// ---------------------------------------------------------------------------------------------
// (6) PostRootProved
// ---------------------------------------------------------------------------------------------

/// The `fixtures/s10/block{14,166,169}.calldata.json` proofs carry the guest's already-committed 32-byte
/// block hash, not the source header bytes; reconstructing a header that hashes to that fixed value is a
/// keccak preimage. So this exercises `PostRootProved`'s real wiring (registry lookup, vkey binding, the
/// cheap-checks-first ordering) with the real fixture proof and a synthetic header, and asserts it fails
/// at the header-hash binding, cheaply, before the expensive pairing ever runs. The companion CU
/// measurement runs the same BPF `veritas::verify_zisk` bytecode via that program's own entrypoint
/// instead.
#[tokio::test]
async fn post_root_proved_with_real_fixture_fails_header_binding_before_the_expensive_verify() {
    let (pt, settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = 3;
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;

    let acc = keccak::hashv(&[b"inbox acc"]).to_bytes();
    let inbox_pda = sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 1);
    ctx.set_account(
        &inbox_pda,
        &inbox_batch_account(
            c.inbox_program,
            c.chain_id,
            1,
            settlement_program,
            c.authority.pubkey(),
            true,
            acc,
        )
        .into(),
    );

    let synthetic_header = build_synthetic_header(1, [0x11u8; 32], [0x22u8; 32]);
    let args = sclient::PostRootFields {
        chain_id: c.chain_id,
        batch: 1,
        prev_batch: 0,
        pre_state_root: c.genesis_state_root,
        first_block: 1,
        last_block: 1,
        state_root: [0x22u8; 32],
        block_roots_merkle: [0u8; 32],
        inbox_commitment: acc,
        forced_outcome_commitment: rome_zk_layouts::forced_empty_root(&rome_zk_merkle::keccak256),
        // Matches the synthetic header's own parent_hash so the rejection this test targets happens at
        // the header-hash binding (the next, more expensive check), not the cheaper parent_hash check.
        parent_hash: [0x11u8; 32],
        last_block_hash: [0u8; 32],
        gas_in_batch: 0,
    };
    let ix = sclient::post_root_proved_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &c.inbox_program,
        &c.treasury,
        args,
        zisk_proof_abi(14),
        synthetic_header,
    );
    let (err, cu) = send_capturing_cu(&mut ctx, &[ix], &c.authority).await;
    let err = err.expect_err("a header not matching the fixture's committed hash must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::HeaderHashMismatch as u32)
    );
    eprintln!("PostRootProved (rejected at header binding, before the pairing) consumed {cu} CU");
}

/// The registry carries a DECOY `BN254`/`PLONK` entry — same curve and scheme as the real fixture
/// proof, but a different `vkey_hash` — so a lookup keyed only on `(curve, scheme)` would wrongly
/// accept it; `registry::find` is keyed on `(curve, scheme, vkey_hash)` and must reject. This also
/// proves the cheap-checks-first ordering: the registry miss must be caught before `header_rlp` is ever
/// parsed (passed empty here — parsing it would itself fail) and certainly before the ~442k CU pairing,
/// so the whole rejection stays cheap.
#[tokio::test]
async fn post_root_proved_rejects_unregistered_vkey_cheaply_before_header_or_proof_work() {
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
    let mut c = default_chain(settlement_program);
    c.chain_id = 6;
    pt.add_account(c.authority.pubkey(), funded_account());
    let acc = keccak::hashv(&[b"inbox acc registry test"]).to_bytes();
    let inbox_pda = sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 1);
    pt.add_account(
        inbox_pda,
        inbox_batch_account(
            c.inbox_program,
            c.chain_id,
            1,
            settlement_program,
            c.authority.pubkey(),
            true,
            acc,
        ),
    );
    let mut ctx = pt.start_with_context().await;

    let decoy_vkey = [0x77u8; 32];
    assert_ne!(
        decoy_vkey,
        zisk_program_vk(14),
        "decoy must not accidentally equal the real vkey"
    );
    ensure_global_config(&mut ctx, &payer, &c).await;
    ensure_reserved_allowed(&mut ctx, &payer, &c).await;
    let mut fields = default_init_chain_fields(&c, 16);
    fields.registry_entries = vec![sclient::RegistryEntry {
        curve: reg_layout::CURVE_BN254,
        scheme: reg_layout::SCHEME_PLONK,
        vkey_hash: decoy_vkey,
        layout_id: reg_layout::LAYOUT_HEADER_FALLBACK,
    }];
    let init_ix = sclient::init_chain_reserved_ix(
        &settlement_program,
        &payer.pubkey(),
        &c.authority.pubkey(),
        &c.registry_authority.pubkey(),
        c.chain_id,
        fields,
    );
    send(
        &mut ctx,
        &[init_ix],
        &payer,
        &[&c.authority, &c.registry_authority],
    )
    .await
    .expect("InitChain should succeed");

    let args = sclient::PostRootFields {
        chain_id: c.chain_id,
        batch: 1,
        prev_batch: 0,
        pre_state_root: c.genesis_state_root,
        first_block: 1,
        last_block: 1,
        state_root: [0u8; 32],
        block_roots_merkle: [0u8; 32],
        inbox_commitment: acc,
        forced_outcome_commitment: rome_zk_layouts::forced_empty_root(&rome_zk_merkle::keccak256),
        parent_hash: [0u8; 32],
        last_block_hash: [0u8; 32],
        gas_in_batch: 0,
    };
    let ix = sclient::post_root_proved_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &c.inbox_program,
        &c.treasury,
        args,
        zisk_proof_abi(14),
        vec![], // never reached — the registry miss must reject before header_rlp is parsed
    );
    let (err, cu) = send_capturing_cu(&mut ctx, &[ix], &c.authority).await;
    let err = err.expect_err("an unregistered vkey (decoy sharing curve+scheme) must be rejected");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::RegistryEntryNotFound as u32)
    );
    eprintln!(
        "PostRootProved (rejected: unregistered vkey, before header parse/pairing) consumed {cu} CU"
    );
    // PostRootProved's account list includes chain_config/global_config/ treasury — three more accounts
    // for the runtime to load and deserialize on every call, including this rejection path, before this
    // instruction's own logic ever runs. Budget widened from 25_000 to cover that fixed cost; still
    // nowhere near the header decode + ~442k CU pairing this test exists to prove never runs.
    assert!(
        cu < 40_000,
        "a wrong-vkey rejection must stay cheap (before header decode + the ~442k CU pairing): got {cu} CU"
    );
}

/// Thin adapter over `rome_zk_testkit::send_measuring_cu` — this file's callers want both the pass/fail
/// result and the CU figure (unlike `send` above, which only wants the CU figure on success).
async fn send_capturing_cu(
    ctx: &mut solana_program_test::ProgramTestContext,
    ixs: &[Instruction],
    payer: &Keypair,
) -> (Result<(), TransactionError>, u64) {
    let (result, cu, _logs) = rome_zk_testkit::send_measuring_cu(ctx, ixs, payer, &[]).await;
    (result, cu)
}

fn build_synthetic_header(number: u64, parent_hash: [u8; 32], state_root: [u8; 32]) -> Vec<u8> {
    use alloy_consensus::Header;
    use alloy_primitives::{B256, U256};
    let h = Header {
        number,
        parent_hash: B256::from(parent_hash),
        state_root: B256::from(state_root),
        gas_limit: 30_000_000,
        timestamp: 1_700_000_000,
        base_fee_per_gas: Some(7),
        difficulty: U256::ZERO,
        ..Default::default()
    };
    alloy_rlp::encode(&h)
}

// ---------------------------------------------------------------------------------------------
// (7) PostRootProved layout 1, the v2 public-values struct. The real layout-1 proof
// (`fixtures/prover-input/txv1-dev-reset6-batch-1.plonk.bin`) is exercised by rome-zk-prover's tests, not
// here, so every test here either feeds it the real calldata fixture's (layout-2-shaped) publics or a
// synthetic, hand-built v2 blob paired with the real fixture's proof bytes (mismatched on purpose for
// the "reaches the pairing" test).
// ---------------------------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn build_public_values_v2(
    chain_id: u64,
    first_number: u64,
    last_number: u64,
    open_unix_ts: u64,
    max_drift_secs: u64,
    gas_used: u64,
    parent_hash: [u8; 32],
    last_block_hash: [u8; 32],
    state_root: [u8; 32],
    inbox_commitment: [u8; 32],
    forced_outcome_commitment: [u8; 32],
) -> [u8; rome_zk_layouts::public_values::PUBLIC_VALUES_LEN] {
    use rome_zk_layouts::public_values::*;
    let mut d = [0u8; PUBLIC_VALUES_LEN];
    d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].copy_from_slice(&chain_id.to_le_bytes());
    d[OFF_FIRST_NUMBER..OFF_FIRST_NUMBER + 8].copy_from_slice(&first_number.to_le_bytes());
    d[OFF_LAST_NUMBER..OFF_LAST_NUMBER + 8].copy_from_slice(&last_number.to_le_bytes());
    d[OFF_OPEN_UNIX_TS..OFF_OPEN_UNIX_TS + 8].copy_from_slice(&open_unix_ts.to_le_bytes());
    d[OFF_MAX_DRIFT_SECS..OFF_MAX_DRIFT_SECS + 8].copy_from_slice(&max_drift_secs.to_le_bytes());
    d[OFF_GAS_USED..OFF_GAS_USED + 8].copy_from_slice(&gas_used.to_le_bytes());
    d[OFF_PARENT_HASH..OFF_PARENT_HASH + 32].copy_from_slice(&parent_hash);
    d[OFF_LAST_BLOCK_HASH..OFF_LAST_BLOCK_HASH + 32].copy_from_slice(&last_block_hash);
    d[OFF_STATE_ROOT..OFF_STATE_ROOT + 32].copy_from_slice(&state_root);
    d[OFF_INBOX_COMMITMENT..OFF_INBOX_COMMITMENT + 32].copy_from_slice(&inbox_commitment);
    d[OFF_FORCED_OUTCOME_COMMITMENT..OFF_FORCED_OUTCOME_COMMITMENT + 32]
        .copy_from_slice(&forced_outcome_commitment);
    d
}

/// The real fixture's proof bytes / programVK / rootC, paired with a HAND-BUILT v2 public-values
/// blob instead of the fixture's own (layout-2-shaped) publics — the proof itself was never generated
/// for these publics, so `verify_zisk` must fail once it actually runs; the point of this helper is to
/// get a well-formed layout-1 ABI blob to the pairing, not to produce a valid proof.
fn synthetic_layout1_proof_abi(
    block: u32,
    pv: &[u8; rome_zk_layouts::public_values::PUBLIC_VALUES_LEN],
) -> Vec<u8> {
    let j = calldata_fixture(block);
    let mut d = hx(&j["proofBytes"]);
    d.extend(hx(&j["programVK"]));
    d.extend(hx(&j["rootCVadcopFinal"]));
    let words = rome_zk_layouts::public_values::pack_zisk_outputs(pv);
    for w in words {
        d.extend_from_slice(&w.to_le_bytes());
    }
    assert_eq!(d.len(), 768 + 32 + 32 + 512);
    d
}

fn layout1_registry_entries() -> Vec<sclient::RegistryEntry> {
    vec![sclient::RegistryEntry {
        curve: reg_layout::CURVE_BN254,
        scheme: reg_layout::SCHEME_PLONK,
        vkey_hash: zisk_program_vk(14),
        layout_id: reg_layout::LAYOUT_ZISK_V1,
    }]
}

/// The programVK of the first real proof of the rome batch guest (ZisK 1.2.0-alpha, CPU) on Tiber's batch
/// 3930 (path-dependent ELF `47ef8db8…`) — a real, on-record verifying key, not a placeholder.
/// `SetRegistryEntry`'s own tests register THIS key (the registry rotation instruction is what the real
/// reset uses) rather than reusing the fixture's vkey, which is what `layout1_registry_entries` above
/// still carries.
fn batch_guest_program_vk() -> [u8; 32] {
    hex::decode("44916015a37f85417e8e5c5bd4c8a56498821d3380b3d90874df471a8d12ca91")
        .unwrap()
        .try_into()
        .unwrap()
}

/// Same shape as [`synthetic_layout1_proof_abi`] (the fixture's real proof bytes + rootC, paired with
/// a hand-built v2 publics blob — never a valid proof, `verify_zisk` must fail once it runs), except the
/// 32-byte programVK slot (proof_abi[768..800]) is OVERRIDDEN to `vkey` instead of the fixture's own —
/// so a registry entry registered under `vkey` (via `SetRegistryEntry`, carrying the real vkey) is
/// exactly what `registry::find` looks up, proving the *registered* entry is what let this reach the
/// pairing, not whatever vkey happened to already exist in a fixture.
fn synthetic_layout1_proof_abi_with_vkey(
    block: u32,
    vkey: &[u8; 32],
    pv: &[u8; rome_zk_layouts::public_values::PUBLIC_VALUES_LEN],
) -> Vec<u8> {
    let j = calldata_fixture(block);
    let mut d = hx(&j["proofBytes"]);
    d.extend_from_slice(vkey);
    d.extend(hx(&j["rootCVadcopFinal"]));
    let words = rome_zk_layouts::public_values::pack_zisk_outputs(pv);
    for w in words {
        d.extend_from_slice(&w.to_le_bytes());
    }
    assert_eq!(d.len(), 768 + 32 + 32 + 512);
    d
}

/// The `fixtures/s10/block14.calldata.json` proof carries the header-hash-only guest's publics (bincode
/// `0x20` prefix + a 32-byte hash in words 0-8; measured directly against the fixture file: words 9-63 are
/// all zero). So `unpack_zisk_outputs` decodes it structurally (every word fits a `u32`, every tail word is
/// zero), and the reinterpreted `chain_id` (built from the guest's hash bytes, not a real chain id) then
/// fails `bind_layout1_public_values`'s very first check, cheaply, long before the ~456k CU pairing.
/// CU-measured to prove the pairing never ran.
#[tokio::test]
async fn post_root_proved_layout1_rejects_the_real_header_hash_fixture_before_the_pairing() {
    let (pt, settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = 20;
    let mut ctx = pt.start_with_context().await;

    let mut fields = default_init_chain_fields(&c, 16);
    fields.registry_entries = layout1_registry_entries();
    ensure_global_config(&mut ctx, &payer, &c).await;
    ensure_reserved_allowed(&mut ctx, &payer, &c).await;
    let init_ix = sclient::init_chain_reserved_ix(
        &settlement_program,
        &payer.pubkey(),
        &c.authority.pubkey(),
        &c.registry_authority.pubkey(),
        c.chain_id,
        fields,
    );
    send(
        &mut ctx,
        &[init_ix],
        &payer,
        &[&c.authority, &c.registry_authority],
    )
    .await
    .expect("InitChain should succeed");

    let acc = keccak::hashv(&[b"layout1 inbox acc"]).to_bytes();
    let inbox_pda = sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 1);
    ctx.set_account(
        &inbox_pda,
        &inbox_batch_account(
            c.inbox_program,
            c.chain_id,
            1,
            settlement_program,
            c.authority.pubkey(),
            true,
            acc,
        )
        .into(),
    );

    let args = sclient::PostRootFields {
        chain_id: c.chain_id,
        batch: 1,
        prev_batch: 0,
        pre_state_root: c.genesis_state_root,
        first_block: 1,
        last_block: 1,
        state_root: [0u8; 32],
        block_roots_merkle: [0u8; 32],
        inbox_commitment: acc,
        forced_outcome_commitment: rome_zk_layouts::forced_empty_root(&rome_zk_merkle::keccak256),
        parent_hash: [0u8; 32],
        last_block_hash: [0u8; 32],
        gas_in_batch: 0,
    };
    let ix = sclient::post_root_proved_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &c.inbox_program,
        &c.treasury,
        args,
        zisk_proof_abi(14),
        vec![], // empty header_rlp — required for layout 1
    );
    let (err, cu) = send_capturing_cu(&mut ctx, &[ix], &c.authority).await;
    let err = err
        .expect_err("the real fixture's reinterpreted chain_id must not match args.chain_id (20)");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::PublicValuesChainMismatch as u32),
        "must be refused at the very first layout-1 binding check, not the pairing"
    );
    eprintln!(
        "PostRootProved layout 1 (rejected: PublicValuesChainMismatch, before the pairing) consumed {cu} CU"
    );
    assert!(
        cu < 60_000,
        "must be refused long before the ~456k CU pairing: got {cu} CU"
    );
}

/// A chain still on `chain_config` v1 (never brought forward by `MigrateChainV2`) must refuse layout 1 by
/// name (`DriftBoundUnset`) even when the proof's own publics would otherwise bind cleanly — there is no
/// drift bound on record yet to check them against. CU-measured to prove this happens before the pairing.
#[tokio::test]
async fn post_root_proved_layout1_rejects_a_v1_chain_config_before_the_pairing() {
    let (pt, settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = 21;
    let mut ctx = pt.start_with_context().await;

    let mut fields = default_init_chain_fields(&c, 16);
    fields.registry_entries = layout1_registry_entries();
    ensure_global_config(&mut ctx, &payer, &c).await;
    ensure_reserved_allowed(&mut ctx, &payer, &c).await;
    let init_ix = sclient::init_chain_reserved_ix(
        &settlement_program,
        &payer.pubkey(),
        &c.authority.pubkey(),
        &c.registry_authority.pubkey(),
        c.chain_id,
        fields,
    );
    send(
        &mut ctx,
        &[init_ix],
        &payer,
        &[&c.authority, &c.registry_authority],
    )
    .await
    .expect("InitChain should succeed");

    // Force chain_config back down to v1 bytes (as if this chain predated the drift bound and was never migrated)
    // — every field `InitChain` wrote survives except the drift bound, which a v1 account cannot carry.
    let (cc_pda, _) = sclient::chain_config_pda(&settlement_program, c.chain_id);
    let mut cc_account = ctx
        .banks_client
        .get_account(cc_pda)
        .await
        .unwrap()
        .expect("InitChain must have created chain_config");
    let cc_before = sclient::decode_chain_config_account(&cc_account.data).unwrap();
    let mut v1_bytes = vec![0u8; rome_zk_layouts::chain_config::LEN_V1];
    rome_zk_layouts::chain_config::write(
        &mut v1_bytes,
        &rome_zk_layouts::chain_config::ChainConfigFields {
            chain_id: cc_before.chain_id,
            reserved: cc_before.reserved,
            deposit_lamports: cc_before.deposit_lamports,
            deposit_refunded: cc_before.deposit_refunded,
            registered_slot: cc_before.registered_slot,
            posted_batches: cc_before.posted_batches,
            fee_base_lamports: cc_before.fee_base_lamports,
            fee_bps: cc_before.fee_bps,
            max_drift_secs: None,
        },
    );
    cc_account.data = v1_bytes;
    ctx.set_account(&cc_pda, &cc_account.into());

    let open_unix_ts: i64 = 1_700_000_000;
    let acc = keccak::hashv(&[b"layout1 inbox acc v1"]).to_bytes();
    let inbox_pda = sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 1);
    ctx.set_account(
        &inbox_pda,
        &inbox_batch_account_with_open_ts(
            c.inbox_program,
            c.chain_id,
            1,
            settlement_program,
            c.authority.pubkey(),
            true,
            acc,
            open_unix_ts,
        )
        .into(),
    );

    let parent_hash = GENESIS_BLOCK_HASH;
    let last_block_hash = [0x22u8; 32];
    let state_root = [0x33u8; 32];
    let forced_outcome_commitment = rome_zk_layouts::forced_empty_root(&rome_zk_merkle::keccak256);
    let gas_used = 21_000u64;
    let pv_bytes = build_public_values_v2(
        c.chain_id,
        1,
        1,
        open_unix_ts as u64,
        60,
        gas_used,
        parent_hash,
        last_block_hash,
        state_root,
        acc,
        forced_outcome_commitment,
    );
    let proof_abi = synthetic_layout1_proof_abi(14, &pv_bytes);

    let args = sclient::PostRootFields {
        chain_id: c.chain_id,
        batch: 1,
        prev_batch: 0,
        pre_state_root: c.genesis_state_root,
        first_block: 1,
        last_block: 1,
        state_root,
        block_roots_merkle: [0u8; 32],
        inbox_commitment: acc,
        forced_outcome_commitment,
        parent_hash,
        last_block_hash,
        gas_in_batch: gas_used,
    };
    let ix = sclient::post_root_proved_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &c.inbox_program,
        &c.treasury,
        args,
        proof_abi,
        vec![],
    );
    let (err, cu) = send_capturing_cu(&mut ctx, &[ix], &c.authority).await;
    let err = err.expect_err("a v1 chain_config must refuse layout 1 before the pairing");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::DriftBoundUnset as u32)
    );
    eprintln!(
        "PostRootProved layout 1 (rejected: DriftBoundUnset, before the pairing) consumed {cu} CU"
    );
    assert!(
        cu < 60_000,
        "must be refused long before the ~456k CU pairing: got {cu} CU"
    );
}

/// The layout-1 "`header_rlp` must be empty" guard had no test. Layout 1 has exactly one decoder path
/// (the committed public values); a caller supplying an RLP header is refused before unpack, binding
/// or pairing. Mutation: delete the guard → red.
#[tokio::test]
async fn post_root_proved_layout1_refuses_a_non_empty_header_rlp_before_the_pairing() {
    let (pt, settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = 25;
    let mut ctx = pt.start_with_context().await;

    let mut fields = default_init_chain_fields(&c, 16);
    fields.registry_entries = layout1_registry_entries();
    ensure_global_config(&mut ctx, &payer, &c).await;
    ensure_reserved_allowed(&mut ctx, &payer, &c).await;
    let init_ix = sclient::init_chain_reserved_ix(
        &settlement_program,
        &payer.pubkey(),
        &c.authority.pubkey(),
        &c.registry_authority.pubkey(),
        c.chain_id,
        fields,
    );
    send(
        &mut ctx,
        &[init_ix],
        &payer,
        &[&c.authority, &c.registry_authority],
    )
    .await
    .expect("InitChain should succeed");

    let open_unix_ts: i64 = 1_700_000_000;
    let acc = keccak::hashv(&[b"layout1 inbox acc v1"]).to_bytes();
    let inbox_pda = sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 1);
    ctx.set_account(
        &inbox_pda,
        &inbox_batch_account_with_open_ts(
            c.inbox_program,
            c.chain_id,
            1,
            settlement_program,
            c.authority.pubkey(),
            true,
            acc,
            open_unix_ts,
        )
        .into(),
    );

    let parent_hash = GENESIS_BLOCK_HASH;
    let last_block_hash = [0x22u8; 32];
    let state_root = [0x33u8; 32];
    let forced_outcome_commitment = rome_zk_layouts::forced_empty_root(&rome_zk_merkle::keccak256);
    let gas_used = 21_000u64;
    let pv_bytes = build_public_values_v2(
        c.chain_id,
        1,
        1,
        open_unix_ts as u64,
        60,
        gas_used,
        parent_hash,
        last_block_hash,
        state_root,
        acc,
        forced_outcome_commitment,
    );
    let proof_abi = synthetic_layout1_proof_abi(14, &pv_bytes);

    let args = sclient::PostRootFields {
        chain_id: c.chain_id,
        batch: 1,
        prev_batch: 0,
        pre_state_root: c.genesis_state_root,
        first_block: 1,
        last_block: 1,
        state_root,
        block_roots_merkle: [0u8; 32],
        inbox_commitment: acc,
        forced_outcome_commitment,
        parent_hash,
        last_block_hash,
        gas_in_batch: gas_used,
    };
    let ix = sclient::post_root_proved_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &c.inbox_program,
        &c.treasury,
        args,
        proof_abi,
        vec![0xc0], // any non-empty RLP: layout 1 has no header decoder path
    );
    let (err, cu) = send_capturing_cu(&mut ctx, &[ix], &c.authority).await;
    let err = err.expect_err("layout 1 must refuse a non-empty header_rlp before the pairing");
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::InvalidInstructionData),
        "no second decoder path under layout 1"
    );
    eprintln!("PostRootProved layout 1 (rejected: non-empty header_rlp, before the pairing) consumed {cu} CU");
    assert!(
        cu < 60_000,
        "must be refused long before the ~456k CU pairing: got {cu} CU"
    );
}

/// Every layout-1 binding matches (chain_id, block range, both hashes, state root, both
/// commitments, `open_unix_ts`, `max_drift_secs`, `gas_used`) against a v2 `chain_config` — so the
/// rejection must come from INSIDE `verify_zisk` (the synthetic publics don't match the real fixture's
/// proof bytes), not from any binding name. This is the CU-measured proof that layout 1 actually reaches
/// the pairing when every cheap check passes.
#[tokio::test]
async fn post_root_proved_layout1_with_matching_bindings_reaches_the_pairing() {
    let (pt, settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = 22;
    let mut ctx = pt.start_with_context().await;

    let mut fields = default_init_chain_fields(&c, 16); // max_drift_secs: 60
    fields.registry_entries = layout1_registry_entries();
    ensure_global_config(&mut ctx, &payer, &c).await;
    ensure_reserved_allowed(&mut ctx, &payer, &c).await;
    let init_ix = sclient::init_chain_reserved_ix(
        &settlement_program,
        &payer.pubkey(),
        &c.authority.pubkey(),
        &c.registry_authority.pubkey(),
        c.chain_id,
        fields,
    );
    send(
        &mut ctx,
        &[init_ix],
        &payer,
        &[&c.authority, &c.registry_authority],
    )
    .await
    .expect("InitChain should succeed");

    let open_unix_ts: i64 = 1_700_000_000;
    let acc = keccak::hashv(&[b"layout1 inbox acc v2"]).to_bytes();
    let inbox_pda = sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 1);
    ctx.set_account(
        &inbox_pda,
        &inbox_batch_account_with_open_ts(
            c.inbox_program,
            c.chain_id,
            1,
            settlement_program,
            c.authority.pubkey(),
            true,
            acc,
            open_unix_ts,
        )
        .into(),
    );

    let parent_hash = GENESIS_BLOCK_HASH;
    let last_block_hash = [0x22u8; 32];
    let state_root = [0x33u8; 32];
    let forced_outcome_commitment = rome_zk_layouts::forced_empty_root(&rome_zk_merkle::keccak256);
    let gas_used = 21_000u64;
    let pv_bytes = build_public_values_v2(
        c.chain_id,
        1,
        1,
        open_unix_ts as u64,
        60, // matches InitChain's max_drift_secs above
        gas_used,
        parent_hash,
        last_block_hash,
        state_root,
        acc,
        forced_outcome_commitment,
    );
    let proof_abi = synthetic_layout1_proof_abi(14, &pv_bytes);

    let args = sclient::PostRootFields {
        chain_id: c.chain_id,
        batch: 1,
        prev_batch: 0,
        pre_state_root: c.genesis_state_root,
        first_block: 1,
        last_block: 1,
        state_root,
        block_roots_merkle: [0u8; 32],
        inbox_commitment: acc,
        forced_outcome_commitment,
        parent_hash,
        last_block_hash,
        gas_in_batch: gas_used,
    };
    let ix = sclient::post_root_proved_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &c.inbox_program,
        &c.treasury,
        args,
        proof_abi,
        vec![],
    );
    let (err, cu) = send_capturing_cu(&mut ctx, &[ix], &c.authority).await;
    let err = err.expect_err(
        "a synthetic (unproved) public-values blob must fail the pairing, not any binding check",
    );
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::InvalidInstructionData),
        "the pairing's own failure is a plain InvalidInstructionData, not a SettleError::Custom"
    );
    assert_eq!(
        custom_error(&err),
        None,
        "must NOT be one of this program's named binding errors — every one of those was satisfied"
    );
    eprintln!(
        "PostRootProved layout 1 (every binding matched, failed inside verify_zisk) consumed {cu} CU"
    );
    assert!(
        cu > 400_000,
        "reaching the real ~442k-CU pairing must show up as real CU, not a cheap binding rejection: got {cu} CU"
    );
}

/// A 512-byte publics blob that is not a v2 packing (a non-zero tail word) is refused under its OWN
/// name (`BadPublicValuesPacking`) before the pairing — never a bare `InvalidInstructionData`, which is
/// also what the pairing itself returns and would make the two indistinguishable in a refusal log.
#[tokio::test]
async fn post_root_proved_layout1_refuses_a_malformed_zisk_packing_by_name_before_the_pairing() {
    let (pt, settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = 24;
    let mut ctx = pt.start_with_context().await;

    let mut fields = default_init_chain_fields(&c, 16); // max_drift_secs: 60
    fields.registry_entries = layout1_registry_entries();
    ensure_global_config(&mut ctx, &payer, &c).await;
    ensure_reserved_allowed(&mut ctx, &payer, &c).await;
    let init_ix = sclient::init_chain_reserved_ix(
        &settlement_program,
        &payer.pubkey(),
        &c.authority.pubkey(),
        &c.registry_authority.pubkey(),
        c.chain_id,
        fields,
    );
    send(
        &mut ctx,
        &[init_ix],
        &payer,
        &[&c.authority, &c.registry_authority],
    )
    .await
    .expect("InitChain should succeed");

    let open_unix_ts: i64 = 1_700_000_000;
    let acc = keccak::hashv(&[b"layout1 inbox acc v2"]).to_bytes();
    let inbox_pda = sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 1);
    ctx.set_account(
        &inbox_pda,
        &inbox_batch_account_with_open_ts(
            c.inbox_program,
            c.chain_id,
            1,
            settlement_program,
            c.authority.pubkey(),
            true,
            acc,
            open_unix_ts,
        )
        .into(),
    );

    let parent_hash = GENESIS_BLOCK_HASH;
    let last_block_hash = [0x22u8; 32];
    let state_root = [0x33u8; 32];
    let forced_outcome_commitment = rome_zk_layouts::forced_empty_root(&rome_zk_merkle::keccak256);
    let gas_used = 21_000u64;
    let pv_bytes = build_public_values_v2(
        c.chain_id,
        1,
        1,
        open_unix_ts as u64,
        60, // matches InitChain's max_drift_secs above
        gas_used,
        parent_hash,
        last_block_hash,
        state_root,
        acc,
        forced_outcome_commitment,
    );
    let proof_abi = synthetic_layout1_proof_abi(14, &pv_bytes);
    // Word 63 (a tail word the v2 struct never uses) made non-zero: no guest can commit this.
    let mut proof_abi = proof_abi;
    proof_abi[832 + 63 * 8..832 + 64 * 8].copy_from_slice(&7u64.to_le_bytes());

    let args = sclient::PostRootFields {
        chain_id: c.chain_id,
        batch: 1,
        prev_batch: 0,
        pre_state_root: c.genesis_state_root,
        first_block: 1,
        last_block: 1,
        state_root,
        block_roots_merkle: [0u8; 32],
        inbox_commitment: acc,
        forced_outcome_commitment,
        parent_hash,
        last_block_hash,
        gas_in_batch: gas_used,
    };
    let ix = sclient::post_root_proved_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &c.inbox_program,
        &c.treasury,
        args,
        proof_abi,
        vec![],
    );
    let (err, cu) = send_capturing_cu(&mut ctx, &[ix], &c.authority).await;
    let err = err.expect_err("a malformed ZisK packing must be refused before the pairing");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::BadPublicValuesPacking as u32),
        "refused under the packing's own name, not a bare InvalidInstructionData"
    );
    eprintln!("PostRootProved layout 1 (rejected: BadPublicValuesPacking, before the pairing) consumed {cu} CU");
    assert!(
        cu < 60_000,
        "must be refused long before the ~456k CU pairing: got {cu} CU"
    );
}

// ---------------------------------------------------------------------------------------------
// (8) SetRegistryEntry (25) — verifier-key rotation with an activation delay.
// Every test here starts a chain with ONLY the default layout-2 entries (no layout-1 entry at
// `InitChainV2` time) and registers layout 1 afterward via `SetRegistryEntry` — this instruction is the
// production rotation path (Tiber's own layout-1 entry lands once, at reset #5, via `InitChainV2`; every
// rotation after that goes through here).
// ---------------------------------------------------------------------------------------------

/// The end-to-end shape: `SetRegistryEntry` appends a layout-1 entry carrying the
/// REAL programVK to a chain initialised with only the layout-2 entries, and a `PostRootProved` under
/// layout 1 then reaches the pairing (fails inside `verify_zisk` — the fixture's proof bytes were
/// never generated for that vkey — but only AFTER the registry lookup succeeds, proving the
/// `SetRegistryEntry`-appended entry, not some pre-existing one, is what let it through).
#[tokio::test]
async fn set_registry_entry_appends_layout1_entry_and_post_root_proved_reaches_the_pairing() {
    let (pt, settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = 40;
    let mut ctx = pt.start_with_context().await;

    // InitChain with the unmodified default fields: two layout-2 entries, no layout-1 entry exists yet.
    init_chain(&mut ctx, &payer, &c).await;

    let (registry_pda, _) = sclient::registry_pda(&settlement_program, c.chain_id);
    let before = ctx
        .banks_client
        .get_account(registry_pda)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        before.data.len(),
        reg_layout::REGISTRY_LEN,
        "still v1-length before any rotation"
    );
    let before_decoded = sclient::decode_registry_account(&before.data).unwrap();
    assert_eq!(before_decoded.count, 2);

    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let batch_guest_vkey = batch_guest_program_vk();
    let entry = sclient::RegistryEntry {
        curve: reg_layout::CURVE_BN254,
        scheme: reg_layout::SCHEME_PLONK,
        vkey_hash: batch_guest_vkey,
        layout_id: reg_layout::LAYOUT_ZISK_V1,
    };
    let set_ix = sclient::set_registry_entry_ix(
        &settlement_program,
        &c.registry_authority.pubkey(),
        &payer.pubkey(),
        c.chain_id,
        entry,
        now, // immediate activation
    );
    send(&mut ctx, &[set_ix], &payer, &[&c.registry_authority])
        .await
        .expect("SetRegistryEntry should succeed");

    let after = ctx
        .banks_client
        .get_account(registry_pda)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after.data.len(),
        reg_layout::REGISTRY_LEN_V2,
        "grown to v2 on first rotation"
    );
    let after_decoded = sclient::decode_registry_account(&after.data).unwrap();
    assert_eq!(
        after_decoded.count, 3,
        "appended, not replaced — layout_id differs from both existing entries"
    );
    assert_eq!(after_decoded.entries[2].vkey_hash, batch_guest_vkey);
    assert_eq!(
        after_decoded.entries[2].layout_id,
        reg_layout::LAYOUT_ZISK_V1
    );
    assert_eq!(after_decoded.entries[2].activation_slot, now);
    // the two pre-existing layout-2 entries (including the one sharing (curve, scheme) with the new
    // layout-1 entry) are byte-for-byte untouched — `SetRegistryEntry` matches on the vkey itself, not
    // curve/scheme (or curve/scheme/layout_id) alone (mutation: drop `vkey_hash` from the
    // match condition -> this goes red, count would stay 2 and an existing entry would be clobbered
    // instead of appending a third).
    assert_eq!(after_decoded.entries[0], before_decoded.entries[0]);
    assert_eq!(after_decoded.entries[1], before_decoded.entries[1]);

    let open_unix_ts: i64 = 1_700_000_000;
    let acc = keccak::hashv(&[b"set_registry_entry layout1 inbox acc"]).to_bytes();
    let inbox_pda = sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 1);
    ctx.set_account(
        &inbox_pda,
        &inbox_batch_account_with_open_ts(
            c.inbox_program,
            c.chain_id,
            1,
            settlement_program,
            c.authority.pubkey(),
            true,
            acc,
            open_unix_ts,
        )
        .into(),
    );

    let parent_hash = GENESIS_BLOCK_HASH;
    let last_block_hash = [0x22u8; 32];
    let state_root = [0x33u8; 32];
    let forced_outcome_commitment = rome_zk_layouts::forced_empty_root(&rome_zk_merkle::keccak256);
    let gas_used = 21_000u64;
    let pv_bytes = build_public_values_v2(
        c.chain_id,
        1,
        1,
        open_unix_ts as u64,
        60, // matches InitChain's default max_drift_secs
        gas_used,
        parent_hash,
        last_block_hash,
        state_root,
        acc,
        forced_outcome_commitment,
    );
    let proof_abi = synthetic_layout1_proof_abi_with_vkey(14, &batch_guest_vkey, &pv_bytes);

    let args = sclient::PostRootFields {
        chain_id: c.chain_id,
        batch: 1,
        prev_batch: 0,
        pre_state_root: c.genesis_state_root,
        first_block: 1,
        last_block: 1,
        state_root,
        block_roots_merkle: [0u8; 32],
        inbox_commitment: acc,
        forced_outcome_commitment,
        parent_hash,
        last_block_hash,
        gas_in_batch: gas_used,
    };
    let ix = sclient::post_root_proved_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &c.inbox_program,
        &c.treasury,
        args,
        proof_abi,
        vec![],
    );
    let (err, cu) = send_capturing_cu(&mut ctx, &[ix], &c.authority).await;
    let err = err.expect_err(
        "a synthetic (unproved) public-values blob must fail the pairing, not any binding check",
    );
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::InvalidInstructionData),
        "the pairing's own failure is a plain InvalidInstructionData, not a SettleError::Custom"
    );
    assert_eq!(
        custom_error(&err),
        None,
        "must NOT be RegistryEntryNotFound or any other named binding error — the SetRegistryEntry-appended entry must have been found"
    );
    eprintln!(
        "PostRootProved layout 1 via a SetRegistryEntry-appended entry (real batch-guest vkey) consumed {cu} CU"
    );
    assert!(
        cu > 400_000,
        "reaching the real ~456k-CU pairing must show up as real CU, not a cheap RegistryEntryNotFound: got {cu} CU"
    );
}

/// The whole flow on a PERMISSIONLESS chain: it starts with an empty registry, so a
/// proved root has no verifier key to be checked against and is refused by name, cheaply; once the
/// registry authority adds the layout-1 key with `SetRegistryEntry` (first entry, index 0), the very same
/// submission is found and runs into the real pairing (the blob is unproved, so the pairing is where it
/// ends, exactly as in the reserved-chain tests above).
#[tokio::test]
async fn permissionless_chain_is_inert_until_the_registry_authority_registers_its_layout1_vkey() {
    let (pt, settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = sclient::derive_permissionless_chain_id(&c.authority.pubkey(), 0);
    let mut ctx = pt.start_with_context().await;

    ensure_global_config(&mut ctx, &payer, &c).await;
    let mut fields = default_init_chain_fields(&c, 16);
    fields.registry_entries = vec![];
    let init_ix = sclient::init_chain_permissionless_ix(
        &settlement_program,
        &payer.pubkey(),
        &c.authority.pubkey(),
        c.chain_id,
        0,
        fields,
    );
    send(&mut ctx, &[init_ix], &payer, &[&c.authority])
        .await
        .expect("a permissionless InitChain with an empty registry should succeed");

    let (registry_pda, _) = sclient::registry_pda(&settlement_program, c.chain_id);
    let before = sclient::decode_registry_account(
        &ctx.banks_client
            .get_account(registry_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap();
    assert_eq!(
        before.count, 0,
        "a permissionless chain starts with no verifier keys"
    );

    let open_unix_ts: i64 = 1_700_000_000;
    let acc = keccak::hashv(&[b"permissionless inert inbox acc"]).to_bytes();
    let inbox_pda = sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 1);
    ctx.set_account(
        &inbox_pda,
        &inbox_batch_account_with_open_ts(
            c.inbox_program,
            c.chain_id,
            1,
            settlement_program,
            c.authority.pubkey(),
            true,
            acc,
            open_unix_ts,
        )
        .into(),
    );
    let parent_hash = GENESIS_BLOCK_HASH;
    let last_block_hash = [0x22u8; 32];
    let state_root = [0x33u8; 32];
    let forced_outcome_commitment = rome_zk_layouts::forced_empty_root(&rome_zk_merkle::keccak256);
    let gas_used = 21_000u64;
    let pv_bytes = build_public_values_v2(
        c.chain_id,
        1,
        1,
        open_unix_ts as u64,
        60,
        gas_used,
        parent_hash,
        last_block_hash,
        state_root,
        acc,
        forced_outcome_commitment,
    );
    let batch_guest_vkey = batch_guest_program_vk();
    let proof_abi = synthetic_layout1_proof_abi_with_vkey(14, &batch_guest_vkey, &pv_bytes);
    let submit = |proof_abi: Vec<u8>| {
        sclient::post_root_proved_ix(
            &settlement_program,
            &c.authority.pubkey(),
            &c.inbox_program,
            &c.treasury,
            sclient::PostRootFields {
                chain_id: c.chain_id,
                batch: 1,
                prev_batch: 0,
                pre_state_root: c.genesis_state_root,
                first_block: 1,
                last_block: 1,
                state_root,
                block_roots_merkle: [0u8; 32],
                inbox_commitment: acc,
                forced_outcome_commitment,
                parent_hash,
                last_block_hash,
                gas_in_batch: gas_used,
            },
            proof_abi,
            vec![],
        )
    };

    // 1. Empty registry: refused on the missing key, before any pairing work.
    let (err, cu) = send_capturing_cu(&mut ctx, &[submit(proof_abi.clone())], &c.authority).await;
    let err = err.expect_err("an empty registry has no key to verify the proof under");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::RegistryEntryNotFound as u32),
        "an inert chain must refuse a proved root by name"
    );
    assert!(
        cu < 60_000,
        "must be refused before the pairing: got {cu} CU"
    );

    // 2. The registry authority registers the layout-1 key, effective now.
    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let set_ix = sclient::set_registry_entry_ix(
        &settlement_program,
        &c.registry_authority.pubkey(),
        &payer.pubkey(),
        c.chain_id,
        sclient::RegistryEntry {
            curve: reg_layout::CURVE_BN254,
            scheme: reg_layout::SCHEME_PLONK,
            vkey_hash: batch_guest_vkey,
            layout_id: reg_layout::LAYOUT_ZISK_V1,
        },
        now,
    );
    send(&mut ctx, &[set_ix], &payer, &[&c.registry_authority])
        .await
        .expect("SetRegistryEntry should append into the empty registry");
    let after = sclient::decode_registry_account(
        &ctx.banks_client
            .get_account(registry_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap();
    assert_eq!(after.count, 1);
    assert_eq!(
        after.entries[0].vkey_hash, batch_guest_vkey,
        "the first entry lands at index 0, the layout-1 primary slot"
    );
    assert_eq!(after.entries[0].layout_id, reg_layout::LAYOUT_ZISK_V1);
    assert_eq!(after.entries[0].activation_slot, now);

    // 3. The same submission now finds its key and reaches the pairing.
    let (err2, cu2) = send_capturing_cu(&mut ctx, &[submit(proof_abi)], &c.authority).await;
    let err2 = err2.expect_err("the blob is unproved, so the pairing is where it must end");
    assert_eq!(
        custom_error(&err2),
        None,
        "once registered the key is found: no RegistryEntryNotFound"
    );
    assert!(
        cu2 > 400_000,
        "must reach the real pairing once the key is registered: got {cu2} CU"
    );
}

/// The delay itself: an entry registered for a future slot is invisible to `PostRootProved`
/// (`RegistryEntryNotFound` — the same refusal a never-registered vkey gets) until the chain's clock
/// reaches it, then reaches the pairing exactly like the immediate-activation case above. This test warps
/// (to reach the activation slot), so — unlike the other `SetRegistryEntry` tests above, which inject
/// their inbox batch account post-start via `ctx.set_account` — the batch-1 inbox account is registered
/// on `pt` BEFORE `start_with_context()` (the same pre-start requirement `post_root_rig`'s own doc names:
/// a post-start `set_account`'s lamports aren't part of the bank's genesis supply, and `warp_to_slot`'s
/// accounts-hash verification panics on that mismatch).
#[tokio::test]
async fn set_registry_entry_activation_delay_refuses_until_the_slot_then_reaches_the_pairing() {
    let (mut pt, settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = 41;
    let open_unix_ts: i64 = 1_700_000_000;
    let acc = keccak::hashv(&[b"set_registry_entry delay inbox acc"]).to_bytes();
    let inbox_pda = sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 1);
    pt.add_account(
        inbox_pda,
        inbox_batch_account_with_open_ts(
            c.inbox_program,
            c.chain_id,
            1,
            settlement_program,
            c.authority.pubkey(),
            true,
            acc,
            open_unix_ts,
        ),
    );
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;

    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let activation_slot = now + 1_000;
    let batch_guest_vkey = batch_guest_program_vk();
    let entry = sclient::RegistryEntry {
        curve: reg_layout::CURVE_BN254,
        scheme: reg_layout::SCHEME_PLONK,
        vkey_hash: batch_guest_vkey,
        layout_id: reg_layout::LAYOUT_ZISK_V1,
    };
    let set_ix = sclient::set_registry_entry_ix(
        &settlement_program,
        &c.registry_authority.pubkey(),
        &payer.pubkey(),
        c.chain_id,
        entry,
        activation_slot,
    );
    send(&mut ctx, &[set_ix], &payer, &[&c.registry_authority])
        .await
        .expect("SetRegistryEntry should succeed");

    let parent_hash = GENESIS_BLOCK_HASH;
    let last_block_hash = [0x22u8; 32];
    let state_root = [0x33u8; 32];
    let forced_outcome_commitment = rome_zk_layouts::forced_empty_root(&rome_zk_merkle::keccak256);
    let gas_used = 21_000u64;
    let pv_bytes = build_public_values_v2(
        c.chain_id,
        1,
        1,
        open_unix_ts as u64,
        60,
        gas_used,
        parent_hash,
        last_block_hash,
        state_root,
        acc,
        forced_outcome_commitment,
    );
    let proof_abi = synthetic_layout1_proof_abi_with_vkey(14, &batch_guest_vkey, &pv_bytes);
    let args = sclient::PostRootFields {
        chain_id: c.chain_id,
        batch: 1,
        prev_batch: 0,
        pre_state_root: c.genesis_state_root,
        first_block: 1,
        last_block: 1,
        state_root,
        block_roots_merkle: [0u8; 32],
        inbox_commitment: acc,
        forced_outcome_commitment,
        parent_hash,
        last_block_hash,
        gas_in_batch: gas_used,
    };
    let ix = sclient::post_root_proved_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &c.inbox_program,
        &c.treasury,
        args,
        proof_abi.clone(),
        vec![],
    );

    // Before the activation slot: refused exactly like an unregistered vkey, cheaply (no pairing).
    let (err, cu) = send_capturing_cu(&mut ctx, &[ix], &c.authority).await;
    let err = err.expect_err("an entry registered for a future slot must not be visible yet");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::RegistryEntryNotFound as u32),
        "the delay must refuse by the SAME name a never-registered vkey gets"
    );
    eprintln!("PostRootProved layout 1 (rejected: RegistryEntryNotFound, before activation) consumed {cu} CU");
    assert!(
        cu < 60_000,
        "must be refused before the pairing: got {cu} CU"
    );

    // Warp to the activation slot: now the entry is visible and the pairing runs.
    ctx.warp_to_slot(activation_slot.max(ctx.banks_client.get_root_slot().await.unwrap() + 1))
        .unwrap();
    let ix2 = sclient::post_root_proved_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &c.inbox_program,
        &c.treasury,
        sclient::PostRootFields {
            chain_id: c.chain_id,
            batch: 1,
            prev_batch: 0,
            pre_state_root: c.genesis_state_root,
            first_block: 1,
            last_block: 1,
            state_root,
            block_roots_merkle: [0u8; 32],
            inbox_commitment: acc,
            forced_outcome_commitment,
            parent_hash,
            last_block_hash,
            gas_in_batch: gas_used,
        },
        proof_abi,
        vec![],
    );
    let (err2, cu2) = send_capturing_cu(&mut ctx, &[ix2], &c.authority).await;
    let err2 = err2
        .expect_err("still an unproved synthetic blob — must fail the pairing, not any binding");
    assert_eq!(
        custom_error(&err2),
        None,
        "once activated, the entry is found — no more RegistryEntryNotFound"
    );
    eprintln!("PostRootProved layout 1 (activated, reached the pairing) consumed {cu2} CU");
    assert!(
        cu2 > 400_000,
        "must reach the real pairing once active: got {cu2} CU"
    );
}

/// A second `SetRegistryEntry` for the SAME vkey (same `curve`/`scheme`/`vkey_hash`, same
/// `layout_id`) only updates that entry's `activation_slot` in place — `count`, every other entry, and
/// every byte of THIS entry except the 8-byte activation tail stay identical.
#[tokio::test]
async fn set_registry_entry_same_vkey_updates_only_the_activation_slot() {
    let (pt, settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = 42;
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;

    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let vkey = batch_guest_program_vk();
    let entry = sclient::RegistryEntry {
        curve: reg_layout::CURVE_BN254,
        scheme: reg_layout::SCHEME_PLONK,
        vkey_hash: vkey,
        layout_id: reg_layout::LAYOUT_ZISK_V1,
    };
    send(
        &mut ctx,
        &[sclient::set_registry_entry_ix(
            &settlement_program,
            &c.registry_authority.pubkey(),
            &payer.pubkey(),
            c.chain_id,
            entry,
            now,
        )],
        &payer,
        &[&c.registry_authority],
    )
    .await
    .expect("first SetRegistryEntry (append) should succeed");

    let (registry_pda, _) = sclient::registry_pda(&settlement_program, c.chain_id);
    let before = ctx
        .banks_client
        .get_account(registry_pda)
        .await
        .unwrap()
        .unwrap();
    let before_decoded = sclient::decode_registry_account(&before.data).unwrap();
    assert_eq!(before_decoded.count, 3);
    assert_eq!(before.data.len(), reg_layout::REGISTRY_LEN_V2);

    // Same vkey, same layout, a new activation slot — this must be the update-only path, never a fresh
    // append (mutation: if the vkey match is ever dropped from the key, this appends a 4th entry
    // instead, and `count` goes red below).
    let updated_activation = now + 500;
    send(
        &mut ctx,
        &[sclient::set_registry_entry_ix(
            &settlement_program,
            &c.registry_authority.pubkey(),
            &payer.pubkey(),
            c.chain_id,
            entry, // identical curve/scheme/vkey_hash/layout_id
            updated_activation,
        )],
        &payer,
        &[&c.registry_authority],
    )
    .await
    .expect("second SetRegistryEntry (same vkey) should succeed");

    let after = ctx
        .banks_client
        .get_account(registry_pda)
        .await
        .unwrap()
        .unwrap();
    let after_decoded = sclient::decode_registry_account(&after.data).unwrap();
    assert_eq!(
        after_decoded.count, 3,
        "same-vkey update must not change count"
    );
    assert_eq!(after_decoded.entries[0], before_decoded.entries[0]);
    assert_eq!(after_decoded.entries[1], before_decoded.entries[1]);
    assert_eq!(after_decoded.entries[2].vkey_hash, vkey);
    assert_eq!(
        after_decoded.entries[2].curve,
        before_decoded.entries[2].curve
    );
    assert_eq!(
        after_decoded.entries[2].scheme,
        before_decoded.entries[2].scheme
    );
    assert_eq!(
        after_decoded.entries[2].layout_id,
        before_decoded.entries[2].layout_id
    );
    assert_eq!(after_decoded.entries[2].activation_slot, updated_activation);
    // Only the 8-byte activation tail for this one entry differs — everything else in the account,
    // including this entry's own 35 fixed bytes, is byte-identical.
    assert_eq!(
        &after.data[..reg_layout::OFF_ACTIVATION],
        &before.data[..reg_layout::OFF_ACTIVATION],
        "no fixed entry bytes may change on a same-vkey activation update"
    );
    let this_entry_tail = reg_layout::OFF_ACTIVATION + 2 * reg_layout::ACTIVATION_ENTRY_LEN;
    assert_eq!(
        &after.data[..this_entry_tail],
        &before.data[..this_entry_tail],
        "only entry 2's own activation slot may change"
    );
}

/// The SAME vkey registered again under a DIFFERENT `layout_id` is refused by name — one
/// vkey is one ELF is one layout, so a rotation can only ever update that entry's activation slot, never
/// its layout.
#[tokio::test]
async fn set_registry_entry_same_vkey_different_layout_is_layout_mismatch() {
    let (pt, settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = 48;
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;

    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let vkey = batch_guest_program_vk();
    send(
        &mut ctx,
        &[sclient::set_registry_entry_ix(
            &settlement_program,
            &c.registry_authority.pubkey(),
            &payer.pubkey(),
            c.chain_id,
            sclient::RegistryEntry {
                curve: reg_layout::CURVE_BN254,
                scheme: reg_layout::SCHEME_PLONK,
                vkey_hash: vkey,
                layout_id: reg_layout::LAYOUT_ZISK_V1,
            },
            now,
        )],
        &payer,
        &[&c.registry_authority],
    )
    .await
    .expect("register under layout 1 should succeed");

    let err = send(
        &mut ctx,
        &[sclient::set_registry_entry_ix(
            &settlement_program,
            &c.registry_authority.pubkey(),
            &payer.pubkey(),
            c.chain_id,
            sclient::RegistryEntry {
                curve: reg_layout::CURVE_BN254,
                scheme: reg_layout::SCHEME_PLONK,
                vkey_hash: vkey,                               // same vkey
                layout_id: reg_layout::LAYOUT_HEADER_FALLBACK, // different layout
            },
            now,
        )],
        &payer,
        &[&c.registry_authority],
    )
    .await
    .expect_err("the same vkey under a different layout must be refused");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::LayoutMismatch as u32)
    );
}

/// Without a duplicate check, `InitChainV2` would only bound `registry_entries.len() > MAX_ENTRIES` and never
/// check the entries against each other, so two of them could name the exact same
/// `(curve, scheme, vkey_hash)` (for example genesis `[L2, L2, A, A]`). That is not a harmless redundancy:
/// `set_registry_entry`'s later scan for that same key would have no single well-defined entry to act on, and
/// a "retire" naming that vkey could silently land on the WRONG copy while `find` keeps serving the other one
/// — the retirement lever would become a no-op. One vkey is one ELF is one layout, so a duplicate is never
/// meaningful even when it names two DIFFERENT layouts. This test pins the refusal.
#[tokio::test]
async fn init_chain_v2_refuses_a_genesis_registry_with_duplicate_vkeys() {
    let (pt, _settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = 470;
    let mut ctx = pt.start_with_context().await;
    ensure_global_config(&mut ctx, &payer, &c).await;
    ensure_reserved_allowed(&mut ctx, &payer, &c).await;

    let vkey_a = batch_guest_program_vk();

    // Exact duplicate: same curve/scheme/vkey_hash, same layout too (the `[L2, L2, A, A]`
    // shape reduced to its essential pair).
    let mut fields = default_init_chain_fields(&c, 16);
    fields.registry_entries = vec![
        sclient::RegistryEntry {
            curve: reg_layout::CURVE_BN254,
            scheme: reg_layout::SCHEME_PLONK,
            vkey_hash: vkey_a,
            layout_id: reg_layout::LAYOUT_ZISK_V1,
        },
        sclient::RegistryEntry {
            curve: reg_layout::CURVE_BN254,
            scheme: reg_layout::SCHEME_PLONK,
            vkey_hash: vkey_a,
            layout_id: reg_layout::LAYOUT_ZISK_V1,
        },
    ];
    let ix = sclient::init_chain_reserved_ix(
        &c.settlement_program,
        &payer.pubkey(),
        &c.authority.pubkey(),
        &c.registry_authority.pubkey(),
        c.chain_id,
        fields,
    );
    let err = send(
        &mut ctx,
        &[ix],
        &payer,
        &[&c.authority, &c.registry_authority],
    )
    .await
    .expect_err("a genesis registry naming the same vkey twice must be refused");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::DuplicateRegistryEntry as u32),
        "duplicate vkeys at InitChainV2 must be refused by name, not silently accepted"
    );

    // Same-vkey-under-a-different-layout is ALSO a duplicate (the `[A/L1, A/L2]` shape) — one
    // vkey is one ELF is one layout, so two layouts for the same key are never both meaningful.
    let mut fields2 = default_init_chain_fields(&c, 16);
    fields2.registry_entries = vec![
        sclient::RegistryEntry {
            curve: reg_layout::CURVE_BN254,
            scheme: reg_layout::SCHEME_PLONK,
            vkey_hash: vkey_a,
            layout_id: reg_layout::LAYOUT_ZISK_V1,
        },
        sclient::RegistryEntry {
            curve: reg_layout::CURVE_BN254,
            scheme: reg_layout::SCHEME_PLONK,
            vkey_hash: vkey_a,
            layout_id: reg_layout::LAYOUT_HEADER_FALLBACK,
        },
    ];
    let ix2 = sclient::init_chain_reserved_ix(
        &c.settlement_program,
        &payer.pubkey(),
        &c.authority.pubkey(),
        &c.registry_authority.pubkey(),
        c.chain_id,
        fields2,
    );
    let err2 = send(
        &mut ctx,
        &[ix2],
        &payer,
        &[&c.authority, &c.registry_authority],
    )
    .await
    .expect_err("the same vkey under two different layouts must also be refused");
    assert_eq!(
        custom_error(&err2),
        Some(zk_settlement::errors::SettleError::DuplicateRegistryEntry as u32),
        "same-vkey-different-layout is still a duplicate — refused the same way"
    );
}

/// Retiring a vkey (`activation_slot = RETIRED_SLOT`) succeeds even after warping well past
/// slot 0 — `u64::MAX` is never "in the past" (`ActivationInPast` compares `activation_slot < now`, and
/// no real `now` is ever `>= u64::MAX`), pinned directly against `ActivationInPast` rather than inferred
/// from the longer rotation test further down.
#[tokio::test]
async fn set_registry_entry_retire_never_hits_activation_in_past() {
    let (pt, settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = 49;
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;

    let vkey = batch_guest_program_vk();
    let entry = sclient::RegistryEntry {
        curve: reg_layout::CURVE_BN254,
        scheme: reg_layout::SCHEME_PLONK,
        vkey_hash: vkey,
        layout_id: reg_layout::LAYOUT_ZISK_V1,
    };
    let now = ctx.banks_client.get_root_slot().await.unwrap();
    send(
        &mut ctx,
        &[sclient::set_registry_entry_ix(
            &settlement_program,
            &c.registry_authority.pubkey(),
            &payer.pubkey(),
            c.chain_id,
            entry,
            now,
        )],
        &payer,
        &[&c.registry_authority],
    )
    .await
    .expect("register should succeed");

    ctx.warp_to_slot(now + 50_000).unwrap();

    send(
        &mut ctx,
        &[sclient::set_registry_entry_ix(
            &settlement_program,
            &c.registry_authority.pubkey(),
            &payer.pubkey(),
            c.chain_id,
            entry,
            reg_layout::RETIRED_SLOT,
        )],
        &payer,
        &[&c.registry_authority],
    )
    .await
    .expect("retiring with RETIRED_SLOT (u64::MAX) must never be ActivationInPast");
}

// ---------------------------------------------------------------------------------------------
// Five guards that were found untested (removing all five at once left every existing
// `set_registry_entry_*` test green, because every one of them drives
// `zk_settlement_client::set_registry_entry_ix`, which always builds the right PDA, layout 1, and known
// constants). Each test below hand-builds the `Instruction` itself so it can violate exactly the one
// field its guard checks.
// ---------------------------------------------------------------------------------------------

/// Hand-built `SetRegistryEntry` instruction — never the client builder (see above).
#[allow(clippy::too_many_arguments)]
fn raw_set_registry_entry_ix(
    settlement_program: Pubkey,
    registry_authority: Pubkey,
    payer: Pubkey,
    payer_is_signer: bool,
    global_config: Pubkey,
    registry: Pubkey,
    sys_program: Pubkey,
    chain_id: u64,
    entry: sclient::RegistryEntry,
    activation_slot: u64,
) -> Instruction {
    Instruction {
        program_id: settlement_program,
        accounts: vec![
            AccountMeta::new_readonly(registry_authority, true),
            AccountMeta::new(payer, payer_is_signer),
            AccountMeta::new_readonly(global_config, false),
            AccountMeta::new(registry, false),
            AccountMeta::new_readonly(sys_program, false),
        ],
        data: borsh::to_vec(&zk_settlement::SettleIx::SetRegistryEntry {
            chain_id,
            entry,
            activation_slot,
        })
        .unwrap(),
    }
}

fn valid_entry() -> sclient::RegistryEntry {
    sclient::RegistryEntry {
        curve: reg_layout::CURVE_BN254,
        scheme: reg_layout::SCHEME_PLONK,
        vkey_hash: batch_guest_program_vk(),
        layout_id: reg_layout::LAYOUT_ZISK_V1,
    }
}

/// Guard 1: the `registry` account must be `registry_pda(program_id, chain_id)` for the instruction's OWN
/// `chain_id` argument — a PDA for any other chain is refused before any account data is even read.
/// Mutation (drop the PDA check entirely): this test goes red — the wrong PDA (never initialized as a
/// chain-47-labelled registry) would instead fail later with a decode error, not `InvalidSeeds` by name.
#[tokio::test]
async fn set_registry_entry_guard_registry_must_be_this_chain_ids_own_pda() {
    let (pt, settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = 51;
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;

    let (global_config, _) = sclient::global_config_pda(&settlement_program);
    let (wrong_registry_pda, _) = sclient::registry_pda(&settlement_program, c.chain_id + 1);
    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let ix = raw_set_registry_entry_ix(
        settlement_program,
        c.registry_authority.pubkey(),
        payer.pubkey(),
        true,
        global_config,
        wrong_registry_pda,
        system_program::id(),
        c.chain_id,
        valid_entry(),
        now,
    );
    let err = send(&mut ctx, &[ix], &payer, &[&c.registry_authority])
        .await
        .expect_err("a registry account that is not this chain_id's own PDA must be refused");
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::InvalidSeeds)
    );
}

/// Guard 2: the registry account's OWN encoded `chain_id` (its header, not its key) must equal the
/// instruction's `chain_id` argument. Reachable independently of guard 1 (the PDA key can be exactly
/// right while the bytes inside it disagree, e.g. tampered or aliased data) — tampering the real
/// `chain_id`'s own registry account directly, post-start, at its correct PDA. Mutation (drop the header
/// check): this test goes red — `SetRegistryEntry` would silently accept and rotate a registry account
/// whose header claims a different chain.
#[tokio::test]
async fn set_registry_entry_guard_header_chain_id_must_match_the_argument() {
    let (pt, settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = 52;
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;

    let (registry_pda, _) = sclient::registry_pda(&settlement_program, c.chain_id);
    let mut acct = ctx
        .banks_client
        .get_account(registry_pda)
        .await
        .unwrap()
        .unwrap();
    acct.data[reg_layout::OFF_CHAIN_ID..reg_layout::OFF_CHAIN_ID + 8]
        .copy_from_slice(&(c.chain_id + 999).to_le_bytes());
    ctx.set_account(&registry_pda, &acct.into());

    let (global_config, _) = sclient::global_config_pda(&settlement_program);
    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let ix = raw_set_registry_entry_ix(
        settlement_program,
        c.registry_authority.pubkey(),
        payer.pubkey(),
        true,
        global_config,
        registry_pda,
        system_program::id(),
        c.chain_id, // the instruction's own argument -- disagrees with the tampered header
        valid_entry(),
        now,
    );
    let err = send(&mut ctx, &[ix], &payer, &[&c.registry_authority])
        .await
        .expect_err(
            "a registry whose own header disagrees with the chain_id argument must be refused",
        );
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::InvalidAccountData)
    );
}

/// Guard 3: `entry.layout_id` must be `LAYOUT_ZISK_V1` or `LAYOUT_HEADER_FALLBACK` — `3` is refused by
/// name before the registry is even touched. Mutation (drop the layout check): red.
#[tokio::test]
async fn set_registry_entry_guard_unknown_layout_is_refused() {
    let (pt, settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = 53;
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;

    let (global_config, _) = sclient::global_config_pda(&settlement_program);
    let (registry_pda, _) = sclient::registry_pda(&settlement_program, c.chain_id);
    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let mut entry = valid_entry();
    entry.layout_id = 3;
    let ix = raw_set_registry_entry_ix(
        settlement_program,
        c.registry_authority.pubkey(),
        payer.pubkey(),
        true,
        global_config,
        registry_pda,
        system_program::id(),
        c.chain_id,
        entry,
        now,
    );
    let err = send(&mut ctx, &[ix], &payer, &[&c.registry_authority])
        .await
        .expect_err("layout_id 3 must be refused");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::UnknownLayout as u32)
    );
}

/// Guard 4: `entry.curve`/`entry.scheme` must each be one of the known constants — `curve = 2` (and,
/// independently, `scheme = 2`) is refused by name. Mutation (drop the curve/scheme check): red.
#[tokio::test]
async fn set_registry_entry_guard_unknown_curve_or_scheme_is_refused() {
    let (pt, settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = 54;
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;

    let (global_config, _) = sclient::global_config_pda(&settlement_program);
    let (registry_pda, _) = sclient::registry_pda(&settlement_program, c.chain_id);
    let now = ctx.banks_client.get_root_slot().await.unwrap();

    let mut bad_curve = valid_entry();
    bad_curve.curve = 2;
    let ix = raw_set_registry_entry_ix(
        settlement_program,
        c.registry_authority.pubkey(),
        payer.pubkey(),
        true,
        global_config,
        registry_pda,
        system_program::id(),
        c.chain_id,
        bad_curve,
        now,
    );
    let err = send(&mut ctx, &[ix], &payer, &[&c.registry_authority])
        .await
        .expect_err("curve 2 must be refused");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::UnknownCurveOrScheme as u32)
    );

    let mut bad_scheme = valid_entry();
    bad_scheme.scheme = 2;
    let ix2 = raw_set_registry_entry_ix(
        settlement_program,
        c.registry_authority.pubkey(),
        payer.pubkey(),
        true,
        global_config,
        registry_pda,
        system_program::id(),
        c.chain_id,
        bad_scheme,
        now,
    );
    let err2 = send(&mut ctx, &[ix2], &payer, &[&c.registry_authority])
        .await
        .expect_err("scheme 2 must be refused");
    assert_eq!(
        custom_error(&err2),
        Some(zk_settlement::errors::SettleError::UnknownCurveOrScheme as u32)
    );
}

/// Guard 5 (one `if`, two ways to trip it): the `payer` account must actually be a signer, and the last
/// account must actually be the system program — either violation is `MissingRequiredSignature`.
/// Mutation (drop this whole guard): both sub-cases go red.
#[tokio::test]
async fn set_registry_entry_guard_payer_signer_and_system_program_are_checked() {
    let (pt, settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = 55;
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;

    let (global_config, _) = sclient::global_config_pda(&settlement_program);
    let (registry_pda, _) = sclient::registry_pda(&settlement_program, c.chain_id);
    let now = ctx.banks_client.get_root_slot().await.unwrap();

    // (a) the `payer` account is never a signer of this transaction at all.
    let impostor_payer = Pubkey::new_unique();
    let ix_no_signer = raw_set_registry_entry_ix(
        settlement_program,
        c.registry_authority.pubkey(),
        impostor_payer,
        false, // not a signer
        global_config,
        registry_pda,
        system_program::id(),
        c.chain_id,
        valid_entry(),
        now,
    );
    let err = send(&mut ctx, &[ix_no_signer], &payer, &[&c.registry_authority])
        .await
        .expect_err("a non-signing payer account must be refused");
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::MissingRequiredSignature)
    );

    // (b) the trailing account is not the real system program.
    let ix_wrong_sys = raw_set_registry_entry_ix(
        settlement_program,
        c.registry_authority.pubkey(),
        payer.pubkey(),
        true,
        global_config,
        registry_pda,
        Pubkey::new_unique(), // not system_program::id()
        c.chain_id,
        valid_entry(),
        now,
    );
    let err2 = send(&mut ctx, &[ix_wrong_sys], &payer, &[&c.registry_authority])
        .await
        .expect_err("a non-system-program trailing account must be refused");
    assert_eq!(
        err2,
        TransactionError::InstructionError(0, InstructionError::MissingRequiredSignature)
    );
}

/// The payer-signer / system-program guard proven where it is the ONLY refusal. The v1 cases above are
/// also caught by the runtime's own CPI check (the v1 -> v2 realloc invokes the system program's transfer
/// with `payer` as a signer), so there the explicit guard only controls the error NAME. On an account that
/// is already v2-length no transfer happens (rent is covered), and this guard is the one thing standing
/// between a non-signing payer and a written entry — with it removed inside `set_registry_entry`, both
/// calls below SUCCEED.
#[tokio::test]
async fn set_registry_entry_guard_payer_signer_and_system_program_are_checked_on_a_v2_registry() {
    let (pt, settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = 56;
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;

    let (global_config, _) = sclient::global_config_pda(&settlement_program);
    let (registry_pda, _) = sclient::registry_pda(&settlement_program, c.chain_id);
    let impostor_payer = Pubkey::new_unique();

    // Grow the registry to v2 with one legitimate call first.
    let now_grow = ctx.banks_client.get_root_slot().await.unwrap();
    send(
        &mut ctx,
        &[sclient::set_registry_entry_ix(
            &settlement_program,
            &c.registry_authority.pubkey(),
            &payer.pubkey(),
            c.chain_id,
            valid_entry(),
            now_grow,
        )],
        &payer,
        &[&c.registry_authority],
    )
    .await
    .expect("growing the registry to v2 with a legitimate call should succeed");

    let now_v2 = ctx.banks_client.get_root_slot().await.unwrap();
    let ix_no_signer_v2 = raw_set_registry_entry_ix(
        settlement_program,
        c.registry_authority.pubkey(),
        impostor_payer,
        false, // not a signer
        global_config,
        registry_pda,
        system_program::id(),
        c.chain_id,
        valid_entry(),
        now_v2,
    );
    let err3 = send(
        &mut ctx,
        &[ix_no_signer_v2],
        &payer,
        &[&c.registry_authority],
    )
    .await
    .expect_err("a non-signing payer must be refused on the v2-length registry too");
    assert_eq!(
        err3,
        TransactionError::InstructionError(0, InstructionError::MissingRequiredSignature)
    );

    let ix_wrong_sys_v2 = raw_set_registry_entry_ix(
        settlement_program,
        c.registry_authority.pubkey(),
        payer.pubkey(),
        true,
        global_config,
        registry_pda,
        Pubkey::new_unique(), // not system_program::id()
        c.chain_id,
        valid_entry(),
        now_v2,
    );
    let err4 = send(
        &mut ctx,
        &[ix_wrong_sys_v2],
        &payer,
        &[&c.registry_authority],
    )
    .await
    .expect_err(
        "a non-system-program trailing account must be refused on the v2-length registry too",
    );
    assert_eq!(
        err4,
        TransactionError::InstructionError(0, InstructionError::MissingRequiredSignature)
    );
}

/// Defence in depth: `InitChainV2` refuses duplicate vkeys (see its duplicate tests above), but
/// `set_registry_entry` does not trust that invariant blindly. A hand-crafted registry account carrying the
/// SAME `(curve, scheme, vkey_hash)` at two populated indices is refused `InvalidAccountData` rather than
/// silently acting on either copy.
#[tokio::test]
async fn set_registry_entry_refuses_a_corrupt_registry_with_a_duplicate_vkey() {
    let (pt, settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = 472;
    let mut ctx = pt.start_with_context().await;
    ensure_global_config(&mut ctx, &payer, &c).await;

    let vkey = batch_guest_program_vk();
    let (registry_pda, _) = sclient::registry_pda(&settlement_program, c.chain_id);
    let mut d = vec![0u8; reg_layout::REGISTRY_LEN_V2];
    d[reg_layout::OFF_MAGIC..reg_layout::OFF_MAGIC + 4]
        .copy_from_slice(&reg_layout::MAGIC.to_le_bytes());
    d[reg_layout::OFF_CHAIN_ID..reg_layout::OFF_CHAIN_ID + 8]
        .copy_from_slice(&c.chain_id.to_le_bytes());
    d[reg_layout::OFF_INBOX_PROGRAM..reg_layout::OFF_INBOX_PROGRAM + 32]
        .copy_from_slice(c.inbox_program.as_ref());
    d[reg_layout::OFF_COUNT] = 2;
    let dup_entry = reg_layout::RegistryEntry {
        curve: reg_layout::CURVE_BN254,
        scheme: reg_layout::SCHEME_PLONK,
        vkey_hash: vkey,
        layout_id: reg_layout::LAYOUT_ZISK_V1,
    };
    reg_layout::write_entry(&mut d, 0, &dup_entry, 0).unwrap();
    reg_layout::write_entry(&mut d, 1, &dup_entry, 0).unwrap();
    ctx.set_account(
        &registry_pda,
        &Account {
            lamports: rent_exempt(reg_layout::REGISTRY_LEN_V2),
            data: d,
            owner: settlement_program,
            executable: false,
            rent_epoch: 0,
        }
        .into(),
    );

    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let err = send(
        &mut ctx,
        &[sclient::set_registry_entry_ix(
            &settlement_program,
            &c.registry_authority.pubkey(),
            &payer.pubkey(),
            c.chain_id,
            sclient::RegistryEntry {
                curve: dup_entry.curve,
                scheme: dup_entry.scheme,
                vkey_hash: dup_entry.vkey_hash,
                layout_id: dup_entry.layout_id,
            },
            now,
        )],
        &payer,
        &[&c.registry_authority],
    )
    .await
    .expect_err("a corrupt registry with a duplicate vkey must be refused, not acted on");
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::InvalidAccountData)
    );
}

/// `SetRegistryEntry` refuses to append a new vkey once the registry already holds `MAX_ENTRIES` (4) and
/// none of them is retired (`activation_slot == RETIRED_SLOT`) — nothing left to reuse, nothing left to
/// append into. (The retired-slot reuse itself is exercised by the delayed-rotation test above.)
#[tokio::test]
async fn set_registry_entry_refuses_registry_full_with_no_matching_entry_to_replace() {
    let (pt, settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = 43;
    let mut ctx = pt.start_with_context().await;

    // Four distinct entries at InitChain time — the registry starts already full.
    let mut fields = default_init_chain_fields(&c, 16);
    fields.registry_entries = vec![
        sclient::RegistryEntry {
            curve: reg_layout::CURVE_BN254,
            scheme: reg_layout::SCHEME_PLONK,
            vkey_hash: [1u8; 32],
            layout_id: reg_layout::LAYOUT_HEADER_FALLBACK,
        },
        sclient::RegistryEntry {
            curve: reg_layout::CURVE_BN254,
            scheme: reg_layout::SCHEME_GROTH16,
            vkey_hash: [2u8; 32],
            layout_id: reg_layout::LAYOUT_HEADER_FALLBACK,
        },
        sclient::RegistryEntry {
            curve: reg_layout::CURVE_BLS12_381,
            scheme: reg_layout::SCHEME_PLONK,
            vkey_hash: [3u8; 32],
            layout_id: reg_layout::LAYOUT_HEADER_FALLBACK,
        },
        sclient::RegistryEntry {
            curve: reg_layout::CURVE_BLS12_381,
            scheme: reg_layout::SCHEME_GROTH16,
            vkey_hash: [4u8; 32],
            layout_id: reg_layout::LAYOUT_HEADER_FALLBACK,
        },
    ];
    ensure_global_config(&mut ctx, &payer, &c).await;
    ensure_reserved_allowed(&mut ctx, &payer, &c).await;
    let init_ix = sclient::init_chain_reserved_ix(
        &settlement_program,
        &payer.pubkey(),
        &c.authority.pubkey(),
        &c.registry_authority.pubkey(),
        c.chain_id,
        fields,
    );
    send(
        &mut ctx,
        &[init_ix],
        &payer,
        &[&c.authority, &c.registry_authority],
    )
    .await
    .expect("InitChain with 4 entries should succeed");

    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let fifth_entry = sclient::RegistryEntry {
        curve: reg_layout::CURVE_BN254,
        scheme: reg_layout::SCHEME_PLONK,
        vkey_hash: batch_guest_program_vk(),
        layout_id: reg_layout::LAYOUT_ZISK_V1, // a brand-new vkey — none of the 4 existing entries is retired
    };
    let err = send(
        &mut ctx,
        &[sclient::set_registry_entry_ix(
            &settlement_program,
            &c.registry_authority.pubkey(),
            &payer.pubkey(),
            c.chain_id,
            fifth_entry,
            now,
        )],
        &payer,
        &[&c.registry_authority],
    )
    .await
    .expect_err("a 5th distinct entry must be refused — the registry is full");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::RegistryFull as u32)
    );
}

/// A `SetRegistryEntry` not signed by `global_config.registry_authority` is refused, same gate as
/// `SetFee`/`SetDriftBound`.
#[tokio::test]
async fn set_registry_entry_rejects_a_signer_that_is_not_the_registry_authority() {
    let (pt, settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = 44;
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;

    let impostor = funded_keypair();
    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let entry = sclient::RegistryEntry {
        curve: reg_layout::CURVE_BN254,
        scheme: reg_layout::SCHEME_PLONK,
        vkey_hash: batch_guest_program_vk(),
        layout_id: reg_layout::LAYOUT_ZISK_V1,
    };
    let err = send(
        &mut ctx,
        &[sclient::set_registry_entry_ix(
            &settlement_program,
            &impostor.pubkey(),
            &payer.pubkey(),
            c.chain_id,
            entry,
            now,
        )],
        &payer,
        &[&impostor],
    )
    .await
    .expect_err("a non-registry-authority signer must be refused");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::NotRegistryAuthority as u32)
    );
}

/// `activation_slot` strictly before the current slot is refused by name; equal (immediate) is allowed —
/// exercised by the "reaches the pairing" test above, which passes `now` itself. Warps to a known slot
/// first so the "before now" argument is unambiguous regardless of `solana-program-test`'s own starting
/// slot (never assumed to be any particular value).
#[tokio::test]
async fn set_registry_entry_rejects_an_activation_slot_before_now() {
    let (pt, settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = 45;
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;

    ctx.warp_to_slot(1_000).unwrap();
    let now = ctx.banks_client.get_root_slot().await.unwrap();
    assert!(now >= 1_000);
    let entry = sclient::RegistryEntry {
        curve: reg_layout::CURVE_BN254,
        scheme: reg_layout::SCHEME_PLONK,
        vkey_hash: batch_guest_program_vk(),
        layout_id: reg_layout::LAYOUT_ZISK_V1,
    };
    let err = send(
        &mut ctx,
        &[sclient::set_registry_entry_ix(
            &settlement_program,
            &c.registry_authority.pubkey(),
            &payer.pubkey(),
            c.chain_id,
            entry,
            500, // well before the slot just warped to
        )],
        &payer,
        &[&c.registry_authority],
    )
    .await
    .expect_err("an activation_slot behind the current slot must be refused");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::ActivationInPast as u32)
    );
}

/// The one-time v1->v2 realloc must not disturb a single byte of the two pre-existing entries (or the
/// header) — only `count` (2 -> 3, the append) and the newly-appended entry's own bytes change.
#[tokio::test]
async fn set_registry_entry_realloc_preserves_every_byte_of_the_v1_registry() {
    let (pt, settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = 46;
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;

    let (registry_pda, _) = sclient::registry_pda(&settlement_program, c.chain_id);
    let before = ctx
        .banks_client
        .get_account(registry_pda)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before.data.len(), reg_layout::REGISTRY_LEN);
    let before_bytes = before.data.clone();

    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let entry = sclient::RegistryEntry {
        curve: reg_layout::CURVE_BN254,
        scheme: reg_layout::SCHEME_PLONK,
        vkey_hash: batch_guest_program_vk(),
        layout_id: reg_layout::LAYOUT_ZISK_V1,
    };
    send(
        &mut ctx,
        &[sclient::set_registry_entry_ix(
            &settlement_program,
            &c.registry_authority.pubkey(),
            &payer.pubkey(),
            c.chain_id,
            entry,
            now,
        )],
        &payer,
        &[&c.registry_authority],
    )
    .await
    .expect("SetRegistryEntry should succeed");

    let after = ctx
        .banks_client
        .get_account(registry_pda)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.data.len(), reg_layout::REGISTRY_LEN_V2);
    // Every byte up to (not including) `count` is untouched by the realloc.
    assert_eq!(
        &after.data[..reg_layout::OFF_COUNT],
        &before_bytes[..reg_layout::OFF_COUNT]
    );
    assert_eq!(before_bytes[reg_layout::OFF_COUNT], 2);
    assert_eq!(
        after.data[reg_layout::OFF_COUNT],
        3,
        "append bumped count 2 -> 3"
    );
    // The two pre-existing entries' own bytes are untouched.
    let entries_region =
        reg_layout::OFF_ENTRIES..reg_layout::OFF_ENTRIES + 2 * reg_layout::ENTRY_LEN;
    assert_eq!(
        &after.data[entries_region.clone()],
        &before_bytes[entries_region]
    );
}

/// A delayed rotation of the layout-1 key must never clobber the still-in-flight old key. Reproduced on real
/// BPF: registering key A, then key B under the SAME `(curve, scheme, layout_id)` with a delayed
/// activation, silently REPLACED A (the program matched on `(curve, scheme, layout_id)`, not the vkey) —
/// "old key before replace: reaches the pairing (561,175 CU); old key after the replace, before B's
/// activation: `RegistryEntryNotFound` (13,021 CU); new key before activation: `RegistryEntryNotFound`."
/// Every step below is the same chain, the same registry account, sequential — a failed `PostRootProved`
/// never commits state (the pairing/registry lookup always fails on this synthetic proof), so the same
/// batch-1 args are replayed at each point in the story without needing a fresh batch.
#[tokio::test]
async fn set_registry_entry_r1_delayed_rotation_never_touches_the_old_key_until_explicit_retire() {
    let (mut pt, settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = 47;
    let open_unix_ts: i64 = 1_700_000_000;
    let acc = keccak::hashv(&[b"r1 rotation inbox acc"]).to_bytes();
    let inbox_pda = sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 1);
    pt.add_account(
        inbox_pda,
        inbox_batch_account_with_open_ts(
            c.inbox_program,
            c.chain_id,
            1,
            settlement_program,
            c.authority.pubkey(),
            true,
            acc,
            open_unix_ts,
        ),
    );
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await; // two layout-2 entries only, no layout-1 entry yet

    let parent_hash = GENESIS_BLOCK_HASH;
    let last_block_hash = [0x22u8; 32];
    let state_root = [0x33u8; 32];
    let forced_outcome_commitment = rome_zk_layouts::forced_empty_root(&rome_zk_merkle::keccak256);
    let gas_used = 21_000u64;
    let pv_bytes = build_public_values_v2(
        c.chain_id,
        1,
        1,
        open_unix_ts as u64,
        60,
        gas_used,
        parent_hash,
        last_block_hash,
        state_root,
        acc,
        forced_outcome_commitment,
    );
    let post_root_args = || sclient::PostRootFields {
        chain_id: c.chain_id,
        batch: 1,
        prev_batch: 0,
        pre_state_root: c.genesis_state_root,
        first_block: 1,
        last_block: 1,
        state_root,
        block_roots_merkle: [0u8; 32],
        inbox_commitment: acc,
        forced_outcome_commitment,
        parent_hash,
        last_block_hash,
        gas_in_batch: gas_used,
    };
    let (registry_pda, _) = sclient::registry_pda(&settlement_program, c.chain_id);

    // --- register A, prove it reaches the pairing ---
    let vkey_a = batch_guest_program_vk();
    let now = ctx.banks_client.get_root_slot().await.unwrap();
    send(
        &mut ctx,
        &[sclient::set_registry_entry_ix(
            &settlement_program,
            &c.registry_authority.pubkey(),
            &payer.pubkey(),
            c.chain_id,
            sclient::RegistryEntry {
                curve: reg_layout::CURVE_BN254,
                scheme: reg_layout::SCHEME_PLONK,
                vkey_hash: vkey_a,
                layout_id: reg_layout::LAYOUT_ZISK_V1,
            },
            now,
        )],
        &payer,
        &[&c.registry_authority],
    )
    .await
    .expect("register A should succeed");

    let proof_under_a = synthetic_layout1_proof_abi_with_vkey(14, &vkey_a, &pv_bytes);
    let ix_a = sclient::post_root_proved_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &c.inbox_program,
        &c.treasury,
        post_root_args(),
        proof_under_a.clone(),
        vec![],
    );
    let (err, cu) = send_capturing_cu(&mut ctx, std::slice::from_ref(&ix_a), &c.authority).await;
    let err = err.expect_err("synthetic proof must fail the pairing");
    assert_eq!(
        custom_error(&err),
        None,
        "R1 step 1: A must reach the pairing before any rotation"
    );
    assert!(
        cu > 400_000,
        "R1 step 1 (old key before replace): got {cu} CU"
    );
    eprintln!("R1 step 1 (old key A before rotation, reaches the pairing): {cu} CU");

    // --- register B under the SAME (curve, scheme, layout_id), a DIFFERENT vkey, delayed activation ---
    let vkey_b = [0xBBu8; 32];
    let now2 = ctx.banks_client.get_root_slot().await.unwrap();
    let activation_b = now2 + 1_000;
    send(
        &mut ctx,
        &[sclient::set_registry_entry_ix(
            &settlement_program,
            &c.registry_authority.pubkey(),
            &payer.pubkey(),
            c.chain_id,
            sclient::RegistryEntry {
                curve: reg_layout::CURVE_BN254,
                scheme: reg_layout::SCHEME_PLONK,
                vkey_hash: vkey_b,
                layout_id: reg_layout::LAYOUT_ZISK_V1,
            },
            activation_b,
        )],
        &payer,
        &[&c.registry_authority],
    )
    .await
    .expect("register B should succeed");

    // The core claim: A must STILL reach the pairing — a delayed rotation of a DIFFERENT vkey must
    // never clobber A's own entry. Before the fix this was `RegistryEntryNotFound` (13,021 CU) because
    // `set_registry_entry` matched on `(curve, scheme, layout_id)` and overwrote A's slot with B.
    let (err, cu) = send_capturing_cu(&mut ctx, std::slice::from_ref(&ix_a), &c.authority).await;
    let err = err.expect_err("synthetic proof must fail the pairing");
    assert_eq!(
        custom_error(&err),
        None,
        "R1 step 2 (old key after a delayed rotation of a DIFFERENT vkey): A must still reach the \
         pairing, not RegistryEntryNotFound — a rotation must never touch another entry"
    );
    assert!(
        cu > 400_000,
        "R1 step 2 (old key after replace): got {cu} CU, expected the real pairing"
    );
    eprintln!("R1 step 2 (old key A after B is registered, still reaches the pairing): {cu} CU");

    // B, before its own activation slot: refused by the SAME name a never-registered vkey gets.
    let proof_under_b = synthetic_layout1_proof_abi_with_vkey(14, &vkey_b, &pv_bytes);
    let ix_b = sclient::post_root_proved_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &c.inbox_program,
        &c.treasury,
        post_root_args(),
        proof_under_b.clone(),
        vec![],
    );
    let (err, cu) = send_capturing_cu(&mut ctx, std::slice::from_ref(&ix_b), &c.authority).await;
    let err = err.expect_err("B is not active yet");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::RegistryEntryNotFound as u32),
        "R1 step 3 (new key before activation): must be RegistryEntryNotFound"
    );
    assert!(
        cu < 60_000,
        "R1 step 3 (new key before activation): got {cu} CU"
    );
    eprintln!("R1 step 3 (new key B before its activation slot, refused cheaply): {cu} CU");

    // --- warp past B's activation: B now reaches the pairing, A still does too ---
    ctx.warp_to_slot(activation_b.max(ctx.banks_client.get_root_slot().await.unwrap() + 1))
        .unwrap();

    let (err, cu) = send_capturing_cu(&mut ctx, std::slice::from_ref(&ix_b), &c.authority).await;
    let err = err.expect_err("synthetic proof must fail the pairing");
    assert_eq!(
        custom_error(&err),
        None,
        "B must reach the pairing once activated"
    );
    assert!(cu > 400_000, "B activated: got {cu} CU");
    eprintln!("B activated, reaches the pairing: {cu} CU");

    let (err, cu) = send_capturing_cu(&mut ctx, std::slice::from_ref(&ix_a), &c.authority).await;
    let err = err.expect_err("synthetic proof must fail the pairing");
    assert_eq!(
        custom_error(&err),
        None,
        "A must still reach the pairing after B activates — both are live until A is explicitly retired"
    );
    assert!(
        cu > 400_000,
        "A still active after B activates: got {cu} CU"
    );
    eprintln!("A still reaches the pairing after B activates: {cu} CU");

    let before_retire = ctx
        .banks_client
        .get_account(registry_pda)
        .await
        .unwrap()
        .unwrap();
    let before_retire_decoded = sclient::decode_registry_account(&before_retire.data).unwrap();
    assert_eq!(
        before_retire_decoded.count, 4,
        "two default layout-2 entries + A + B = MAX_ENTRIES already"
    );

    // --- retire A explicitly: SetRegistryEntry on A's own vkey with activation_slot = RETIRED_SLOT ---
    send(
        &mut ctx,
        &[sclient::set_registry_entry_ix(
            &settlement_program,
            &c.registry_authority.pubkey(),
            &payer.pubkey(),
            c.chain_id,
            sclient::RegistryEntry {
                curve: reg_layout::CURVE_BN254,
                scheme: reg_layout::SCHEME_PLONK,
                vkey_hash: vkey_a,
                layout_id: reg_layout::LAYOUT_ZISK_V1,
            },
            reg_layout::RETIRED_SLOT,
        )],
        &payer,
        &[&c.registry_authority],
    )
    .await
    .expect("retiring A (activation_slot = RETIRED_SLOT) should succeed — u64::MAX is never in the past");

    let after_retire = ctx
        .banks_client
        .get_account(registry_pda)
        .await
        .unwrap()
        .unwrap();
    let after_retire_decoded = sclient::decode_registry_account(&after_retire.data).unwrap();
    assert_eq!(
        after_retire_decoded.count, 4,
        "retire must not change count"
    );

    // A is refused immediately, no warp needed — u64::MAX is never <= any real Clock::slot.
    let (err, cu) = send_capturing_cu(&mut ctx, std::slice::from_ref(&ix_a), &c.authority).await;
    let err = err.expect_err("A is retired");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::RegistryEntryNotFound as u32),
        "retired A must be refused immediately, the same name an unregistered vkey gets"
    );
    assert!(cu < 60_000, "retired A refused cheaply: got {cu} CU");
    eprintln!("A retired: refused immediately, {cu} CU");

    // B is unaffected by A's retirement.
    let (err, cu) = send_capturing_cu(&mut ctx, std::slice::from_ref(&ix_b), &c.authority).await;
    let err = err.expect_err("synthetic proof must fail the pairing");
    assert_eq!(
        custom_error(&err),
        None,
        "B must be unaffected by A's retirement"
    );
    assert!(cu > 400_000, "B unaffected by A's retirement: got {cu} CU");

    // --- registry is full (4/4) with A's slot now retired: a new vkey C reuses A's slot in place ---
    let vkey_c = [0xCCu8; 32];
    let now3 = ctx.banks_client.get_root_slot().await.unwrap();
    send(
        &mut ctx,
        &[sclient::set_registry_entry_ix(
            &settlement_program,
            &c.registry_authority.pubkey(),
            &payer.pubkey(),
            c.chain_id,
            sclient::RegistryEntry {
                curve: reg_layout::CURVE_BN254,
                scheme: reg_layout::SCHEME_PLONK,
                vkey_hash: vkey_c,
                layout_id: reg_layout::LAYOUT_ZISK_V1,
            },
            now3,
        )],
        &payer,
        &[&c.registry_authority],
    )
    .await
    .expect("registering C should reuse A's retired slot, not RegistryFull");

    let after_reuse = ctx
        .banks_client
        .get_account(registry_pda)
        .await
        .unwrap()
        .unwrap();
    let after_reuse_decoded = sclient::decode_registry_account(&after_reuse.data).unwrap();
    assert_eq!(after_reuse_decoded.count, 4, "reuse must not change count");
    let a_slot_idx = after_retire_decoded
        .entries
        .iter()
        .position(|e| e.vkey_hash == vkey_a)
        .expect("A's slot must still be findable by position before comparing bytes");
    assert_eq!(
        after_reuse_decoded.entries[a_slot_idx].vkey_hash, vkey_c,
        "C must land in A's retired slot"
    );
    assert_eq!(
        after_reuse_decoded.entries[a_slot_idx].activation_slot,
        now3
    );
    // Every OTHER entry's bytes are untouched by the reuse.
    for (i, before_e) in before_retire_decoded.entries.iter().enumerate() {
        if i == a_slot_idx {
            continue;
        }
        assert_eq!(
            after_reuse_decoded.entries[i], *before_e,
            "entry {i} must be byte-identical before/after the retired-slot reuse"
        );
    }
}

/// Retiring A then calling `SetRegistryEntry(A, layout 1, now)` again would otherwise SUCCEED and un-retire
/// it — the same call that is supposed to be the terminal "this key is dead" lever instead reactivates it.
/// Retirement must be terminal: once an entry's stored `activation_slot == RETIRED_SLOT`, no other
/// `activation_slot` may ever be written to it again — only a NEW vkey can take that slot
/// (`SetRegistryEntry`'s absent-vkey append/reuse path, tested elsewhere). Retiring an already-retired entry
/// stays a no-op success.
#[tokio::test]
async fn set_registry_entry_rev_unretire_via_same_vkey_update_is_refused() {
    let (mut pt, settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = 471;
    let open_unix_ts: i64 = 1_700_000_000;
    let acc = keccak::hashv(&[b"unretire inbox acc"]).to_bytes();
    let inbox_pda = sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 1);
    pt.add_account(
        inbox_pda,
        inbox_batch_account_with_open_ts(
            c.inbox_program,
            c.chain_id,
            1,
            settlement_program,
            c.authority.pubkey(),
            true,
            acc,
            open_unix_ts,
        ),
    );
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;

    let vkey = batch_guest_program_vk();
    let now = ctx.banks_client.get_root_slot().await.unwrap();
    send(
        &mut ctx,
        &[sclient::set_registry_entry_ix(
            &settlement_program,
            &c.registry_authority.pubkey(),
            &payer.pubkey(),
            c.chain_id,
            sclient::RegistryEntry {
                curve: reg_layout::CURVE_BN254,
                scheme: reg_layout::SCHEME_PLONK,
                vkey_hash: vkey,
                layout_id: reg_layout::LAYOUT_ZISK_V1,
            },
            now,
        )],
        &payer,
        &[&c.registry_authority],
    )
    .await
    .expect("register A should succeed");

    send(
        &mut ctx,
        &[sclient::set_registry_entry_ix(
            &settlement_program,
            &c.registry_authority.pubkey(),
            &payer.pubkey(),
            c.chain_id,
            sclient::RegistryEntry {
                curve: reg_layout::CURVE_BN254,
                scheme: reg_layout::SCHEME_PLONK,
                vkey_hash: vkey,
                layout_id: reg_layout::LAYOUT_ZISK_V1,
            },
            reg_layout::RETIRED_SLOT,
        )],
        &payer,
        &[&c.registry_authority],
    )
    .await
    .expect("retiring A should succeed");

    // The reproduced bug: naming A again with a REAL activation slot must NOT un-retire it.
    let now2 = ctx.banks_client.get_root_slot().await.unwrap();
    let err = send(
        &mut ctx,
        &[sclient::set_registry_entry_ix(
            &settlement_program,
            &c.registry_authority.pubkey(),
            &payer.pubkey(),
            c.chain_id,
            sclient::RegistryEntry {
                curve: reg_layout::CURVE_BN254,
                scheme: reg_layout::SCHEME_PLONK,
                vkey_hash: vkey,
                layout_id: reg_layout::LAYOUT_ZISK_V1,
            },
            now2,
        )],
        &payer,
        &[&c.registry_authority],
    )
    .await
    .expect_err("re-activating a retired vkey must be refused — retirement is terminal");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::EntryRetired as u32),
        "a retired vkey named again with a non-retired activation_slot must be EntryRetired"
    );

    // A FUTURE activation slot against the retired vkey is refused the same way.
    let err = send(
        &mut ctx,
        &[sclient::set_registry_entry_ix(
            &settlement_program,
            &c.registry_authority.pubkey(),
            &payer.pubkey(),
            c.chain_id,
            sclient::RegistryEntry {
                curve: reg_layout::CURVE_BN254,
                scheme: reg_layout::SCHEME_PLONK,
                vkey_hash: vkey,
                layout_id: reg_layout::LAYOUT_ZISK_V1,
            },
            now2 + 5_000,
        )],
        &payer,
        &[&c.registry_authority],
    )
    .await
    .expect_err("a future activation slot against a retired vkey must be refused too");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::EntryRetired as u32),
        "a retired vkey named again with a FUTURE activation_slot must be EntryRetired"
    );

    // The second half of the shape: A stays refused by PostRootProved (the refusal above was not just
    // the SetRegistryEntry call — nothing re-activated the key).
    let pv_bytes = build_public_values_v2(
        c.chain_id,
        1,
        1,
        open_unix_ts as u64,
        60,
        21_000,
        GENESIS_BLOCK_HASH,
        [0x22u8; 32],
        [0x33u8; 32],
        acc,
        rome_zk_layouts::forced_empty_root(&rome_zk_merkle::keccak256),
    );
    let ix_a = sclient::post_root_proved_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &c.inbox_program,
        &c.treasury,
        sclient::PostRootFields {
            chain_id: c.chain_id,
            batch: 1,
            prev_batch: 0,
            pre_state_root: c.genesis_state_root,
            first_block: 1,
            last_block: 1,
            state_root: [0x33u8; 32],
            block_roots_merkle: [0u8; 32],
            inbox_commitment: acc,
            forced_outcome_commitment: rome_zk_layouts::forced_empty_root(
                &rome_zk_merkle::keccak256,
            ),
            parent_hash: GENESIS_BLOCK_HASH,
            last_block_hash: [0x22u8; 32],
            gas_in_batch: 21_000,
        },
        synthetic_layout1_proof_abi_with_vkey(14, &vkey, &pv_bytes),
        vec![],
    );
    let (err, cu) = send_capturing_cu(&mut ctx, std::slice::from_ref(&ix_a), &c.authority).await;
    let err = err.expect_err("a retired vkey must not prove");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::RegistryEntryNotFound as u32),
        "after the refused re-activation A must still be RegistryEntryNotFound for PostRootProved"
    );
    assert!(
        cu < 60_000,
        "a retired key must be refused before the pairing: got {cu} CU"
    );

    // Idempotent case: retiring an already-retired entry (RETIRED_SLOT -> RETIRED_SLOT) is still Ok.
    send(
        &mut ctx,
        &[sclient::set_registry_entry_ix(
            &settlement_program,
            &c.registry_authority.pubkey(),
            &payer.pubkey(),
            c.chain_id,
            sclient::RegistryEntry {
                curve: reg_layout::CURVE_BN254,
                scheme: reg_layout::SCHEME_PLONK,
                vkey_hash: vkey,
                layout_id: reg_layout::LAYOUT_ZISK_V1,
            },
            reg_layout::RETIRED_SLOT,
        )],
        &payer,
        &[&c.registry_authority],
    )
    .await
    .expect("retiring an already-retired entry again must stay idempotent Ok");
}

/// What "terminal" can and cannot promise on a 4-slot registry, pinned so the docs state a bound and not
/// an absolute: a retired vkey cannot be re-activated WHILE its tombstone occupies a slot — but once that
/// slot has been reused for a new key, the registry no longer remembers the retired vkey and a later
/// `SetRegistryEntry` naming it is an ordinary registration again. Keeping retired vkeys out of the
/// registry for good is the authority's off-chain list, not this account.
#[tokio::test]
async fn set_registry_entry_retired_vkey_re_registers_once_its_slot_has_been_reused() {
    let (mut pt, settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = 472;
    let open_unix_ts: i64 = 1_700_000_000;
    let acc = keccak::hashv(&[b"reuse bound inbox acc"]).to_bytes();
    let inbox_pda = sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 1);
    pt.add_account(
        inbox_pda,
        inbox_batch_account_with_open_ts(
            c.inbox_program,
            c.chain_id,
            1,
            settlement_program,
            c.authority.pubkey(),
            true,
            acc,
            open_unix_ts,
        ),
    );
    let mut ctx = pt.start_with_context().await;

    // Full from genesis: the two layout-2 entries, A (layout 1) and B (layout 1).
    let vkey_a = batch_guest_program_vk();
    let vkey_b = [0xBBu8; 32];
    let vkey_c = [0xCCu8; 32];
    let l1 = |vkey_hash: [u8; 32]| sclient::RegistryEntry {
        curve: reg_layout::CURVE_BN254,
        scheme: reg_layout::SCHEME_PLONK,
        vkey_hash,
        layout_id: reg_layout::LAYOUT_ZISK_V1,
    };
    let mut fields = default_init_chain_fields(&c, 16);
    fields.registry_entries = vec![
        sclient::RegistryEntry {
            curve: reg_layout::CURVE_BN254,
            scheme: reg_layout::SCHEME_PLONK,
            vkey_hash: [1u8; 32],
            layout_id: reg_layout::LAYOUT_HEADER_FALLBACK,
        },
        sclient::RegistryEntry {
            curve: reg_layout::CURVE_BN254,
            scheme: reg_layout::SCHEME_GROTH16,
            vkey_hash: [2u8; 32],
            layout_id: reg_layout::LAYOUT_HEADER_FALLBACK,
        },
        l1(vkey_a),
        l1(vkey_b),
    ];
    ensure_global_config(&mut ctx, &payer, &c).await;
    ensure_reserved_allowed(&mut ctx, &payer, &c).await;
    let init_ix = sclient::init_chain_reserved_ix(
        &settlement_program,
        &payer.pubkey(),
        &c.authority.pubkey(),
        &c.registry_authority.pubkey(),
        c.chain_id,
        fields,
    );
    send(
        &mut ctx,
        &[init_ix],
        &payer,
        &[&c.authority, &c.registry_authority],
    )
    .await
    .expect("InitChain with 4 distinct entries should succeed");
    let (registry_pda, _) = sclient::registry_pda(&settlement_program, c.chain_id);

    let set = |entry: sclient::RegistryEntry, slot: u64| {
        sclient::set_registry_entry_ix(
            &settlement_program,
            &c.registry_authority.pubkey(),
            &payer.pubkey(),
            c.chain_id,
            entry,
            slot,
        )
    };

    // Retire A, then register C: C reuses A's slot (index 2) and A's tombstone is gone.
    send(
        &mut ctx,
        &[set(l1(vkey_a), reg_layout::RETIRED_SLOT)],
        &payer,
        &[&c.registry_authority],
    )
    .await
    .expect("retiring A should succeed");
    let now = ctx.banks_client.get_root_slot().await.unwrap();
    send(
        &mut ctx,
        &[set(l1(vkey_c), now)],
        &payer,
        &[&c.registry_authority],
    )
    .await
    .expect("C should take A's retired slot");
    let reg = ctx
        .banks_client
        .get_account(registry_pda)
        .await
        .unwrap()
        .unwrap();
    let decoded = sclient::decode_registry_account(&reg.data).unwrap();
    assert_eq!(decoded.count, 4);
    assert_eq!(
        decoded.entries[2].vkey_hash, vkey_c,
        "C must sit where A's tombstone was"
    );
    assert!(
        decoded.entries.iter().all(|e| e.vkey_hash != vkey_a),
        "A is no longer in the registry"
    );

    // With the registry full and nothing retired, naming A again is an ordinary (refused) append.
    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let err = send(
        &mut ctx,
        &[set(l1(vkey_a), now)],
        &payer,
        &[&c.registry_authority],
    )
    .await
    .expect_err("registry is full");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::RegistryFull as u32),
        "a forgotten retired vkey is refused as RegistryFull, not EntryRetired — the tombstone is gone"
    );

    // Retire B and name A again: A re-registers into B's slot and proves. This is the bound.
    send(
        &mut ctx,
        &[set(l1(vkey_b), reg_layout::RETIRED_SLOT)],
        &payer,
        &[&c.registry_authority],
    )
    .await
    .expect("retiring B should succeed");
    let now = ctx.banks_client.get_root_slot().await.unwrap();
    send(
        &mut ctx,
        &[set(l1(vkey_a), now)],
        &payer,
        &[&c.registry_authority],
    )
    .await
    .expect("a retired vkey whose tombstone slot was reused re-registers like any new key");
    let reg = ctx
        .banks_client
        .get_account(registry_pda)
        .await
        .unwrap()
        .unwrap();
    let decoded = sclient::decode_registry_account(&reg.data).unwrap();
    assert_eq!(
        decoded.entries[3].vkey_hash, vkey_a,
        "A now sits in B's vacated slot"
    );
    assert!(!decoded.entries[3].retired);

    let forced = rome_zk_layouts::forced_empty_root(&rome_zk_merkle::keccak256);
    let pv_bytes = build_public_values_v2(
        c.chain_id,
        1,
        1,
        open_unix_ts as u64,
        60,
        21_000,
        GENESIS_BLOCK_HASH,
        [0x22u8; 32],
        [0x33u8; 32],
        acc,
        forced,
    );
    let ix_a = sclient::post_root_proved_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &c.inbox_program,
        &c.treasury,
        sclient::PostRootFields {
            chain_id: c.chain_id,
            batch: 1,
            prev_batch: 0,
            pre_state_root: c.genesis_state_root,
            first_block: 1,
            last_block: 1,
            state_root: [0x33u8; 32],
            block_roots_merkle: [0u8; 32],
            inbox_commitment: acc,
            forced_outcome_commitment: forced,
            parent_hash: GENESIS_BLOCK_HASH,
            last_block_hash: [0x22u8; 32],
            gas_in_batch: 21_000,
        },
        synthetic_layout1_proof_abi_with_vkey(14, &vkey_a, &pv_bytes),
        vec![],
    );
    let (err, cu) = send_capturing_cu(&mut ctx, std::slice::from_ref(&ix_a), &c.authority).await;
    let err = err.expect_err("synthetic proof must fail the pairing");
    assert_eq!(
        custom_error(&err),
        None,
        "re-registered A reaches the pairing again — the bound, pinned"
    );
    assert!(
        cu > 400_000,
        "expected the real pairing under re-registered A: got {cu} CU"
    );
    eprintln!(
        "bound: retired A re-registered after its slot was reused reaches the pairing: {cu} CU"
    );
}

/// Layout 2 (the header fallback) is unaffected by layout 1's arrival — a multi-block batch
/// is still `UnsupportedLayout` there (the restriction moved under this layout's own branch, but its
/// behaviour is unchanged).
#[tokio::test]
async fn post_root_proved_layout2_still_rejects_a_multi_block_batch() {
    let (pt, settlement_program, payer, mut c, _) = post_root_rig().await;
    c.chain_id = 23;
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await; // default registry_entries(): LAYOUT_HEADER_FALLBACK

    let acc = keccak::hashv(&[b"layout2 multiblock"]).to_bytes();
    let inbox_pda = sclient::inbox_batch_pda(&c.inbox_program, &settlement_program, c.chain_id, 1);
    ctx.set_account(
        &inbox_pda,
        &inbox_batch_account(
            c.inbox_program,
            c.chain_id,
            1,
            settlement_program,
            c.authority.pubkey(),
            true,
            acc,
        )
        .into(),
    );

    let args = sclient::PostRootFields {
        chain_id: c.chain_id,
        batch: 1,
        prev_batch: 0,
        pre_state_root: c.genesis_state_root,
        first_block: 1,
        last_block: 2, // multi-block — must still be UnsupportedLayout under layout 2
        state_root: [0u8; 32],
        block_roots_merkle: [0u8; 32],
        inbox_commitment: acc,
        forced_outcome_commitment: rome_zk_layouts::forced_empty_root(&rome_zk_merkle::keccak256),
        parent_hash: [0u8; 32],
        last_block_hash: [0u8; 32],
        gas_in_batch: 0,
    };
    let ix = sclient::post_root_proved_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &c.inbox_program,
        &c.treasury,
        args,
        zisk_proof_abi(14),
        vec![], // never parsed — the layout-2 multi-block guard rejects before header_rlp is touched
    );
    let err = send(&mut ctx, &[ix], &c.authority, &[])
        .await
        .expect_err("layout 2 must still refuse a multi-block batch");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::UnsupportedLayout as u32)
    );
}

/// Runs the real `veritas` program's own entrypoint against the same real block-14 proof
/// `PostRootProved` verifies internally — identical BPF bytecode, so this is a faithful, real-BPF CU
/// measurement of "the expensive part" `PostRootProved` pays once header-binding is satisfied (442,210
/// CU on SBPF v3; the earlier verifier measured 543,018 on devnet).
#[tokio::test]
async fn veritas_real_fixture_cu_is_within_budget() {
    let program_id = Pubkey::new_unique();
    let dir = rome_zk_testkit::sbf_out_dir();
    assert!(
        std::path::Path::new(&dir).join("veritas.so").exists(),
        "veritas.so not found — run cargo build-sbf for it first"
    );
    std::env::set_var("SBF_OUT_DIR", &dir);
    let mut pt = ProgramTest::new("veritas", program_id, None);
    pt.prefer_bpf(true);
    pt.set_compute_max_units(1_400_000);
    let mut ctx = pt.start_with_context().await;

    let ix = Instruction {
        program_id,
        accounts: vec![],
        data: zisk_proof_abi(14),
    };
    let payer = ctx.payer.insecure_clone();
    let (result, cu, _logs) =
        rome_zk_testkit::send_measuring_cu(&mut ctx, &[ix], &payer, &[]).await;
    eprintln!("veritas::verify_zisk (real block-14 proof) consumed {cu} CU");
    result.expect("the real fixture proof must verify");
    assert!(
        cu < 460_000,
        "verify_zisk CU {cu} must stay under the 460k pin (442,210 measured on SBPF v3 with veritas; the earlier verifier measured 541,225)"
    );
}

// ---------------------------------------------------------------------------------------------
// (7) ClosePending
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn close_pending_recycles_rent_but_not_the_head() {
    let (mut pt, settlement_program, payer, c, acc) = post_root_rig().await;
    let acc2 = keccak::hashv(&[b"inbox acc 2"]).to_bytes();
    add_batch2_inbox_account(&mut pt, &c, settlement_program, acc2);
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;
    post_first_batch(&mut ctx, settlement_program, &c, acc).await;

    // batch 1 is the head_pending_batch: closing it must fail even though nothing else is wrong yet
    // (it isn't final yet either, so this also independently fails NotFinal — check IsHeadPendingBatch
    // by finalizing first and confirming the "is head" rule still blocks it).
    let now = ctx.banks_client.get_root_slot().await.unwrap();
    ctx.warp_to_slot(now + c.challenge_window_slots as u64 + 2)
        .unwrap();
    send(
        &mut ctx,
        &[sclient::finalize_batch_ix(
            &settlement_program,
            c.chain_id,
            1,
            &[],
        )],
        &payer,
        &[],
    )
    .await
    .unwrap();

    let err = send(
        &mut ctx,
        &[sclient::close_pending_ix(
            &settlement_program,
            &c.authority.pubkey(),
            c.chain_id,
            1,
        )],
        &c.authority,
        &[],
    )
    .await
    .expect_err("closing the head pending batch must be rejected even when final");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::IsHeadPendingBatch as u32)
    );

    // post + finalize batch 2 so batch 1 is no longer the head, then close batch 1
    let args2 = sclient::PostRootFields {
        chain_id: c.chain_id,
        batch: 2,
        prev_batch: 1,
        pre_state_root: keccak::hashv(&[b"state root 1"]).to_bytes(),
        first_block: 2,
        last_block: 2,
        state_root: keccak::hashv(&[b"state root 2"]).to_bytes(),
        block_roots_merkle: [0u8; 32],
        inbox_commitment: acc2,
        forced_outcome_commitment: rome_zk_layouts::forced_empty_root(&rome_zk_merkle::keccak256),
        parent_hash: keccak::hashv(&[b"parent hash 2"]).to_bytes(),
        last_block_hash: keccak::hashv(&[b"block hash 2"]).to_bytes(),
        gas_in_batch: 0,
    };
    send(
        &mut ctx,
        &[sclient::post_root_ix(
            &settlement_program,
            &c.authority.pubkey(),
            &c.inbox_program,
            &c.treasury,
            args2,
        )
        .expect("reserved chain")],
        &c.authority,
        &[],
    )
    .await
    .unwrap();

    let before = ctx
        .banks_client
        .get_balance(c.authority.pubkey())
        .await
        .unwrap();
    let (pending_pda, _) = sclient::pending_pda(&settlement_program, c.chain_id, 1);
    let rent = ctx
        .banks_client
        .get_account(pending_pda)
        .await
        .unwrap()
        .unwrap()
        .lamports;
    let cu = send(
        &mut ctx,
        &[sclient::close_pending_ix(
            &settlement_program,
            &c.authority.pubkey(),
            c.chain_id,
            1,
        )],
        &c.authority,
        &[],
    )
    .await
    .expect("closing a final, non-head pending batch should succeed");
    eprintln!("ClosePending consumed {cu} CU");
    let after = ctx
        .banks_client
        .get_balance(c.authority.pubkey())
        .await
        .unwrap();
    assert!(after > before, "rent must be returned to the authority");
    assert!(
        after - before >= rent - 10_000,
        "returned amount must be close to the account's rent"
    );
    assert!(
        ctx.banks_client
            .get_account(pending_pda)
            .await
            .unwrap()
            .is_none(),
        "pending account must be gone"
    );

    let (root_pda, _) = sclient::root_pda(&settlement_program, c.chain_id);
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
        root.pending_count, 1,
        "pending_count must decrement on close"
    );
}

// ---------------------------------------------------------------------------------------------
// (9) full flow: InitChain -> (inbox) OpenBatch -> 3 SealLeaf -> FinalizeBatch(inbox) -> PostRoot ->
// warp -> FinalizeBatch(settlement) -> RootView ok -> inbox Close now succeeds against the final root.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn full_flow_inbox_to_settlement_to_close() {
    let settlement_program = Pubkey::new_unique();
    let inbox_program = Pubkey::new_unique();
    let mut pt = rome_zk_testkit::program_test(
        &[
            rome_zk_testkit::ProgramSpec::upgradeable("zk_settlement", settlement_program),
            rome_zk_testkit::ProgramSpec::new("zk_inbox", inbox_program),
        ],
        true,
    );
    let payer = funded_keypair();
    pt.add_account(payer.pubkey(), funded_account());
    // `default_chain`'s `chain_id` is always the constant 1 — see `inbox_open_batch_and_close_pass_
    // against_a_real_initchain_root_account`'s own comment on this same pattern.
    let chain_id = 1u64;
    let batch = 1u64;
    pt.add_account(
        zk_inbox_client::cursor_pda(&inbox_program, &settlement_program, chain_id).0,
        cursor_account(inbox_program, chain_id, batch),
    );
    let mut ctx = pt.start_with_context().await;

    let mut c = default_chain(settlement_program);
    c.inbox_program = inbox_program;
    c.authority = funded_keypair();
    assert_eq!(c.chain_id, chain_id);
    pt_add_extra_funding(&mut ctx, &c.authority.pubkey()).await;
    init_chain(&mut ctx, &payer, &c).await;

    let n = 3u32;
    send(
        &mut ctx,
        &[zk_inbox_client::open_batch_ix(
            &inbox_program,
            &c.authority.pubkey(),
            c.chain_id,
            batch,
            n,
            &settlement_program,
        )],
        &c.authority,
        &[],
    )
    .await
    .expect("inbox OpenBatch");
    let bodies: Vec<Vec<u8>> = (0..n)
        .map(|i| format!("full-flow chunk {i}").into_bytes())
        .collect();
    for (idx, body) in bodies.iter().enumerate() {
        let idx = idx as u32;
        let ixs = vec![
            zk_inbox_client::open_chunk_ix(
                &inbox_program,
                &c.authority.pubkey(),
                &settlement_program,
                c.chain_id,
                batch,
                idx,
                body.len() as u32,
            ),
            zk_inbox_client::write_chunk_ix(
                &inbox_program,
                &c.authority.pubkey(),
                &settlement_program,
                c.chain_id,
                batch,
                idx,
                0,
                body.clone(),
            ),
            zk_inbox_client::seal_chunk_ix(
                &inbox_program,
                &c.authority.pubkey(),
                &settlement_program,
                c.chain_id,
                batch,
                idx,
                body.len() as u32,
                zk_inbox_client::chunk_body_hash(body),
            ),
            zk_inbox_client::seal_leaf_ix(
                &inbox_program,
                &settlement_program,
                c.chain_id,
                batch,
                idx,
            ),
        ];
        send(&mut ctx, &ixs, &c.authority, &[])
            .await
            .expect("chunk open+write+seal");
    }
    send(
        &mut ctx,
        &[zk_inbox_client::finalize_batch_ix(
            &inbox_program,
            &c.authority.pubkey(),
            &settlement_program,
            c.chain_id,
            batch,
            0,
        )],
        &c.authority,
        &[],
    )
    .await
    .expect("inbox FinalizeBatch");

    let (inbox_batch_pda, _) =
        zk_inbox_client::batch_pda(&inbox_program, &settlement_program, c.chain_id, batch);
    let inbox_decoded = zk_inbox_client::decode_batch_account(
        &ctx.banks_client
            .get_account(inbox_batch_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap();
    assert!(inbox_decoded.finalized);

    let args = sclient::PostRootFields {
        chain_id: c.chain_id,
        batch,
        prev_batch: 0,
        pre_state_root: c.genesis_state_root,
        first_block: 1,
        last_block: 1,
        state_root: keccak::hashv(&[b"full flow state root"]).to_bytes(),
        block_roots_merkle: keccak::hashv(&[b"full flow block roots"]).to_bytes(),
        inbox_commitment: inbox_decoded.acc,
        forced_outcome_commitment: rome_zk_layouts::forced_empty_root(&rome_zk_merkle::keccak256),
        parent_hash: keccak::hashv(&[b"full flow parent hash"]).to_bytes(),
        last_block_hash: keccak::hashv(&[b"full flow block hash"]).to_bytes(),
        gas_in_batch: 0,
    };
    send(
        &mut ctx,
        &[sclient::post_root_ix(
            &settlement_program,
            &c.authority.pubkey(),
            &inbox_program,
            &c.treasury,
            args,
        )
        .expect("reserved chain")],
        &c.authority,
        &[],
    )
    .await
    .expect("PostRoot");

    let now = ctx.banks_client.get_root_slot().await.unwrap();
    ctx.warp_to_slot(now + c.challenge_window_slots as u64 + 2)
        .unwrap();
    send(
        &mut ctx,
        &[sclient::finalize_batch_ix(
            &settlement_program,
            c.chain_id,
            batch,
            &[],
        )],
        &payer,
        &[],
    )
    .await
    .expect("settlement FinalizeBatch");

    let recent = ctx.get_new_latest_blockhash().await.unwrap();
    let ix = sclient::root_view_ix(&settlement_program, c.chain_id, batch);
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer], recent);
    let sim = ctx.banks_client.simulate_transaction(tx).await.unwrap();
    assert!(
        sim.result.unwrap().is_ok(),
        "RootView must succeed once the batch is final"
    );

    let close_ix = zk_inbox_client::close_chunk_ix(
        &inbox_program,
        &c.authority.pubkey(),
        &settlement_program,
        c.chain_id,
        batch,
        0,
    );
    send(&mut ctx, &[close_ix], &c.authority, &[])
        .await
        .expect("inbox chunk Close must now succeed against the final root — the design's close-after-final, end to end across both programs");
}

#[tokio::test]
async fn finalize_batch_on_an_already_final_batch_advances_the_head_when_the_walk_was_skipped() {
    let (mut pt, settlement_program, payer, c, acc) = post_root_rig().await;

    // Batch 2's pending PDA, fabricated already `Final` — registered on `pt` *before*
    // `start_with_context` (post_root_rig's own doc: a post-start `ctx.set_account`'s lamports aren't
    // part of genesis supply, and this test's `warp_to_slot` would panic on the accounts-hash mismatch).
    let batch2_last_block = 2u64;
    let batch2_state_root = keccak::hashv(&[b"state root 2"]).to_bytes();
    let batch2_parent_hash = keccak::hashv(&[b"parent hash 2"]).to_bytes();
    let batch2_block_hash = keccak::hashv(&[b"block hash 2"]).to_bytes();
    let (pending2_pda, _) = sclient::pending_pda(&settlement_program, c.chain_id, 2);
    let mut d = vec![0u8; rome_zk_layouts::pending::PENDING_LEN];
    d[rome_zk_layouts::pending::OFF_BATCH..rome_zk_layouts::pending::OFF_BATCH + 8]
        .copy_from_slice(&2u64.to_le_bytes());
    d[rome_zk_layouts::pending::OFF_LAST_BLOCK..rome_zk_layouts::pending::OFF_LAST_BLOCK + 8]
        .copy_from_slice(&batch2_last_block.to_le_bytes());
    d[rome_zk_layouts::pending::OFF_STATE_ROOT..rome_zk_layouts::pending::OFF_STATE_ROOT + 32]
        .copy_from_slice(&batch2_state_root);
    d[rome_zk_layouts::pending::OFF_PARENT_HASH..rome_zk_layouts::pending::OFF_PARENT_HASH + 32]
        .copy_from_slice(&batch2_parent_hash);
    d[rome_zk_layouts::pending::OFF_LAST_BLOCK_HASH
        ..rome_zk_layouts::pending::OFF_LAST_BLOCK_HASH + 32]
        .copy_from_slice(&batch2_block_hash);
    d[rome_zk_layouts::pending::OFF_STATUS] = rome_zk_layouts::pending::STATUS_FINAL;
    pt.add_account(
        pending2_pda,
        Account {
            lamports: rent_exempt(d.len()),
            data: d,
            owner: settlement_program,
            executable: false,
            rent_epoch: 0,
        },
    );

    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;
    post_first_batch(&mut ctx, settlement_program, &c, acc).await;

    let now = ctx.banks_client.get_root_slot().await.unwrap();
    ctx.warp_to_slot(now + c.challenge_window_slots as u64 + 2)
        .unwrap();
    // Finalize batch 1 WITHOUT passing batch 2 as a trailing account: head_final stops at 1 even
    // though batch 2 is already Final (a proved batch posted out of order).
    send(
        &mut ctx,
        &[sclient::finalize_batch_ix(
            &settlement_program,
            c.chain_id,
            1,
            &[],
        )],
        &payer,
        &[],
    )
    .await
    .expect("FinalizeBatch(1) without a walk must succeed");
    let (root_pda, _) = sclient::root_pda(&settlement_program, c.chain_id);
    let read_root = |data: Vec<u8>| sclient::decode_root_account(&data).unwrap();
    let root = read_root(
        ctx.banks_client
            .get_account(root_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    );
    assert_eq!(
        root.head_final_batch, 1,
        "no trailing account → the head stops at 1"
    );

    // Liveness: the chain must be able to advance past the already-Final batch 2 by
    // a plain FinalizeBatch(2) — today this fails NotPending and head_final_batch is stuck forever.
    send(
        &mut ctx,
        &[sclient::finalize_batch_ix(
            &settlement_program,
            c.chain_id,
            2,
            &[],
        )],
        &payer,
        &[],
    )
    .await
    .expect(
        "FinalizeBatch(2) on an already-Final batch must advance the head, not fail NotPending",
    );
    let root = read_root(
        ctx.banks_client
            .get_account(root_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    );
    assert_eq!(
        root.head_final_batch, 2,
        "head must advance over the already-final batch 2"
    );
    assert_eq!(root.number, batch2_last_block);
    assert_eq!(root.state_root, batch2_state_root);
    assert_eq!(root.block_hash, batch2_block_hash);
    assert_eq!(
        root.pending_count, 0,
        "an already-final batch was never counted as pending"
    );

    // And it is idempotent-safe: a third call on batch 2 (now the head) must be rejected, not double-advance.
    let err = send(
        &mut ctx,
        &[sclient::finalize_batch_ix(
            &settlement_program,
            c.chain_id,
            2,
            &[],
        )],
        &payer,
        &[],
    )
    .await
    .expect_err("finalizing the current head again must be rejected");
    let _ = err;
}

// ============================= exit-config governance =============================
// `ProposeExitConfig` (26) + `ActivateExitConfig` (27): the chain authority proposes, with a delay >= the
// challenge window and no bootstrap exception; the bond is not enforced, a number only; the cap is in gwei,
// 0 = disabled.

/// Same chain-registration rig as [`init_chain`]'s other callers, with an explicit
/// `challenge_window_slots` (most exit-config tests want the default 5; the window-zero test needs 0).
/// `c.authority` is `root.authority` (the chain authority `ProposeExitConfig` must be signed by) and
/// `c.registry_authority` is a DIFFERENT key (`default_chain`) — exactly the Tiber-shaped distinction the
/// `propose_by_registry_authority_is_refused` test needs, with no extra setup.
async fn exit_config_rig(
    challenge_window_slots: u32,
) -> (
    solana_program_test::ProgramTestContext,
    Pubkey,
    Keypair,
    Chain,
) {
    // Fixed, not `Pubkey::new_unique()`: `ProposeExitConfig`/`ActivateExitConfig` both derive the
    // `exit_config`/root PDAs, so their measured CU wobbles with the bump-seed search depth against
    // a random program id — pin it exactly like the other CU-sensitive suites already
    // do (`rome_zk_testkit::fixed_settlement_program_id`) for a bit-exact, run-to-run reproducible
    // number.
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
    let c = Chain {
        challenge_window_slots,
        ..default_chain(settlement_program)
    };
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &c).await;
    (ctx, settlement_program, payer, c)
}

/// Builds the real V1 (SIMD-0385) wire shape for `ix` (same pattern as `rome-zk-prover::poster`'s own
/// gate test) and asserts it fits the 4,096-byte envelope — a legacy/ program-test `Transaction` does
/// not reflect the real V1 header this asserts against. `fee_payer` and `extra_signers` are
/// `solana_sdk::signature::Keypair` test keys, converted to the `solana_keypair::Keypair` by their raw
/// secret bytes (both wrap the same ed25519 keypair layout) — never a second, hand-rolled signing key.
fn assert_v1_tx_fits_envelope(ix: &Instruction, fee_payer: &Keypair, extra_signers: &[&Keypair]) {
    use solana_signer::Signer as V1Signer;

    let to_v1_keypair = |kp: &Keypair| -> solana_keypair::Keypair {
        let secret: [u8; 32] = kp.to_bytes()[..32].try_into().unwrap();
        solana_keypair::Keypair::new_from_array(secret)
    };
    let payer_v1 = to_v1_keypair(fee_payer);
    let extra_v1: Vec<solana_keypair::Keypair> =
        extra_signers.iter().map(|k| to_v1_keypair(k)).collect();

    let v1_ix = rome_zk_solana_sender::compat::to_v1_instruction(ix);
    let config = solana_message::v1::TransactionConfig::empty()
        .with_compute_unit_limit(30_000)
        .with_loaded_accounts_data_size_limit(256 * 1024);
    let message = solana_message::v1::Message::try_compile_with_config(
        &payer_v1.pubkey(),
        &[v1_ix],
        solana_hash::Hash::default(),
        config,
    )
    .expect("the exit-config V1 message must compile");

    let mut signers: Vec<&solana_keypair::Keypair> = vec![&payer_v1];
    signers.extend(extra_v1.iter());
    let tx = solana_transaction::versioned::VersionedTransaction::try_new(
        solana_message::VersionedMessage::V1(message),
        &signers,
    )
    .expect("signing the exit-config V1 message must succeed");
    let bytes = wincode::serialize(&tx).expect("a signed V1 transaction always serializes");
    eprintln!("exit-config V1 tx is {} signed bytes", bytes.len());
    assert!(
        bytes.len() <= 4_096,
        "exit-config V1 tx is {} bytes, over the 4,096-byte SIMD-0385 envelope",
        bytes.len()
    );
}

/// `ProposeExitConfig` signed by the registry authority (not `root.authority`) must be refused, and
/// must not create the `exit_config` account (0 writes) — the Tiber-shaped distinction this rig's
/// `default_chain` already carries (`c.authority` != `c.registry_authority`).
#[tokio::test]
async fn propose_by_registry_authority_is_refused() {
    let (mut ctx, settlement_program, payer, c) = exit_config_rig(5).await;
    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let activation_slot = now + c.challenge_window_slots as u64 + 10;
    let ix = sclient::propose_exit_config_ix(
        &settlement_program,
        &c.registry_authority.pubkey(),
        &payer.pubkey(),
        c.chain_id,
        Some([7u8; 20]),
        None,
        None,
        None,
        activation_slot,
    );
    let err = send(&mut ctx, &[ix], &payer, &[&c.registry_authority])
        .await
        .expect_err(
            "ProposeExitConfig signed by the registry authority, not root.authority, must be refused",
        );
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::WrongChainAuthority as u32)
    );
    let (exit_config_pda, _) = sclient::exit_config_pda(&settlement_program, c.chain_id);
    assert!(
        ctx.banks_client
            .get_account(exit_config_pda)
            .await
            .unwrap()
            .is_none(),
        "a refused ProposeExitConfig must not create the exit_config account"
    );
}

/// An `activation_slot` one slot below `Clock::slot + challenge_window_slots` must be refused — the
/// delay must be at least one FULL challenge window, not "almost one".
#[tokio::test]
async fn propose_activation_below_window_is_refused() {
    let (mut ctx, settlement_program, payer, c) = exit_config_rig(5).await;
    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let activation_slot = now + c.challenge_window_slots as u64 - 1;
    let ix = sclient::propose_exit_config_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &payer.pubkey(),
        c.chain_id,
        Some([7u8; 20]),
        None,
        None,
        None,
        activation_slot,
    );
    let err = send(&mut ctx, &[ix], &payer, &[&c.authority])
        .await
        .expect_err(
            "an activation_slot below Clock::slot + challenge_window_slots must be refused",
        );
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::ActivationTooSoon as u32)
    );
}

/// A chain whose `root.challenge_window_slots == 0` can never configure exits (fail-closed)
/// — refused before the activation-delay check even runs.
#[tokio::test]
async fn propose_with_challenge_window_zero_is_refused() {
    let (mut ctx, settlement_program, payer, c) = exit_config_rig(0).await;
    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let ix = sclient::propose_exit_config_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &payer.pubkey(),
        c.chain_id,
        Some([7u8; 20]),
        None,
        None,
        None,
        now + 1_000,
    );
    let err = send(&mut ctx, &[ix], &payer, &[&c.authority])
        .await
        .expect_err("a chain whose challenge_window_slots == 0 must refuse ProposeExitConfig");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::ChallengeWindowZero as u32)
    );
}

/// A second `ProposeExitConfig` while one is still pending (not yet activated) must be refused — one
/// proposal in flight at a time; activate it or wait, then propose again (no cancel path in this program).
#[tokio::test]
async fn second_proposal_while_pending_is_refused() {
    let (mut ctx, settlement_program, payer, c) = exit_config_rig(5).await;
    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let activation_slot = now + c.challenge_window_slots as u64 + 10;
    let ix1 = sclient::propose_exit_config_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &payer.pubkey(),
        c.chain_id,
        Some([7u8; 20]),
        None,
        None,
        None,
        activation_slot,
    );
    send(&mut ctx, &[ix1], &payer, &[&c.authority])
        .await
        .expect("the first ProposeExitConfig should succeed");

    let ix2 = sclient::propose_exit_config_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &payer.pubkey(),
        c.chain_id,
        Some([8u8; 20]),
        None,
        None,
        None,
        activation_slot + 1,
    );
    let err = send(&mut ctx, &[ix2], &payer, &[&c.authority])
        .await
        .expect_err("a second ProposeExitConfig while one is pending must be refused");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::PendingExitConfigExists as u32)
    );
}

/// An attacker donating lamports to the `exit_config` PDA before the real
/// `ProposeExitConfig` lands must not block it — the account is ADOPTED (create-or-adopt), not refused.
#[tokio::test]
async fn prefunded_exit_config_pda_is_adopted() {
    let (mut ctx, settlement_program, payer, c) = exit_config_rig(5).await;
    let (exit_config_pda, _) = sclient::exit_config_pda(&settlement_program, c.chain_id);
    prefund_pda(&mut ctx, exit_config_pda).await;
    let donated = ctx
        .banks_client
        .get_account(exit_config_pda)
        .await
        .unwrap()
        .expect("prefund must have created a system-owned exit_config account")
        .lamports;
    assert!(donated > 0);

    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let activation_slot = now + c.challenge_window_slots as u64 + 10;
    let ix = sclient::propose_exit_config_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &payer.pubkey(),
        c.chain_id,
        Some([7u8; 20]),
        None,
        None,
        None,
        activation_slot,
    );
    send(&mut ctx, &[ix], &payer, &[&c.authority])
        .await
        .expect("ProposeExitConfig against a pre-funded exit_config PDA should adopt it");

    let acc = ctx
        .banks_client
        .get_account(exit_config_pda)
        .await
        .unwrap()
        .expect("exit_config account must exist");
    assert_eq!(acc.owner, settlement_program);
    assert_eq!(acc.data.len(), rome_zk_layouts::exit::exit_config::LEN);
    assert!(acc.lamports >= donated);
    let f = sclient::decode_exit_config_account(&acc.data).unwrap();
    assert_eq!(f.pending_exit_portal, [7u8; 20]);
    assert_eq!(
        f.pending_mask,
        rome_zk_layouts::exit::exit_config::PENDING_MASK_PORTAL
    );
}

/// A valid proposal writes only the PENDING slots and `activation_slot` — the CURRENT
/// `exit_portal`/`bridge_program` fields stay untouched (zero, on a brand-new account) until
/// `ActivateExitConfig` copies the proposal across; a proposal is invisible to `ProveExit` until then.
/// Also the CU gate for `ProposeExitConfig` and the tx-size gate.
#[tokio::test]
async fn propose_writes_pending_not_current() {
    let (mut ctx, settlement_program, payer, c) = exit_config_rig(5).await;
    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let activation_slot = now + c.challenge_window_slots as u64 + 10;
    let bridge_program = Pubkey::new_unique();
    let ix = sclient::propose_exit_config_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &payer.pubkey(),
        c.chain_id,
        Some([7u8; 20]),
        Some(bridge_program),
        Some(1_000),
        Some(2_000),
        activation_slot,
    );
    assert_v1_tx_fits_envelope(&ix, &payer, &[&c.authority]);
    let cu = send(&mut ctx, &[ix], &payer, &[&c.authority])
        .await
        .expect("a valid ProposeExitConfig should succeed");
    eprintln!("ProposeExitConfig consumed {cu} CU");
    assert!(
        cu <= 30_000,
        "ProposeExitConfig CU {cu} exceeds the 30k budget"
    );

    let (exit_config_pda, _) = sclient::exit_config_pda(&settlement_program, c.chain_id);
    let acc = ctx
        .banks_client
        .get_account(exit_config_pda)
        .await
        .unwrap()
        .unwrap();
    let f = sclient::decode_exit_config_account(&acc.data).unwrap();
    assert_eq!(
        f.exit_portal, [0u8; 20],
        "current portal must stay untouched until activation"
    );
    assert_eq!(
        f.bridge_program,
        Pubkey::default(),
        "current bridge program must stay untouched until activation"
    );
    assert_eq!(f.pending_exit_portal, [7u8; 20]);
    assert_eq!(f.pending_bridge_program, bridge_program);
    assert_eq!(f.pending_exit_cap, 1_000);
    assert_eq!(f.pending_poster_bond, 2_000);
    assert_eq!(f.activation_slot, activation_slot);
    let expect_mask = rome_zk_layouts::exit::exit_config::PENDING_MASK_PORTAL
        | rome_zk_layouts::exit::exit_config::PENDING_MASK_BRIDGE
        | rome_zk_layouts::exit::exit_config::PENDING_MASK_CAP
        | rome_zk_layouts::exit::exit_config::PENDING_MASK_BOND;
    assert_eq!(f.pending_mask, expect_mask);
}

/// `ActivateExitConfig` before `Clock::slot >= activation_slot` must be refused.
#[tokio::test]
async fn activate_before_slot_is_refused() {
    let (mut ctx, settlement_program, payer, c) = exit_config_rig(5).await;
    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let activation_slot = now + c.challenge_window_slots as u64 + 10;
    let propose_ix = sclient::propose_exit_config_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &payer.pubkey(),
        c.chain_id,
        Some([7u8; 20]),
        None,
        Some(1_000),
        None,
        activation_slot,
    );
    send(&mut ctx, &[propose_ix], &payer, &[&c.authority])
        .await
        .expect("ProposeExitConfig should succeed");

    let activate_ix = sclient::activate_exit_config_ix(&settlement_program, c.chain_id);
    let err = send(&mut ctx, &[activate_ix], &payer, &[])
        .await
        .expect_err("ActivateExitConfig before the activation slot must be refused");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::ActivationNotReached as u32)
    );
}

/// `ActivateExitConfig` against an `exit_config` with `pending_mask == 0` (nothing proposed, or a
/// proposal already activated) must be refused.
#[tokio::test]
async fn activate_with_no_pending_is_refused() {
    let (mut ctx, settlement_program, payer, c) = exit_config_rig(5).await;
    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let activation_slot = now + c.challenge_window_slots as u64 + 10;
    let propose_ix = sclient::propose_exit_config_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &payer.pubkey(),
        c.chain_id,
        Some([7u8; 20]),
        None,
        Some(1_000),
        None,
        activation_slot,
    );
    send(&mut ctx, &[propose_ix], &payer, &[&c.authority])
        .await
        .expect("ProposeExitConfig should succeed");

    ctx.warp_to_slot(activation_slot).unwrap();
    let activate_ix = sclient::activate_exit_config_ix(&settlement_program, c.chain_id);
    send(&mut ctx, std::slice::from_ref(&activate_ix), &payer, &[])
        .await
        .expect("the first ActivateExitConfig should succeed");

    let err = send(&mut ctx, &[activate_ix], &payer, &[])
        .await
        .expect_err("a second ActivateExitConfig with nothing pending must be refused");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::NoPendingExitConfig as u32)
    );
}

/// Once the activation slot is reached, `ActivateExitConfig` copies the pending exit portal into
/// `exit_config`'s CURRENT field and the pending cap into the ROOT's `exit_cap_per_window` (the root
/// stays the source of truth for the cap/bond, unchanged layout), clearing every pending
/// slot and the mask. Also the CU gate for `ActivateExitConfig` and its tx-size gate.
#[tokio::test]
async fn activate_after_slot_writes_root_cap_and_config_portal() {
    let (mut ctx, settlement_program, payer, c) = exit_config_rig(5).await;
    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let activation_slot = now + c.challenge_window_slots as u64 + 10;
    let propose_ix = sclient::propose_exit_config_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &payer.pubkey(),
        c.chain_id,
        Some([7u8; 20]),
        None,
        Some(1_000),
        Some(2_000),
        activation_slot,
    );
    send(&mut ctx, &[propose_ix], &payer, &[&c.authority])
        .await
        .expect("ProposeExitConfig should succeed");

    ctx.warp_to_slot(activation_slot).unwrap();
    let activate_ix = sclient::activate_exit_config_ix(&settlement_program, c.chain_id);
    assert_v1_tx_fits_envelope(&activate_ix, &payer, &[]);
    let cu = send(&mut ctx, &[activate_ix], &payer, &[])
        .await
        .expect("ActivateExitConfig at the activation slot should succeed");
    eprintln!("ActivateExitConfig consumed {cu} CU");
    assert!(
        cu <= 30_000,
        "ActivateExitConfig CU {cu} exceeds the 30k budget"
    );

    let (root_pda, _) = sclient::root_pda(&settlement_program, c.chain_id);
    let root = sclient::decode_root_account(
        &ctx.banks_client
            .get_account(root_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap();
    assert_eq!(root.exit_cap_per_window, 1_000);
    assert_eq!(root.poster_bond, 2_000);

    let (exit_config_pda, _) = sclient::exit_config_pda(&settlement_program, c.chain_id);
    let f = sclient::decode_exit_config_account(
        &ctx.banks_client
            .get_account(exit_config_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap();
    assert_eq!(f.exit_portal, [7u8; 20]);
    assert_eq!(f.pending_mask, 0);
    assert_eq!(f.pending_exit_portal, [0u8; 20]);
    assert_eq!(f.pending_exit_cap, 0);
    assert_eq!(f.pending_poster_bond, 0);
    assert_eq!(f.activation_slot, 0);
}

/// `ActivateExitConfig` is permissionless — a fee payer wholly unrelated to the chain authority or
/// registry authority, signing alone, activates a due proposal successfully.
#[tokio::test]
async fn activate_is_permissionless() {
    let (mut ctx, settlement_program, payer, c) = exit_config_rig(5).await;
    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let activation_slot = now + c.challenge_window_slots as u64 + 10;
    let propose_ix = sclient::propose_exit_config_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &payer.pubkey(),
        c.chain_id,
        Some([7u8; 20]),
        None,
        None,
        None,
        activation_slot,
    );
    send(&mut ctx, &[propose_ix], &payer, &[&c.authority])
        .await
        .expect("ProposeExitConfig should succeed");

    ctx.warp_to_slot(activation_slot).unwrap();
    let stranger = funded_keypair();
    ctx.set_account(
        &stranger.pubkey(),
        &solana_sdk::account::AccountSharedData::from(funded_account()),
    );
    let activate_ix = sclient::activate_exit_config_ix(&settlement_program, c.chain_id);
    send(&mut ctx, &[activate_ix], &stranger, &[]).await.expect(
        "ActivateExitConfig must succeed when sent by a fee payer unrelated to either authority",
    );
}

/// Governance must be RE-governable — a SECOND `ProposeExitConfig`, made after the
/// first proposal's own cycle has already been activated, must succeed and its new values must be the
/// ones a later `ActivateExitConfig` installs. `exit_config` is never closed by `ActivateExitConfig`, so
/// on this second cycle the account is already program-owned and `exit_config::LEN`-sized —
/// `propose_exit_config` must not walk that state into `create_or_adopt_pda`'s `adopt_funded_pda` path,
/// whose `*pda.owner != system_program::id()` guard has no way to tell "already ours from a prior cycle"
/// apart from "griefed" and refuses either one.
#[tokio::test]
async fn repeat_governance_second_propose_after_activate() {
    let (mut ctx, settlement_program, payer, c) = exit_config_rig(5).await;

    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let activation_slot_1 = now + c.challenge_window_slots as u64 + 10;
    let propose_1 = sclient::propose_exit_config_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &payer.pubkey(),
        c.chain_id,
        Some([7u8; 20]),
        None,
        Some(1_000),
        None,
        activation_slot_1,
    );
    send(&mut ctx, &[propose_1], &payer, &[&c.authority])
        .await
        .expect("the first ProposeExitConfig should succeed");

    ctx.warp_to_slot(activation_slot_1).unwrap();
    let activate_1 = sclient::activate_exit_config_ix(&settlement_program, c.chain_id);
    send(&mut ctx, &[activate_1], &payer, &[])
        .await
        .expect("the first ActivateExitConfig should succeed");

    // A brand-new later governance cycle — nothing pending, the account already exists.
    let now2 = ctx.banks_client.get_root_slot().await.unwrap();
    let activation_slot_2 = now2 + c.challenge_window_slots as u64 + 10;
    let propose_2 = sclient::propose_exit_config_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &payer.pubkey(),
        c.chain_id,
        Some([9u8; 20]),
        None,
        Some(2_000),
        None,
        activation_slot_2,
    );
    send(&mut ctx, &[propose_2], &payer, &[&c.authority])
        .await
        .expect("a SECOND ProposeExitConfig after activation must succeed");

    ctx.warp_to_slot(activation_slot_2).unwrap();
    let activate_2 = sclient::activate_exit_config_ix(&settlement_program, c.chain_id);
    send(&mut ctx, &[activate_2], &payer, &[])
        .await
        .expect("the second ActivateExitConfig should succeed");

    let (root_pda, _) = sclient::root_pda(&settlement_program, c.chain_id);
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
        root.exit_cap_per_window, 2_000,
        "the second cycle's cap must be the one live after its own activation"
    );

    let (exit_config_pda, _) = sclient::exit_config_pda(&settlement_program, c.chain_id);
    let f = sclient::decode_exit_config_account(
        &ctx.banks_client
            .get_account(exit_config_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap();
    assert_eq!(
        f.exit_portal, [9u8; 20],
        "the second cycle's portal must be the one live after its own activation"
    );
}

/// A `ProposeExitConfig` whose four optional fields are ALL `None` proposes nothing —
/// refuse it by name instead of writing an inert `exit_config` (`pending_mask == 0`) that
/// `ActivateExitConfig` can only ever refuse (`NoPendingExitConfig`) after spending the chain authority's
/// rent on a no-op.
#[tokio::test]
async fn propose_all_none_is_refused() {
    let (mut ctx, settlement_program, payer, c) = exit_config_rig(5).await;
    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let activation_slot = now + c.challenge_window_slots as u64 + 10;
    let ix = sclient::propose_exit_config_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &payer.pubkey(),
        c.chain_id,
        None,
        None,
        None,
        None,
        activation_slot,
    );
    let err = send(&mut ctx, &[ix], &payer, &[&c.authority])
        .await
        .expect_err("a ProposeExitConfig with every optional field None must be refused");
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::InvalidArgument)
    );
    let (exit_config_pda, _) = sclient::exit_config_pda(&settlement_program, c.chain_id);
    assert!(
        ctx.banks_client
            .get_account(exit_config_pda)
            .await
            .unwrap()
            .is_none(),
        "a refused all-None ProposeExitConfig must not create the exit_config account"
    );
}

/// Control for `propose_all_none_is_refused`: a proposal with exactly one field set (the cap)
/// still succeeds — the all-None refusal must not over-refuse a legitimate single-field proposal.
#[tokio::test]
async fn propose_single_field_succeeds() {
    let (mut ctx, settlement_program, payer, c) = exit_config_rig(5).await;
    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let activation_slot = now + c.challenge_window_slots as u64 + 10;
    let ix = sclient::propose_exit_config_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &payer.pubkey(),
        c.chain_id,
        None,
        None,
        Some(500),
        None,
        activation_slot,
    );
    send(&mut ctx, &[ix], &payer, &[&c.authority])
        .await
        .expect("a proposal with only the cap set must still succeed");
    let (exit_config_pda, _) = sclient::exit_config_pda(&settlement_program, c.chain_id);
    let f = sclient::decode_exit_config_account(
        &ctx.banks_client
            .get_account(exit_config_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap();
    assert_eq!(f.pending_exit_cap, 500);
    assert_eq!(
        f.pending_mask,
        rome_zk_layouts::exit::exit_config::PENDING_MASK_CAP
    );
}

// ---------------------------------------------------------------------------------------------
// INBOX-THIRD-PARTY-HALT: a third party's own settlement deployment against the shared inbox.
//
// The inbox keys a chain's batch cursor and batch accounts by chain id alone and takes the chain's
// authority from a root account under whichever settlement program the caller names. The attacker here
// needs nothing injected: he deploys his OWN copy of zk-settlement (a second program id), runs a genuine
// `InitChain` for the victim's chain id on it (so a genuine root with HIS authority exists at
// `["root", chain_id]` under his program), and then drives the shared inbox through it.
//
// Every test asserts the SAFE behaviour, so it is red while the defect exists. No program code changes here.
// ---------------------------------------------------------------------------------------------

struct TwoDeployments {
    ctx: solana_program_test::ProgramTestContext,
    inbox: Pubkey,
    /// The chain's real settlement program, chain and authority.
    victim: Chain,
    /// The attacker's own settlement deployment, with a genuine `InitChain` for the same chain id.
    attacker: Chain,
}

async fn two_deployments() -> TwoDeployments {
    let real_settlement = Pubkey::new_unique();
    let attacker_settlement = Pubkey::new_unique();
    let inbox = Pubkey::new_unique();
    let payer = funded_keypair();
    let mut pt = rome_zk_testkit::program_test(
        &[
            rome_zk_testkit::ProgramSpec::upgradeable("zk_settlement", real_settlement),
            rome_zk_testkit::ProgramSpec::upgradeable("zk_settlement", attacker_settlement),
            rome_zk_testkit::ProgramSpec::new("zk_inbox", inbox),
        ],
        true,
    );
    let mut victim = default_chain(real_settlement);
    victim.inbox_program = inbox;
    let mut attacker = default_chain(attacker_settlement);
    attacker.inbox_program = inbox;
    assert_eq!(victim.chain_id, attacker.chain_id);
    pt.add_account(payer.pubkey(), funded_account());
    pt.add_account(victim.authority.pubkey(), funded_account());
    pt.add_account(attacker.authority.pubkey(), funded_account());
    let mut ctx = pt.start_with_context().await;
    init_chain(&mut ctx, &payer, &victim).await;
    init_chain(&mut ctx, &payer, &attacker).await;
    TwoDeployments {
        ctx,
        inbox,
        victim,
        attacker,
    }
}

/// Opens `batch` for `c` (naming `c.settlement_program`), seals one chunk and finalizes it, signed by
/// `c.authority`. Stops at the first refusal and returns it.
async fn open_and_finalize_one_chunk_batch(
    ctx: &mut solana_program_test::ProgramTestContext,
    inbox: &Pubkey,
    c: &Chain,
    batch: u64,
) -> Result<(), TransactionError> {
    let a = &c.authority;
    let body = format!("chunk of batch {batch}").into_bytes();
    send(
        ctx,
        &[zk_inbox_client::open_batch_ix(
            inbox,
            &a.pubkey(),
            c.chain_id,
            batch,
            1,
            &c.settlement_program,
        )],
        a,
        &[],
    )
    .await?;
    send(
        ctx,
        &[
            zk_inbox_client::open_chunk_ix(
                inbox,
                &a.pubkey(),
                &c.settlement_program,
                c.chain_id,
                batch,
                0,
                body.len() as u32,
            ),
            zk_inbox_client::write_chunk_ix(
                inbox,
                &a.pubkey(),
                &c.settlement_program,
                c.chain_id,
                batch,
                0,
                0,
                body.clone(),
            ),
            zk_inbox_client::seal_chunk_ix(
                inbox,
                &a.pubkey(),
                &c.settlement_program,
                c.chain_id,
                batch,
                0,
                body.len() as u32,
                zk_inbox_client::chunk_body_hash(&body),
            ),
            zk_inbox_client::seal_leaf_ix(inbox, &c.settlement_program, c.chain_id, batch, 0),
        ],
        a,
        &[],
    )
    .await?;
    send(
        ctx,
        &[zk_inbox_client::finalize_batch_ix(
            inbox,
            &a.pubkey(),
            &c.settlement_program,
            c.chain_id,
            batch,
            0,
        )],
        a,
        &[],
    )
    .await?;
    Ok(())
}

async fn inbox_batch_acc(
    ctx: &mut solana_program_test::ProgramTestContext,
    inbox: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
    batch: u64,
) -> Option<[u8; 32]> {
    let acct = ctx
        .banks_client
        .get_account(zk_inbox_client::batch_pda(inbox, settlement_program, chain_id, batch).0)
        .await
        .unwrap()?;
    Some(zk_inbox_client::decode_batch_account(&acct.data).ok()?.acc)
}

async fn cursor_next(
    ctx: &mut solana_program_test::ProgramTestContext,
    inbox: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
) -> u64 {
    let acct = ctx
        .banks_client
        .get_account(zk_inbox_client::cursor_pda(inbox, settlement_program, chain_id).0)
        .await
        .unwrap()
        .expect("cursor exists");
    zk_inbox_client::decode_batch_cursor(&acct.data)
        .unwrap()
        .next_batch
}

fn post_root_for(c: &Chain, batch: u64, acc: [u8; 32]) -> sclient::PostRootFields {
    sclient::PostRootFields {
        chain_id: c.chain_id,
        batch,
        prev_batch: batch - 1,
        pre_state_root: c.genesis_state_root,
        first_block: 1,
        last_block: 1,
        state_root: keccak::hashv(&[b"state root after the attack"]).to_bytes(),
        block_roots_merkle: [0u8; 32],
        inbox_commitment: acc,
        forced_outcome_commitment: rome_zk_layouts::forced_empty_root(&rome_zk_merkle::keccak256),
        parent_hash: keccak::hashv(&[b"parent hash"]).to_bytes(),
        last_block_hash: keccak::hashv(&[b"block hash"]).to_bytes(),
        gas_in_batch: 0,
    }
}

async fn victim_post_root(
    w: &mut TwoDeployments,
    batch: u64,
    acc: [u8; 32],
) -> Result<u64, TransactionError> {
    let ix = sclient::post_root_ix(
        &w.victim.settlement_program,
        &w.victim.authority.pubkey(),
        &w.inbox,
        &w.victim.treasury,
        post_root_for(&w.victim, batch, acc),
    )
    .expect("reserved chain");
    send(&mut w.ctx, &[ix], &w.victim.authority, &[]).await
}

/// Third party burns the victim's first batch id through his own settlement deployment (opens it, then
/// abandons it). The attack's own outcome is not asserted here; the tests assert what it does to the victim.
async fn attacker_burns_batch(w: &mut TwoDeployments, batch: u64) {
    let open = zk_inbox_client::open_batch_ix(
        &w.inbox,
        &w.attacker.authority.pubkey(),
        w.attacker.chain_id,
        batch,
        1,
        &w.attacker.settlement_program,
    );
    let _ = send(&mut w.ctx, &[open], &w.attacker.authority, &[]).await;
    let abandon = zk_inbox_client::abandon_batch_ix(
        &w.inbox,
        &w.attacker.authority.pubkey(),
        &w.attacker.settlement_program,
        w.attacker.chain_id,
        batch,
    );
    let _ = send(&mut w.ctx, &[abandon], &w.attacker.authority, &[]).await;
}

/// The attacker's deployment initialises the victim chain id's cursor first, far above any real batch id.
/// SAFE behaviour: the chain still bootstraps, opens, and settles its first batch.
#[tokio::test]
async fn third_party_cursor_init_first_does_not_stop_the_chain_posting_its_first_batch() {
    let mut w = two_deployments().await;
    let attack = zk_inbox_client::init_batch_cursor_ix(
        &w.inbox,
        &w.attacker.authority.pubkey(),
        w.attacker.chain_id,
        u64::MAX,
        &w.attacker.settlement_program,
    );
    let _ = send(&mut w.ctx, &[attack], &w.attacker.authority, &[]).await;

    let init = zk_inbox_client::init_batch_cursor_ix(
        &w.inbox,
        &w.victim.authority.pubkey(),
        w.victim.chain_id,
        1,
        &w.victim.settlement_program,
    );
    send(&mut w.ctx, &[init], &w.victim.authority, &[])
        .await
        .expect("the real chain's own InitBatchCursor was refused");
    let inbox = w.inbox;
    let c = clone_chain(&w.victim);
    open_and_finalize_one_chunk_batch(&mut w.ctx, &inbox, &c, 1)
        .await
        .expect("the real chain could not open and finalize its first batch");
    let acc = inbox_batch_acc(&mut w.ctx, &inbox, &c.settlement_program, c.chain_id, 1)
        .await
        .unwrap();
    victim_post_root(&mut w, 1, acc)
        .await
        .expect("settlement refused the real chain's first batch");
}

/// The attacker's deployment burns the victim's first batch id after the victim bootstrapped its cursor.
/// SAFE behaviour: the real chain still opens, finalizes and posts its first batch.
#[tokio::test]
async fn third_party_burning_the_first_batch_id_does_not_stop_the_chain_posting_it() {
    let mut w = two_deployments().await;
    let init = zk_inbox_client::init_batch_cursor_ix(
        &w.inbox,
        &w.victim.authority.pubkey(),
        w.victim.chain_id,
        1,
        &w.victim.settlement_program,
    );
    send(&mut w.ctx, &[init], &w.victim.authority, &[])
        .await
        .expect("victim InitBatchCursor");

    attacker_burns_batch(&mut w, 1).await;

    let c = clone_chain(&w.victim);
    let inbox = w.inbox;
    open_and_finalize_one_chunk_batch(&mut w.ctx, &inbox, &c, 1)
        .await
        .expect("the real chain cannot open its first batch: a third party consumed id 1");
    let acc = inbox_batch_acc(&mut w.ctx, &inbox, &c.settlement_program, c.chain_id, 1)
        .await
        .unwrap();
    victim_post_root(&mut w, 1, acc)
        .await
        .expect("settlement refused the real chain's first batch");
}

/// Same attack, but the victim does the only other thing it can: it follows its cursor and uses whatever id
/// the cursor hands out next. Settlement posts only `head_pending + 1`, so this must still end with the
/// chain's batch accepted. SAFE behaviour: the cursor hands out batch 1 (the attack did not move it).
#[tokio::test]
async fn real_chain_following_its_cursor_after_a_third_party_burn_still_settles() {
    let mut w = two_deployments().await;
    let init = zk_inbox_client::init_batch_cursor_ix(
        &w.inbox,
        &w.victim.authority.pubkey(),
        w.victim.chain_id,
        1,
        &w.victim.settlement_program,
    );
    send(&mut w.ctx, &[init], &w.victim.authority, &[])
        .await
        .expect("victim InitBatchCursor");

    attacker_burns_batch(&mut w, 1).await;

    let (inbox, chain_id) = (w.inbox, w.victim.chain_id);
    let next = cursor_next(&mut w.ctx, &inbox, &w.victim.settlement_program, chain_id).await;
    let c = clone_chain(&w.victim);
    open_and_finalize_one_chunk_batch(&mut w.ctx, &inbox, &c, next)
        .await
        .expect("the real chain cannot open the batch its own cursor names");
    let acc = inbox_batch_acc(
        &mut w.ctx,
        &inbox,
        &w.victim.settlement_program,
        chain_id,
        next,
    )
    .await
    .unwrap();
    // `PostRoot` for batch `next` needs `prev_batch == head_pending == 0`, so build it directly.
    let mut args = post_root_for(&w.victim, next, acc);
    args.prev_batch = 0;
    let ix = sclient::post_root_ix(
        &w.victim.settlement_program,
        &w.victim.authority.pubkey(),
        &w.inbox,
        &w.victim.treasury,
        args,
    )
    .expect("reserved chain");
    send(&mut w.ctx, &[ix], &w.victim.authority, &[])
        .await
        .unwrap_or_else(|e| {
            panic!(
                "settlement refused batch {next} from the real chain's cursor (it only posts batch 1 first): {e:?} (custom {:?})",
                custom_error(&e)
            )
        });
}

/// Defence in depth. The attacker's deployment opens AND finalizes the victim's batch 1 with his own data.
/// The inbox batch account records the attacker's settlement program and authority. SAFE behaviour:
/// settlement refuses to post a root over a batch that its own chain's authority and program did not open.
#[tokio::test]
async fn post_root_refuses_a_batch_opened_through_another_settlement_program() {
    let mut w = two_deployments().await;
    let init = zk_inbox_client::init_batch_cursor_ix(
        &w.inbox,
        &w.victim.authority.pubkey(),
        w.victim.chain_id,
        1,
        &w.victim.settlement_program,
    );
    send(&mut w.ctx, &[init], &w.victim.authority, &[])
        .await
        .expect("victim InitBatchCursor");

    let (inbox, chain_id) = (w.inbox, w.victim.chain_id);
    let a = clone_chain(&w.attacker);
    let _ = open_and_finalize_one_chunk_batch(&mut w.ctx, &inbox, &a, 1).await;
    let foreign_acc = inbox_batch_acc(&mut w.ctx, &inbox, &a.settlement_program, chain_id, 1)
        .await
        .unwrap_or([0x99u8; 32]);

    let err = victim_post_root(&mut w, 1, foreign_acc).await.expect_err(
        "settlement posted a root over inbox batch 1 that was opened and finalized \
         through another settlement program by another authority",
    );
    // The victim's batch address holds nothing the attacker made (his batch lives at his own address), so
    // the batch account at the address settlement derives is not a batch at all.
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::WrongInboxAccount as u32),
        "unexpected refusal: {err:?}"
    );
}

fn clone_chain(c: &Chain) -> Chain {
    Chain {
        settlement_program: c.settlement_program,
        inbox_program: c.inbox_program,
        chain_id: c.chain_id,
        authority: c.authority.insecure_clone(),
        genesis_state_root: c.genesis_state_root,
        challenge_window_slots: c.challenge_window_slots,
        registry_authority: c.registry_authority.insecure_clone(),
        treasury: c.treasury,
        upgrade_authority: c.upgrade_authority.insecure_clone(),
    }
}

/// Gives the attacker deployment's own root for the chain a `head_final_batch` high enough that a finality
/// check alone would pass, so a refusal below can only come from the root belonging to another program.
async fn make_attacker_root_final(w: &mut TwoDeployments) {
    let key = sclient::root_pda(&w.attacker.settlement_program, w.attacker.chain_id).0;
    let mut acct = w
        .ctx
        .banks_client
        .get_account(key)
        .await
        .unwrap()
        .expect("attacker root exists");
    let o = rome_zk_layouts::root::OFF_HEAD_FINAL_BATCH;
    acct.data[o..o + 8].copy_from_slice(&1_000u64.to_le_bytes());
    w.ctx.set_account(&key, &acct.into());
}

/// The victim's batch 1 is open and finalized, with one chunk, under the victim's own settlement program.
async fn victim_batch_one_finalized(w: &mut TwoDeployments) {
    let init = zk_inbox_client::init_batch_cursor_ix(
        &w.inbox,
        &w.victim.authority.pubkey(),
        w.victim.chain_id,
        1,
        &w.victim.settlement_program,
    );
    send(&mut w.ctx, &[init], &w.victim.authority, &[])
        .await
        .expect("victim InitBatchCursor");
    let (inbox, v) = (w.inbox, clone_chain(&w.victim));
    open_and_finalize_one_chunk_batch(&mut w.ctx, &inbox, &v, 1)
        .await
        .expect("victim opens and finalizes batch 1");
}

async fn account_is_live(w: &mut TwoDeployments, key: &Pubkey) -> bool {
    w.ctx
        .banks_client
        .get_account(*key)
        .await
        .unwrap()
        .map(|a| a.lamports > 0)
        .unwrap_or(false)
}

/// `CloseBatch` against a root that belongs to another settlement program is refused, even when that root
/// is "final" for the batch id: the batch only answers to the root of the program it was opened through.
#[tokio::test]
async fn close_batch_against_another_programs_root_is_refused() {
    let mut w = two_deployments().await;
    victim_batch_one_finalized(&mut w).await;
    make_attacker_root_final(&mut w).await;

    let mut ix = zk_inbox_client::close_batch_ix(
        &w.inbox,
        &w.victim.authority.pubkey(),
        &w.victim.settlement_program,
        w.victim.chain_id,
        1,
    );
    ix.accounts[2] = AccountMeta::new_readonly(
        sclient::root_pda(&w.attacker.settlement_program, w.attacker.chain_id).0,
        false,
    );
    let err = send(&mut w.ctx, &[ix], &w.victim.authority, &[])
        .await
        .expect_err("CloseBatch accepted a root under another settlement program");
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::IncorrectProgramId),
        "CloseBatch against another program's root (the batch names its own settlement program, which does not own that root)"
    );
    let batch_key =
        zk_inbox_client::batch_pda(&w.inbox, &w.victim.settlement_program, w.victim.chain_id, 1).0;
    assert!(account_is_live(&mut w, &batch_key).await);
}

/// A chunk's `Close` against a root that belongs to another settlement program is refused, even when that
/// root is "final" and the batch slot is the victim's live batch (or the address the batch would have under
/// the other program); the victim's chunk survives. Under its own program's root it closes only once final.
#[tokio::test]
async fn chunk_close_against_another_programs_root_is_refused() {
    let mut w = two_deployments().await;
    victim_batch_one_finalized(&mut w).await;
    make_attacker_root_final(&mut w).await;
    let chunk_key = zk_inbox_client::chunk_pda(
        &w.inbox,
        &w.victim.settlement_program,
        w.victim.chain_id,
        1,
        0,
    )
    .0;
    assert!(account_is_live(&mut w, &chunk_key).await);
    let attacker_root = sclient::root_pda(&w.attacker.settlement_program, w.attacker.chain_id).0;

    for swap_batch_slot in [false, true] {
        let mut ix = zk_inbox_client::close_chunk_ix(
            &w.inbox,
            &w.victim.authority.pubkey(),
            &w.victim.settlement_program,
            w.victim.chain_id,
            1,
            0,
        );
        if swap_batch_slot {
            ix.accounts[2] = AccountMeta::new_readonly(
                zk_inbox_client::batch_pda(
                    &w.inbox,
                    &w.attacker.settlement_program,
                    w.victim.chain_id,
                    1,
                )
                .0,
                false,
            );
        }
        ix.accounts[3] = AccountMeta::new_readonly(attacker_root, false);
        let err = send(&mut w.ctx, &[ix], &w.victim.authority, &[])
            .await
            .expect_err("chunk Close accepted a root under another settlement program");
        // The chunk address is derived from the settlement program that owns the root passed.
        assert_eq!(
            err,
            TransactionError::InstructionError(0, InstructionError::InvalidSeeds),
            "chunk Close against another program's root (batch slot swapped: {swap_batch_slot})"
        );
        assert!(account_is_live(&mut w, &chunk_key).await);
    }

    // Under its own program's root the victim's batch is not final yet (nothing was posted), so Close is
    // still refused — the chunk is held until the real chain's own root says the batch is final.
    let own = zk_inbox_client::close_chunk_ix(
        &w.inbox,
        &w.victim.authority.pubkey(),
        &w.victim.settlement_program,
        w.victim.chain_id,
        1,
        0,
    );
    let err = send(&mut w.ctx, &[own], &w.victim.authority, &[])
        .await
        .expect_err("a chunk of a batch whose root is not final yet must stay");
    assert_eq!(
        custom_error(&err),
        Some(zk_inbox::batch::BatchError::RootNotFinal as u32),
        "unexpected refusal: {err:?}"
    );
    assert!(account_is_live(&mut w, &chunk_key).await);
}

/// The explicit `f.settlement_program != program_id` check on its own. The batch is hand-built at the
/// address settlement derives for the victim chain (so the address check passes) but records another
/// settlement program in its own header: both post instructions refuse it with `WrongInboxAccount`.
#[tokio::test]
async fn post_root_and_post_root_proved_refuse_a_batch_at_the_right_address_recording_another_program(
) {
    let mut w = two_deployments().await;
    victim_batch_one_finalized(&mut w).await;
    let key =
        zk_inbox_client::batch_pda(&w.inbox, &w.victim.settlement_program, w.victim.chain_id, 1).0;
    let mut acct = w
        .ctx
        .banks_client
        .get_account(key)
        .await
        .unwrap()
        .expect("the victim's batch 1");
    let acc = zk_inbox_client::decode_batch_account(&acct.data)
        .unwrap()
        .acc;
    let o = rome_zk_layouts::batch::OFF_SETTLEMENT_PROGRAM;
    acct.data[o..o + 32].copy_from_slice(&w.attacker.settlement_program.to_bytes());
    w.ctx.set_account(&key, &acct.into());

    let wrong = zk_settlement::errors::SettleError::WrongInboxAccount as u32;

    let err = victim_post_root(&mut w, 1, acc)
        .await
        .expect_err("PostRoot accepted a batch recording another settlement program");
    assert_eq!(custom_error(&err), Some(wrong), "PostRoot: {err:?}");

    let args = post_root_for(&w.victim, 1, acc);
    let header = build_synthetic_header(1, args.parent_hash, args.state_root);
    let ix = sclient::post_root_proved_ix(
        &w.victim.settlement_program,
        &w.victim.authority.pubkey(),
        &w.inbox,
        &w.victim.treasury,
        args,
        synthetic_layout2_proof_abi(&header),
        header,
    );
    let err = send(&mut w.ctx, &[ix], &w.victim.authority, &[])
        .await
        .expect_err("PostRootProved accepted a batch recording another settlement program");
    assert_eq!(custom_error(&err), Some(wrong), "PostRootProved: {err:?}");
}
