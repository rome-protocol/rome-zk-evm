//! End-to-end: start the real `rome-zk-sequencer` binary on ephemeral ports, send 2,000 raw signed txs
//! from 50 senders over real sockets via `eth_sendRawTransaction`, subscribe to the preconfirmation feed,
//! and measure the ack latency p50/p99 (the gate: p99 < 100 ms on one host, mock executor).

use alloy::signers::local::PrivateKeySigner;
use jsonrpsee::core::client::{ClientT, SubscriptionClientT};
use jsonrpsee::http_client::HttpClientBuilder;
use jsonrpsee::ws_client::WsClientBuilder;
use rome_zk_sequencer::testutil::signed_raw_tx;
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::tempdir;

mod common;

const CHAIN_ID: u64 = 424_242;
const SENDERS: usize = 50;
const TXS_PER_SENDER: usize = 40; // 50 * 40 = 2,000
const TOTAL_TXS: usize = SENDERS * TXS_PER_SENDER;

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Fetch `/metrics` as lines, for diagnostics only (not asserted on — a scrape failure here should never
/// fail the actual latency gate above it).
async fn scrape_metrics(port: u16) -> Vec<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Ok(mut stream) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await else {
        return vec![];
    };
    if stream
        .write_all(b"GET /metrics HTTP/1.1\r\n\r\n")
        .await
        .is_err()
    {
        return vec![];
    }
    let mut body = String::new();
    let _ = stream.read_to_string(&mut body).await;
    body.lines().map(str::to_string).collect()
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

// Performance-sensitive: a debug build's unoptimized secp256k1 sign/recover and RLP/keccak paths are
// slow enough (measured: real, non-zero seal lateness against the 50ms deadline, unlike the release
// build's sum-zero lateness) to blow the 100ms budget on their own, independent of the sequencer's
// design. `cargo test --workspace` (the default CI gate, debug profile) must stay green, so this test
// is `#[ignore]`d there and run explicitly in release: `cargo test -p rome-zk-sequencer --test e2e
// --release -- --ignored`.
#[ignore = "performance-sensitive; run with --release, see comment above"]
#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
async fn e2e_2000_txs_from_50_senders_preconf_p99_under_100ms() {
    let dir = tempdir().unwrap();
    let log_dir = dir.path().join("log");
    let key_path = dir.path().join("sequencer.key");
    std::fs::write(
        &key_path,
        hex::encode(PrivateKeySigner::random().to_bytes()),
    )
    .unwrap();

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
            "#,
            log_dir.display(),
            key_path.display(),
        ),
    )
    .unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rome-zk-sequencer"));
    cmd.arg("--config").arg(&config_path);
    let (child, ports) = common::spawn_reporting_ports(&mut cmd);
    let (rpc_port, metrics_port) = (ports.rpc, ports.metrics);
    let _guard = ChildGuard(child);

    wait_for_port(rpc_port, Duration::from_secs(10)).await;

    // Subscribe to the preconfirmation feed too — proves it stays alive and streaming under the same
    // load the latency measurement below generates.
    let ws = WsClientBuilder::default()
        .build(format!("ws://127.0.0.1:{rpc_port}"))
        .await
        .unwrap();
    let mut feed: jsonrpsee::core::client::Subscription<serde_json::Value> = ws
        .subscribe(
            "rome_subscribe",
            jsonrpsee::rpc_params![vec!["preconfirmations"]],
            "rome_unsubscribe",
        )
        .await
        .unwrap();
    let feed_task = tokio::spawn(async move {
        loop {
            match feed.next().await {
                Some(Ok(item)) if !item["tx_hashes"].as_array().unwrap().is_empty() => break,
                Some(Ok(_)) => continue,
                _ => break,
            }
        }
    });

    // Warm up the process (JIT/allocator/page-cache/TCP stack) with a few throwaway txs before the timed
    // measurement — otherwise the first several sub-blocks after a cold start disproportionately eat
    // into the tail latency stats for reasons unrelated to steady-state sequencer behavior.
    {
        let warmup_signer = PrivateKeySigner::random();
        let warmup_client = HttpClientBuilder::default()
            .build(format!("http://127.0.0.1:{rpc_port}"))
            .unwrap();
        for nonce in 0..10u64 {
            let raw = signed_raw_tx(&warmup_signer, CHAIN_ID, nonce);
            let raw_hex = format!("0x{}", hex::encode(&raw));
            let _: String = warmup_client
                .request("eth_sendRawTransaction", jsonrpsee::rpc_params![raw_hex])
                .await
                .unwrap();
        }
    }

    let senders: Vec<PrivateKeySigner> = (0..SENDERS).map(|_| PrivateKeySigner::random()).collect();

    // Build each sender's client and establish its TCP connection (a cheap `eth_chainId` round trip)
    // before the timed measurement starts. Otherwise the first request from each of the 50 senders all
    // trigger a fresh TCP handshake in the same ~10ms window once the loop below starts — a connection-
    // establishment burst that is a test-harness cold-start cost, not sequencer ack latency.
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

    // Flood `rome_getPreconfirmation` concurrently with the send load
    // below. This used to be served through the actor's own bounded command channel — the same one
    // `eth_sendRawTransaction` uses — so a poller competed with every submission for one of its 1,024
    // slots; it's now a direct read off shared state, with no such dependency. Spawned here (before the
    // timed send loop starts) so both loads genuinely overlap; each call targets an arbitrary,
    // never-included hash — the point is concurrent load on the lookup path, not a hit rate.
    //
    // Over WebSocket, not HTTP/1.1: a jsonrpsee `WsClient` multiplexes many concurrent in-flight
    // requests over one connection (matched by JSON-RPC id), so a handful of shared clients can carry
    // genuinely concurrent load without each request queueing behind the last on a saturated
    // short-lived HTTP connection — an HTTP-per-call flood at this concurrency measures client-side
    // connection contention, not the sequencer's lookup path.
    const PRECONF_LOOKUP_FLOOD: usize = 10_000;
    const PRECONF_LOOKUP_CLIENTS: usize = 8;
    let mut lookup_clients = Vec::with_capacity(PRECONF_LOOKUP_CLIENTS);
    for _ in 0..PRECONF_LOOKUP_CLIENTS {
        lookup_clients.push(Arc::new(
            WsClientBuilder::default()
                .build(format!("ws://127.0.0.1:{rpc_port}"))
                .await
                .unwrap(),
        ));
    }
    let mut lookup_tasks = Vec::with_capacity(PRECONF_LOOKUP_FLOOD);
    for i in 0..PRECONF_LOOKUP_FLOOD {
        let client = lookup_clients[i % PRECONF_LOOKUP_CLIENTS].clone();
        lookup_tasks.push(tokio::spawn(async move {
            let hash_hex = format!("0x{i:064x}");
            let started = Instant::now();
            let result: Result<serde_json::Value, _> = client
                .request("rome_getPreconfirmation", jsonrpsee::rpc_params![hash_hex])
                .await;
            (result.is_ok(), started.elapsed())
        }));
    }

    let start = Instant::now();
    let mut tasks = Vec::with_capacity(SENDERS);
    // Each sender submits its own txs strictly in nonce order, one at a time (as a real wallet would).
    // A small random jitter before each send decorrelates the 50 senders from each other and from the
    // sealer's 50 ms cadence — without it every sender's next request tends to land in the same instant
    // (right after the previous tick released every ack at once), which inflates tail latency as a
    // measurement artifact of synchronized load generation, not a sequencer property (measured: p50
    // pinned at ~50ms — one full period — with every sender phase-locked to the tick).
    for (sender_idx, (signer, http)) in senders.into_iter().zip(clients).enumerate() {
        tasks.push(tokio::spawn(async move {
            let mut rng_state: u64 =
                0x9E3779B97F4A7C15 ^ (sender_idx as u64).wrapping_mul(2_654_435_761);
            let mut latencies = Vec::with_capacity(TXS_PER_SENDER);
            for nonce in 0..TXS_PER_SENDER as u64 {
                // xorshift64 — good enough for jitter, no external RNG crate needed on this hot path.
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
        "e2e: {TOTAL_TXS} txs from {SENDERS} senders in {:?} wall clock ({:.0} tx/s) — p50 {:?}, p99 {:?}",
        wall_clock,
        TOTAL_TXS as f64 / wall_clock.as_secs_f64(),
        p50,
        p99
    );
    eprintln!("--- seal_lateness_seconds / queue depth (diagnostic) ---");
    for line in scrape_metrics(metrics_port).await {
        if line.contains("seal_lateness") || line.contains("queue_depth") {
            eprintln!("{line}");
        }
    }

    assert!(
        p99 < Duration::from_millis(100),
        "p99 {p99:?} must be under the 100ms design budget"
    );

    // The 10,000-call `rome_getPreconfirmation` flood ran concurrently
    // with the send load timed above — collect its own latencies and confirm every call succeeded (no
    // RPC error, no hang) and stayed fast, proving the lookup path never contended with
    // `eth_sendRawTransaction` for the actor's command channel.
    let mut lookup_latencies = Vec::with_capacity(PRECONF_LOOKUP_FLOOD);
    let mut lookup_failures = 0u32;
    for t in lookup_tasks {
        let (ok, elapsed) = t.await.unwrap();
        if !ok {
            lookup_failures += 1;
        }
        lookup_latencies.push(elapsed);
    }
    assert_eq!(
        lookup_failures, 0,
        "every rome_getPreconfirmation call in the flood must succeed"
    );
    lookup_latencies.sort();
    let lookup_p50 = lookup_latencies[lookup_latencies.len() / 2];
    let lookup_p99 = lookup_latencies[(lookup_latencies.len() as f64 * 0.99) as usize];
    eprintln!(
        "e2e: {PRECONF_LOOKUP_FLOOD} concurrent rome_getPreconfirmation calls (during the same send \
         load) — p50 {lookup_p50:?}, p99 {lookup_p99:?}"
    );
    assert!(
        lookup_p99 < Duration::from_millis(100),
        "rome_getPreconfirmation p99 {lookup_p99:?} must stay under the 100ms design budget even \
         under concurrent send load"
    );

    // Give the feed task a moment to observe at least one non-empty sub-block, then stop waiting on it.
    let _ = tokio::time::timeout(Duration::from_secs(2), feed_task).await;
}

/// The same real-binary, real-socket e2e harness as the test above,
/// but with a chain profile declaring 25 ms sub-blocks x 40 per block (still a 1 s block time) instead
/// of the design default 50 ms x 20 — proving the cadence is genuinely read from `[profile]` end to
/// end, not just accepted by `Profile::validate`. Before `[profile]` existed, this
/// config could not even be expressed; an earlier binary would have rejected `sub_block_ms`/
/// `sub_blocks_per_block` as unknown fields. Performance-sensitive like its sibling above — run with
/// `--release --ignored`.
#[ignore = "performance-sensitive; run with --release, see comment above"]
#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
async fn e2e_25ms_sub_blocks_40_per_block_preconf_p99_under_100ms() {
    const PROFILE_SENDERS: usize = 50;
    const PROFILE_TXS_PER_SENDER: usize = 20; // 50 * 20 = 1,000
    const PROFILE_TOTAL_TXS: usize = PROFILE_SENDERS * PROFILE_TXS_PER_SENDER;

    let dir = tempdir().unwrap();
    let log_dir = dir.path().join("log");
    let key_path = dir.path().join("sequencer.key");
    std::fs::write(
        &key_path,
        hex::encode(PrivateKeySigner::random().to_bytes()),
    )
    .unwrap();

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

            [profile]
            sub_block_ms = 25
            sub_blocks_per_block = 40
            prover_gas_per_sec = 200000000
            da_bytes_per_sec = 704761

            [admission]
            queue_capacity = 4096
            queue_timeout_secs = 12
            park_expiry_secs = 30
            max_tx_size = 131072
            "#,
            log_dir.display(),
            key_path.display(),
        ),
    )
    .unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rome-zk-sequencer"));
    cmd.arg("--config").arg(&config_path);
    let (child, ports) = common::spawn_reporting_ports(&mut cmd);
    let (rpc_port, metrics_port) = (ports.rpc, ports.metrics);
    let _guard = ChildGuard(child);

    wait_for_port(rpc_port, Duration::from_secs(10)).await;

    {
        let warmup_signer = PrivateKeySigner::random();
        let warmup_client = HttpClientBuilder::default()
            .build(format!("http://127.0.0.1:{rpc_port}"))
            .unwrap();
        for nonce in 0..10u64 {
            let raw = signed_raw_tx(&warmup_signer, CHAIN_ID, nonce);
            let raw_hex = format!("0x{}", hex::encode(&raw));
            let _: String = warmup_client
                .request("eth_sendRawTransaction", jsonrpsee::rpc_params![raw_hex])
                .await
                .unwrap();
        }
    }

    let senders: Vec<PrivateKeySigner> = (0..PROFILE_SENDERS)
        .map(|_| PrivateKeySigner::random())
        .collect();
    let mut clients = Vec::with_capacity(PROFILE_SENDERS);
    for _ in 0..PROFILE_SENDERS {
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
    let mut tasks = Vec::with_capacity(PROFILE_SENDERS);
    for (sender_idx, (signer, http)) in senders.into_iter().zip(clients).enumerate() {
        tasks.push(tokio::spawn(async move {
            let mut rng_state: u64 =
                0x9E3779B97F4A7C15 ^ (sender_idx as u64).wrapping_mul(2_654_435_761);
            let mut latencies = Vec::with_capacity(PROFILE_TXS_PER_SENDER);
            for nonce in 0..PROFILE_TXS_PER_SENDER as u64 {
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

    let mut all_latencies = Vec::with_capacity(PROFILE_TOTAL_TXS);
    for t in tasks {
        all_latencies.extend(t.await.unwrap());
    }
    let wall_clock = start.elapsed();

    assert_eq!(all_latencies.len(), PROFILE_TOTAL_TXS);
    all_latencies.sort();
    let p50 = all_latencies[all_latencies.len() / 2];
    let p99 = all_latencies[(all_latencies.len() as f64 * 0.99) as usize];

    eprintln!(
        "e2e (25ms x 40/block profile): {PROFILE_TOTAL_TXS} txs from {PROFILE_SENDERS} senders in \
         {:?} wall clock ({:.0} tx/s) — p50 {:?}, p99 {:?}",
        wall_clock,
        PROFILE_TOTAL_TXS as f64 / wall_clock.as_secs_f64(),
        p50,
        p99
    );
    eprintln!("--- seal_lateness_seconds (diagnostic) ---");
    for line in scrape_metrics(metrics_port).await {
        if line.contains("seal_lateness") {
            eprintln!("{line}");
        }
    }

    assert!(
        p99 < Duration::from_millis(100),
        "p99 {p99:?} must be under the 100ms design budget at the 25ms x 40/block profile"
    );
}
