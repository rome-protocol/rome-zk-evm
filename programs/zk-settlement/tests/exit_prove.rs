//! Real-BPF `solana-program-test` integration tests for `ProveExit` (discriminant 28).
//! Loads the real, `cargo build-sbf`-compiled `.so` — run `cargo build-sbf --manifest-path
//! programs/zk-settlement/Cargo.toml` before `cargo test -p zk-settlement`.
//!
//! Fixtures (`fixtures/exit/*.json`, the committed proofs; `fixtures/exit/README.md` has the provenance):
//! the BASE scenario (`anvil_getProof.json`/`anvil_state_root.json`/`message_hash.json`, nonce 0 — one
//! `initiateExit` call) and the MULTI scenario (`anvil_getProof_multi.json`/`anvil_state_root_multi.json`/
//! `message_hash_multi.json`, 20 calls, nonce 7's slot proved as an inclusion) are two INDEPENDENT anvil
//! chains with two different `state_root`s but the SAME deterministic portal address — used together here
//! (one as an older Final batch, one as the head) wherever a test needs two distinct, genuinely provable
//! exits without a warp between them. Nonce 999's "never sent" message
//! (`l2_sender`/`sol_recipient`/`asset`/`amount` identical to nonce 0's, per `anvil_getProof_unsent.json`'s
//! own README doc "same shape") is confirmed against the fixture by direct `cast keccak` computation
//! (its `storage_slot` matches `anvil_getProof_unsent.json`'s key exactly) — not re-derived here.

use rome_zk_layouts::exit::{exit_config, exit_nullifier, exit_record};
use rome_zk_testkit::{funded_keypair, prefund_pda, rent_exempt};
use serde::Deserialize;
use solana_program::{instruction::Instruction, pubkey::Pubkey};
use solana_program_test::ProgramTestContext;
use solana_sdk::{
    account::Account,
    instruction::InstructionError,
    signature::{Keypair, Signer},
    transaction::TransactionError,
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

async fn send(
    ctx: &mut ProgramTestContext,
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

// ---------------------------------------------------------------------------------------------
// Fixture loading
// ---------------------------------------------------------------------------------------------

fn hex_bytes(s: &str) -> Vec<u8> {
    hex::decode(s.trim_start_matches("0x")).unwrap()
}
fn hex32(s: &str) -> [u8; 32] {
    hex_bytes(s).try_into().unwrap()
}
fn hex20(s: &str) -> [u8; 20] {
    hex_bytes(s).try_into().unwrap()
}
fn nodes_from_hex(v: &[String]) -> Vec<Vec<u8>> {
    v.iter().map(|h| hex_bytes(h)).collect()
}

#[derive(Deserialize)]
struct StorageProofJson {
    proof: Vec<String>,
}
#[derive(Deserialize)]
struct GetProofJson {
    #[serde(rename = "accountProof")]
    account_proof: Vec<String>,
    #[serde(rename = "storageProof")]
    storage_proof: Vec<StorageProofJson>,
}
#[derive(Deserialize)]
struct StateRootJson {
    state_root: String,
}
#[derive(Deserialize)]
struct PortalJson {
    portal: String,
}

const BASE_GET_PROOF: &str = include_str!("../../../fixtures/exit/anvil_getProof.json");
const BASE_UNSENT_GET_PROOF: &str =
    include_str!("../../../fixtures/exit/anvil_getProof_unsent.json");
const BASE_STATE_ROOT: &str = include_str!("../../../fixtures/exit/anvil_state_root.json");
const BASE_MESSAGE: &str = include_str!("../../../fixtures/exit/message_hash.json");
const MULTI_GET_PROOF: &str = include_str!("../../../fixtures/exit/anvil_getProof_multi.json");
const MULTI_STATE_ROOT: &str = include_str!("../../../fixtures/exit/anvil_state_root_multi.json");

fn base_portal() -> [u8; 20] {
    let m: PortalJson = serde_json::from_str(BASE_MESSAGE).unwrap();
    hex20(&m.portal)
}
fn base_state_root() -> [u8; 32] {
    let r: StateRootJson = serde_json::from_str(BASE_STATE_ROOT).unwrap();
    hex32(&r.state_root)
}
fn multi_state_root() -> [u8; 32] {
    let r: StateRootJson = serde_json::from_str(MULTI_STATE_ROOT).unwrap();
    hex32(&r.state_root)
}

/// Nonce 0 — the BASE scenario's one real, sent exit.
fn nonce0_message() -> sclient::ExitMessageArg {
    sclient::ExitMessageArg {
        nonce: 0,
        l2_sender: hex20("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"),
        sol_recipient: hex32("0x0101010101010101010101010101010101010101010101010101010101010101"),
        asset: [0u8; 20],
        amount: 1_000_000_000_000_000_000u128,
    }
}
fn nonce0_proof() -> rome_zk_mpt::ExitProof {
    let p: GetProofJson = serde_json::from_str(BASE_GET_PROOF).unwrap();
    rome_zk_mpt::ExitProof {
        account_nodes: nodes_from_hex(&p.account_proof),
        storage_nodes: nodes_from_hex(&p.storage_proof[0].proof),
    }
}

/// Nonce 999 — the BASE scenario's never-sent message (README: "same shape" as nonce 0, only the nonce
/// changes). Confirmed against `anvil_getProof_unsent.json`'s own key by direct `cast keccak` computation
/// (see the module doc) — its `storage_slot()` matches exactly.
fn nonce999_message() -> sclient::ExitMessageArg {
    sclient::ExitMessageArg {
        nonce: 999,
        ..nonce0_message()
    }
}
fn nonce999_unsent_proof() -> rome_zk_mpt::ExitProof {
    let p: GetProofJson = serde_json::from_str(BASE_UNSENT_GET_PROOF).unwrap();
    rome_zk_mpt::ExitProof {
        account_nodes: nodes_from_hex(&p.account_proof),
        storage_nodes: nodes_from_hex(&p.storage_proof[0].proof),
    }
}

/// Nonce 7 — the MULTI scenario's (a SEPARATE anvil chain / `state_root`) proved inclusion exit.
fn nonce7_message() -> sclient::ExitMessageArg {
    sclient::ExitMessageArg {
        nonce: 7,
        l2_sender: hex20("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"),
        sol_recipient: hex32("0x0202020202020202020202020202020202020202020202020202020202020202"),
        asset: [0u8; 20],
        amount: 8_000_000_000_000_000_000u128,
    }
}
fn nonce7_proof() -> rome_zk_mpt::ExitProof {
    let p: GetProofJson = serde_json::from_str(MULTI_GET_PROOF).unwrap();
    rome_zk_mpt::ExitProof {
        account_nodes: nodes_from_hex(&p.account_proof),
        storage_nodes: nodes_from_hex(&p.storage_proof[0].proof),
    }
}

fn to_layout(m: sclient::ExitMessageArg) -> rome_zk_layouts::exit::ExitMessage {
    rome_zk_layouts::exit::ExitMessage {
        nonce: m.nonce,
        l2_sender: m.l2_sender,
        sol_recipient: m.sol_recipient,
        asset: m.asset,
        amount: m.amount,
    }
}

// ---------------------------------------------------------------------------------------------
// Rig
// ---------------------------------------------------------------------------------------------

struct ExitRig {
    ctx: ProgramTestContext,
    settlement_program: Pubkey,
    payer: Keypair,
    chain_id: u64,
    challenge_window_slots: u32,
}

/// Builds a chain with a hand-written root account (no `InitChain`/`PostRootProved` ceremony needed — this
/// test only needs the FINAL-root/config/cap state `ProveExit` reads) and, optionally, an `exit_config`
/// account and one older `Final` pending PDA (`older_final = Some((batch, state_root))`, for `batch <
/// head_final_batch`) — every account is registered pre-start (`pt.add_account`), never via a post-start
/// `set_account`, so a later `warp_to_slot` never hits the accounts-hash panic `post_root_rig`'s doc warns
/// about.
#[allow(clippy::too_many_arguments)]
async fn build_exit_rig(
    chain_id: u64,
    challenge_window_slots: u32,
    exit_cap_per_window: u64,
    head_final_batch: u64,
    head_state_root: [u8; 32],
    portal: Option<[u8; 20]>,
    older_final: Option<(u64, [u8; 32])>,
) -> ExitRig {
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

    let (root_pda, _) = sclient::root_pda(&settlement_program, chain_id);
    let root_fields = rome_zk_layouts::root::RootFields {
        chain_id,
        number: head_final_batch,
        parent_hash: [0u8; 32],
        state_root: head_state_root,
        block_hash: [0u8; 32],
        updates: 0,
        profile: 0,
        challenge_window_slots,
        prove_window_slots: 1000,
        proving_policy: 1,
        poster_bond: 0,
        exit_cap_per_window,
        authority: Pubkey::new_unique().to_bytes(),
        head_pending_batch: head_final_batch,
        head_final_batch,
        pending_count: 0,
        max_pending: 16,
    };
    let root_bytes = rome_zk_layouts::root::write(&root_fields).to_vec();
    pt.add_account(
        root_pda,
        Account {
            lamports: rent_exempt(root_bytes.len()),
            data: root_bytes,
            owner: settlement_program,
            executable: false,
            rent_epoch: 0,
        },
    );

    if let Some(p) = portal {
        let (exit_config_pda, _) = sclient::exit_config_pda(&settlement_program, chain_id);
        let cfg = exit_config::ExitConfigFields {
            chain_id,
            exit_portal: p,
            bridge_program: [0x01u8; 32],
            pending_exit_portal: [0u8; 20],
            pending_bridge_program: [0u8; 32],
            pending_exit_cap: 0,
            pending_poster_bond: 0,
            activation_slot: 0,
            pending_mask: 0,
        };
        let d = exit_config::write(&cfg).to_vec();
        pt.add_account(
            exit_config_pda,
            Account {
                lamports: rent_exempt(d.len()),
                data: d,
                owner: settlement_program,
                executable: false,
                rent_epoch: 0,
            },
        );
    }

    if let Some((batch, state_root)) = older_final {
        let (pending_pda, _) = sclient::pending_pda(&settlement_program, chain_id, batch);
        let mut d = vec![0u8; rome_zk_layouts::pending::PENDING_LEN];
        d[rome_zk_layouts::pending::OFF_BATCH..rome_zk_layouts::pending::OFF_BATCH + 8]
            .copy_from_slice(&batch.to_le_bytes());
        d[rome_zk_layouts::pending::OFF_STATE_ROOT..rome_zk_layouts::pending::OFF_STATE_ROOT + 32]
            .copy_from_slice(&state_root);
        d[rome_zk_layouts::pending::OFF_STATUS] = rome_zk_layouts::pending::STATUS_FINAL;
        pt.add_account(
            pending_pda,
            Account {
                lamports: rent_exempt(d.len()),
                data: d,
                owner: settlement_program,
                executable: false,
                rent_epoch: 0,
            },
        );
    }

    let ctx = pt.start_with_context().await;
    ExitRig {
        ctx,
        settlement_program,
        payer,
        chain_id,
        challenge_window_slots,
    }
}

/// The window index `ProveExit`'s own `Clock::get()?.slot / root.challenge_window_slots` would compute
/// right now — the client-side half of the same formula.
async fn current_window(rig: &mut ExitRig) -> u64 {
    let now = rig.ctx.banks_client.get_root_slot().await.unwrap();
    now / rig.challenge_window_slots as u64
}

async fn prove(
    rig: &mut ExitRig,
    batch: u64,
    message: sclient::ExitMessageArg,
    proof: rome_zk_mpt::ExitProof,
) -> Result<u64, TransactionError> {
    let window_index = current_window(rig).await;
    let page = rome_zk_layouts::exit::nullifier_page(message.nonce);
    let ix = sclient::prove_exit_ix(
        &rig.settlement_program,
        &rig.payer.pubkey(),
        rig.chain_id,
        batch,
        message,
        proof,
        window_index,
        page,
    );
    let payer = rig.payer.insecure_clone();
    send(&mut rig.ctx, &[ix], &payer, &[]).await
}

async fn get_record(
    rig: &mut ExitRig,
    message_hash: [u8; 32],
) -> Option<sclient::ExitRecordAccount> {
    let (record_pda, _) =
        sclient::exit_record_pda(&rig.settlement_program, rig.chain_id, message_hash);
    let acc = rig
        .ctx
        .banks_client
        .get_account(record_pda)
        .await
        .unwrap()?;
    Some(sclient::decode_exit_record_account(&acc.data).unwrap())
}

async fn get_window(rig: &mut ExitRig, window_index: u64) -> Option<sclient::ExitWindowAccount> {
    let (window_pda, _) =
        sclient::exit_window_pda(&rig.settlement_program, rig.chain_id, window_index);
    let acc = rig
        .ctx
        .banks_client
        .get_account(window_pda)
        .await
        .unwrap()?;
    Some(sclient::decode_exit_window_account(&acc.data).unwrap())
}

async fn nullifier_bit_set(rig: &mut ExitRig, page: u64, nonce: u64) -> bool {
    let (nullifier_pda, _) =
        sclient::exit_nullifier_pda(&rig.settlement_program, rig.chain_id, page);
    let acc = rig
        .ctx
        .banks_client
        .get_account(nullifier_pda)
        .await
        .unwrap()
        .expect("nullifier page must exist");
    let bits = &acc.data[exit_nullifier::OFF_BITS..exit_nullifier::LEN];
    rome_zk_layouts::exit::bit_is_set(bits, page, nonce).unwrap()
}

// ---------------------------------------------------------------------------------------------
// Proof refusals
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn prove_exit_valid_writes_record_window_page() {
    let chain_id = 1;
    let mut rig = build_exit_rig(
        chain_id,
        100,
        1_000_000_000,
        1,
        base_state_root(),
        Some(base_portal()),
        None,
    )
    .await;
    let message = nonce0_message();
    let window_index = current_window(&mut rig).await;
    let cu = prove(&mut rig, 1, message, nonce0_proof())
        .await
        .expect("a valid exit proof must succeed");
    eprintln!("ProveExit consumed {cu} CU (anvil fixture: 3 account + 2 storage nodes)");

    let message_hash = to_layout(message).message_hash();
    let record = get_record(&mut rig, message_hash)
        .await
        .expect("exit_record must exist");
    assert_eq!(record.status, exit_record::STATUS_PROVED);
    assert_eq!(record.batch, 1);
    assert_eq!(record.window_index, window_index);
    assert_eq!(record.sol_recipient, message.sol_recipient);
    assert_eq!(record.amount, message.amount);
    assert_eq!(record.asset, message.asset);

    let window = get_window(&mut rig, window_index)
        .await
        .expect("exit_window must exist");
    assert_eq!(window.exits, 1);
    assert_eq!(window.spent_cap_units, 1_000_000_000); // 1e18 wei / 1e9 wei-per-unit
    assert!(nullifier_bit_set(&mut rig, 0, 0).await);
}

/// Genesis: a PERMISSIONLESS chain has no final root until its first proved batch. Batch 0
/// is the genesis the chain authority itself wrote at `InitChainV2`, so it was never proved by anyone;
/// `ProveExit` against it must refuse (`NotFinal`), exactly as `RootView(0)` does, or an exit could be
/// proved against a state root the authority simply chose. The rig is a permissionless chain at the
/// genesis sentinel (`head_final_batch == 0`) holding a state root a real exit proof is valid under —
/// so the only thing that can refuse the proof is the finality predicate.
#[tokio::test]
async fn prove_exit_against_the_genesis_batch_of_a_permissionless_chain_is_not_final() {
    let chain_id = rome_zk_layouts::chainid::PERMISSIONLESS_BASE + 7;
    assert!(!rome_zk_layouts::chainid::is_reserved(chain_id));
    let mut rig = build_exit_rig(
        chain_id,
        100,
        1_000_000_000,
        0,
        base_state_root(),
        Some(base_portal()),
        None,
    )
    .await;
    let err = prove(&mut rig, 0, nonce0_message(), nonce0_proof())
        .await
        .expect_err("the genesis batch of a permissionless chain is not final");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::NotFinal as u32)
    );
}

/// Control for the test above: the identical rig on a RESERVED chain id keeps its genesis as the final
/// root (Rome's own chains are not proved-only), so the same proof is accepted against batch 0.
#[tokio::test]
async fn prove_exit_against_the_genesis_batch_of_a_reserved_chain_still_proves() {
    let chain_id = 1;
    assert!(rome_zk_layouts::chainid::is_reserved(chain_id));
    let mut rig = build_exit_rig(
        chain_id,
        100,
        1_000_000_000,
        0,
        base_state_root(),
        Some(base_portal()),
        None,
    )
    .await;
    prove(&mut rig, 0, nonce0_message(), nonce0_proof())
        .await
        .expect("a reserved chain's genesis is its final root until a batch is posted");
}

#[tokio::test]
async fn prove_exit_against_pending_root_is_not_final() {
    let chain_id = 1;
    let mut rig = build_exit_rig(
        chain_id,
        100,
        1_000_000_000,
        0,
        [0u8; 32],
        Some(base_portal()),
        None,
    )
    .await;
    // batch 1's own pending PDA, still Pending (not yet Final) — head_final_batch stays 0.
    let (pending_pda, _) = sclient::pending_pda(&rig.settlement_program, chain_id, 1);
    let mut d = vec![0u8; rome_zk_layouts::pending::PENDING_LEN];
    d[rome_zk_layouts::pending::OFF_BATCH..rome_zk_layouts::pending::OFF_BATCH + 8]
        .copy_from_slice(&1u64.to_le_bytes());
    d[rome_zk_layouts::pending::OFF_STATUS] = rome_zk_layouts::pending::STATUS_PENDING;
    rig.ctx.set_account(
        &pending_pda,
        &Account {
            lamports: rent_exempt(d.len()),
            data: d,
            owner: rig.settlement_program,
            executable: false,
            rent_epoch: 0,
        }
        .into(),
    );

    let err = prove(&mut rig, 1, nonce0_message(), nonce0_proof())
        .await
        .expect_err("a Pending (not yet Final) batch must be refused");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::NotFinal as u32)
    );
}

#[tokio::test]
async fn prove_exit_proof_for_a_different_final_batch_root_is_refused() {
    let chain_id = 1;
    // The head's Final state_root is the BASE scenario's — but the proof presented is the MULTI
    // scenario's (a different anvil chain, different root): the account proof's first node does not hash
    // to this root at all.
    let mut rig = build_exit_rig(
        chain_id,
        100,
        1_000_000_000,
        1,
        base_state_root(),
        Some(base_portal()),
        None,
    )
    .await;
    let err = prove(&mut rig, 1, nonce7_message(), nonce7_proof())
        .await
        .expect_err("a proof rooted in a different batch's state_root must be refused");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::ExitProofInvalid as u32)
    );
}

#[tokio::test]
async fn prove_exit_for_non_portal_account_is_refused() {
    let chain_id = 1;
    // exit_config names a portal address the real proof was never generated for.
    let wrong_portal = [0x99u8; 20];
    let mut rig = build_exit_rig(
        chain_id,
        100,
        1_000_000_000,
        1,
        base_state_root(),
        Some(wrong_portal),
        None,
    )
    .await;
    let err = prove(&mut rig, 1, nonce0_message(), nonce0_proof())
        .await
        .expect_err("a proof of the wrong account must be refused — the portal is bound on-chain");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::ExitProofInvalid as u32)
    );
}

#[tokio::test]
async fn prove_exit_replay_is_refused() {
    let chain_id = 1;
    let mut rig = build_exit_rig(
        chain_id,
        100,
        1_000_000_000,
        1,
        base_state_root(),
        Some(base_portal()),
        None,
    )
    .await;
    prove(&mut rig, 1, nonce0_message(), nonce0_proof())
        .await
        .expect("first prove should succeed");
    let err = prove(&mut rig, 1, nonce0_message(), nonce0_proof())
        .await
        .expect_err("a second prove of the same nonce must be refused");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::ExitAlreadyProved as u32)
    );
}

#[tokio::test]
async fn prove_exit_exclusion_proof_is_refused() {
    let chain_id = 1;
    let mut rig = build_exit_rig(
        chain_id,
        100,
        1_000_000_000,
        1,
        base_state_root(),
        Some(base_portal()),
        None,
    )
    .await;
    let err = prove(&mut rig, 1, nonce999_message(), nonce999_unsent_proof())
        .await
        .expect_err("an exclusion proof (never sent) must be refused");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::ExitNotSent as u32)
    );
}

#[tokio::test]
async fn prove_exit_with_config_unset_is_refused() {
    let chain_id = 1;
    // No `portal` passed -> no exit_config account at all (absent = disabled).
    let mut rig = build_exit_rig(
        chain_id,
        100,
        1_000_000_000,
        1,
        base_state_root(),
        None,
        None,
    )
    .await;
    let err = prove(&mut rig, 1, nonce0_message(), nonce0_proof())
        .await
        .expect_err("an absent exit_config must refuse (exits disabled)");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::ExitConfigUnset as u32)
    );
}

#[tokio::test]
async fn prove_exit_with_cap_zero_is_refused() {
    let chain_id = 1;
    let mut rig = build_exit_rig(
        chain_id,
        100,
        0,
        1,
        base_state_root(),
        Some(base_portal()),
        None,
    )
    .await;
    let err = prove(&mut rig, 1, nonce0_message(), nonce0_proof())
        .await
        .expect_err("root.exit_cap_per_window == 0 must refuse (fail-closed)");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::ExitCapUnset as u32)
    );
}

#[tokio::test]
async fn prove_exit_with_zero_window_is_refused() {
    let chain_id = 1;
    let mut rig = build_exit_rig(
        chain_id,
        0,
        1_000_000_000,
        1,
        base_state_root(),
        Some(base_portal()),
        None,
    )
    .await;
    // `current_window()`/the client's own `window = slot / challenge_window_slots` divides by zero here
    // (challenge_window_slots == 0 IS the case under test) — pass a placeholder window/page directly
    // instead of going through `prove()`; the refusal fires before the program ever computes its own
    // window, so which placeholder is irrelevant to what is being asserted.
    let ix = sclient::prove_exit_ix(
        &rig.settlement_program,
        &rig.payer.pubkey(),
        chain_id,
        1,
        nonce0_message(),
        nonce0_proof(),
        0,
        0,
    );
    let payer = rig.payer.insecure_clone();
    let err = send(&mut rig.ctx, &[ix], &payer, &[])
        .await
        .expect_err("root.challenge_window_slots == 0 must refuse");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::ChallengeWindowZero as u32)
    );
}

#[tokio::test]
async fn prove_exit_with_unsupported_asset_is_refused() {
    let chain_id = 1;
    let mut rig = build_exit_rig(
        chain_id,
        100,
        1_000_000_000,
        1,
        base_state_root(),
        Some(base_portal()),
        None,
    )
    .await;
    let message = sclient::ExitMessageArg {
        asset: [0x11u8; 20],
        ..nonce0_message()
    };
    let err = prove(&mut rig, 1, message, nonce0_proof())
        .await
        .expect_err("a non-native asset must be refused in v1");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::UnsupportedAsset as u32)
    );
}

#[tokio::test]
async fn prefunded_record_window_page_are_adopted() {
    let chain_id = 1;
    let mut rig = build_exit_rig(
        chain_id,
        100,
        1_000_000_000,
        1,
        base_state_root(),
        Some(base_portal()),
        None,
    )
    .await;
    let message = nonce0_message();
    let message_hash = to_layout(message).message_hash();
    let window_index = current_window(&mut rig).await;

    let (record_pda, _) = sclient::exit_record_pda(&rig.settlement_program, chain_id, message_hash);
    let (window_pda, _) = sclient::exit_window_pda(&rig.settlement_program, chain_id, window_index);
    let (nullifier_pda, _) = sclient::exit_nullifier_pda(&rig.settlement_program, chain_id, 0);
    prefund_pda(&mut rig.ctx, record_pda).await;
    prefund_pda(&mut rig.ctx, window_pda).await;
    prefund_pda(&mut rig.ctx, nullifier_pda).await;

    prove(&mut rig, 1, message, nonce0_proof()).await.expect(
        "ProveExit must adopt pre-funded record/window/page PDAs, not fail AccountAlreadyInUse",
    );
    let record = get_record(&mut rig, message_hash).await.unwrap();
    assert_eq!(record.status, exit_record::STATUS_PROVED);
}

// ---------------------------------------------------------------------------------------------
// Duplicated state (sharing the window/page is the NORMAL case)
// ---------------------------------------------------------------------------------------------

/// Two real, distinct exits (nonce 0 against the BASE scenario's Final batch 1, nonce 7 against the MULTI
/// scenario's Final batch 2 — no slot warp between them, so both land in the SAME window) must both
/// succeed and share one `exit_window` account.
#[tokio::test]
async fn two_exits_in_the_same_window_both_succeed() {
    let chain_id = 1;
    let mut rig = build_exit_rig(
        chain_id,
        100,
        100_000_000_000,
        2,
        multi_state_root(),
        Some(base_portal()),
        Some((1, base_state_root())),
    )
    .await;
    let window_index = current_window(&mut rig).await;

    prove(&mut rig, 1, nonce0_message(), nonce0_proof())
        .await
        .expect("exit A (batch 1, nonce 0) should succeed");
    prove(&mut rig, 2, nonce7_message(), nonce7_proof())
        .await
        .expect("exit B (batch 2, nonce 7), same window, should ALSO succeed");

    let window = get_window(&mut rig, window_index).await.unwrap();
    assert_eq!(
        window.exits, 2,
        "both exits must be counted in one exit_window"
    );
    assert_eq!(
        window.spent_cap_units,
        1_000_000_000 + 8_000_000_000,
        "both exits' units must be summed in one exit_window"
    );
}

/// The same two exits (nonce 0 and nonce 7) both fall on nullifier page 0 (`nonce >> 13 == 0` for both) —
/// both bits must end up set on the ONE shared page account.
#[tokio::test]
async fn two_exits_on_the_same_nullifier_page_both_succeed() {
    let chain_id = 1;
    let mut rig = build_exit_rig(
        chain_id,
        100,
        100_000_000_000,
        2,
        multi_state_root(),
        Some(base_portal()),
        Some((1, base_state_root())),
    )
    .await;

    prove(&mut rig, 1, nonce0_message(), nonce0_proof())
        .await
        .expect("exit A (nonce 0, page 0) should succeed");
    prove(&mut rig, 2, nonce7_message(), nonce7_proof())
        .await
        .expect("exit B (nonce 7, page 0), same page, should ALSO succeed");

    assert!(
        nullifier_bit_set(&mut rig, 0, 0).await,
        "nonce 0's bit must be set"
    );
    assert!(
        nullifier_bit_set(&mut rig, 0, 7).await,
        "nonce 7's bit must be set"
    );
}

// ---------------------------------------------------------------------------------------------
// Over-cap → refused → re-queued to the next window → admitted
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn prove_exit_over_cap_refused_and_next_window_admits() {
    let chain_id = 1;
    // cap = exactly exit B's units (8e9): A (1e9) alone fits; A + B (9e9) exceeds; B alone in a fresh
    // window (8e9) fits exactly.
    let cap = 8_000_000_000u64;
    let mut rig = build_exit_rig(
        chain_id,
        50,
        cap,
        2,
        multi_state_root(),
        Some(base_portal()),
        Some((1, base_state_root())),
    )
    .await;
    let window_index = current_window(&mut rig).await;

    prove(&mut rig, 1, nonce0_message(), nonce0_proof())
        .await
        .expect("exit A alone (1e9 units <= 8e9 cap) should succeed");

    let err = prove(&mut rig, 2, nonce7_message(), nonce7_proof())
        .await
        .expect_err("exit B on top of A (9e9 units > 8e9 cap) must be refused");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::ExitCapExceeded as u32)
    );
    // The refusal must not have burned the nullifier — nonce 7 is still re-provable.
    let (nullifier_pda, _) = sclient::exit_nullifier_pda(&rig.settlement_program, chain_id, 0);
    if let Some(acc) = rig
        .ctx
        .banks_client
        .get_account(nullifier_pda)
        .await
        .unwrap()
    {
        let bits = &acc.data[exit_nullifier::OFF_BITS..exit_nullifier::LEN];
        assert!(
            !rome_zk_layouts::exit::bit_is_set(bits, 0, 7).unwrap(),
            "a cap-exceeded refusal must leave nonce 7's bit clear"
        );
    }
    let window = get_window(&mut rig, window_index).await.unwrap();
    assert_eq!(
        window.spent_cap_units, 1_000_000_000,
        "a cap-exceeded refusal must not have written exit B's units into the window"
    );

    // Warp into the next window: B alone (8e9 units) fits the same cap in a fresh window.
    let now = rig.ctx.banks_client.get_root_slot().await.unwrap();
    rig.ctx
        .warp_to_slot(now + rig.challenge_window_slots as u64 + 2)
        .unwrap();
    prove(&mut rig, 2, nonce7_message(), nonce7_proof())
        .await
        .expect("exit B in the NEXT window should be admitted");
    let next_window = current_window(&mut rig).await;
    assert_ne!(
        next_window, window_index,
        "the warp must cross a window boundary"
    );
    let window2 = get_window(&mut rig, next_window).await.unwrap();
    assert_eq!(window2.spent_cap_units, 8_000_000_000);
}

/// The cap boundary is `spent + units > cap` (strictly greater refuses) — an exit whose units land
/// EXACTLY on the cap must succeed, not be refused. Mutation: `>=` in place of `>` turns this test red
/// (an exact-cap exit would then be wrongly refused).
#[tokio::test]
async fn prove_exit_at_exact_cap_boundary_succeeds() {
    let chain_id = 1;
    let cap = 8_000_000_000u64; // exactly nonce 7's units (8e18 wei / 1e9)
    let mut rig = build_exit_rig(
        chain_id,
        100,
        cap,
        1,
        multi_state_root(),
        Some(base_portal()),
        None,
    )
    .await;
    let window_index = current_window(&mut rig).await;
    prove(&mut rig, 1, nonce7_message(), nonce7_proof())
        .await
        .expect("an exit landing EXACTLY on the cap must succeed, not be refused");
    let window = get_window(&mut rig, window_index).await.unwrap();
    assert_eq!(window.spent_cap_units, cap);
}

// ---------------------------------------------------------------------------------------------
// CU + tx-size gate
// ---------------------------------------------------------------------------------------------

/// Builds the real V1 (SIMD-0385) wire shape for `ix`, same pattern as `tests/settlement.rs`'s own
/// `assert_v1_tx_fits_envelope` (duplicated here — this is a separate test binary — never a second
/// implementation of the V1-compat conversion itself, which stays
/// `rome_zk_solana_sender::compat::to_v1_instruction`).
fn assert_v1_tx_fits_envelope(ix: &Instruction, fee_payer: &Keypair) -> usize {
    use solana_signer::Signer as V1Signer;

    let to_v1_keypair = |kp: &Keypair| -> solana_keypair::Keypair {
        let secret: [u8; 32] = kp.to_bytes()[..32].try_into().unwrap();
        solana_keypair::Keypair::new_from_array(secret)
    };
    let payer_v1 = to_v1_keypair(fee_payer);

    let v1_ix = rome_zk_solana_sender::compat::to_v1_instruction(ix);
    let config = solana_message::v1::TransactionConfig::empty()
        .with_compute_unit_limit(150_000)
        .with_loaded_accounts_data_size_limit(256 * 1024);
    let message = solana_message::v1::Message::try_compile_with_config(
        &payer_v1.pubkey(),
        &[v1_ix],
        solana_hash::Hash::default(),
        config,
    )
    .expect("the ProveExit V1 message must compile");

    let tx = solana_transaction::versioned::VersionedTransaction::try_new(
        solana_message::VersionedMessage::V1(message),
        &[&payer_v1],
    )
    .expect("signing the ProveExit V1 message must succeed");
    let bytes = wincode::serialize(&tx).expect("a signed V1 transaction always serializes");
    eprintln!(
        "ProveExit V1 tx is {} signed bytes (anvil fixture)",
        bytes.len()
    );
    assert!(
        bytes.len() <= 4_096,
        "ProveExit V1 tx is {} bytes, over the 4,096-byte SIMD-0385 envelope",
        bytes.len()
    );
    bytes.len()
}

#[tokio::test]
async fn prove_exit_cu_and_tx_size_with_anvil_fixture() {
    let chain_id = 1;
    let mut rig = build_exit_rig(
        chain_id,
        100,
        1_000_000_000,
        1,
        base_state_root(),
        Some(base_portal()),
        None,
    )
    .await;
    let message = nonce0_message();
    let proof = nonce0_proof();
    let window_index = current_window(&mut rig).await;
    let page = 0u64;

    let ix = sclient::prove_exit_ix(
        &rig.settlement_program,
        &rig.payer.pubkey(),
        chain_id,
        1,
        message,
        proof.clone(),
        window_index,
        page,
    );
    assert_v1_tx_fits_envelope(&ix, &rig.payer);

    let payer = rig.payer.insecure_clone();
    let cu = send(&mut rig.ctx, &[ix], &payer, &[])
        .await
        .expect("ProveExit should succeed for the CU measurement");
    eprintln!(
        "ProveExit CU = {cu} (anvil fixture: {} account nodes, {} storage nodes — a FLOOR, not the \
         design's assumed 120k ceiling for a 16+8-node proof; the 60-block-scale proof needs its own \
         measurement)",
        proof.account_nodes.len(),
        proof.storage_nodes.len()
    );
}
