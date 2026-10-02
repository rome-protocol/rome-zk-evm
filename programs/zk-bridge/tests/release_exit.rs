//! Real-BPF tests for `ReleaseExit` — the fund-safety core. Two real programs (`zk_bridge` + `zk_settlement`)
//! plus the real SPL Token program, `prefer_bpf`. Run:
//! `cargo build-sbf --manifest-path programs/zk-bridge/Cargo.toml --sbf-out-dir <workspace>/target/deploy`,
//! `cargo build-sbf --manifest-path programs/zk-settlement/Cargo.toml --sbf-out-dir <workspace>/target/deploy`,
//! and build the real SPL Token program `.so` (see `README.md`) before `cargo test -p zk-bridge`.
//!
//! Every test hand-builds `vault_config`/the vault's SPL account/`exit_config`/`exit_record`/the
//! recipient's ATA directly (`pt.add_account`) — the same "fixture, not a full ceremony" pattern
//! `programs/zk-settlement/tests/exit_consume.rs` already establishes: `ReleaseExit`'s own logic does not
//! care how a record reached PROVED, only that it did, and this is a real settlement CPI regardless (the
//! `exit_config`/`exit_record` accounts are read by the REAL `zk_settlement` program during `ConsumeExit`,
//! not by a stub).

mod common;

use common::*;
use rome_zk_layouts::exit::exit_record;
use solana_program::{instruction::AccountMeta, pubkey::Pubkey};
use solana_sdk::{
    instruction::InstructionError,
    signature::{Keypair, Signer},
    transaction::TransactionError,
};

const MINT_DECIMALS: u8 = 9;
const AMOUNT_WEI: u128 = 1_000_000_000_000_000_000; // 1e18 wei = 1 whole unit of an 18-decimal asset
const VAULT_STARTING_BALANCE: u64 = 5_000_000_000;

struct Scene {
    ctx: solana_program_test::ProgramTestContext,
    mint: Pubkey,
    recipient: Pubkey,
    payer_refund: Pubkey,
    recipient_ata: Pubkey,
    vault_token_pda: Pubkey,
    exit_record_pda: Pubkey,
    payer: Keypair,
}

/// Builds the full rig for a release test: a PROVED `exit_record` (native asset, `amount_wei`), a funded
/// vault, and the recipient's own (empty) ATA already existing. `asset`/`record_owner` are overridable so
/// the unsupported-asset / wrong-settlement-owner tests can each change exactly one thing.
///
/// `mint`/`recipient` are pinned to `rome_zk_testkit::fixed_mint_pubkey`/`fixed_recipient_pubkey`,
/// not `Pubkey::new_unique()`: `vault_token_pda`/`recipient_ata` are both PDAs
/// seeded by these values, so a `Pubkey::new_unique()` mint/recipient makes the measured
/// `ReleaseExit`/`InitVault` CU vary run to run with that PDA's own bump-seed search depth — see
/// `rome_zk_testkit::fixed_mint_pubkey`'s own doc.
async fn build_scene(amount_wei: u128, asset: [u8; 20], record_owner: Pubkey) -> Scene {
    let mint = rome_zk_testkit::fixed_mint_pubkey();
    let recipient = rome_zk_testkit::fixed_recipient_pubkey();
    let payer_refund = Pubkey::new_unique();
    let (vault_authority, _) = zk_bridge_client::vault_authority_pda(
        &bridge_program_id(),
        &settlement_program_id(),
        CHAIN_ID,
    );
    let (vault_config_pda, _) = zk_bridge_client::vault_config_pda(
        &bridge_program_id(),
        &settlement_program_id(),
        CHAIN_ID,
    );
    let (vault_token_pda, _) = zk_bridge_client::vault_token_pda(
        &bridge_program_id(),
        &settlement_program_id(),
        CHAIN_ID,
        &mint,
    );
    let (exit_config_pda, _) =
        zk_settlement_client::exit_config_pda(&settlement_program_id(), CHAIN_ID);
    let (exit_record_pda, _) =
        zk_settlement_client::exit_record_pda(&settlement_program_id(), CHAIN_ID, MESSAGE_HASH);
    let recipient_ata = zk_bridge_client::recipient_ata(&recipient, &mint);

    let payer = Keypair::new();
    let mut pt = base_program_test();
    pt.add_account(payer.pubkey(), funded_account(10_000_000_000));
    pt.add_account(
        vault_config_pda,
        vault_config_account(
            settlement_program_id(),
            mint,
            MINT_DECIMALS,
            Pubkey::new_unique(),
        ),
    );
    pt.add_account(
        vault_token_pda,
        token_account(&mint, &vault_authority, VAULT_STARTING_BALANCE),
    );
    pt.add_account(
        exit_config_pda,
        exit_config_account(settlement_program_id(), bridge_program_id()),
    );
    pt.add_account(
        exit_record_pda,
        exit_record_account(
            record_owner,
            recipient.to_bytes(),
            amount_wei,
            payer_refund,
            asset,
            exit_record::STATUS_PROVED,
        ),
    );
    pt.add_account(recipient_ata, token_account(&mint, &recipient, 0));
    pt.add_account(payer_refund, funded_account(0));

    let ctx = pt.start_with_context().await;
    Scene {
        ctx,
        mint,
        recipient,
        payer_refund,
        recipient_ata,
        vault_token_pda,
        exit_record_pda,
        payer,
    }
}

/// Same rig as `build_scene`, EXCEPT the recipient's ATA is never pre-created
/// (`create_ata_idempotent_then_release_lands_for_a_recipient_with_no_ata`: a first-time recipient's ATA
/// genuinely does not exist yet — `release.rs:133-137` derives and checks the address but never creates
/// it, so a caller that skips provisioning would otherwise hit a missing-account error on the SPL
/// transfer) AND, unlike `build_scene`, the `mint` account itself is a real on-chain SPL Token `Mint`
/// (`build_scene`'s other tests never CPI the real Associated Token Program, so its hand-rolled
/// `token::transfer_ix` never reads the mint account and `build_scene` never bothers creating it; the
/// REAL, deployed `CreateIdempotent` this test CPIs for real DOES read it — via its own internal
/// `GetAccountDataSize` CPI to the token program — so it must actually exist here).
async fn build_scene_no_recipient_ata(amount_wei: u128) -> Scene {
    let mint = rome_zk_testkit::fixed_mint_pubkey();
    let recipient = rome_zk_testkit::fixed_recipient_pubkey();
    let payer_refund = Pubkey::new_unique();
    let (vault_authority, _) = zk_bridge_client::vault_authority_pda(
        &bridge_program_id(),
        &settlement_program_id(),
        CHAIN_ID,
    );
    let (vault_config_pda, _) = zk_bridge_client::vault_config_pda(
        &bridge_program_id(),
        &settlement_program_id(),
        CHAIN_ID,
    );
    let (vault_token_pda, _) = zk_bridge_client::vault_token_pda(
        &bridge_program_id(),
        &settlement_program_id(),
        CHAIN_ID,
        &mint,
    );
    let (exit_config_pda, _) =
        zk_settlement_client::exit_config_pda(&settlement_program_id(), CHAIN_ID);
    let (exit_record_pda, _) =
        zk_settlement_client::exit_record_pda(&settlement_program_id(), CHAIN_ID, MESSAGE_HASH);
    let recipient_ata = zk_bridge_client::recipient_ata(&recipient, &mint);

    let payer = Keypair::new();
    let mut pt = base_program_test();
    pt.add_account(payer.pubkey(), funded_account(10_000_000_000));
    pt.add_account(mint, mint_account(MINT_DECIMALS));
    pt.add_account(
        vault_config_pda,
        vault_config_account(
            settlement_program_id(),
            mint,
            MINT_DECIMALS,
            Pubkey::new_unique(),
        ),
    );
    pt.add_account(
        vault_token_pda,
        token_account(&mint, &vault_authority, VAULT_STARTING_BALANCE),
    );
    pt.add_account(
        exit_config_pda,
        exit_config_account(settlement_program_id(), bridge_program_id()),
    );
    pt.add_account(
        exit_record_pda,
        exit_record_account(
            settlement_program_id(),
            recipient.to_bytes(),
            amount_wei,
            payer_refund,
            [0u8; 20],
            exit_record::STATUS_PROVED,
        ),
    );
    // NOTE: no `pt.add_account(recipient_ata, ...)` — this is the whole point of this scene.
    pt.add_account(payer_refund, funded_account(0));

    let ctx = pt.start_with_context().await;
    Scene {
        ctx,
        mint,
        recipient,
        payer_refund,
        recipient_ata,
        vault_token_pda,
        exit_record_pda,
        payer,
    }
}

fn release_ix(scene: &Scene) -> solana_program::instruction::Instruction {
    zk_bridge_client::release_exit_ix(
        &bridge_program_id(),
        &settlement_program_id(),
        CHAIN_ID,
        MESSAGE_HASH,
        &scene.mint,
        &zk_settlement_client::exit_config_pda(&settlement_program_id(), CHAIN_ID).0,
        &scene.exit_record_pda,
        &scene.payer_refund,
        &scene.recipient,
    )
}

fn create_ata_ix(scene: &Scene) -> solana_program::instruction::Instruction {
    zk_bridge_client::create_recipient_ata_idempotent_ix(
        &scene.payer.pubkey(),
        &scene.recipient,
        &scene.mint,
    )
}

fn custom_error(err: &TransactionError) -> Option<u32> {
    match err {
        TransactionError::InstructionError(_, InstructionError::Custom(c)) => Some(*c),
        _ => None,
    }
}

/// `release_pays_only_record_recipient_and_closes_record`: the whole fund-safety property, proved with a
/// genuine `ConsumeExit` CPI — the recipient's ATA receives exactly the decimal-scaled amount, the vault's
/// balance drops by exactly that, `exit_record` is closed (system-owned, empty), and `payer_refund`
/// receives the record's exact rent lamports.
///
/// **Mutation (recipient from an ix arg): with `release.rs` step (6) changed to derive `recipient` from a
/// new `ReleaseExitArgs::recipient` field instead of `record.sol_recipient`, this test's own assertion —
/// `recipient_ata`'s balance increased — still passes as long as the caller happens to pass the correct
/// recipient; the mutation is instead caught by `release_to_wrong_recipient_ata_is_refused` below, whose
/// whole point is a caller that does NOT.**
#[tokio::test]
async fn release_pays_only_record_recipient_and_closes_record() {
    let mut scene = build_scene(AMOUNT_WEI, [0u8; 20], settlement_program_id()).await;
    let expected_mint_amount = 1_000_000_000u64; // 1e18 wei / 10^(18-9)

    let record_rent = get_account(&mut scene.ctx, scene.exit_record_pda)
        .await
        .unwrap()
        .lamports;

    let ix = release_ix(&scene);
    let payer = scene.payer.insecure_clone();
    let (result, cu, logs) =
        rome_zk_testkit::send_measuring_cu(&mut scene.ctx, &[ix], &payer, &[]).await;
    result.expect("a genuine ReleaseExit against a PROVED record must succeed");
    eprintln!(
        "ReleaseExit (real BPF, incl. the ConsumeExit CPI + the SPL transfer) consumed {cu} CU"
    );
    // A ceiling, not an exact-equality pin (the prior figure varied run to run because `build_scene`'s
    // mint/recipient were `Pubkey::new_unique()`; now pinned to
    // `rome_zk_testkit::fixed_mint_pubkey`/`fixed_recipient_pubkey`, this figure IS bit-exact run to run
    // on the same binary — see the CHANGELOG/README for the measured value). A ceiling still catches a
    // regression without flaking on an unrelated toolchain rebuild that shifts CU by a handful of units.
    assert!(
        cu <= 65_000,
        "ReleaseExit CU regressed past the reproducible ceiling: {cu}"
    );
    if let Some(logs) = logs {
        let settlement = settlement_program_id().to_string();
        let consume_cu_line = logs
            .iter()
            .find(|l| l.starts_with(&format!("Program {settlement} consumed")));
        eprintln!("ConsumeExit-via-real-CPI CU (nested log line): {consume_cu_line:?}");
    }

    let recipient_bal = decode_token_amount(
        &get_account(&mut scene.ctx, scene.recipient_ata)
            .await
            .unwrap()
            .data,
    );
    assert_eq!(recipient_bal, expected_mint_amount);

    let vault_bal = decode_token_amount(
        &get_account(&mut scene.ctx, scene.vault_token_pda)
            .await
            .unwrap()
            .data,
    );
    assert_eq!(vault_bal, VAULT_STARTING_BALANCE - expected_mint_amount);

    let record_after = get_account(&mut scene.ctx, scene.exit_record_pda).await;
    match record_after {
        None => {}
        Some(acc) => {
            assert_eq!(acc.lamports, 0);
            assert_eq!(acc.data.len(), 0);
            assert_eq!(acc.owner, solana_system_interface::program::id());
        }
    }

    let refund_after = get_account(&mut scene.ctx, scene.payer_refund)
        .await
        .unwrap()
        .lamports;
    assert_eq!(
        refund_after, record_rent,
        "record.payer must receive the exact rent"
    );
}

/// `release_to_wrong_payer_refund_is_refused`:
/// naming a `payer_refund` account other than `record.payer` is refused (`WrongPayerRefund`, error 6)
/// before the settlement CPI or any transfer ever runs — proves `release.rs`'s own named guard is not
/// decorative (previously only `zk-settlement::exit::consume_exit`'s generic `InvalidArgument` on the CPI
/// caught this).
#[tokio::test]
async fn release_to_wrong_payer_refund_is_refused() {
    let mut scene = build_scene(AMOUNT_WEI, [0u8; 20], settlement_program_id()).await;
    let wrong_payer_refund = Pubkey::new_unique();
    scene.ctx.set_account(
        &wrong_payer_refund,
        &solana_sdk::account::AccountSharedData::from(funded_account(0)),
    );

    let mut ix = release_ix(&scene);
    // accounts[5] = payer_refund (see `zk_bridge_client::release_exit_ix`'s own doc for the order).
    ix.accounts[5] = AccountMeta::new(wrong_payer_refund, false);

    let payer = scene.payer.insecure_clone();
    let (result, ..) = rome_zk_testkit::send_measuring_cu(&mut scene.ctx, &[ix], &payer, &[]).await;
    let err = result.expect_err("a payer_refund other than record.payer must be refused");
    assert_eq!(
        custom_error(&err),
        Some(zk_bridge::errors::BridgeError::WrongPayerRefund as u32)
    );

    let record_still_there = get_account(&mut scene.ctx, scene.exit_record_pda).await;
    assert!(
        record_still_there.is_some(),
        "the record must not be consumed by a refused call"
    );
    let vault_bal = decode_token_amount(
        &get_account(&mut scene.ctx, scene.vault_token_pda)
            .await
            .unwrap()
            .data,
    );
    assert_eq!(
        vault_bal, VAULT_STARTING_BALANCE,
        "nothing must move when payer_refund is wrong"
    );
}

/// `release_to_wrong_recipient_ata_is_refused`: an attacker names their own ATA as `recipient_ata` —
/// refused before the settlement CPI or any transfer ever runs; nothing moves.
///
/// **This is the test that turns red if the recipient comes from an instruction argument**: if `release.rs`
/// took the recipient from a caller-supplied argument instead of `record.sol_recipient`, an attacker's own
/// pubkey there would make THIS instruction's own derived `expect_ata` equal the attacker's ATA — the
/// check would pass and the funds would move to the attacker.
#[tokio::test]
async fn release_to_wrong_recipient_ata_is_refused() {
    let mut scene = build_scene(AMOUNT_WEI, [0u8; 20], settlement_program_id()).await;
    let attacker = Pubkey::new_unique();
    let attacker_ata = zk_bridge_client::recipient_ata(&attacker, &scene.mint);
    scene.ctx.set_account(
        &attacker_ata,
        &solana_sdk::account::AccountSharedData::from(token_account(&scene.mint, &attacker, 0)),
    );

    let mut ix = release_ix(&scene);
    // accounts[8] = recipient_ata (see `zk_bridge_client::release_exit_ix`'s own doc for the order).
    ix.accounts[8] = AccountMeta::new(attacker_ata, false);

    let payer = scene.payer.insecure_clone();
    let (result, ..) = rome_zk_testkit::send_measuring_cu(&mut scene.ctx, &[ix], &payer, &[]).await;
    let err =
        result.expect_err("naming any ATA other than record.sol_recipient's own must be refused");
    assert_eq!(
        custom_error(&err),
        Some(zk_bridge::errors::BridgeError::WrongRecipientAta as u32)
    );

    let attacker_bal = decode_token_amount(
        &get_account(&mut scene.ctx, attacker_ata)
            .await
            .unwrap()
            .data,
    );
    assert_eq!(attacker_bal, 0, "nothing must move to the attacker");
    let vault_bal = decode_token_amount(
        &get_account(&mut scene.ctx, scene.vault_token_pda)
            .await
            .unwrap()
            .data,
    );
    assert_eq!(
        vault_bal, VAULT_STARTING_BALANCE,
        "the vault must be untouched"
    );
    let record_still_there = get_account(&mut scene.ctx, scene.exit_record_pda).await;
    assert!(
        record_still_there.is_some(),
        "the record must not be consumed by a refused call"
    );
}

/// `release_unsupported_asset_refused`: `exit_record.asset != [0; 20]` — refused before any CPI or
/// transfer, v1 native-asset-only, matching `zk-settlement::ProveExit`'s own restriction.
#[tokio::test]
async fn release_unsupported_asset_refused() {
    let mut scene = build_scene(AMOUNT_WEI, [0x99u8; 20], settlement_program_id()).await;
    let ix = release_ix(&scene);
    let payer = scene.payer.insecure_clone();
    let (result, ..) = rome_zk_testkit::send_measuring_cu(&mut scene.ctx, &[ix], &payer, &[]).await;
    let err = result.expect_err("a non-native-asset exit record must be refused");
    assert_eq!(
        custom_error(&err),
        Some(zk_bridge::errors::BridgeError::UnsupportedAsset as u32)
    );
    let record_still_there = get_account(&mut scene.ctx, scene.exit_record_pda).await;
    assert!(
        record_still_there.is_some(),
        "the record must not be consumed by a refused call"
    );
}

/// `release_with_wrong_settlement_owner_refused`: the `exit_record` account exists at the right address but
/// is owned by some OTHER program, not `vault_config.settlement_program` — refused (`WrongSettlementOwner`)
/// before the record is even decoded.
#[tokio::test]
async fn release_with_wrong_settlement_owner_refused() {
    let not_settlement = Pubkey::new_unique();
    let mut scene = build_scene(AMOUNT_WEI, [0u8; 20], not_settlement).await;
    let ix = release_ix(&scene);
    let payer = scene.payer.insecure_clone();
    let (result, ..) = rome_zk_testkit::send_measuring_cu(&mut scene.ctx, &[ix], &payer, &[]).await;
    let err = result.expect_err(
        "a record owned by a program other than vault_config.settlement_program must be refused",
    );
    assert_eq!(
        custom_error(&err),
        Some(zk_bridge::errors::BridgeError::WrongSettlementOwner as u32)
    );
}

/// `release_twice_is_refused`: the second call finds the record recycled (system-owned by the first
/// call's own `ConsumeExit`) and hits the same `WrongSettlementOwner` guard — no double payout, because the
/// CPI that closes the record runs BEFORE the transfer and a failed CPI aborts the whole transaction.
#[tokio::test]
async fn release_twice_is_refused() {
    let mut scene = build_scene(AMOUNT_WEI, [0u8; 20], settlement_program_id()).await;
    let payer = scene.payer.insecure_clone();

    let ix1 = release_ix(&scene);
    let (r1, ..) = rome_zk_testkit::send_measuring_cu(&mut scene.ctx, &[ix1], &payer, &[]).await;
    r1.expect("the first ReleaseExit must succeed");
    let recipient_bal_after_first = decode_token_amount(
        &get_account(&mut scene.ctx, scene.recipient_ata)
            .await
            .unwrap()
            .data,
    );

    let payer2 = scene.payer.insecure_clone();
    let ix2 = release_ix(&scene);
    let (r2, ..) = rome_zk_testkit::send_measuring_cu(&mut scene.ctx, &[ix2], &payer2, &[]).await;
    let err = r2.expect_err("releasing the same exit twice must be refused — no double payout");
    assert_eq!(
        custom_error(&err),
        Some(zk_bridge::errors::BridgeError::WrongSettlementOwner as u32)
    );

    let recipient_bal_after_second = decode_token_amount(
        &get_account(&mut scene.ctx, scene.recipient_ata)
            .await
            .unwrap()
            .data,
    );
    assert_eq!(
        recipient_bal_after_first, recipient_bal_after_second,
        "the recipient must not receive a second payout"
    );
}

/// `decimal_scaling_rounds_down_dust_stays_in_vault`: `amount = 1e18 + 500` wei, `mint_decimals = 9` ->
/// `mint_amount = 1e9` exactly (integer division truncates); the 500-wei dust is never transferred and
/// simply stays in the vault's SPL balance.
#[tokio::test]
async fn decimal_scaling_rounds_down_dust_stays_in_vault() {
    let amount_wei = AMOUNT_WEI + 500; // 1e18 + 500 wei
    let mut scene = build_scene(amount_wei, [0u8; 20], settlement_program_id()).await;
    let expected_mint_amount = 1_000_000_000u64; // floor((1e18 + 500) / 1e9) = 1e9 exactly

    let ix = release_ix(&scene);
    let payer = scene.payer.insecure_clone();
    let (result, ..) = rome_zk_testkit::send_measuring_cu(&mut scene.ctx, &[ix], &payer, &[]).await;
    result.expect("ReleaseExit must succeed");

    let recipient_bal = decode_token_amount(
        &get_account(&mut scene.ctx, scene.recipient_ata)
            .await
            .unwrap()
            .data,
    );
    assert_eq!(
        recipient_bal, expected_mint_amount,
        "the 500-wei remainder must be truncated, never rounded up"
    );
    let vault_bal = decode_token_amount(
        &get_account(&mut scene.ctx, scene.vault_token_pda)
            .await
            .unwrap()
            .data,
    );
    assert_eq!(
        vault_bal,
        VAULT_STARTING_BALANCE - expected_mint_amount,
        "the vault must have transferred exactly the truncated amount — the dust's WEI value has no mint-unit representative and is never asked to leave"
    );
}

/// `create_ata_idempotent_then_release_lands_for_a_recipient_with_no_ata` — the recipient's ATA does not
/// exist yet; `[create_recipient_ata_idempotent_ix, ReleaseExit]` sent as ONE transaction lands: the
/// Associated Token Program creates the account for real (a real CPI, real BPF), then the real SPL
/// transfer into it succeeds. Also proves the create is a genuine no-op when the ATA already exists: a
/// second, standalone `create_recipient_ata_idempotent_ix` against the now-existing account still
/// succeeds (never `AccountAlreadyInitialized` or any other error) and leaves its balance untouched.
#[tokio::test]
async fn create_ata_idempotent_then_release_lands_for_a_recipient_with_no_ata() {
    let mut scene = build_scene_no_recipient_ata(AMOUNT_WEI).await;
    let expected_mint_amount = 1_000_000_000u64; // 1e18 wei / 10^(18-9)

    assert!(
        get_account(&mut scene.ctx, scene.recipient_ata)
            .await
            .is_none(),
        "this scene's whole point is that the ATA does not exist yet"
    );

    let ata_ix = create_ata_ix(&scene);
    let release_ix = release_ix(&scene);
    let payer = scene.payer.insecure_clone();
    let (result, cu, _logs) =
        rome_zk_testkit::send_measuring_cu(&mut scene.ctx, &[ata_ix, release_ix], &payer, &[])
            .await;
    result.expect(
        "CreateIdempotent + ReleaseExit in one transaction must land for a recipient with no ATA yet",
    );
    eprintln!("CreateIdempotent + ReleaseExit (real BPF, both real CPIs) consumed {cu} CU");

    let recipient_bal = decode_token_amount(
        &get_account(&mut scene.ctx, scene.recipient_ata)
            .await
            .expect("the Associated Token Program must have created the account for real")
            .data,
    );
    assert_eq!(recipient_bal, expected_mint_amount);

    let vault_bal = decode_token_amount(
        &get_account(&mut scene.ctx, scene.vault_token_pda)
            .await
            .unwrap()
            .data,
    );
    assert_eq!(vault_bal, VAULT_STARTING_BALANCE - expected_mint_amount);

    // The idempotent create, run again now that the ATA exists, is a genuine no-op — never an error —
    // and never touches the balance the release just paid in.
    let payer2 = scene.payer.insecure_clone();
    let second_create_ix = create_ata_ix(&scene);
    let (result2, ..) =
        rome_zk_testkit::send_measuring_cu(&mut scene.ctx, &[second_create_ix], &payer2, &[]).await;
    result2
        .expect("CreateIdempotent against an already-existing ATA must be a no-op, never an error");
    let recipient_bal_after = decode_token_amount(
        &get_account(&mut scene.ctx, scene.recipient_ata)
            .await
            .unwrap()
            .data,
    );
    assert_eq!(
        recipient_bal_after, expected_mint_amount,
        "a no-op create must never touch the account's existing balance"
    );
}
