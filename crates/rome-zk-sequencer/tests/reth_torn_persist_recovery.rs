//! End-to-end recovery: reproduces the real torn persist
//! shape (`rome-zk-executor-reth`'s own `torn_headers_conf_heals_at_open_...` tests do this at the
//! executor layer only) one level up, through the SAME path a real Tiber restart takes —
//! `RethExecutor::new` (forward heal at open) followed by `recovery::replay_into_executor` (tail
//! replay) — and proves the result is byte-identical, block-hash for block-hash, to a control run
//! that was never torn at all.
//!
//! The torn shape here is the FORWARD-HEAL shape: the `.conf` snapshot is restored over
//! the last block's committed config while MDBX stays at that block — a `.conf` one row behind a
//! COMMITTED MDBX. Under reth's verified commit order (static data+offsets `sync_all`, then the
//! `.conf` rename and directory fsync, then MDBX `tx.commit()`) a kill does not produce this shape; the forward heal in
//! `rome-zk-executor-reth::static_heal` is kept for it as defence in depth and heals it in place,
//! reaching the full log tip with nothing left for `replay_into_executor` to recompute
//! (`already_persisted` covers every record the log holds). The shape a real kill DOES produce (the
//! Tiber incident) is pinned by `real_kill_shape_*` in `reth_torn_persist_resume.rs`. Multi-block
//! tail catch-up after a clean, non-torn restart is already exercised by `reth_tail_replay.rs`,
//! unaffected by this change; this test's job is proving the forward heal and tail replay COMPOSE
//! without any refusal — not re-proving tail replay's own reach.
#![cfg(feature = "reth")]

use alloy::primitives::{Address, U256};
use alloy::signers::local::PrivateKeySigner;
use reth_chainspec::ChainSpec;
use rome_zk_executor_api::SubBlockLimits;
use rome_zk_executor_reth::{genesis_from_path, open_provider_factory, RethConfig, RethExecutor};
use rome_zk_sequencer::executor::Executor;
use rome_zk_sequencer::preconf::ChannelSink;
use rome_zk_sequencer::recovery::replay_into_executor;
use rome_zk_sequencer::sealer::{SealerState, DEFAULT_BLOCK_GAS_LIMIT, SUB_BLOCKS_PER_BLOCK};
use rome_zk_sequencer::testutil::signed_raw_tx;
use std::sync::Arc;
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

/// Builds the control run (never torn) and the torn-then-healed run against independent datadirs
/// sharing the same signer/genesis/timestamps, and asserts the healed run's final head — block
/// number, hash, and state root — is byte-identical to the control run's. `signer` selects the
/// blocks-with-txs or all-empty-blocks variant (both share this one harness).
async fn torn_persist_heals_to_the_same_head_as_a_never_torn_run(signer: Option<PrivateKeySigner>) {
    let funded = PrivateKeySigner::random();
    let sequencer_key = PrivateKeySigner::random();
    let sequencer_address = sequencer_key.address();
    let base_ts = 1_757_100_000_000_000u64;
    let tx_signer = signer.as_ref().unwrap_or(&funded);

    // ---- Control run: never torn, TOTAL_BLOCKS sealed in one continuous session, its own datadir
    // and log. ----
    let control_log_dir = tempdir().unwrap();
    let control_reth_dir = tempdir().unwrap();
    let control_genesis = write_genesis(control_reth_dir.path(), tx_signer.address());
    let control_head = {
        let executor =
            RethExecutor::new(reth_config(control_reth_dir.path(), control_genesis)).unwrap();
        let mut sealer = SealerState::new(
            executor,
            rome_zk_sequencer::log::LogWriter::open(control_log_dir.path(), 1_000).unwrap(),
            sequencer_key.clone(),
            ChannelSink::new(16),
            CHAIN_ID,
            DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            SUB_BLOCKS_PER_BLOCK,
            rome_zk_sequencer::sealer::ResumePoint::default(),
        )
        .with_empty_block_interval_secs(1);
        let mut nonce = 0u64;
        seal_full_blocks(
            &mut sealer,
            signer.as_ref(),
            &mut nonce,
            base_ts,
            0,
            TOTAL_BLOCKS,
        )
        .await;
        sealer.executor.flush().await.unwrap();
        sealer.executor.head()
    };

    // ---- Torn run: PRE_TEAR_BLOCKS sealed and flushed (durable, `.conf` snapshotted here), then
    // ONE more block sealed and flushed (also fully durable — data+offsets included) before the
    // sealer shuts down. The tear restores the PRE_TEAR_BLOCKS `.conf` snapshot over the now
    // TOTAL_BLOCKS-committed datadir: `extra == 1` —
    // reth's own commit order guarantees the last block's header row is durable in data+offsets
    // regardless of whether its own `.conf` rename landed. The ordered log — the real source of
    // truth — is never touched: it holds every one of the TOTAL_BLOCKS blocks' records, exactly like
    // a real Tiber restart where the log's fsync per sub-block never depends on reth's own
    // (asynchronous) persist at all.
    let log_dir = tempdir().unwrap();
    let reth_dir = tempdir().unwrap();
    let genesis_path = write_genesis(reth_dir.path(), tx_signer.address());
    let conf_path = reth_dir
        .path()
        .join("db")
        .join("static_files")
        .join("static_file_headers_0_499999.conf");

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
        let mut nonce = 0u64;
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
        let snapshot_at_pre_tear = std::fs::read(&conf_path).unwrap();

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
        drop(sealer); // fully durable through TOTAL_BLOCKS, log holds records 1..=TOTAL_BLOCKS

        // The tear: restore the pre-final-block headers config over the current
        // (TOTAL_BLOCKS-committed) datadir.
        std::fs::write(&conf_path, &snapshot_at_pre_tear).unwrap();
    }

    // Control varies the world: confirm the precondition on a raw factory before exercising
    // recovery — MDBX's own checkpoint already reports TOTAL_BLOCKS, but the headers segment does
    // not yet expose it. This is a pure read (never calls `check_consistency`, which would truncate
    // the very row the forward heal below must still find on disk).
    {
        let genesis = genesis_from_path(&genesis_path).unwrap();
        let chain_spec = Arc::new(ChainSpec::from_genesis(genesis));
        let factory = open_provider_factory(&reth_dir.path().join("db"), chain_spec).unwrap();
        let provider = factory.provider().unwrap();
        assert_eq!(
            reth_provider::BlockNumReader::best_block_number(&provider).unwrap(),
            TOTAL_BLOCKS,
            "precondition: MDBX's own checkpoint must already report TOTAL_BLOCKS"
        );
        assert!(
            reth_provider::HeaderProvider::sealed_header(&provider, TOTAL_BLOCKS)
                .unwrap()
                .is_none(),
            "precondition: the torn headers segment must not yet expose TOTAL_BLOCKS's header"
        );
    }

    // ---- Recovery: the real restart path — open (forward-heals to TOTAL_BLOCKS), then tail-replay
    // the log (finds nothing left to replay: the log holds exactly TOTAL_BLOCKS blocks too). ----
    let mut healed_executor = reopen_with_retry(reth_config(reth_dir.path(), genesis_path.clone()));
    assert_eq!(
        healed_executor.last_persisted_block(),
        Some(TOTAL_BLOCKS),
        "the forward heal must reach TOTAL_BLOCKS directly — the single torn block is not lost, \
         so there is nothing left below it for a backward heal to fall back to"
    );

    let resume = replay_into_executor(
        log_dir.path(),
        &mut healed_executor,
        sequencer_address,
        false,
        DEFAULT_BLOCK_GAS_LIMIT,
        Address::ZERO,
        SUB_BLOCKS_PER_BLOCK,
    )
    .await
    .expect("tail replay must succeed with nothing left to replay, not refuse to start");

    assert_eq!(resume.next_block, TOTAL_BLOCKS + 1);
    assert_eq!(resume.next_index, 0);
    assert_eq!(
        healed_executor.head(),
        control_head,
        "a torn persist, healed forward at open, must reach the identical head (block hash \
         included) as a run that was never torn at all"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sequencer_restart_after_a_torn_persist_reaches_the_same_head_as_a_never_torn_run() {
    torn_persist_heals_to_the_same_head_as_a_never_torn_run(Some(PrivateKeySigner::random())).await;
}

/// The empty-block variant (the idle Tiber shape): every block is sealed with zero
/// transactions. Previously the refusal this shape produced differed from
/// the blocks-with-txs shape (`UnexpectedStaticFileBlockNumber` at the first live persist, rather
/// than `ReplayDiverged`) — proving the forward heal (which never runs a live persist to diverge in
/// the first place) covers both shapes identically is this test's own job.
#[tokio::test(flavor = "multi_thread")]
async fn sequencer_restart_after_a_torn_persist_with_empty_blocks_reaches_the_same_head_as_a_never_torn_run(
) {
    torn_persist_heals_to_the_same_head_as_a_never_torn_run(None).await;
}
