//! Real-BPF `solana-program-test` integration tests for `ConsumeExit` (discriminant 29).
//! Loads the real, `cargo build-sbf`-compiled `.so` for both `zk_settlement` and the TEST-ONLY stub bridge
//! program (`programs/zk-settlement/tests/fixtures/stub-bridge`, never a shipped, deployed program) — run
//! `cargo build-sbf --manifest-path programs/zk-settlement/Cargo.toml --sbf-out-dir <workspace>/target/deploy`
//! and
//! `cargo build-sbf --manifest-path programs/zk-settlement/tests/fixtures/stub-bridge/Cargo.toml --sbf-out-dir <workspace>/target/deploy`
//! before `cargo test -p zk-settlement --test exit_consume`.
//!
//! Most of these tests build the `exit_record`/`exit_config` accounts directly (via `pt.add_account`,
//! never a `ProveExit` call first) — `ConsumeExit`'s own logic (the bridge-signer PDA check, the
//! refund-to-payer check, the PROVED-status check) does not care how a record reached PROVED, only that
//! it is one; this is the same "hand-built account fixture, not a full ceremony" pattern
//! `rome_zk_testkit::root_account_with_authority` already establishes for other instructions'
//! authority-gated paths. The one test that DOES need a genuinely proved exit
//! (`prove_exit_after_release_is_refused`, proving the nullifier bit — not the record — is the persistent
//! replay guard) drives a real `ProveExit` first, against the same anvil fixture `tests/exit_prove.rs`
//! uses (a small, deliberate duplication of that file's fixture-loading helpers rather than a larger
//! shared-test-support refactor for one call site).

use rome_zk_layouts::exit::{exit_config, exit_record};
use rome_zk_testkit::{funded_keypair, rent_exempt};
use serde::Deserialize;
use solana_program::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use solana_program_test::ProgramTestContext;
use solana_sdk::{
    account::Account,
    instruction::InstructionError,
    signature::{Keypair, Signer},
    transaction::TransactionError,
};
use solana_system_interface::program as system_program;
use zk_settlement_client as sclient;

fn funded_account(lamports: u64) -> Account {
    Account {
        lamports,
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

async fn send_with_logs(
    ctx: &mut ProgramTestContext,
    ixs: &[Instruction],
    payer: &Keypair,
    extra_signers: &[&Keypair],
) -> (Result<(), TransactionError>, u64, Option<Vec<String>>) {
    rome_zk_testkit::send_measuring_cu(ctx, ixs, payer, extra_signers).await
}

fn custom_error(err: &TransactionError) -> Option<u32> {
    match err {
        TransactionError::InstructionError(_, InstructionError::Custom(c)) => Some(*c),
        _ => None,
    }
}

fn is_incorrect_program_id(err: &TransactionError) -> bool {
    matches!(
        err,
        TransactionError::InstructionError(_, InstructionError::IncorrectProgramId)
    )
}

// ---------------------------------------------------------------------------------------------
// The stub bridge program's own instruction (mirrors `zk_exit_stub_bridge::process_instruction`'s doc):
// accounts [settlement_program, bridge_signer, exit_config, exit_record, payer_refund], data
// = chain_id (u64 LE) ++ message_hash (32 bytes).
// ---------------------------------------------------------------------------------------------

fn stub_consume_ix(
    stub_program: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
    message_hash: [u8; 32],
    exit_config_pda: &Pubkey,
    exit_record_pda: &Pubkey,
    payer_refund: &Pubkey,
) -> Instruction {
    let (bridge_signer, _) =
        Pubkey::find_program_address(&[b"exit_consumer", &chain_id.to_le_bytes()], stub_program);
    let mut data = Vec::with_capacity(40);
    data.extend_from_slice(&chain_id.to_le_bytes());
    data.extend_from_slice(&message_hash);
    Instruction {
        program_id: *stub_program,
        accounts: vec![
            AccountMeta::new_readonly(*settlement_program, false),
            AccountMeta::new_readonly(bridge_signer, false),
            AccountMeta::new_readonly(*exit_config_pda, false),
            AccountMeta::new(*exit_record_pda, false),
            AccountMeta::new(*payer_refund, false),
        ],
        data,
    }
}

// ---------------------------------------------------------------------------------------------
// Rig: settlement program + stub bridge program, an `exit_config` with a chosen `bridge_program`, and an
// optional hand-built `exit_record` (PROVED, with a chosen `payer`) — everything registered pre-start
// (`pt.add_account`), matching `tests/exit_prove.rs`'s own rig pattern. Both program ids are the FIXED
// ones `rome_zk_testkit` pins for CU-gate tests, never `Pubkey::new_unique()` — a PDA's bump-seed search
// depth (and so the measured `ConsumeExit` CU) depends on the program id the search runs against, so a
// random id would make the CU this file prints run-to-run-incomparable.
// ---------------------------------------------------------------------------------------------

struct ConsumeRig {
    ctx: ProgramTestContext,
    settlement_program: Pubkey,
    stub_program: Pubkey,
    payer: Keypair,
}

/// `bridge_program_for_config`: `None` sets `exit_config.bridge_program` to the REAL stub program id (the
/// happy path — matches, so the stub's own signed PDA authorises); `Some(p)` sets it to a caller-chosen
/// value instead (the `consume_by_non_bridge_signer_is_refused` test's whole point: the stub always signs
/// ITS OWN id's `exit_consumer` PDA regardless, so naming a different program here is exactly the
/// mismatch `NotBridgeProgram` catches).
#[allow(clippy::too_many_arguments)]
async fn build_consume_rig(
    chain_id: u64,
    bridge_program_for_config: Option<Pubkey>,
    record: Option<(u8, Pubkey, [u8; 32])>, // (status, payer, message_hash)
    payer_refund_seed: Option<(Pubkey, u64)>, // (pubkey, starting lamports) — pre-funded, distinct from any record payer
) -> ConsumeRig {
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let stub_program = rome_zk_testkit::fixed_stub_bridge_program_id();
    let bridge_program_for_config = bridge_program_for_config.unwrap_or(stub_program);
    let payer = funded_keypair();
    let mut pt = rome_zk_testkit::program_test(
        &[
            rome_zk_testkit::ProgramSpec::upgradeable("zk_settlement", settlement_program),
            rome_zk_testkit::ProgramSpec::new("zk_exit_stub_bridge", stub_program),
        ],
        true,
    );
    pt.add_account(payer.pubkey(), funded_account(50_000_000_000));

    let (exit_config_pda, _) = sclient::exit_config_pda(&settlement_program, chain_id);
    let cfg = exit_config::ExitConfigFields {
        chain_id,
        exit_portal: [0x11u8; 20],
        bridge_program: bridge_program_for_config.to_bytes(),
        pending_exit_portal: [0u8; 20],
        pending_bridge_program: [0u8; 32],
        pending_exit_cap: 0,
        pending_poster_bond: 0,
        activation_slot: 0,
        pending_mask: 0,
    };
    let cfg_bytes = exit_config::write(&cfg).to_vec();
    pt.add_account(
        exit_config_pda,
        Account {
            lamports: rent_exempt(cfg_bytes.len()),
            data: cfg_bytes,
            owner: settlement_program,
            executable: false,
            rent_epoch: 0,
        },
    );

    if let Some((status, record_payer, message_hash)) = record {
        let (record_pda, _) = sclient::exit_record_pda(&settlement_program, chain_id, message_hash);
        let rec = exit_record::ExitRecordFields {
            chain_id,
            batch: 1,
            message_hash,
            sol_recipient: [0x22u8; 32],
            amount: 1_000_000_000u128,
            window_index: 0,
            proved_slot: 0,
            status,
            payer: record_payer.to_bytes(),
            asset: [0u8; 20],
        };
        let rec_bytes = exit_record::write(&rec).to_vec();
        pt.add_account(
            record_pda,
            Account {
                lamports: rent_exempt(rec_bytes.len()),
                data: rec_bytes,
                owner: settlement_program,
                executable: false,
                rent_epoch: 0,
            },
        );
    }

    if let Some((pubkey, lamports)) = payer_refund_seed {
        pt.add_account(pubkey, funded_account(lamports));
    }

    let ctx = pt.start_with_context().await;
    ConsumeRig {
        ctx,
        settlement_program,
        stub_program,
        payer,
    }
}

const MESSAGE_HASH: [u8; 32] = [0x42u8; 32];

// ---------------------------------------------------------------------------------------------
// ConsumeExit tests
// ---------------------------------------------------------------------------------------------

/// `consume_releases_and_closes_and_refunds_payer`: the stub bridge (its own program id set as
/// `exit_config.bridge_program`) CPI-signs `ConsumeExit` — the record closes, its exact rent lamports land
/// at `record.payer`, and the account is gone (owned by the system program, 0 bytes) afterward.
#[tokio::test]
async fn consume_releases_and_closes_and_refunds_payer() {
    let chain_id = 1;
    let record_payer = Pubkey::new_unique();
    let mut rig = build_consume_rig(
        chain_id,
        None, // exit_config.bridge_program = the real stub program id
        Some((exit_record::STATUS_PROVED, record_payer, MESSAGE_HASH)),
        Some((record_payer, 0)),
    )
    .await;

    let (exit_config_pda, _) = sclient::exit_config_pda(&rig.settlement_program, chain_id);
    let (record_pda, _) = sclient::exit_record_pda(&rig.settlement_program, chain_id, MESSAGE_HASH);

    let record_rent_before = rig
        .ctx
        .banks_client
        .get_account(record_pda)
        .await
        .unwrap()
        .expect("record must exist before consume")
        .lamports;
    let refund_before = rig
        .ctx
        .banks_client
        .get_account(record_payer)
        .await
        .unwrap()
        .map(|a| a.lamports)
        .unwrap_or(0);

    let ix = stub_consume_ix(
        &rig.stub_program,
        &rig.settlement_program,
        chain_id,
        MESSAGE_HASH,
        &exit_config_pda,
        &record_pda,
        &record_payer,
    );
    let payer = rig.payer.insecure_clone();
    let (result, cu, logs) = send_with_logs(&mut rig.ctx, &[ix], &payer, &[]).await;
    result.expect("a bridge-signed ConsumeExit against a PROVED record must succeed");
    eprintln!("ConsumeExit (via stub bridge CPI, whole tx) consumed {cu} CU");
    if let Some(logs) = logs {
        let cu_lines: Vec<&String> = logs.iter().filter(|l| l.contains("consumption")).collect();
        eprintln!("stub compute-unit markers around the ConsumeExit CPI: {cu_lines:?}");
    }

    // The record account is gone: closed, system-owned, empty.
    let after = rig.ctx.banks_client.get_account(record_pda).await.unwrap();
    match after {
        None => {}
        Some(acc) => {
            assert_eq!(acc.lamports, 0);
            assert_eq!(acc.data.len(), 0);
            assert_eq!(acc.owner, system_program::id());
        }
    }

    let refund_after = rig
        .ctx
        .banks_client
        .get_account(record_payer)
        .await
        .unwrap()
        .expect("payer_refund account must still exist")
        .lamports;
    assert_eq!(
        refund_after - refund_before,
        record_rent_before,
        "the record's exact rent must land at record.payer, no more, no less"
    );
}

/// `consume_by_non_bridge_signer_is_refused`: a DIFFERENT program (our stub, whose own `exit_consumer`
/// PDA is derived under ITS OWN id) tries to release an exit whose `exit_config.bridge_program` names some
/// OTHER program — `NotBridgeProgram` (71). The stub always signs its own PDA regardless of what
/// `exit_config` says, so pointing `bridge_program` elsewhere is exactly the mismatch this guards.
#[tokio::test]
async fn consume_by_non_bridge_signer_is_refused() {
    let chain_id = 1;
    let record_payer = Pubkey::new_unique();
    let not_the_bridge = Pubkey::new_unique(); // exit_config names THIS, never the stub's own id
    let mut rig = build_consume_rig(
        chain_id,
        Some(not_the_bridge),
        Some((exit_record::STATUS_PROVED, record_payer, MESSAGE_HASH)),
        Some((record_payer, 0)),
    )
    .await;

    let (exit_config_pda, _) = sclient::exit_config_pda(&rig.settlement_program, chain_id);
    let (record_pda, _) = sclient::exit_record_pda(&rig.settlement_program, chain_id, MESSAGE_HASH);
    let ix = stub_consume_ix(
        &rig.stub_program,
        &rig.settlement_program,
        chain_id,
        MESSAGE_HASH,
        &exit_config_pda,
        &record_pda,
        &record_payer,
    );
    let payer = rig.payer.insecure_clone();
    let err = send(&mut rig.ctx, &[ix], &payer, &[])
        .await
        .expect_err("a non-registered bridge's CPI-signed PDA must be refused");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::NotBridgeProgram as u32)
    );

    // The record must be untouched — still PROVED, rent intact.
    let acc = rig
        .ctx
        .banks_client
        .get_account(record_pda)
        .await
        .unwrap()
        .expect("record must still exist after a refused consume");
    let decoded = sclient::decode_exit_record_account(&acc.data).unwrap();
    assert_eq!(decoded.status, exit_record::STATUS_PROVED);
}

/// `consume_refund_to_wrong_account_is_refused`: `payer_refund != record.payer` — `InvalidArgument`,
/// nothing moved. The refund destination is read off the record, never an ix-arg-chosen account.
#[tokio::test]
async fn consume_refund_to_wrong_account_is_refused() {
    let chain_id = 1;
    let record_payer = Pubkey::new_unique();
    let attacker = Pubkey::new_unique(); // NOT record.payer
    let mut rig = build_consume_rig(
        chain_id,
        None,
        Some((exit_record::STATUS_PROVED, record_payer, MESSAGE_HASH)),
        Some((attacker, 1_000_000)),
    )
    .await;

    let (exit_config_pda, _) = sclient::exit_config_pda(&rig.settlement_program, chain_id);
    let (record_pda, _) = sclient::exit_record_pda(&rig.settlement_program, chain_id, MESSAGE_HASH);
    let ix = stub_consume_ix(
        &rig.stub_program,
        &rig.settlement_program,
        chain_id,
        MESSAGE_HASH,
        &exit_config_pda,
        &record_pda,
        &attacker, // wrong refund destination
    );
    let payer = rig.payer.insecure_clone();
    let attacker_before = rig
        .ctx
        .banks_client
        .get_account(attacker)
        .await
        .unwrap()
        .unwrap()
        .lamports;
    let err = send(&mut rig.ctx, &[ix], &payer, &[])
        .await
        .expect_err("a refund to any account other than record.payer must be refused");
    assert!(matches!(
        err,
        TransactionError::InstructionError(_, InstructionError::InvalidArgument)
    ));
    let attacker_after = rig
        .ctx
        .banks_client
        .get_account(attacker)
        .await
        .unwrap()
        .unwrap()
        .lamports;
    assert_eq!(attacker_after, attacker_before, "nothing must move");
    let record_still_there = rig.ctx.banks_client.get_account(record_pda).await.unwrap();
    assert!(
        record_still_there.is_some(),
        "the record must not be closed by a refused call"
    );
}

/// `consume_before_prove_is_refused`: no `exit_record` account at all — refused at the owner/seeds check
/// (a record only ever exists as PROVED, written by `prove_exit`; there is nothing else it could be).
#[tokio::test]
async fn consume_before_prove_is_refused() {
    let chain_id = 1;
    let payer_refund = Pubkey::new_unique();
    let mut rig = build_consume_rig(chain_id, None, None, Some((payer_refund, 0))).await;

    let (exit_config_pda, _) = sclient::exit_config_pda(&rig.settlement_program, chain_id);
    let (record_pda, _) = sclient::exit_record_pda(&rig.settlement_program, chain_id, MESSAGE_HASH);
    let ix = stub_consume_ix(
        &rig.stub_program,
        &rig.settlement_program,
        chain_id,
        MESSAGE_HASH,
        &exit_config_pda,
        &record_pda,
        &payer_refund,
    );
    let payer = rig.payer.insecure_clone();
    let err = send(&mut rig.ctx, &[ix], &payer, &[])
        .await
        .expect_err("consuming an exit that was never proved must be refused");
    assert!(
        is_incorrect_program_id(&err),
        "a non-existent record fails the owner check, not a custom SettleError: {err:?}"
    );
}

/// `consume_twice_is_refused`: after a successful consume the record is gone; a second call hits the same
/// owner-check refusal as `consume_before_prove_is_refused` — the record's recycling IS the refusal.
#[tokio::test]
async fn consume_twice_is_refused() {
    let chain_id = 1;
    let record_payer = Pubkey::new_unique();
    let mut rig = build_consume_rig(
        chain_id,
        None,
        Some((exit_record::STATUS_PROVED, record_payer, MESSAGE_HASH)),
        Some((record_payer, 0)),
    )
    .await;

    let (exit_config_pda, _) = sclient::exit_config_pda(&rig.settlement_program, chain_id);
    let (record_pda, _) = sclient::exit_record_pda(&rig.settlement_program, chain_id, MESSAGE_HASH);
    let ix = || {
        stub_consume_ix(
            &rig.stub_program,
            &rig.settlement_program,
            chain_id,
            MESSAGE_HASH,
            &exit_config_pda,
            &record_pda,
            &record_payer,
        )
    };
    let payer = rig.payer.insecure_clone();
    send(&mut rig.ctx, &[ix()], &payer, &[])
        .await
        .expect("the first consume must succeed");

    let err = send(&mut rig.ctx, &[ix()], &payer, &[])
        .await
        .expect_err("consuming the same exit twice must be refused");
    assert!(
        is_incorrect_program_id(&err),
        "the second consume must fail the owner check against a now-recycled account: {err:?}"
    );
}

/// `consume_record_not_proved_is_refused`: a record that EXISTS, IS OWNED by the settlement program (so
/// step (1)'s seeds/owner check cannot refuse it), but whose `status` is not `STATUS_PROVED` — a state
/// `ProveExit` never actually produces (a record is born PROVED and only ever recycled away, never left
/// behind with any other status) but the exact state `exit::consume_exit`'s own doc names as this check's
/// real reason to exist, distinct from the owner check `consume_before_prove_is_refused`/
/// `consume_twice_is_refused` above exercise. Named `ExitNotProved` (70), not a generic decode error.
#[tokio::test]
async fn consume_record_not_proved_is_refused() {
    let chain_id = 1;
    let record_payer = Pubkey::new_unique();
    let mut rig = build_consume_rig(
        chain_id,
        None,
        Some((exit_record::STATUS_RELEASED, record_payer, MESSAGE_HASH)),
        Some((record_payer, 0)),
    )
    .await;

    let (exit_config_pda, _) = sclient::exit_config_pda(&rig.settlement_program, chain_id);
    let (record_pda, _) = sclient::exit_record_pda(&rig.settlement_program, chain_id, MESSAGE_HASH);
    let ix = stub_consume_ix(
        &rig.stub_program,
        &rig.settlement_program,
        chain_id,
        MESSAGE_HASH,
        &exit_config_pda,
        &record_pda,
        &record_payer,
    );
    let payer = rig.payer.insecure_clone();
    let err = send(&mut rig.ctx, &[ix], &payer, &[])
        .await
        .expect_err("consuming a record whose status is not PROVED must be refused");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::ExitNotProved as u32)
    );
}

// ---------------------------------------------------------------------------------------------
// `prove_exit_after_release_is_refused`: a genuine ProveExit -> ConsumeExit -> re-ProveExit of the SAME
// message. Small, deliberate duplication of `tests/exit_prove.rs`'s fixture-loading helpers for this one
// call site (see the module doc).
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
const BASE_STATE_ROOT: &str = include_str!("../../../fixtures/exit/anvil_state_root.json");
const BASE_MESSAGE: &str = include_str!("../../../fixtures/exit/message_hash.json");

fn base_portal() -> [u8; 20] {
    let m: PortalJson = serde_json::from_str(BASE_MESSAGE).unwrap();
    hex20(&m.portal)
}
fn base_state_root() -> [u8; 32] {
    let r: StateRootJson = serde_json::from_str(BASE_STATE_ROOT).unwrap();
    hex32(&r.state_root)
}
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
fn to_layout(m: sclient::ExitMessageArg) -> rome_zk_layouts::exit::ExitMessage {
    rome_zk_layouts::exit::ExitMessage {
        nonce: m.nonce,
        l2_sender: m.l2_sender,
        sol_recipient: m.sol_recipient,
        asset: m.asset,
        amount: m.amount,
    }
}

#[tokio::test]
async fn prove_exit_after_release_is_refused() {
    let chain_id = 1;
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let stub_program = rome_zk_testkit::fixed_stub_bridge_program_id();
    let payer = funded_keypair();
    let mut pt = rome_zk_testkit::program_test(
        &[
            rome_zk_testkit::ProgramSpec::upgradeable("zk_settlement", settlement_program),
            rome_zk_testkit::ProgramSpec::new("zk_exit_stub_bridge", stub_program),
        ],
        true,
    );
    pt.add_account(payer.pubkey(), funded_account(50_000_000_000));

    let (root_pda, _) = sclient::root_pda(&settlement_program, chain_id);
    let root_fields = rome_zk_layouts::root::RootFields {
        chain_id,
        number: 1,
        parent_hash: [0u8; 32],
        state_root: base_state_root(),
        block_hash: [0u8; 32],
        updates: 0,
        profile: 0,
        challenge_window_slots: 100,
        prove_window_slots: 1000,
        proving_policy: 1,
        poster_bond: 0,
        exit_cap_per_window: 1_000_000_000,
        authority: Pubkey::new_unique().to_bytes(),
        head_pending_batch: 1,
        head_final_batch: 1,
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

    let (exit_config_pda, _) = sclient::exit_config_pda(&settlement_program, chain_id);
    let cfg = exit_config::ExitConfigFields {
        chain_id,
        exit_portal: base_portal(),
        bridge_program: stub_program.to_bytes(),
        pending_exit_portal: [0u8; 20],
        pending_bridge_program: [0u8; 32],
        pending_exit_cap: 0,
        pending_poster_bond: 0,
        activation_slot: 0,
        pending_mask: 0,
    };
    let cfg_bytes = exit_config::write(&cfg).to_vec();
    pt.add_account(
        exit_config_pda,
        Account {
            lamports: rent_exempt(cfg_bytes.len()),
            data: cfg_bytes,
            owner: settlement_program,
            executable: false,
            rent_epoch: 0,
        },
    );

    let mut ctx = pt.start_with_context().await;
    let message = nonce0_message();
    let message_hash = to_layout(message).message_hash();

    // --- first ProveExit: must succeed, writes the record + burns the nullifier bit ---
    let now = ctx.banks_client.get_root_slot().await.unwrap();
    let window_index = now / root_fields.challenge_window_slots as u64;
    let page = rome_zk_layouts::exit::nullifier_page(message.nonce);
    let prove_ix = sclient::prove_exit_ix(
        &settlement_program,
        &payer.pubkey(),
        chain_id,
        1,
        message,
        nonce0_proof(),
        window_index,
        page,
    );
    let payer_clone = payer.insecure_clone();
    send(&mut ctx, std::slice::from_ref(&prove_ix), &payer_clone, &[])
        .await
        .expect("the first ProveExit must succeed");

    // --- ConsumeExit via the stub bridge: releases and closes the record ---
    let (record_pda, _) = sclient::exit_record_pda(&settlement_program, chain_id, message_hash);
    let consume_ix = stub_consume_ix(
        &stub_program,
        &settlement_program,
        chain_id,
        message_hash,
        &exit_config_pda,
        &record_pda,
        &payer.pubkey(),
    );
    let payer_clone = payer.insecure_clone();
    send(&mut ctx, &[consume_ix], &payer_clone, &[])
        .await
        .expect("ConsumeExit must succeed against the just-proved record");
    assert!(
        ctx.banks_client
            .get_account(record_pda)
            .await
            .unwrap()
            .is_none()
            || ctx
                .banks_client
                .get_account(record_pda)
                .await
                .unwrap()
                .unwrap()
                .lamports
                == 0,
        "the record must be closed after ConsumeExit"
    );

    // --- re-ProveExit of the SAME message: the record is gone, but the nullifier bit persists ---
    let payer_clone = payer.insecure_clone();
    let err = send(&mut ctx, &[prove_ix], &payer_clone, &[])
        .await
        .expect_err("re-proving a released exit must still be refused by the persistent nullifier");
    assert_eq!(
        custom_error(&err),
        Some(zk_settlement::errors::SettleError::ExitAlreadyProved as u32),
        "the nullifier bit, not the record, is what makes this a replay"
    );
}
