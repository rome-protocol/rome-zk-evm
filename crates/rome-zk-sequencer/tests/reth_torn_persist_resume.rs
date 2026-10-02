//! End-to-end recovery AND resumption: the two torn-persist
//! shapes a real kill produces, driven through the SAME path a Tiber restart takes — `RethExecutor::new`
//! (heal at open) → `recovery::replay_into_executor` (tail replay) → `SealerState` sealing NEW blocks →
//! flush → a clean reopen — and compared block-hash for block-hash against a run that was never torn.
//!
//! Shapes: `restore_mdbx_too == true` is the REAL kill shape — kill after the static
//! `sync_all` and before the `.conf` rename + MDBX commit: one header row durable in data+offsets, the
//! `.conf` and MDBX both at the previous block (the Tiber incident).
//! The forward heal refuses that row by name (MDBX never committed its hash), reth truncates it, the
//! executor opens at the previous block and the ordered log replays the torn block. `restore_mdbx_too ==
//! false` is the forward-heal shape — `.conf` behind a COMMITTED MDBX — which reth's own commit order
//! (rename + directory fsync strictly before the MDBX commit) does not produce on a kill; it is kept as
//! defence in depth and heals forward to the committed block. Both shapes must then RESUME: seal more
//! blocks, persist them, and reopen cleanly (the empty-block variant could once die at
//! the first live persist — `UnexpectedStaticFileBlockNumber` — which is why sealing after the heal is
//! part of this test, not an afterthought).
#![cfg(feature = "reth")]

use alloy::primitives::{Address, U256};
use alloy::signers::local::PrivateKeySigner;
use rome_zk_executor_api::SubBlockLimits;
use rome_zk_executor_reth::{RethConfig, RethExecutor};
use rome_zk_sequencer::executor::Executor;
use rome_zk_sequencer::preconf::ChannelSink;
use rome_zk_sequencer::recovery::replay_into_executor;
use rome_zk_sequencer::sealer::{SealerState, DEFAULT_BLOCK_GAS_LIMIT, SUB_BLOCKS_PER_BLOCK};
use rome_zk_sequencer::testutil::signed_raw_tx;
use std::time::{Duration, Instant};
use tempfile::tempdir;

const CHAIN_ID: u64 = 200_101;
const GAS_LIMIT_HEX: &str = "0x2540be400";
/// Blocks sealed and durably flushed BEFORE the headers `.conf` is snapshotted — both storage
/// layers agree on all of them; only the block sealed AFTER the snapshot tears.
const PRE_TEAR_BLOCKS: u64 = 3;
/// Total blocks sealed in the torn run — `PRE_TEAR_BLOCKS + 1`: the `+ 1` is the single block whose
/// own `.conf` rename this test rolls back, the shape the forward heal targets (`extra ==
/// 1`).
const TOTAL_BLOCKS: u64 = PRE_TEAR_BLOCKS + 1;

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

fn reth_config(reth_dir: &std::path::Path, genesis_path: std::path::PathBuf) -> RethConfig {
    RethConfig {
        datadir: reth_dir.join("db"),
        genesis_path,
        block_gas_limit: u64::from_str_radix(GAS_LIMIT_HEX.trim_start_matches("0x"), 16).unwrap(),
    }
}

/// See `reth_tail_replay.rs`'s own `reopen_with_retry`: the dropped executor's last background
/// persist may still hold the MDBX environment lock for a few hundred ms.
fn reopen_with_retry(config: RethConfig) -> RethExecutor {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match RethExecutor::new(config.clone()) {
            Ok(ex) => return ex,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => panic!("reopen over the same datadir failed after 10 s: {e}"),
        }
    }
}

fn unbounded() -> SubBlockLimits {
    SubBlockLimits {
        gas_limit: u64::MAX,
        deadline: Instant::now() + Duration::from_secs(3600),
    }
}

/// Seals `count` full blocks (20 sub-blocks each), starting at global sub-block offset
/// `start_sub_block` (so timestamps stay monotonically increasing across a multi-call sealing
/// session) — the same per-block shape `reth_tail_replay.rs` uses. `signer` is `Some` for the
/// blocks-with-txs variant (one signed transfer in sub-block 0 of each block) and `None` for the
/// idle-Tiber, all-empty-blocks variant.
async fn seal_full_blocks(
    sealer: &mut SealerState<RethExecutor, ChannelSink>,
    signer: Option<&PrivateKeySigner>,
    nonce: &mut u64,
    base_ts: u64,
    start_sub_block: u64,
    count: u64,
) {
    for i in 0..(count * SUB_BLOCKS_PER_BLOCK as u64) {
        let txs = match signer {
            Some(signer) if i % (SUB_BLOCKS_PER_BLOCK as u64) == 0 => {
                let tx = signed_raw_tx(signer, CHAIN_ID, *nonce);
                *nonce += 1;
                vec![tx]
            }
            _ => vec![],
        };
        sealer
            .seal_sub_block(txs, base_ts + (start_sub_block + i) * 50_000, unbounded())
            .await
            .unwrap();
    }
}

fn copy_dir_recursive(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir_recursive(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), &dest).unwrap();
        }
    }
}

/// Control: `blocks` blocks sealed continuously, never torn. Returns the head.
async fn control_head(
    signer: Option<&PrivateKeySigner>,
    tx_signer: &PrivateKeySigner,
    sequencer_key: &PrivateKeySigner,
    base_ts: u64,
    blocks: u64,
) -> rome_zk_executor_api::Head {
    let log_dir = tempdir().unwrap();
    let reth_dir = tempdir().unwrap();
    let genesis = write_genesis(reth_dir.path(), tx_signer.address());
    let executor = RethExecutor::new(reth_config(reth_dir.path(), genesis)).unwrap();
    let mut sealer = SealerState::new(
        executor,
        rome_zk_sequencer::log::LogWriter::open(log_dir.path(), 1_000).unwrap(),
        sequencer_key.clone(),
        ChannelSink::new(16),
        CHAIN_ID,
        DEFAULT_BLOCK_GAS_LIMIT,
        Address::ZERO,
        SUB_BLOCKS_PER_BLOCK,
        rome_zk_sequencer::sealer::ResumePoint::default(),
    )
    // This test is about torn-persist/resume mechanics, not idle behaviour — a nonzero
    // interval (the minimum legal one at this profile's 1s block time) keeps every sub-block sealing
    // exactly as it did before this knob existed, including `seal_full_blocks`'s own all-empty variant.
    .with_empty_block_interval_secs(1);
    let mut nonce = 0u64;
    seal_full_blocks(&mut sealer, signer, &mut nonce, base_ts, 0, blocks).await;
    sealer.executor.flush().await.unwrap();
    sealer.executor.head()
}

const MORE_BLOCKS: u64 = 2;

/// `restore_mdbx_too == false`: the forward-heal shape (conf restored, MDBX at TOTAL).
/// `restore_mdbx_too == true`: the REAL kill shape (kill after static sync_all, before the MDBX
/// commit — what the captured Tiber datadir holds): conf AND db/ restored, dangling row TOTAL in
/// data+offsets, MDBX at PRE_TEAR.
async fn heal_replay_resume(signer: Option<PrivateKeySigner>, restore_mdbx_too: bool) {
    let funded = PrivateKeySigner::random();
    let sequencer_key = PrivateKeySigner::random();
    let sequencer_address = sequencer_key.address();
    let base_ts = 1_757_100_000_000_000u64;
    let tx_signer = signer.as_ref().unwrap_or(&funded);

    let control_total = control_head(
        signer.as_ref(),
        tx_signer,
        &sequencer_key,
        base_ts,
        TOTAL_BLOCKS,
    )
    .await;
    let control_more = control_head(
        signer.as_ref(),
        tx_signer,
        &sequencer_key,
        base_ts,
        TOTAL_BLOCKS + MORE_BLOCKS,
    )
    .await;

    let log_dir = tempdir().unwrap();
    let reth_dir = tempdir().unwrap();
    let genesis_path = write_genesis(reth_dir.path(), tx_signer.address());
    let datadir = reth_dir.path().join("db");
    let conf_path = datadir
        .join("static_files")
        .join("static_file_headers_0_499999.conf");
    let mdbx_dir = datadir.join("db");
    let mdbx_snapshot = reth_dir.path().join("mdbx_snapshot");

    let mut nonce = 0u64;
    {
        let executor =
            RethExecutor::new(reth_config(reth_dir.path(), genesis_path.clone())).unwrap();
        let mut sealer = SealerState::new(
            executor,
            rome_zk_sequencer::log::LogWriter::open(log_dir.path(), 1_000).unwrap(),
            sequencer_key.clone(),
            ChannelSink::new(16),
            CHAIN_ID,
            DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            SUB_BLOCKS_PER_BLOCK,
            rome_zk_sequencer::sealer::ResumePoint::default(),
        )
        .with_empty_block_interval_secs(1);
        seal_full_blocks(
            &mut sealer,
            signer.as_ref(),
            &mut nonce,
            base_ts,
            0,
            PRE_TEAR_BLOCKS,
        )
        .await;
        sealer.executor.flush().await.unwrap();
        assert_eq!(sealer.executor.head().block, PRE_TEAR_BLOCKS);
        drop(sealer);
        std::thread::sleep(Duration::from_millis(500));
        // Quiescent snapshots (nothing holds the datadir open): the headers .conf and the MDBX dir.
        let snapshot_conf = std::fs::read(&conf_path).unwrap();
        copy_dir_recursive(&mdbx_dir, &mdbx_snapshot);

        // Reopen (clean), replay (nothing to do), seal exactly ONE more block, flush, close.
        let mut executor = reopen_with_retry(reth_config(reth_dir.path(), genesis_path.clone()));
        assert_eq!(executor.last_persisted_block(), Some(PRE_TEAR_BLOCKS));
        let resume = replay_into_executor(
            log_dir.path(),
            &mut executor,
            sequencer_address,
            false,
            DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            SUB_BLOCKS_PER_BLOCK,
        )
        .await
        .unwrap();
        assert_eq!(resume.next_block, PRE_TEAR_BLOCKS + 1);
        let mut sealer = SealerState::new(
            executor,
            rome_zk_sequencer::log::LogWriter::open(log_dir.path(), 1_000).unwrap(),
            sequencer_key.clone(),
            ChannelSink::new(16),
            CHAIN_ID,
            DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            SUB_BLOCKS_PER_BLOCK,
            resume,
        )
        .with_empty_block_interval_secs(1);
        seal_full_blocks(
            &mut sealer,
            signer.as_ref(),
            &mut nonce,
            base_ts,
            PRE_TEAR_BLOCKS * SUB_BLOCKS_PER_BLOCK as u64,
            1,
        )
        .await;
        sealer.executor.flush().await.unwrap();
        assert_eq!(sealer.executor.head().block, TOTAL_BLOCKS);
        drop(sealer);
        std::thread::sleep(Duration::from_millis(500));

        // The tear.
        std::fs::write(&conf_path, &snapshot_conf).unwrap();
        if restore_mdbx_too {
            std::fs::remove_dir_all(&mdbx_dir).unwrap();
            copy_dir_recursive(&mdbx_snapshot, &mdbx_dir);
        }
    }

    let mut healed = reopen_with_retry(reth_config(reth_dir.path(), genesis_path.clone()));
    let expected_open = if restore_mdbx_too {
        PRE_TEAR_BLOCKS
    } else {
        TOTAL_BLOCKS
    };
    assert_eq!(
        healed.last_persisted_block(),
        Some(expected_open),
        "open head after heal"
    );

    let resume = replay_into_executor(
        log_dir.path(),
        &mut healed,
        sequencer_address,
        false,
        DEFAULT_BLOCK_GAS_LIMIT,
        Address::ZERO,
        SUB_BLOCKS_PER_BLOCK,
    )
    .await
    .expect("tail replay after the heal must succeed");
    assert_eq!(resume.next_block, TOTAL_BLOCKS + 1);
    assert_eq!(
        healed.head(),
        control_total,
        "head after heal + replay == never-torn control"
    );

    // RESUME: seal MORE_BLOCKS new blocks on the healed executor and persist them.
    let mut sealer = SealerState::new(
        healed,
        rome_zk_sequencer::log::LogWriter::open(log_dir.path(), 1_000).unwrap(),
        sequencer_key.clone(),
        ChannelSink::new(16),
        CHAIN_ID,
        DEFAULT_BLOCK_GAS_LIMIT,
        Address::ZERO,
        SUB_BLOCKS_PER_BLOCK,
        resume,
    )
    .with_empty_block_interval_secs(1);
    seal_full_blocks(
        &mut sealer,
        signer.as_ref(),
        &mut nonce,
        base_ts,
        TOTAL_BLOCKS * SUB_BLOCKS_PER_BLOCK as u64,
        MORE_BLOCKS,
    )
    .await;
    sealer
        .executor
        .flush()
        .await
        .expect("first live persist after the heal must succeed");
    assert_eq!(
        sealer.executor.head(),
        control_more,
        "head after resuming == never-torn control"
    );
    drop(sealer);

    // And a clean reopen afterwards must see the resumed head with (None, None).
    let reopened = reopen_with_retry(reth_config(reth_dir.path(), genesis_path.clone()));
    assert_eq!(
        reopened.last_persisted_block(),
        Some(TOTAL_BLOCKS + MORE_BLOCKS)
    );
    assert_eq!(reopened.head(), control_more);
}

#[tokio::test(flavor = "multi_thread")]
async fn forward_heal_shape_then_resume_with_txs() {
    heal_replay_resume(Some(PrivateKeySigner::random()), false).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn forward_heal_shape_then_resume_with_empty_blocks() {
    heal_replay_resume(None, false).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn real_kill_shape_then_resume_with_txs() {
    heal_replay_resume(Some(PrivateKeySigner::random()), true).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn real_kill_shape_then_resume_with_empty_blocks() {
    heal_replay_resume(None, true).await;
}
