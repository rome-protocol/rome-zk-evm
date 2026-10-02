//! `examples/inspect_datadir.rs` builds and, exercised once here against a
//! healthy (never-torn) datadir, prints and exits 0 — the operator confirmation tool's own smoke
//! test.

use alloy_primitives::{Address, U256};
use rome_zk_executor_api::Executor;
use rome_zk_executor_reth::{RethConfig, RethExecutor};
use std::process::Command;
use tempfile::tempdir;

const CHAIN_ID: u64 = 200_101;
const GAS_LIMIT_HEX: &str = "0x2540be400";

fn write_genesis(dir: &std::path::Path, funded: Address) -> std::path::PathBuf {
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
        "gasLimit": GAS_LIMIT_HEX, "difficulty": "0x0",
        "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "coinbase": "0x0000000000000000000000000000000000000000",
        "alloc": { format!("{funded:#x}"): { "balance": format!("{:#x}", U256::from(10u128).pow(U256::from(24u8))) } },
        "number": "0x0", "gasUsed": "0x0",
        "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "baseFeePerGas": "0x3b9aca00"
    });
    let path = dir.join("genesis.json");
    std::fs::write(&path, genesis.to_string()).unwrap();
    path
}

#[tokio::test]
async fn inspect_datadir_prints_and_exits_0_on_a_healthy_datadir() {
    let reth_dir = tempdir().unwrap();
    let signer = alloy_signer_local::PrivateKeySigner::random();
    let genesis_path = write_genesis(reth_dir.path(), signer.address());
    let config = RethConfig {
        datadir: reth_dir.path().join("db"),
        genesis_path: genesis_path.clone(),
        block_gas_limit: u64::from_str_radix(GAS_LIMIT_HEX.trim_start_matches("0x"), 16).unwrap(),
    };
    // Build a real, healthy datadir (genesis only is enough — the tool never assumes any block was
    // ever sealed).
    let mut ex = RethExecutor::new(config.clone()).unwrap();
    ex.flush().await.unwrap();
    drop(ex);

    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let output = Command::new(env!("CARGO"))
        .args([
            "run",
            "--quiet",
            "-p",
            "rome-zk-executor-reth",
            "--example",
            "inspect_datadir",
            "--",
        ])
        .arg(&config.datadir)
        .arg(&genesis_path)
        .current_dir(format!("{manifest_dir}/../.."))
        .output()
        .expect("failed to run the inspect_datadir example");

    assert!(
        output.status.success(),
        "inspect_datadir must exit 0 on a healthy datadir; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("static file segments"),
        "expected the static-file section header in stdout, got:\n{stdout}"
    );
    assert!(
        stdout.contains("best_block_number"),
        "expected the MDBX section in stdout, got:\n{stdout}"
    );
    assert!(
        stdout.contains("every stage checkpoint"),
        "expected the stage-checkpoint section in stdout, got:\n{stdout}"
    );
    assert!(
        stdout.contains("header presence"),
        "expected the header-presence section in stdout, got:\n{stdout}"
    );
}

/// A directory with no `db/mdbx.dat` — a typo,
/// or a path that was never a real datadir — must be refused BY NAME, never silently turned into a
/// brand-new empty chain via reth's own `init_db`.
#[tokio::test]
async fn inspect_datadir_refuses_a_directory_with_no_mdbx_dat() {
    let empty_dir = tempdir().unwrap();
    let genesis_path = write_genesis(
        empty_dir.path(),
        alloy_signer_local::PrivateKeySigner::random().address(),
    );
    let no_mdbx_datadir = empty_dir.path().join("not-a-real-datadir");

    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let output = Command::new(env!("CARGO"))
        .args([
            "run",
            "--quiet",
            "-p",
            "rome-zk-executor-reth",
            "--example",
            "inspect_datadir",
            "--",
        ])
        .arg(&no_mdbx_datadir)
        .arg(&genesis_path)
        .current_dir(format!("{manifest_dir}/../.."))
        .output()
        .expect("failed to run the inspect_datadir example");

    assert!(
        !output.status.success(),
        "inspect_datadir must exit non-zero when db/mdbx.dat does not exist"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("mdbx.dat") && stderr.contains("does not exist"),
        "expected the named mdbx.dat refusal in stderr, got:\n{stderr}"
    );
    assert!(
        !no_mdbx_datadir.exists(),
        "the tool must never create the datadir it refused to inspect"
    );
}
