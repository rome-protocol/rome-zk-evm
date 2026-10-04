//! The real binary must exit non-zero on a fatal shutdown, so a
//! supervisor configured to restart on failure (systemd, compose `restart: on-failure`) actually
//! restarts it. This forces a genuine, live `SequencerFatal` (not a simulated one) by making a segment
//! roll fail with a permission error partway through a real run, then asserts the process's actual exit
//! code — the only thing a supervisor ever sees.

use alloy::signers::local::PrivateKeySigner;
use jsonrpsee::core::client::ClientT;
use jsonrpsee::http_client::HttpClientBuilder;
use rome_zk_sequencer::testutil::signed_raw_tx;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::tempdir;

mod common;

const CHAIN_ID: u64 = 424_243;

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

/// Forces a live `SealError::Log` — and so a real `SequencerFatal` — by revoking write permission on
/// the log directory partway through a run with `blocks_per_segment = 1` (a fast roll cadence: every
/// block, i.e. every 20 sub-blocks at the 50ms cadence, ~1s). The *first* roll (block 0 -> block 1)
/// succeeds normally, proving the setup itself is sound; permission is revoked only after that, so the
/// *second* roll (block 1 -> block 2) fails at `OpenOptions::open` — creating a file in a directory
/// requires write permission on the directory itself, which an already-open fd on an existing segment
/// file does not need and so is not itself affected.
#[tokio::test(flavor = "multi_thread")]
async fn a_live_segment_roll_failure_exits_the_process_non_zero() {
    let dir = tempdir().unwrap();
    let log_dir = dir.path().join("log");
    std::fs::create_dir_all(&log_dir).unwrap();
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
            blocks_per_segment = 1

            # This test needs a SECOND block to open and close on its own, with no
            # further transactions submitted, so the forced permission failure actually bites the next
            # segment roll — a nonzero empty_block_interval_secs (the minimum legal one at the default
            # profile's 1s block time) keeps the chain sealing on a timer exactly as every chain did
            # before this knob existed; this test is about fatal-exit propagation, not idle behaviour.
            [profile]
            empty_block_interval_secs = 1
            "#,
            log_dir.display(),
            key_path.display(),
        ),
    )
    .unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rome-zk-sequencer"));
    cmd.arg("--config").arg(&config_path);
    let (mut child, ports) = common::spawn_reporting_ports(&mut cmd);
    let rpc_port = ports.rpc;

    wait_for_port(rpc_port, Duration::from_secs(10)).await;

    // Submit one tx so the process is doing real work, not just idling.
    let client = HttpClientBuilder::default()
        .build(format!("http://127.0.0.1:{rpc_port}"))
        .unwrap();
    let sender = PrivateKeySigner::random();
    let raw = signed_raw_tx(&sender, CHAIN_ID, 0);
    let raw_hex = format!("0x{}", hex::encode(&raw));
    let _: String = client
        .request("eth_sendRawTransaction", jsonrpsee::rpc_params![raw_hex])
        .await
        .expect("warmup tx must be preconfirmed");

    // Let the first block close and its segment roll succeed normally (20 sub-blocks * 50ms + margin).
    tokio::time::sleep(Duration::from_millis(1_500)).await;

    // Revoke write permission on the log directory — the *next* segment roll's `OpenOptions::create`
    // will fail with a permission error. An already-open append fd on the current segment file is
    // unaffected (permission is checked at open, not at write), so this only bites the next roll.
    std::fs::set_permissions(&log_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

    // Wait for the process to exit on its own — the next segment roll (another ~1s away) must turn
    // into a fatal `SealError::Log`, which the actor propagates as `SequencerFatal`, which must exit
    // the process non-zero.
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "process did not exit after the forced segment-roll failure"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    // Restore permissions so tempdir cleanup can remove the directory.
    let _ = std::fs::set_permissions(&log_dir, std::fs::Permissions::from_mode(0o755));

    assert!(
        !status.success(),
        "a fatal segment-roll failure must exit the process non-zero, got {status:?}"
    );
}
