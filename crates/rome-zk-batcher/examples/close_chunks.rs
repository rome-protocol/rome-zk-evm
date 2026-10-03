//! Closes every existing chunk PDA (idx `0..scan_range`) under one or more abandoned batch ids, reclaiming their rent
//! to the chunk authority — the reclaim path `AbandonBatch` itself does not cover (chunk rent is not permanently
//! stranded: `programs/zk-inbox/src/lib.rs`'s `Close` handler lets the chunk's own authority close it unconditionally
//! once the covering batch account is gone — no root-finality wait — proved by
//! `chunk_close_succeeds_for_an_abandoned_batch_via_chunk_authority_alone`). Not part of the shipped binary; a
//! cleanup tool for exactly the batch ids this crate's own devnet measurement runs left behind.
//!
//! Usage: `cargo run -p rome-zk-batcher --example close_chunks -- <keypair_path> <inbox_program_id> <settlement_program_id> <chain_id> <batch_id>... [--scan-range N]`
//!
//! `--scan-range N` (default 1000, matching the operator's deploy config's earlier `max_frames_per_batch`)
//! bounds how many chunk indices (`0..N`) are probed per batch id — a batch opened before the
//! `MAX_OPENABLE_LEAVES` (312) cap may have up to that many.

use rome_zk_batcher::sender::{RpcSender, SendTuning, Sender, MAX_MULTIPLE_ACCOUNTS};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_keypair::read_keypair_file;
use solana_program::pubkey::Pubkey;
use std::str::FromStr;
use std::time::Duration;

const DEFAULT_SCAN_RANGE: u32 = 1000;

#[tokio::main]
async fn main() {
    let mut argv: Vec<String> = std::env::args().skip(1).collect();

    let mut scan_range = DEFAULT_SCAN_RANGE;
    if let Some(pos) = argv.iter().position(|a| a == "--scan-range") {
        let value = argv
            .get(pos + 1)
            .expect("--scan-range needs a value")
            .parse()
            .expect("bad --scan-range value");
        argv.drain(pos..=pos + 1);
        scan_range = value;
    }

    assert!(
        argv.len() >= 5,
        "usage: close_chunks <keypair_path> <inbox_program_id> <settlement_program_id> <chain_id> \
         <batch_id>... [--scan-range N]"
    );
    let payer = read_keypair_file(&argv[0]).expect("read keypair");
    let program_id = Pubkey::from_str(&argv[1]).expect("bad inbox_program_id");
    let settlement_program_id = Pubkey::from_str(&argv[2]).expect("bad settlement_program_id");
    let chain_id: u64 = argv[3].parse().expect("bad chain_id");

    let rpc_url = "https://api.devnet.solana.com".to_string();
    let rpc = RpcClient::new(rpc_url.clone());
    let sender = RpcSender::new(rpc_url, payer);
    let authority = sender.pubkey();
    let tuning = SendTuning {
        compute_unit_limit: 200_000,
        loaded_accounts_data_size_limit:
            rome_zk_batcher::config::default_loaded_accounts_data_size_limit(),
        priority_fee_micro_lamports: 1_000,
        max_priority_fee_micro_lamports: 200_000,
        confirm_timeout: Duration::from_secs(45),
        ..Default::default()
    };

    let mut total_reclaimed_lamports: u64 = 0;
    let mut total_closed: usize = 0;

    for batch_str in &argv[4..] {
        let batch: u64 = batch_str.parse().expect("bad batch_id");
        let pdas: Vec<Pubkey> = (0..scan_range)
            .map(|idx| {
                zk_inbox_client::chunk_pda(
                    &program_id,
                    &settlement_program_id,
                    chain_id,
                    batch,
                    idx,
                )
                .0
            })
            .collect();

        let mut existing: Vec<(u32, Pubkey, u64)> = Vec::new();
        for (chunk_start, pdas_chunk) in pdas.chunks(MAX_MULTIPLE_ACCOUNTS).enumerate() {
            let accounts = rpc
                .get_multiple_accounts(pdas_chunk)
                .await
                .expect("get_multiple_accounts");
            for (i, account) in accounts.into_iter().enumerate() {
                if let Some(account) = account {
                    let idx = (chunk_start * MAX_MULTIPLE_ACCOUNTS + i) as u32;
                    existing.push((idx, pdas_chunk[i], account.lamports));
                }
            }
        }

        if existing.is_empty() {
            println!("batch {batch}: no chunk PDAs found in 0..{scan_range}");
            continue;
        }
        println!(
            "batch {batch}: {} chunk PDA(s) found, closing...",
            existing.len()
        );

        for (idx, pda, lamports) in existing {
            let close_ix = zk_inbox_client::close_chunk_ix(
                &program_id,
                &authority,
                &settlement_program_id,
                chain_id,
                batch,
                idx,
            );
            match sender
                .send_and_confirm(std::slice::from_ref(&close_ix), tuning)
                .await
            {
                Ok(sig) => {
                    println!(
                        "  chunk idx {idx} ({pda}): closed, {lamports} lamports reclaimed, sig={sig}"
                    );
                    total_reclaimed_lamports += lamports;
                    total_closed += 1;
                }
                Err(e) => println!("  chunk idx {idx} ({pda}): close failed: {e}"),
            }
        }
    }

    println!(
        "=== total: {total_closed} chunk(s) closed, {total_reclaimed_lamports} lamports ({} SOL) reclaimed ===",
        total_reclaimed_lamports as f64 / 1_000_000_000.0
    );
}
