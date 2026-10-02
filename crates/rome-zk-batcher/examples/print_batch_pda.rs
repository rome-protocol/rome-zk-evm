//! Prints the `["batch", chain_id, batch]` PDA for one or more batch ids — a small operational helper
//! (e.g. for an evidence table, or checking what a `--start-batch` value would land on) so
//! nobody has to hand-derive PDAs. Not part of the shipped binary.
//!
//! Usage: `cargo run -p rome-zk-batcher --example print_batch_pda -- <inbox_program_id> <chain_id> <batch_id>...`
//! or, for a chunk PDA: `... -- --chunk <inbox_program_id> <chain_id> <batch_id> <idx>`

use solana_program::pubkey::Pubkey;
use std::str::FromStr;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(|s| s.as_str()) == Some("--chunk") {
        let program_id = Pubkey::from_str(&argv[1]).expect("bad inbox_program_id");
        let chain_id: u64 = argv[2].parse().expect("bad chain_id");
        let batch: u64 = argv[3].parse().expect("bad batch_id");
        let idx: u32 = argv[4].parse().expect("bad idx");
        let (pda, _bump) = zk_inbox_client::chunk_pda(&program_id, chain_id, batch, idx);
        println!("chunk (batch {batch}, idx {idx}): {pda}");
        return;
    }
    assert!(
        argv.len() >= 3,
        "usage: print_batch_pda <inbox_program_id> <chain_id> <batch_id>..."
    );
    let program_id = Pubkey::from_str(&argv[0]).expect("bad inbox_program_id");
    let chain_id: u64 = argv[1].parse().expect("bad chain_id");
    for batch_str in &argv[2..] {
        let batch: u64 = batch_str.parse().expect("bad batch_id");
        let (pda, _bump) = zk_inbox_client::batch_pda(&program_id, chain_id, batch);
        println!("batch {batch}: {pda}");
    }
}
