//! The same `tests/e2e.rs` shape (2,000 raw signed txs from 50 senders,
//! preconf p99 < 100 ms), started with `--executor reth` instead of the default mock — proves the
//! real in-process reth executor meets the same budget under the same load, not just the
//! flat-gas mock. Only compiled when the `reth` cargo feature is on (default) — see this crate's
//! Cargo.toml.
#![cfg(feature = "reth")]

use alloy::signers::local::PrivateKeySigner;
use jsonrpsee::core::client::ClientT;
use jsonrpsee::http_client::HttpClientBuilder;
use rome_zk_sequencer::testutil::signed_raw_tx;
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::tempdir;

mod common;

const CHAIN_ID: u64 = 424_243;
const SENDERS: usize = 50;
const TXS_PER_SENDER: usize = 40; // 50 * 40 = 2,000
const TOTAL_TXS: usize = SENDERS * TXS_PER_SENDER;
const GENESIS_BALANCE_HEX: &str = "0x33b2e3c9fd0803ce8000000"; // matches genesis.json.template

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn wait_for_port(port: u16, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "sequencer did not open its RPC port in time"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// Same performance-sensitive rationale as `tests/e2e.rs`: debug-build secp256k1/RLP/keccak and, for
// this variant, real revm execution are all slow enough to blow the 100ms budget on their own.
// `cargo test --workspace` (debug) must stay green, so this is `#[ignore]`d there and run explicitly:
// `cargo test -p rome-zk-sequencer --test e2e_reth --release -- --ignored`.
#[ignore = "performance-sensitive; run with --release, see comment above"]
#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
async fn e2e_2000_txs_from_50_senders_with_reth_executor_preconf_p99_under_100ms() {
    let dir = tempdir().unwrap();
    let log_dir = dir.path().join("log");
    let key_path = dir.path().join("sequencer.key");
    std::fs::write(
        &key_path,
        hex::encode(PrivateKeySigner::random().to_bytes()),
    )
    .unwrap();

    // 50 senders, each funded in genesis so all 40 of its sequential-nonce transfers succeed.
    let senders: Vec<PrivateKeySigner> = (0..SENDERS).map(|_| PrivateKeySigner::random()).collect();
    let mut alloc = serde_json::Map::new();
    for s in &senders {
        alloc.insert(
            format!("{:#x}", s.address()),
            serde_json::json!({ "balance": GENESIS_BALANCE_HEX }),
        );
    }
    // Shaped exactly like the deploy genesis template (chain id substituted to this test's own, Prague
    // at 0).
    let genesis = serde_json::json!({
        "config": {
            "chainId": CHAIN_ID,
            "homesteadBlock": 0, "eip150Block": 0, "eip155Block": 0, "eip158Block": 0,
            "byzantiumBlock": 0, "constantinopleBlock": 0, "petersburgBlock": 0,
            "istanbulBlock": 0, "berlinBlock": 0, "londonBlock": 0,
            "terminalTotalDifficulty": 0, "terminalTotalDifficultyPassed": true,
            "shanghaiTime": 0, "cancunTime": 0, "pragueTime": 0
        },
        "nonce": "0x0", "timestamp": "0x0", "extraData": "0x",
        "gasLimit": "0x5f5e100", "difficulty": "0x0",
        "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "coinbase": "0x0000000000000000000000000000000000000000",
        "alloc": alloc,
        "number": "0x0", "gasUsed": "0x0",
        "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "baseFeePerGas": "0x3b9aca00"
    });
    let genesis_path = dir.path().join("genesis.json");
    std::fs::write(&genesis_path, genesis.to_string()).unwrap();

    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
            chain_id = {CHAIN_ID}
            rpc_addr = "127.0.0.1:0"
            metrics_addr = "127.0.0.1:0"
            log_dir = "{}"
            sequencer_key_path = "{}"

            [admission]
            queue_capacity = 4096
            queue_timeout_secs = 12
            park_expiry_secs = 30
            max_tx_size = 131072

            [reth]
            datadir = "{}"
            genesis_path = "{}"
            "#,
            log_dir.display(),
            key_path.display(),
            dir.path().join("reth-db").display(),
            genesis_path.display(),
        ),
    )
    .unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rome-zk-sequencer"));
    cmd.arg("--config").arg(&config_path);
    cmd.arg("--executor").arg("reth");
    let (child, ports) = common::spawn_reporting_ports(&mut cmd);
    let (rpc_port, metrics_port) = (ports.rpc, ports.metrics);
    let _guard = ChildGuard(child);

    wait_for_port(rpc_port, Duration::from_secs(20)).await;

    let mut clients = Vec::with_capacity(SENDERS);
    for _ in 0..SENDERS {
        let client = Arc::new(
            HttpClientBuilder::default()
                .build(format!("http://127.0.0.1:{rpc_port}"))
                .unwrap(),
        );
        let _: String = client
            .request("eth_chainId", jsonrpsee::rpc_params![])
            .await
            .unwrap();
        clients.push(client);
    }

    let start = Instant::now();
    let mut tasks = Vec::with_capacity(SENDERS);
    for (sender_idx, (signer, http)) in senders.into_iter().zip(clients).enumerate() {
        tasks.push(tokio::spawn(async move {
            let mut rng_state: u64 =
                0x9E3779B97F4A7C15 ^ (sender_idx as u64).wrapping_mul(2_654_435_761);
            let mut latencies = Vec::with_capacity(TXS_PER_SENDER);
            for nonce in 0..TXS_PER_SENDER as u64 {
                rng_state ^= rng_state << 13;
                rng_state ^= rng_state >> 7;
                rng_state ^= rng_state << 17;
                let jitter_ms = rng_state % 15;
                tokio::time::sleep(Duration::from_millis(jitter_ms)).await;

                let raw = signed_raw_tx(&signer, CHAIN_ID, nonce);
                let raw_hex = format!("0x{}", hex::encode(&raw));
                let sent_at = Instant::now();
                let result: Result<String, _> = http
                    .request("eth_sendRawTransaction", jsonrpsee::rpc_params![raw_hex])
                    .await;
                let latency = sent_at.elapsed();
                result.unwrap_or_else(|e| panic!("tx {nonce} for sender rejected: {e}"));
                latencies.push(latency);
            }
            latencies
        }));
    }

    let mut all_latencies = Vec::with_capacity(TOTAL_TXS);
    for t in tasks {
        all_latencies.extend(t.await.unwrap());
    }
    let wall_clock = start.elapsed();

    assert_eq!(all_latencies.len(), TOTAL_TXS);
    all_latencies.sort();
    let p50 = all_latencies[all_latencies.len() / 2];
    let p99 = all_latencies[(all_latencies.len() as f64 * 0.99) as usize];

    eprintln!(
        "e2e (reth executor): {TOTAL_TXS} txs from {SENDERS} senders in {:?} wall clock ({:.0} tx/s) \
         — p50 {:?}, p99 {:?}",
        wall_clock,
        TOTAL_TXS as f64 / wall_clock.as_secs_f64(),
        p50,
        p99
    );

    // Same correlation as `tests/e2e_reth_node.rs`'s own p99 test — report the
    // log-fsync-duration and ack-latency histograms from this SAME run, printed before the budget
    // assertion so the report survives even if it fails.
    let metrics_output = std::process::Command::new("curl")
        .args(["-s", &format!("http://127.0.0.1:{metrics_port}/metrics")])
        .output()
        .expect("run curl");
    let metrics_text = String::from_utf8_lossy(&metrics_output.stdout);
    eprintln!("--- log_fsync_duration_seconds / preconf_latency_seconds (this run) ---");
    for line in metrics_text.lines() {
        if line.contains("log_fsync_duration_seconds")
            || line.contains("preconf_latency_seconds")
            || line.contains("seal_lateness_seconds")
        {
            eprintln!("{line}");
        }
    }

    assert!(
        p99 < Duration::from_millis(100),
        "p99 {p99:?} must be under the 100ms design budget with the real reth executor"
    );
}
