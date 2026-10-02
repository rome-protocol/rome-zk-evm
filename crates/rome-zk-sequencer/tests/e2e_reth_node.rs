//! End-to-end tests against the real binary, `--executor reth` with `[reth].http_addr`/
//! `ws_addr` configured — i.e. the `node::serve` RPC surface (reth's own `eth_*`/`net`/`web3`/
//! `debug`/`trace`/`ots`, with `eth_sendRawTransaction`/`rome_*` overridden/added onto it), not the
//! older `rpc::serve` path `tests/e2e_reth.rs` still covers.
//!
//! Covers: (1) node starts, `eth_chainId`/`net_version`/`web3_clientVersion` answer; (2) a tx sent via
//! `eth_sendRawTransaction` is reflected by `eth_getTransactionReceipt`/`eth_getBlockByNumber`/
//! `eth_getBalance` once its block seals, and `ots_getApiLevel`/`ots_getBlockDetails` work; (3) e2e p99
//! < 100 ms with the node serving RPC.
#![cfg(feature = "reth")]

use alloy::primitives::U256;
use alloy::signers::local::PrivateKeySigner;
use jsonrpsee::core::client::ClientT;
use jsonrpsee::http_client::HttpClientBuilder;
use jsonrpsee::ws_client::WsClientBuilder;
use rome_zk_sequencer::testutil::signed_raw_tx;
use serde_json::Value;
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::tempdir;

const CHAIN_ID: u64 = 424_244;
const GENESIS_BALANCE_HEX: &str = "0x33b2e3c9fd0803ce8000000";

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

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
            "node did not open its RPC port in time"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Spawns `rome-zk-sequencer --executor reth` with a `[reth]` section carrying `http_addr`/
/// `ws_addr` — the new `node::serve` path — funded with `senders`, and returns the guard + the
/// bound http port.
fn spawn_node(
    dir: &std::path::Path,
    senders: &[PrivateKeySigner],
) -> (ChildGuard, u16, u16, std::path::PathBuf) {
    let log_dir = dir.join("log");
    let key_path = dir.join("sequencer.key");
    std::fs::write(
        &key_path,
        hex::encode(PrivateKeySigner::random().to_bytes()),
    )
    .unwrap();

    let mut alloc = serde_json::Map::new();
    for s in senders {
        alloc.insert(
            format!("{:#x}", s.address()),
            serde_json::json!({ "balance": GENESIS_BALANCE_HEX }),
        );
    }
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
    let genesis_path = dir.join("genesis.json");
    std::fs::write(&genesis_path, genesis.to_string()).unwrap();

    let rpc_port = free_port(); // unused by this path but Config still requires the field
    let metrics_port = free_port();
    let http_port = free_port();
    let ws_port = free_port();
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
            chain_id = {CHAIN_ID}
            rpc_addr = "127.0.0.1:{rpc_port}"
            metrics_addr = "127.0.0.1:{metrics_port}"
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
            http_addr = "127.0.0.1:{http_port}"
            ws_addr = "127.0.0.1:{ws_port}"
            "#,
            log_dir.display(),
            key_path.display(),
            dir.join("reth-db").display(),
            genesis_path.display(),
        ),
    )
    .unwrap();

    (
        spawn_from_config(&config_path),
        http_port,
        ws_port,
        config_path,
    )
}

/// Launches `rome-zk-sequencer --executor reth` against an already-written config file — factored
/// out of `spawn_node` so a test can restart the SAME binary over the SAME `[reth].datadir`/
/// `log_dir` (and the SAME bound ports, since those are fixed in the config file too) after killing
/// the first instance.
fn spawn_from_config(config_path: &std::path::Path) -> ChildGuard {
    let bin = env!("CARGO_BIN_EXE_rome-zk-sequencer");
    let child = Command::new(bin)
        .arg("--config")
        .arg(config_path)
        .arg("--executor")
        .arg("reth")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn rome-zk-sequencer binary");
    ChildGuard(child)
}

async fn rpc_call(client: &jsonrpsee::http_client::HttpClient, method: &str) -> Value {
    client
        .request(method, jsonrpsee::rpc_params![])
        .await
        .unwrap_or_else(|e| panic!("{method} failed: {e}"))
}

/// Test 1: the node starts on genesis with the RPC servers up; `eth_chainId`/`net_version`/
/// `web3_clientVersion` answer.
#[tokio::test(flavor = "multi_thread")]
async fn node_starts_and_answers_identity_methods() {
    let dir = tempdir().unwrap();
    let (_guard, http_port, _ws_port, _config_path) = spawn_node(dir.path(), &[]);
    wait_for_port(http_port, Duration::from_secs(20)).await;
    let client = HttpClientBuilder::default()
        .build(format!("http://127.0.0.1:{http_port}"))
        .unwrap();

    let chain_id = rpc_call(&client, "eth_chainId").await;
    assert_eq!(
        chain_id.as_str().unwrap(),
        format!("{CHAIN_ID:#x}"),
        "eth_chainId must report this chain's id"
    );
    let net_version = rpc_call(&client, "net_version").await;
    assert_eq!(net_version.as_str().unwrap(), CHAIN_ID.to_string());
    let client_version = rpc_call(&client, "web3_clientVersion").await;
    assert!(client_version.is_string(), "web3_clientVersion must answer");
}

/// `node::serve`'s server used `RpcServerConfig::http(Default::default())` /
/// `.with_ws(Default::default())` — each builds a `ServerConfigBuilder` at jsonrpsee's own hardcoded
/// default (`MAX_CONNECTIONS: u32 = 100`, `jsonrpsee-server-0.26.0`'s `src/server.rs`'s
/// `TowerServiceNoHttp::call`, whose connection permit is held for a WS connection's whole lifetime,
/// unlike a plain HTTP request's transient one), never consulting `Config::rpc`/`RpcConfig` at all —
/// the exact trap already fixed for this crate's OTHER server (`rpc.rs::serve`,
/// `max_connections: 10_000`). 200 concurrent, still-open WS connections must not stop a 201st WS
/// client from connecting and getting `eth_chainId` well inside the RPC latency budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn the_201st_ws_connection_is_not_capped_at_jsonrpsees_default_of_100() {
    const HELD_OPEN: usize = 200;

    let dir = tempdir().unwrap();
    let (_guard, _http_port, ws_port, _config_path) = spawn_node(dir.path(), &[]);
    wait_for_port(ws_port, Duration::from_secs(20)).await;

    // Open HELD_OPEN concurrent WS connections and hold them open in `_clients` for the rest of the
    // test (never dropped) — a WS connection's permit is held for its whole session, unlike a plain
    // HTTP request's (released the moment the response is sent), so this genuinely occupies
    // HELD_OPEN of jsonrpsee's connection slots.
    let mut _clients = Vec::with_capacity(HELD_OPEN);
    for _ in 0..HELD_OPEN {
        let client = WsClientBuilder::default()
            .build(format!("ws://127.0.0.1:{ws_port}"))
            .await
            .expect("open a WS connection");
        _clients.push(client);
    }

    // The property is the CAP, not the latency: a server capped at 100 never answers the 201st client
    // at all (its connection permit is never granted), so any bound distinguishes it — a tight one only
    // turns a loaded shared runner into a false red (it has happened twice). Latency stays observable in
    // the eprintln below; the RPC latency budget is the p99 test's job.
    let started = Instant::now();
    let outcome = tokio::time::timeout(Duration::from_secs(5), async {
        let client_201 = WsClientBuilder::default()
            .build(format!("ws://127.0.0.1:{ws_port}"))
            .await?;
        client_201
            .request::<Value, _>("eth_chainId", jsonrpsee::rpc_params![])
            .await
    })
    .await;
    let elapsed = started.elapsed();
    eprintln!("201st WS connection: eth_chainId resolved in {elapsed:?} ({outcome:?})");

    let response = outcome
        .unwrap_or_else(|_| {
            panic!(
                "201st WS client did not connect and answer within 5s ({HELD_OPEN} connections \
                 already open) — jsonrpsee's built-in max_connections default (100) is capping this \
                 server instead of this crate's own RpcConfig"
            )
        })
        .expect("201st WS client's eth_chainId call must succeed, not be rejected");
    assert_eq!(
        response.as_str().unwrap(),
        format!("{CHAIN_ID:#x}"),
        "the 201st connection must still get a real answer"
    );
}

/// Test 2: a tx sent via `eth_sendRawTransaction` is pre-confirmed and, after its block seals,
/// `eth_getTransactionReceipt`/`eth_getBlockByNumber`/`eth_getBalance` reflect it from reth's own
/// RPC (reading the node's real MDBX, not this crate's own bookkeeping); `ots_getApiLevel`/
/// `ots_getBlockDetails` answer.
#[tokio::test(flavor = "multi_thread")]
async fn sent_tx_is_reflected_by_reths_own_rpc_after_its_block_seals() {
    let dir = tempdir().unwrap();
    let sender = PrivateKeySigner::random();
    let recipient = alloy::primitives::Address::repeat_byte(0x42);
    let (_guard, http_port, _ws_port, _config_path) =
        spawn_node(dir.path(), std::slice::from_ref(&sender));
    wait_for_port(http_port, Duration::from_secs(20)).await;
    let client = HttpClientBuilder::default()
        .build(format!("http://127.0.0.1:{http_port}"))
        .unwrap();

    // Poll eth_chainId until it succeeds — first-request warm-up.
    let _ = rpc_call(&client, "eth_chainId").await;

    let raw = signed_raw_tx(&sender, CHAIN_ID, 0);
    let raw_hex = format!("0x{}", hex::encode(&raw));
    let tx_hash: String = client
        .request("eth_sendRawTransaction", jsonrpsee::rpc_params![raw_hex])
        .await
        .expect("eth_sendRawTransaction");
    assert!(tx_hash.starts_with("0x"));

    // The tx is pre-confirmed by the time eth_sendRawTransaction returns (design: it awaits the
    // preconf before replying), but its BLOCK only seals every 20th sub-block (1s) — poll for the
    // receipt to appear.
    let deadline = Instant::now() + Duration::from_secs(10);
    let receipt = loop {
        let r: Value = client
            .request(
                "eth_getTransactionReceipt",
                jsonrpsee::rpc_params![tx_hash.clone()],
            )
            .await
            .unwrap();
        if !r.is_null() {
            break r;
        }
        assert!(Instant::now() < deadline, "receipt never appeared");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(receipt["status"], "0x1", "the transfer must have succeeded");
    let block_number = receipt["blockNumber"].as_str().unwrap().to_string();

    // Reth's own "latest"-tag resolution (`eth_blockNumber`,
    // `eth_getBlockByNumber("latest", ..)`, `eth_getBalance(addr, "latest")`, ...) reads
    // `BlockchainProvider::canonical_in_memory_state` — `RethExecutor::seal_block` now advances it
    // after each commit, and `RethExecutor::rpc_provider()` hands this node a clone of that SAME
    // instance (an earlier bug served a fresh, independent one built from a bare
    // `ProviderFactory`). See `latest_tag_advances_as_blocks_seal_not_stuck_at_genesis` below for the
    // dedicated coverage.

    let block: Value = client
        .request(
            "eth_getBlockByNumber",
            jsonrpsee::rpc_params![block_number.clone(), false],
        )
        .await
        .unwrap();
    assert!(
        !block.is_null(),
        "eth_getBlockByNumber must find the sealed block"
    );
    let tx_hashes = block["transactions"].as_array().unwrap();
    assert!(
        tx_hashes
            .iter()
            .any(|h| h.as_str() == Some(tx_hash.as_str())),
        "the sealed block must list our tx"
    );

    let balance: String = client
        .request(
            "eth_getBalance",
            jsonrpsee::rpc_params![format!("{recipient:#x}"), "latest"],
        )
        .await
        .unwrap();
    assert_eq!(
        U256::from_str_radix(balance.trim_start_matches("0x"), 16).unwrap(),
        U256::ZERO,
        "this test never sends value; only asserting eth_getBalance answers over a real address"
    );

    let ots_level: Value = client
        .request("ots_getApiLevel", jsonrpsee::rpc_params![])
        .await
        .expect("ots_getApiLevel");
    assert!(
        ots_level.is_number(),
        "ots_getApiLevel must answer a number"
    );

    let ots_details: Value = client
        .request("ots_getBlockDetails", jsonrpsee::rpc_params![block_number])
        .await
        .expect("ots_getBlockDetails");
    assert!(!ots_details.is_null());
}

/// Test 7: 2,000 raw signed txs from 50 senders (same shape as `tests/e2e.rs`/`tests/e2e_reth.rs`),
/// this time against the NEW node RPC surface — p99 < 100 ms design budget.
#[ignore = "performance-sensitive; run with --release, see tests/e2e_reth.rs's comment"]
#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
async fn e2e_2000_txs_via_node_rpc_preconf_p99_under_100ms() {
    const SENDERS: usize = 50;
    const TXS_PER_SENDER: usize = 40;
    const TOTAL_TXS: usize = SENDERS * TXS_PER_SENDER;

    let dir = tempdir().unwrap();
    let senders: Vec<PrivateKeySigner> = (0..SENDERS).map(|_| PrivateKeySigner::random()).collect();
    let (_guard, http_port, _ws_port, config_path) = spawn_node(dir.path(), &senders);
    wait_for_port(http_port, Duration::from_secs(20)).await;

    let mut clients = Vec::with_capacity(SENDERS);
    for _ in 0..SENDERS {
        let client = Arc::new(
            HttpClientBuilder::default()
                .build(format!("http://127.0.0.1:{http_port}"))
                .unwrap(),
        );
        let _: Value = client
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
                result.unwrap_or_else(|e| panic!("tx {nonce} rejected: {e}"));
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
        "e2e (node RPC): {TOTAL_TXS} txs from {SENDERS} senders in {:?} wall clock ({:.0} tx/s) — \
         p50 {:?}, p99 {:?}",
        wall_clock,
        TOTAL_TXS as f64 / wall_clock.as_secs_f64(),
        p50,
        p99
    );

    // Report the log-fsync-duration and ack-latency histograms alongside the
    // e2e p99 above, from the SAME run — this is the correlation the I/O-contention hypothesis needs
    // (does the log-layer fsync's own tail explain the residual gap between seal_block's now-negligible
    // foreground cost and the observed e2e p99?). Printed before the assertion below so the report
    // survives even if the budget assertion itself fails.
    let config_text = std::fs::read_to_string(&config_path).unwrap();
    let metrics_addr = config_text
        .lines()
        .find_map(|l| l.trim().strip_prefix("metrics_addr = "))
        .expect("metrics_addr in the written config")
        .trim_matches('"')
        .to_string();
    let metrics_output = std::process::Command::new("curl")
        .args(["-s", &format!("http://{metrics_addr}/metrics")])
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
        "p99 {p99:?} must be under the 100ms design budget serving RPC through the node"
    );
}

/// `RethExecutor::seal_block` now returns as soon as re-execution/state-root
/// are done, spawning its MDBX write into the background (`rome-zk-executor-reth`'s `seal_block`/
/// `open_block` docs) — a real `kill -9` right after a block's ack, before that background write is
/// guaranteed to have landed, must be safe by design: restarting over the SAME `[reth].datadir`/
/// `log_dir` must come back up (MDBX itself never half-commits a transaction — an interrupted write
/// simply never became visible) and reach the identical chain state a clean run would. This SIGKILLs
/// the real OS process (not an in-process drop) so the background persist thread genuinely dies with
/// it, unlike dropping a `RethExecutor` in the same test process would.
#[tokio::test(flavor = "multi_thread")]
async fn kill_9_right_after_a_block_seals_then_restart_reaches_the_same_state() {
    let dir = tempdir().unwrap();
    let sender = PrivateKeySigner::random();
    let recipient = alloy::primitives::Address::repeat_byte(0x77);
    let (mut guard, http_port, _ws_port, config_path) =
        spawn_node(dir.path(), std::slice::from_ref(&sender));
    wait_for_port(http_port, Duration::from_secs(20)).await;
    let client = HttpClientBuilder::default()
        .build(format!("http://127.0.0.1:{http_port}"))
        .unwrap();
    let _ = rpc_call(&client, "eth_chainId").await; // warm-up

    let raw = signed_raw_tx(&sender, CHAIN_ID, 0);
    let raw_hex = format!("0x{}", hex::encode(&raw));
    let tx_hash: String = client
        .request("eth_sendRawTransaction", jsonrpsee::rpc_params![raw_hex])
        .await
        .expect("eth_sendRawTransaction");

    // Poll for the receipt: the tx is pre-confirmed immediately, but its block (the ack this test
    // cares about) only closes once every 20th sub-block. The receipt appearing means `seal_block`
    // already returned `BlockOutcome` — its background MDBX write may
    // or may not have finished yet. Kill NOW, deliberately racing that window rather than trying to
    // win it: the fix must not depend on which side of the race we land on.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let r: Value = client
            .request(
                "eth_getTransactionReceipt",
                jsonrpsee::rpc_params![tx_hash.clone()],
            )
            .await
            .unwrap();
        if !r.is_null() {
            break;
        }
        assert!(Instant::now() < deadline, "receipt never appeared");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    // A real SIGKILL — not a graceful shutdown, no `Executor::flush`, no chance for the background
    // persist thread to finish on its own terms. `ChildGuard::drop` would do the same kill()+wait(),
    // but doing it explicitly here (before restart) is the point of this test, not incidental cleanup.
    guard.0.kill().expect("kill the sequencer process");
    guard.0.wait().expect("reap the killed process");

    // Restart the SAME binary over the SAME `[reth].datadir` and `log_dir` (same config file, so the
    // same bound ports too) — must come back up cleanly (MDBX opening a datadir an interrupted write
    // never committed to is exactly the ACID property this relies on) and, via tail replay
    // (`recovery::replay_into_executor`), reach the state the tx should have produced either way.
    let guard2 = spawn_from_config(&config_path);
    wait_for_port(http_port, Duration::from_secs(20)).await;
    let client2 = HttpClientBuilder::default()
        .build(format!("http://127.0.0.1:{http_port}"))
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    let receipt = loop {
        let r: Value = client2
            .request(
                "eth_getTransactionReceipt",
                jsonrpsee::rpc_params![tx_hash.clone()],
            )
            .await
            .unwrap();
        if !r.is_null() {
            break r;
        }
        assert!(
            Instant::now() < deadline,
            "the restarted node never recovered the tx's receipt (recovery::replay_into_executor \
             must repair whatever the killed process's background persist did not finish)"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(
        receipt["status"], "0x1",
        "the tx must still show as successful after restart+replay"
    );

    let balance: String = client2
        .request(
            "eth_getBalance",
            jsonrpsee::rpc_params![format!("{recipient:#x}"), "latest"],
        )
        .await
        .unwrap();
    assert_eq!(
        U256::from_str_radix(balance.trim_start_matches("0x"), 16).unwrap(),
        U256::ZERO,
        "this test never sends value; only asserting the restarted node's own state is coherent"
    );

    drop(guard2);
}

/// Reth's own "latest"-tag resolution must actually advance as blocks seal —
/// `BlockchainProvider::best_block_number` reads `canonical_in_memory_state.get_canonical_block_number()`
/// (an in-memory pointer distinct from the on-disk MDBX head `RethExecutor` itself reads), and an earlier
/// version never called `set_canonical_head` on it. Sends one tx, waits for 3 real blocks to have
/// sealed past it (block cadence is time-driven — 20 sub-blocks x 50ms — so this happens regardless of
/// tx volume), then asserts every "latest"-tag read a real client depends on: `eth_blockNumber` must be
/// at least the tx's own block number (not stuck at `0x0`/genesis); `eth_getBlockByNumber("latest",
/// false)` must resolve to a real block whose own number matches `eth_blockNumber`; and
/// `eth_getBalance(sender, "latest")` must reflect the real post-tx state (strictly less than the
/// genesis balance — proves this reads live state at a resolved number, not a stale/default one).
#[tokio::test(flavor = "multi_thread")]
async fn latest_tag_advances_as_blocks_seal_not_stuck_at_genesis() {
    let dir = tempdir().unwrap();
    let sender = PrivateKeySigner::random();
    let (_guard, http_port, _ws_port, _config_path) =
        spawn_node(dir.path(), std::slice::from_ref(&sender));
    wait_for_port(http_port, Duration::from_secs(20)).await;
    let client = HttpClientBuilder::default()
        .build(format!("http://127.0.0.1:{http_port}"))
        .unwrap();
    let _ = rpc_call(&client, "eth_chainId").await; // warm-up

    let raw = signed_raw_tx(&sender, CHAIN_ID, 0);
    let raw_hex = format!("0x{}", hex::encode(&raw));
    let tx_hash: String = client
        .request("eth_sendRawTransaction", jsonrpsee::rpc_params![raw_hex])
        .await
        .expect("eth_sendRawTransaction");

    // BY-NUMBER path: poll the receipt to learn our tx's own block number.
    let deadline = Instant::now() + Duration::from_secs(10);
    let tx_block_number: u64 = loop {
        let r: Value = client
            .request(
                "eth_getTransactionReceipt",
                jsonrpsee::rpc_params![tx_hash.clone()],
            )
            .await
            .unwrap();
        if !r.is_null() {
            let hex = r["blockNumber"].as_str().unwrap().to_string();
            break u64::from_str_radix(hex.trim_start_matches("0x"), 16).unwrap();
        }
        assert!(Instant::now() < deadline, "receipt never appeared");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(tx_block_number >= 1, "the tx must have sealed a real block");

    // Let 2 more full blocks seal past it (block cadence is time-driven, independent of tx volume) so
    // "latest" genuinely needs to have moved beyond the tx's own block, not just have gotten lucky.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let block_number_hex: String = client
        .request("eth_blockNumber", jsonrpsee::rpc_params![])
        .await
        .expect("eth_blockNumber");
    let latest_number = u64::from_str_radix(block_number_hex.trim_start_matches("0x"), 16).unwrap();
    assert!(
        latest_number >= tx_block_number,
        "eth_blockNumber ({latest_number}) must have advanced to at least the tx's own block \
         ({tx_block_number}), not be stuck at genesis (0)"
    );

    let latest_block: Value = client
        .request(
            "eth_getBlockByNumber",
            jsonrpsee::rpc_params!["latest", false],
        )
        .await
        .expect("eth_getBlockByNumber(latest)");
    assert!(
        !latest_block.is_null(),
        "eth_getBlockByNumber(\"latest\") must resolve to a real block"
    );
    let latest_block_number = u64::from_str_radix(
        latest_block["number"]
            .as_str()
            .unwrap()
            .trim_start_matches("0x"),
        16,
    )
    .unwrap();
    assert_eq!(
        latest_block_number, latest_number,
        "eth_getBlockByNumber(\"latest\")'s own block number must match eth_blockNumber"
    );

    let balance_hex: String = client
        .request(
            "eth_getBalance",
            jsonrpsee::rpc_params![format!("{:#x}", sender.address()), "latest"],
        )
        .await
        .expect("eth_getBalance(sender, latest)");
    let genesis_balance =
        U256::from_str_radix(GENESIS_BALANCE_HEX.trim_start_matches("0x"), 16).unwrap();
    let latest_balance = U256::from_str_radix(balance_hex.trim_start_matches("0x"), 16).unwrap();
    assert!(
        latest_balance < genesis_balance,
        "eth_getBalance(sender, \"latest\") ({latest_balance}) must reflect the gas the sender \
         actually paid, not the untouched genesis balance ({genesis_balance})"
    );
}

/// `--executor reth` serves `node::serve` unconditionally now — a `[reth]`
/// section with NO `http_addr`/`ws_addr` keys at all (the shape a config written before the node RPC
/// existed has) must still come up and answer `eth_getBalance`, over the top-level `rpc_addr` socket
/// (`RethSettings::resolved_rpc_addrs`'s one-port fallback), not fail to start or silently fall back
/// to the old `rpc::serve` (which has no `eth_getBalance` at all — only `eth_sendRawTransaction`/
/// `rome_*`).
#[tokio::test(flavor = "multi_thread")]
async fn executor_reth_without_reth_http_ws_keys_still_serves_eth_get_balance() {
    let dir = tempdir().unwrap();
    let log_dir = dir.path().join("log");
    let key_path = dir.path().join("sequencer.key");
    std::fs::write(
        &key_path,
        hex::encode(PrivateKeySigner::random().to_bytes()),
    )
    .unwrap();

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
        "alloc": {},
        "number": "0x0", "gasUsed": "0x0",
        "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "baseFeePerGas": "0x3b9aca00"
    });
    let genesis_path = dir.path().join("genesis.json");
    std::fs::write(&genesis_path, genesis.to_string()).unwrap();

    let rpc_port = free_port();
    let metrics_port = free_port();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
            chain_id = {CHAIN_ID}
            rpc_addr = "127.0.0.1:{rpc_port}"
            metrics_addr = "127.0.0.1:{metrics_port}"
            log_dir = "{}"
            sequencer_key_path = "{}"

            [admission]
            queue_capacity = 4096
            queue_timeout_secs = 12
            park_expiry_secs = 30
            max_tx_size = 131072

            # No http_addr/ws_addr — the pre-C.4c [reth] shape.
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

    let _guard = spawn_from_config(&config_path);
    wait_for_port(rpc_port, Duration::from_secs(20)).await;
    let client = HttpClientBuilder::default()
        .build(format!("http://127.0.0.1:{rpc_port}"))
        .unwrap();

    let chain_id = rpc_call(&client, "eth_chainId").await;
    assert_eq!(chain_id.as_str().unwrap(), format!("{CHAIN_ID:#x}"));

    let recipient = alloy::primitives::Address::repeat_byte(0x11);
    let balance: String = client
        .request(
            "eth_getBalance",
            jsonrpsee::rpc_params![format!("{recipient:#x}"), "latest"],
        )
        .await
        .expect("eth_getBalance must answer over the one-port fallback");
    assert_eq!(
        U256::from_str_radix(balance.trim_start_matches("0x"), 16).unwrap(),
        U256::ZERO
    );
}

/// RSS after 1,000 blocks of ~100 txs each,
/// with the node RPC actually serving (a real OS process, not an in-process harness) — 50 senders
/// each submit a steady trickle of txs for real wall-clock time (block cadence is 50ms/sub-block x
/// 20 = 1s/block, time-driven, not tx-driven, so this genuinely takes ~1000s+). Reads the process's
/// own RSS via `ps` (macOS/Linux compatible) once the metrics endpoint's own `blocks_sealed_total`
/// (the sequencer's own counter — `eth_blockNumber` would work equally well now that
/// "latest"-tag resolution works, but this metric needs no RPC round trip) shows the design
/// load point was reached. No pass/fail threshold — a measurement, like this crate's other
/// `#[ignore]`d perf tests.
#[ignore = "performance-sensitive AND slow (~1000s+ real wall-clock, block cadence is time-driven); \
            run with --release --nocapture, see comment above"]
#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
async fn rss_after_1000_blocks_of_100_txs_with_node_rpc_serving() {
    const TARGET_BLOCKS_SEALED: u64 = 1_000;
    const SENDERS: usize = 50;

    let dir = tempdir().unwrap();
    let senders: Vec<PrivateKeySigner> = (0..SENDERS).map(|_| PrivateKeySigner::random()).collect();
    let (guard, http_port, _ws_port, config_path) = spawn_node(dir.path(), &senders);
    wait_for_port(http_port, Duration::from_secs(20)).await;
    let pid = guard.0.id();

    let config_text = std::fs::read_to_string(&config_path).unwrap();
    let metrics_addr = config_text
        .lines()
        .find_map(|l| l.trim().strip_prefix("metrics_addr = "))
        .expect("metrics_addr in the written config")
        .trim_matches('"')
        .to_string();

    let mut tasks = Vec::with_capacity(SENDERS);
    for signer in senders {
        tasks.push(tokio::spawn(async move {
            let client = HttpClientBuilder::default()
                .build(format!("http://127.0.0.1:{http_port}"))
                .unwrap();
            // ~100 txs/block design load point / 50 senders / 20 sub-blocks per block: roughly one
            // tx per sender every other sub-block (~100ms) keeps pace with the 1s/block cadence for
            // the whole run without unboundedly growing the admission queue.
            let mut nonce = 0u64;
            loop {
                let raw = signed_raw_tx(&signer, CHAIN_ID, nonce);
                let raw_hex = format!("0x{}", hex::encode(&raw));
                let result: Result<String, _> = client
                    .request("eth_sendRawTransaction", jsonrpsee::rpc_params![raw_hex])
                    .await;
                if result.is_ok() {
                    nonce += 1;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }));
    }

    fn blocks_sealed_total(metrics_addr: &str) -> u64 {
        let output = std::process::Command::new("curl")
            .args(["-s", &format!("http://{metrics_addr}/metrics")])
            .output()
            .expect("run curl");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|l| l.strip_prefix("rome_zk_sequencer_blocks_sealed_total "))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    }

    let started = Instant::now();
    loop {
        let sealed = blocks_sealed_total(&metrics_addr);
        if sealed >= TARGET_BLOCKS_SEALED {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(1_300),
            "did not reach {TARGET_BLOCKS_SEALED} blocks_sealed_total in time (at {sealed})"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let wall_clock = started.elapsed();

    let ps_output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .expect("run ps");
    let rss_kb: u64 = String::from_utf8_lossy(&ps_output.stdout)
        .trim()
        .parse()
        .expect("parse ps rss= output as a number of KB");

    eprintln!(
        "MEASURED: after {TARGET_BLOCKS_SEALED} blocks sealed (~{TARGET_BLOCKS_SEALED}00 txs \
         submitted, node RPC serving) in {wall_clock:?}, process RSS = {rss_kb} KB \
         ({:.1} MB)",
        rss_kb as f64 / 1024.0
    );

    for t in tasks {
        t.abort();
    }
    drop(guard);
}
