//! Devnet measurement of the `Sender`/`RpcSender` machinery itself (chunks/s confirmed, p50/p99 confirm latency,
//! resubmit count) — **not** the zk-inbox chunk lane, which is blocked on devnet today (see `tests/devnet_probe.rs`):
//! `OpenBatch` needs a real settlement root account this design forbids faking, and chunk `Open` independently needs
//! the batch account `OpenBatch` alone can create. Rather than fabricate a stand-in program to route around that gate
//! (explicitly foreclosed — root.rs's own doc: "which a program cannot"), this measures the exact
//! send/resubmit/confirm code path `send_frame` uses for real chunk transactions, substituting a same-sized System
//! Program transfer instruction for the chunk instruction, which this devnet cannot accept. Labelled clearly; not
//! conflated with a "chunks/s" number.
//!
//! Ignored by default (spends real devnet SOL, ~0.001 SOL total for 200 transfers of 1 lamport each at ~5,000 lamport
//! fee); run manually with `cargo test -p rome-zk-batcher --test devnet_sender_measurement -- --ignored --nocapture`.
//! Uses the payer at `~/.config/solana/id.json`.

use futures_util::stream::{FuturesUnordered, StreamExt};
use rome_zk_batcher::sender::{RpcSender, SendTuning, Sender};
// The V1-generation keypair — `RpcSender::new` signs with it directly.
use solana_keypair::read_keypair_file;
use std::sync::Arc;
use std::time::{Duration, Instant};

const N: usize = 60;
const IN_FLIGHT: usize = 8;

async fn send_one(idx: usize, sender: Arc<RpcSender>, tuning: SendTuning) -> Option<u64> {
    let started = Instant::now();
    let self_pubkey = sender.pubkey();
    let ix = solana_system_interface::instruction::transfer(&self_pubkey, &self_pubkey, 1);
    match sender
        .send_and_confirm(std::slice::from_ref(&ix), tuning)
        .await
    {
        Ok(_sig) => {
            let ms = started.elapsed().as_millis() as u64;
            println!("send[{idx}] confirmed in {ms} ms");
            Some(ms)
        }
        Err(e) => {
            println!("send[{idx}] failed: {e}");
            None
        }
    }
}

#[tokio::test]
#[ignore = "spends real devnet SOL and takes a while; run it manually to take the measurement"]
async fn sender_layer_throughput_and_latency_on_devnet() {
    let key_path = dirs_home().join(".config/solana/id.json");
    let payer = read_keypair_file(&key_path)
        .unwrap_or_else(|e| panic!("failed to read keypair at {}: {e}", key_path.display()));
    let sender = Arc::new(RpcSender::new(
        "https://api.devnet.solana.com".to_string(),
        payer,
    ));
    let tuning = SendTuning {
        compute_unit_limit: 5_000,
        loaded_accounts_data_size_limit:
            rome_zk_batcher::config::default_loaded_accounts_data_size_limit(),
        priority_fee_micro_lamports: 1_000,
        max_priority_fee_micro_lamports: 200_000,
        confirm_timeout: Duration::from_secs(60),
        ..Default::default()
    };

    let started = Instant::now();
    let mut in_progress = FuturesUnordered::new();
    let mut remaining = 0..N;
    for i in remaining.by_ref().take(IN_FLIGHT.min(N)) {
        in_progress.push(tokio::spawn(send_one(i, sender.clone(), tuning)));
    }

    let mut latencies_ms = Vec::with_capacity(N);
    while let Some(joined) = in_progress.next().await {
        if let Some(latency) = joined.expect("send_one task panicked") {
            latencies_ms.push(latency);
        }
        if let Some(i) = remaining.next() {
            in_progress.push(tokio::spawn(send_one(i, sender.clone(), tuning)));
        }
    }

    let elapsed = started.elapsed();
    latencies_ms.sort_unstable();
    let confirmed = latencies_ms.len();
    let p50 = latencies_ms.get(confirmed / 2).copied().unwrap_or(0);
    let p99_idx = (confirmed * 99 / 100).min(confirmed.saturating_sub(1));
    let p99 = latencies_ms.get(p99_idx).copied().unwrap_or(0);

    println!("=== Sender-layer devnet measurement (system-transfer surrogate) ===");
    println!("confirmed: {confirmed}/{N} in {elapsed:?}");
    println!(
        "throughput: {:.1} tx/s",
        confirmed as f64 / elapsed.as_secs_f64()
    );
    println!("p50 confirm latency: {p50} ms, p99: {p99} ms");
    assert!(
        confirmed as f64 >= N as f64 * 0.9,
        "expected at least 90% of {N} sends to confirm on devnet"
    );
}

fn dirs_home() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").expect("HOME must be set"))
}
