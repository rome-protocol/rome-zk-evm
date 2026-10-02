//! `MigrateChainV2` (registration-and-revenue proposal; v2 — registry-authority-only): the one-time
//! bring-forward for a chain that predates the `chain_config` account — Tiber (chain 200101) — creating its
//! `chain_config` account against the already-live `root`/`registry` accounts, which stay untouched.
//! Requires `InitGlobalConfig` to have already run for this program deployment.
//!
//! Keys: both keypairs are read from FILE PATHS, never printed or committed.
//!
//! Usage:
//!   cargo run -p zk-settlement-client --features devnet-driver --example migrate_chain -- \
//!     --settlement <PROGRAM_ID> --chain-id 200101 \
//!     --registry-keypair /path/to/registry_authority.json --payer-keypair /path/to/payer.json \
//!     --max-drift-secs 60 [--rpc-url URL]
//!
//! `--max-drift-secs` (required, no default — an ops value, never a program constant): the chain's
//! timestamp drift bound. Idempotent against a chain already on `chain_config` v2 in
//! the sense that this prints and exits rather than sending a doomed transaction — see the v2 check
//! below.
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::{
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

#[tokio::main]
async fn main() {
    let settlement = Pubkey::from_str(&arg("--settlement").expect("--settlement")).unwrap();
    let chain_id: u64 = arg("--chain-id").expect("--chain-id").parse().unwrap();
    let registry_keypair_path = arg("--registry-keypair").expect("--registry-keypair");
    let payer_keypair_path = arg("--payer-keypair").expect("--payer-keypair");
    let max_drift_secs: u64 = arg("--max-drift-secs")
        .expect("--max-drift-secs")
        .parse()
        .expect("--max-drift-secs must be a u64");
    let sol_rpc = arg("--rpc-url").unwrap_or_else(|| "https://api.devnet.solana.com".to_string());

    let registry_authority =
        read_keypair_file(&registry_keypair_path).expect("read registry authority keypair");
    let payer = read_keypair_file(&payer_keypair_path).expect("read payer keypair");

    let rpc = RpcClient::new_with_commitment(sol_rpc, CommitmentConfig::confirmed());
    let (chain_config_pda, _) = zk_settlement_client::chain_config_pda(&settlement, chain_id);
    if let Ok(acc) = rpc.get_account(&chain_config_pda).await {
        let cfg = zk_settlement_client::decode_chain_config_account(&acc.data)
            .expect("decode chain_config");
        if cfg.max_drift_secs.is_some() {
            println!("chain {chain_id} already migrated to v2: chain_config {chain_config_pda}");
            return;
        }
        println!("chain {chain_id} has a v1 chain_config {chain_config_pda} — migrating to v2");
    }

    let ix = zk_settlement_client::migrate_chain_ix(
        &settlement,
        &registry_authority.pubkey(),
        &payer.pubkey(),
        chain_id,
        max_drift_secs,
    );
    let bh = rpc.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[&payer, &registry_authority],
        bh,
    );
    let sig = rpc
        .send_and_confirm_transaction(&tx)
        .await
        .expect("MigrateChainV2");
    println!("chain {chain_id} migrated: chain_config {chain_config_pda}, sig {sig}");
}
