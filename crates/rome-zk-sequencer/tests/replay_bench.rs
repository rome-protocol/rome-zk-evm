//! Replay-time measurement. On restart, `recovery::replay_into_executor`
//! re-derives the whole chain by re-executing every logged sub-block through a fresh `RethExecutor` —
//! this crate keeps no on-disk state beyond genesis (see `rome-zk-executor-reth`'s module doc). This
//! test measures how long that takes for a chain with real history, to motivate a real
//! reth node in-process with MDBX + `eth_*` RPC, so a restart is a database open, not a full replay.
#![cfg(feature = "reth")]

use alloy::primitives::{Address, U256};
use alloy::signers::local::PrivateKeySigner;
use rome_zk_executor_reth::{RethConfig, RethExecutor};
use rome_zk_sequencer::executor::SubBlockLimits;
use rome_zk_sequencer::log::LogWriter;
use rome_zk_sequencer::preconf::ChannelSink;
use rome_zk_sequencer::recovery::replay_into_executor;
use rome_zk_sequencer::sealer::{
    ResumePoint, SealerState, DEFAULT_BLOCK_GAS_LIMIT, SUB_BLOCKS_PER_BLOCK,
};
use rome_zk_sequencer::testutil::signed_raw_tx;
use std::time::Instant;
use tempfile::tempdir;

const CHAIN_ID: u64 = 424_244;
const BLOCKS: u64 = 1_000;
const TXS_PER_SUB_BLOCK: u64 = 5; // x SUB_BLOCKS_PER_BLOCK (20) = 100 txs/block
const GAS_LIMIT_HEX: &str = "0x2540be400";

fn genesis_json(funded: Address) -> serde_json::Value {
    serde_json::json!({
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
        "alloc": {
            format!("{funded:#x}"): { "balance": format!("{:#x}", U256::from(10u128).pow(U256::from(30u8))) }
        },
        "number": "0x0", "gasUsed": "0x0",
        "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "baseFeePerGas": "0x3b9aca00"
    })
}

/// Measurement: 1,000 blocks of 100 txs each (100,000 txs total),
/// sealed live with a real `RethExecutor`, then replayed from the ordered log alone into a fresh
/// `RethExecutor` over a fresh datadir — the exact recovery path a restart takes. Debug-build
/// secp256k1/RLP/keccak are unrepresentative (this crate's other perf tests document the same trap),
/// so this is `#[ignore]`d and run explicitly:
/// `cargo test -p rome-zk-sequencer --test replay_bench --release -- --ignored --nocapture`.
#[ignore = "performance-sensitive; run with --release --nocapture, see comment above"]
#[tokio::test(flavor = "multi_thread")]
async fn replay_1000_blocks_of_100_txs_wall_time() {
    let sender = PrivateKeySigner::random();
    let sequencer_key = PrivateKeySigner::random();
    let sequencer_address = sequencer_key.address();

    let live_dir = tempdir().unwrap();
    let genesis_path = live_dir.path().join("genesis.json");
    std::fs::write(&genesis_path, genesis_json(sender.address()).to_string()).unwrap();
    let log_dir = live_dir.path().join("log");

    let executor = RethExecutor::new(RethConfig {
        datadir: live_dir.path().join("db"),
        genesis_path: genesis_path.clone(),
        block_gas_limit: u64::from_str_radix(GAS_LIMIT_HEX.trim_start_matches("0x"), 16).unwrap(),
    })
    .unwrap();
    let log = LogWriter::open(&log_dir, 10_000).unwrap();
    let mut sealer = SealerState::new(
        executor,
        log,
        sequencer_key,
        ChannelSink::new(16),
        CHAIN_ID,
        DEFAULT_BLOCK_GAS_LIMIT,
        Address::ZERO,
        SUB_BLOCKS_PER_BLOCK,
        ResumePoint::default(),
    );

    let base_ts_us = 1_757_000_000_000_000u64;
    let mut nonce = 0u64;
    let seal_start = Instant::now();
    for b in 0..BLOCKS {
        for i in 0..SUB_BLOCKS_PER_BLOCK {
            let txs: Vec<_> = (0..TXS_PER_SUB_BLOCK)
                .map(|_| {
                    let tx = signed_raw_tx(&sender, CHAIN_ID, nonce);
                    nonce += 1;
                    tx
                })
                .collect();
            let ts_us = base_ts_us + (b * u64::from(SUB_BLOCKS_PER_BLOCK) + u64::from(i)) * 50_000;
            sealer
                .seal_sub_block(txs, ts_us, SubBlockLimits::unbounded())
                .await
                .unwrap();
        }
    }
    let seal_elapsed = seal_start.elapsed();
    println!(
        "MEASURED: sealing {BLOCKS} blocks x 100 txs ({} total txs) live took {seal_elapsed:?}",
        BLOCKS * u64::from(SUB_BLOCKS_PER_BLOCK) * TXS_PER_SUB_BLOCK
    );

    // Replay: a fresh RethExecutor over a fresh datadir with the same genesis, driven purely from the
    // ordered log — exactly `bin/rome-zk-sequencer.rs`'s startup path.
    let replay_dir = tempdir().unwrap();
    let mut fresh = RethExecutor::new(RethConfig {
        datadir: replay_dir.path().join("db"),
        genesis_path,
        block_gas_limit: u64::from_str_radix(GAS_LIMIT_HEX.trim_start_matches("0x"), 16).unwrap(),
    })
    .unwrap();

    let replay_start = Instant::now();
    replay_into_executor(
        &log_dir,
        &mut fresh,
        sequencer_address,
        false,
        DEFAULT_BLOCK_GAS_LIMIT,
        Address::ZERO,
        SUB_BLOCKS_PER_BLOCK,
    )
    .await
    .unwrap();
    let replay_elapsed = replay_start.elapsed();

    println!(
        "MEASURED: replay of {BLOCKS} blocks x 100 txs ({} total txs) from the ordered log took \
         {replay_elapsed:?} (the case for a real reth node in-process with MDBX + eth_* RPC, \
         so restart is a database open, not a full replay)",
        BLOCKS * u64::from(SUB_BLOCKS_PER_BLOCK) * TXS_PER_SUB_BLOCK
    );
}
