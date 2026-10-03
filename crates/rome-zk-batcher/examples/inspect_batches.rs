//! Reads and decodes one or more batch accounts on devnet — a small operational helper (evidence for a
//! write-up, or checking what state a batch id is in before reusing it) so nobody has to hand-decode
//! account bytes. Not part of the shipped binary.
//!
//! Usage: `cargo run -p rome-zk-batcher --example inspect_batches -- <inbox_program_id> <settlement_program_id> <chain_id> <batch_id>...`

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_program::pubkey::Pubkey;
use std::str::FromStr;

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    assert!(
        argv.len() >= 4,
        "usage: inspect_batches <inbox_program_id> <settlement_program_id> <chain_id> <batch_id>..."
    );
    let program_id = Pubkey::from_str(&argv[0]).expect("bad inbox_program_id");
    let settlement_program_id = Pubkey::from_str(&argv[1]).expect("bad settlement_program_id");
    let chain_id: u64 = argv[2].parse().expect("bad chain_id");
    let rpc = RpcClient::new("https://api.devnet.solana.com".to_string());

    let mut total_lamports = 0u64;
    for batch_str in &argv[3..] {
        let batch: u64 = batch_str.parse().expect("bad batch_id");
        let (pda, _) =
            zk_inbox_client::batch_pda(&program_id, &settlement_program_id, chain_id, batch);
        match rpc.get_account(&pda).await {
            Err(e) => println!("batch {batch} ({pda}): no account ({e})"),
            Ok(acc) => match zk_inbox_client::decode_batch_account(&acc.data) {
                Err(e) => println!(
                    "batch {batch} ({pda}): {} lamports, decode error: {e}",
                    acc.lamports
                ),
                Ok(d) => {
                    total_lamports += acc.lamports;
                    println!(
                        "batch {batch} ({pda}): {} lamports, leaves_present={}/{}, finalized={}",
                        acc.lamports, d.leaves_present, d.expected_count, d.finalized
                    );
                }
            },
        }
    }
    println!(
        "total batch-account lamports across the ids above: {total_lamports} ({} SOL)",
        total_lamports as f64 / 1_000_000_000.0
    );
}
