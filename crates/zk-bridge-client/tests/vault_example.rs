//! `vault` example tests:
//!
//!   - `vault_example_init_vault_refuses_without_confirm_on_unreachable_rpc` /
//!     `vault_example_fund_refuses_without_confirm_on_unreachable_rpc`: the `vault` example, run in its
//!     DEFAULT mode (no `--confirm`), against an RPC endpoint nothing listens on, exits non-zero with a
//!     NAMED error before it could ever reach a send — the same shape
//!     `release_exit_example_refuses_without_confirm` already proves for `release-exit`. Both
//!     subcommands read chain state before touching a keypair or branching on `--dry-run`/`--confirm` at
//!     all, so this same failure fires in EITHER mode.
//!   - `fund_refuses_vault_settlement_mismatch_before_any_transfer` (real-BPF): a `vault_config` account
//!     placed at the PDA a `--settlement` value of `settlement_a` derives, but whose OWN recorded
//!     `settlement_program` field is `settlement_b` — `zk_bridge_client::check_vault_settlement` (the
//!     exact function the `fund` subcommand calls before building `fund_ix`) refuses BY NAME
//!     (`VaultSettlementMismatch`), and the funder's real SPL token balance is provably unchanged
//!     afterward.
//!
//! Shells out to `cargo run --example vault --features devnet-driver` for the first two (the example's
//! own arg parsing reads `std::env::args()` directly, mirrors `release-exit.rs`/`governance.rs`), same
//! isolated-`CARGO_TARGET_DIR` reasoning `release_exit_example.rs`'s own test documents (a nested `cargo
//! run` racing the outer test binary's own concurrent compilation corrupts the outer run's linking).

mod common;

use common::*;
use solana_program::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use std::process::Command;

fn run_vault_example(subcommand: &str, args: &[&str]) -> std::process::Output {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    // Named from the SUBCOMMAND only (never the full arg list — an RPC URL like
    // `http://127.0.0.1:1` contains `:`/`/`, which breaks path-list env vars like
    // `DYLD_FALLBACK_LIBRARY_PATH` on macOS if it ever ends up inside a directory name here).
    let isolated_target_dir = std::env::temp_dir().join(format!(
        "vault-example-test-{}-{subcommand}",
        std::process::id()
    ));
    let mut full_args = vec![
        "run",
        "--quiet",
        "--example",
        "vault",
        "--features",
        "devnet-driver",
        "--",
    ];
    full_args.extend_from_slice(args);
    let output = Command::new(env!("CARGO"))
        .current_dir(manifest_dir)
        .env("CARGO_TARGET_DIR", &isolated_target_dir)
        .args(&full_args)
        .output()
        .expect("spawn `cargo run --example vault`");
    let _ = std::fs::remove_dir_all(&isolated_target_dir);
    output
}

#[test]
fn vault_example_init_vault_refuses_without_confirm_on_unreachable_rpc() {
    let output = run_vault_example(
        "init-vault",
        &[
            "init-vault",
            "--authority-keypair",
            "/nonexistent/authority.json",
            "--mint",
            "11111111111111111111111111111111",
            "--mint-decimals",
            "6",
            "--settlement",
            "11111111111111111111111111111111",
            "--bridge",
            "11111111111111111111111111111111",
            "--chain-id",
            "200101",
            "--rpc-url",
            "http://127.0.0.1:1",
        ],
    );
    assert!(
        !output.status.success(),
        "an unreachable RPC must exit non-zero, not silently continue toward a send.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("VaultConfigCheckFailed"),
        "the failure must be named (VaultConfigCheckFailed), reached before the authority keypair is \
         ever read and before any --dry-run/--confirm branch: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("-- sent:"),
        "must never reach a send: {stdout}"
    );
}

#[test]
fn vault_example_fund_refuses_without_confirm_on_unreachable_rpc() {
    let output = run_vault_example(
        "fund",
        &[
            "fund",
            "--payer-keypair",
            "/nonexistent/payer.json",
            "--amount",
            "1000",
            "--settlement",
            "11111111111111111111111111111111",
            "--bridge",
            "11111111111111111111111111111111",
            "--chain-id",
            "200101",
            "--rpc-url",
            "http://127.0.0.1:1",
        ],
    );
    assert!(
        !output.status.success(),
        "an unreachable RPC must exit non-zero, not silently continue toward a send.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("VaultConfigFetchFailed"),
        "the failure must be named (VaultConfigFetchFailed), reached before the payer keypair is ever \
         read and before any --dry-run/--confirm branch: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("-- sent:"),
        "must never reach a send: {stdout}"
    );
}

/// Real-BPF (loads the actual `zk_bridge`/SPL Token `.so` files — see `common::base_program_test`):
/// a `vault_config` sitting at the PDA `settlement_a` derives, but whose OWN recorded
/// `settlement_program` field is `settlement_b` — the exact "data disagrees with the address it was
/// derived from" scenario `zk_bridge_client::check_vault_settlement` exists to catch, checked here the
/// SAME way the `fund` subcommand does: fetch, decode, then check against the settlement the caller
/// intended (`settlement_a`) BEFORE ever building or sending `fund_ix`.
#[tokio::test]
async fn fund_refuses_vault_settlement_mismatch_before_any_transfer() {
    let settlement_a = Pubkey::new_unique(); // what the caller derives the vault_config PDA from
    let settlement_b = Pubkey::new_unique(); // what is actually recorded inside that account's data
    let mint = Pubkey::new_unique();
    let authority = Pubkey::new_unique();
    let funder = Keypair::new();
    let funder_starting_balance: u64 = 5_000_000;

    let (vault_config_pda, _) =
        zk_bridge_client::vault_config_pda(&bridge_program_id(), &settlement_a, CHAIN_ID);
    let funder_token_account = zk_bridge_client::recipient_ata(&funder.pubkey(), &mint);

    let mut pt = base_program_test();
    pt.add_account(
        vault_config_pda,
        vault_config_account(settlement_b, mint, 6, authority),
    );
    pt.add_account(
        funder_token_account,
        token_account(&mint, &funder.pubkey(), funder_starting_balance),
    );
    let mut ctx = pt.start_with_context().await;

    let vault_acc = get_account(&mut ctx, vault_config_pda)
        .await
        .expect("vault_config must exist (real-BPF, solana-program-test-served)");
    let cfg = zk_bridge_client::decode_vault_config_account(&vault_acc.data)
        .expect("decode vault_config");
    assert_eq!(
        cfg.settlement_program, settlement_b,
        "sanity: the account's own recorded settlement_program is settlement_b"
    );

    // The exact check `fund` runs before building fund_ix — settlement_a is what the caller derived the
    // PDA from (their stated intent), settlement_b is what is actually recorded there.
    let err = zk_bridge_client::check_vault_settlement(&cfg, &settlement_a).expect_err(
        "a vault whose recorded settlement_program disagrees with the derivation must refuse",
    );
    assert_eq!(err.expected, settlement_a);
    assert_eq!(err.actual, settlement_b);
    assert!(err.to_string().starts_with("VaultSettlementMismatch"));

    // Sender never touched: the funder's real SPL token balance is exactly what it started as — nothing
    // in this flow ever built or sent a Fund/transfer instruction (the check above returns before either
    // exists), and this reads the REAL on-chain-shaped account back to prove it, not just an assertion
    // that no code path was taken.
    let funder_acc_after = get_account(&mut ctx, funder_token_account)
        .await
        .expect("funder token account must still exist");
    let balance_after = zk_bridge::token::read_token_amount(&funder_acc_after.data);
    assert_eq!(
        balance_after, funder_starting_balance,
        "the funder's balance must be untouched — no transfer was ever attempted"
    );
}
