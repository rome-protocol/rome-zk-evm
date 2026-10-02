//! `release-exit`: reads a PROVED settlement `exit_record`, derives the recipient's ATA for the vault's
//! mint, and sends `ReleaseExit` prefixed by an idempotent ATA-create, so `ReleaseExit` does not revert
//! when `record.sol_recipient` has no ATA yet (handled at the CLIENT).
//! `programs/zk-bridge/src/release.rs:133-137` only DERIVES and CHECKS the recipient ATA (refusing a
//! wrong one); it never creates one, so provisioning it is this tool's job.
//!
//! `--dry-run` is the DEFAULT: both instructions are built and signed against an all-zero placeholder
//! blockhash — never fetched, never sent to any cluster — and every account is printed, the same
//! sign-for-inspection-only shape `zk-settlement-client/examples/governance.rs`'s own `--dry-run` uses.
//! Only `--confirm` sends for real. The exit-prover key never appears here — this is a separate, operator-
//! run tool, not part of the follower's own loop.
//!
//! Refuses by name (never a bare `unwrap` panic message) if `exit_record` does not exist yet, or exists
//! but is not `PROVED` (already released, or never proved at all).
//!
//! Usage:
//! ```text
//! cargo run -p zk-bridge-client --example release-exit --features devnet-driver -- \
//!   --settlement <settlement program pubkey> --bridge <zk-bridge program pubkey> \
//!   --chain-id <u64> --message-hash 0x<32 bytes> --payer-keypair /path/to/payer.json \
//!   [--rpc-url URL] [--confirm]
//! ```

use base64::Engine as _;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::{
    hash::Hash,
    pubkey::Pubkey,
    signature::{read_keypair_file, Signer},
    transaction::Transaction,
};
use std::str::FromStr;

fn arg(name: &str) -> Option<String> {
    let mut it = std::env::args();
    while let Some(a) = it.next() {
        if a == name {
            return it.next();
        }
    }
    None
}
fn required_arg(name: &str) -> String {
    arg(name).unwrap_or_else(|| panic!("{name} is required"))
}
fn pubkey_arg(name: &str) -> Pubkey {
    Pubkey::from_str(&required_arg(name)).unwrap_or_else(|_| panic!("{name}: bad pubkey"))
}
fn u64_arg(name: &str) -> u64 {
    required_arg(name)
        .parse()
        .unwrap_or_else(|_| panic!("{name}: bad u64"))
}
fn hex32_arg(name: &str) -> [u8; 32] {
    let s = required_arg(name);
    let bytes =
        hex::decode(s.trim_start_matches("0x")).unwrap_or_else(|_| panic!("{name}: not valid hex"));
    bytes
        .try_into()
        .unwrap_or_else(|_| panic!("{name}: expected 32 bytes"))
}
/// Bare boolean flag (no value) — mirrors `governance.rs`'s own `dry_run()`, inverted: here `--confirm`
/// is the opt-in flag and dry-run is the default.
fn confirm() -> bool {
    std::env::args().any(|a| a == "--confirm")
}
fn rpc_url() -> String {
    arg("--rpc-url").unwrap_or_else(|| "https://api.devnet.solana.com".to_string())
}

fn print_account(pubkey: &Pubkey, writable: bool, signer: bool) {
    println!(
        "      {pubkey}{}{}",
        if writable { "  (writable)" } else { "" },
        if signer { "  (signer)" } else { "" },
    );
}

#[tokio::main]
async fn main() {
    let settlement = pubkey_arg("--settlement");
    let bridge = pubkey_arg("--bridge");
    let chain_id = u64_arg("--chain-id");
    let message_hash = hex32_arg("--message-hash");
    let payer_keypair_path = required_arg("--payer-keypair");
    let rpc = RpcClient::new_with_commitment(rpc_url(), CommitmentConfig::confirmed());

    // Chain reads happen BEFORE the payer keypair is ever touched (and before EITHER `--dry-run` or
    // `--confirm` branches): an operator finding out an exit is not actually PROVED yet, or that the RPC
    // is unreachable, should never need their key material unlocked first — and it means an unreachable
    // RPC fails loud, by name, in BOTH modes (`release_exit_example_refuses_without_confirm`).
    let (exit_record_pda, _) =
        zk_settlement_client::exit_record_pda(&settlement, chain_id, message_hash);
    let record_account = rpc.get_account(&exit_record_pda).await.unwrap_or_else(|e| {
        panic!("exit_record {exit_record_pda} not found (never proved, or already released and closed): {e}")
    });
    let record = zk_settlement_client::decode_exit_record_account(&record_account.data)
        .expect("decode exit_record");
    if record.status != rome_zk_layouts::exit::exit_record::STATUS_PROVED {
        panic!(
            "exit_record {exit_record_pda} is not PROVED (status={}) — ReleaseExit only accepts a \
             freshly proved record",
            record.status
        );
    }

    let (vault_config_pda, _) = zk_bridge_client::vault_config_pda(&bridge, &settlement, chain_id);
    let vault_config_account = rpc
        .get_account(&vault_config_pda)
        .await
        .unwrap_or_else(|e| panic!("vault_config {vault_config_pda} not found: {e}"));
    let vault_config = zk_bridge_client::decode_vault_config_account(&vault_config_account.data)
        .expect("decode vault_config");

    let recipient = Pubkey::new_from_array(record.sol_recipient);
    let payer_refund = record.payer;
    let (exit_config_pda, _) = zk_settlement_client::exit_config_pda(&settlement, chain_id);

    let payer = read_keypair_file(&payer_keypair_path)
        .unwrap_or_else(|e| panic!("read payer keypair {payer_keypair_path}: {e}"));

    let ata_ix = zk_bridge_client::create_recipient_ata_idempotent_ix(
        &payer.pubkey(),
        &recipient,
        &vault_config.mint,
    );
    let release_ix = zk_bridge_client::release_exit_ix(
        &bridge,
        &settlement,
        chain_id,
        message_hash,
        &vault_config.mint,
        &exit_config_pda,
        &exit_record_pda,
        &payer_refund,
        &recipient,
    );

    println!(
        "-- release-exit: chain {chain_id}, message_hash 0x{}",
        hex::encode(message_hash)
    );
    println!("  recipient      {recipient}");
    println!(
        "  recipient_ata  {}",
        zk_bridge_client::recipient_ata(&recipient, &vault_config.mint)
    );
    println!("  amount (wei)   {}", record.amount);
    println!("  mint           {}", vault_config.mint);
    println!("  instruction 1: CreateIdempotent (Associated Token Program)");
    for m in &ata_ix.accounts {
        print_account(&m.pubkey, m.is_writable, m.is_signer);
    }
    println!("  instruction 2: ReleaseExit (zk-bridge)");
    for m in &release_ix.accounts {
        print_account(&m.pubkey, m.is_writable, m.is_signer);
    }

    let ixs = [ata_ix, release_ix];

    if !confirm() {
        // Sign for byte-level inspection only — an all-zero placeholder blockhash, never fetched from
        // any RPC, so `--dry-run` needs no cluster access beyond the two reads above.
        let tx = Transaction::new_signed_with_payer(
            &ixs,
            Some(&payer.pubkey()),
            &[&payer],
            Hash::default(),
        );
        let bytes = bincode::serialize(&tx).expect("serialize tx");
        println!("-- dry run: built and signed, nothing sent to any cluster --");
        println!(
            "  tx (base64)  {}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
        return;
    }

    let bh = rpc
        .get_latest_blockhash()
        .await
        .expect("get_latest_blockhash");
    let tx = Transaction::new_signed_with_payer(&ixs, Some(&payer.pubkey()), &[&payer], bh);
    let sig = rpc
        .send_and_confirm_transaction(&tx)
        .await
        .expect("send ReleaseExit");
    println!("-- sent: {sig}");
}
