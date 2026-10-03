//! Sends `AbandonBatch` for one or more batch ids, reclaiming each batch account's own rent to the payer.
//! This does **not** reclaim the individual chunk PDAs opened under those batches — a separate rent pool,
//! and a separate reclaim path: once a batch is abandoned, its chunk PDAs become closable *unconditionally*
//! by their own authority (no root-finality wait — see `programs/zk-inbox/src/lib.rs`'s `Close` handler
//! and `chunk_close_succeeds_for_an_abandoned_batch_via_chunk_authority_alone`). Use the `close_chunks`
//! example for that. Not part of the shipped binary; a cleanup tool for exactly the situation this crate's
//! own devnet measurement runs left behind.
//!
//! Usage: `cargo run -p rome-zk-batcher --example abandon_batches -- <keypair_path> <inbox_program_id> <settlement_program_id> <chain_id> <batch_id>...`

use rome_zk_batcher::sender::{RpcSender, SendTuning, Sender};
use solana_keypair::read_keypair_file;
use solana_program::pubkey::Pubkey;
use std::str::FromStr;
use std::time::Duration;

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    assert!(
        argv.len() >= 5,
        "usage: abandon_batches <keypair_path> <inbox_program_id> <settlement_program_id> <chain_id> <batch_id>..."
    );
    let payer = read_keypair_file(&argv[0]).expect("read keypair");
    let program_id = Pubkey::from_str(&argv[1]).expect("bad inbox_program_id");
    let settlement_program_id = Pubkey::from_str(&argv[2]).expect("bad settlement_program_id");
    let chain_id: u64 = argv[3].parse().expect("bad chain_id");
    let sender = RpcSender::new("https://api.devnet.solana.com".to_string(), payer);
    let tuning = SendTuning {
        compute_unit_limit: 200_000,
        loaded_accounts_data_size_limit:
            rome_zk_batcher::config::default_loaded_accounts_data_size_limit(),
        priority_fee_micro_lamports: 1_000,
        max_priority_fee_micro_lamports: 200_000,
        confirm_timeout: Duration::from_secs(45),
        ..Default::default()
    };
    for batch_str in &argv[4..] {
        let batch: u64 = batch_str.parse().expect("bad batch_id");
        let ix = zk_inbox_client::abandon_batch_ix(
            &program_id,
            &sender.pubkey(),
            &settlement_program_id,
            chain_id,
            batch,
        );
        match sender
            .send_and_confirm(std::slice::from_ref(&ix), tuning)
            .await
        {
            Ok(sig) => println!("batch {batch}: abandoned, sig={sig}"),
            Err(e) => println!("batch {batch}: abandon failed: {e}"),
        }
    }
}
