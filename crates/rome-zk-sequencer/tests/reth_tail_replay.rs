//! Tail-only replay against the real `RethExecutor` — seal 50 full blocks
//! (plus a partial 51st), "crash", reopen over the SAME datadir, and confirm `recovery::
//! replay_into_executor` replays only the tail beyond the persisted head, reaching the identical
//! live head — and that a log/database divergence is a named fatal error, not silently accepted.
#![cfg(feature = "reth")]

use alloy::primitives::{Address, Bytes, U256};
use alloy::signers::local::PrivateKeySigner;
use rome_zk_executor_api::{
    BlockEnv, BlockOutcome, BlockSealInputs, ExecutorError, Head, SubBlockLimits, SubBlockOutcome,
};
use rome_zk_executor_reth::{RethConfig, RethExecutor};
use rome_zk_sequencer::executor::Executor;
use rome_zk_sequencer::preconf::ChannelSink;
use rome_zk_sequencer::recovery::{replay_into_executor, RecoveryError};
use rome_zk_sequencer::sealer::{SealerState, DEFAULT_BLOCK_GAS_LIMIT, SUB_BLOCKS_PER_BLOCK};
use rome_zk_sequencer::testutil::signed_raw_tx;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
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

/// Delegates every `Executor` call to a real `RethExecutor`, counting `execute_sub_block` calls —
/// used only to prove replay's tail is bounded, since the trait itself has no other way to observe
/// "how much execution happened".
struct CountingRethExecutor {
    inner: RethExecutor,
    sub_block_calls: Arc<AtomicUsize>,
}

impl Executor for CountingRethExecutor {
    async fn open_block(&mut self, env: BlockEnv) -> Result<(), ExecutorError> {
        self.inner.open_block(env).await
    }
    async fn execute_sub_block(
        &mut self,
        txs: &[Bytes],
        limits: SubBlockLimits,
    ) -> Result<SubBlockOutcome, ExecutorError> {
        self.sub_block_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.execute_sub_block(txs, limits).await
    }
    async fn seal_block(&mut self, inputs: BlockSealInputs) -> Result<BlockOutcome, ExecutorError> {
        self.inner.seal_block(inputs).await
    }
    fn head(&self) -> Head {
        self.inner.head()
    }
    fn nonce(&self, addr: Address) -> u64 {
        self.inner.nonce(addr)
    }
    fn last_persisted_block(&self) -> Option<u64> {
        self.inner.last_persisted_block()
    }
}

fn unbounded() -> SubBlockLimits {
    SubBlockLimits {
        gas_limit: u64::MAX,
        deadline: Instant::now() + Duration::from_secs(3600),
    }
}

/// `RethExecutor::new` over a datadir whose previous owner may still be flushing: retry while MDBX
/// reports the environment as busy, for at most 10 s.
fn reopen_with_retry(config: rome_zk_executor_reth::RethConfig) -> RethExecutor {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match RethExecutor::new(config.clone()) {
            Ok(ex) => return ex,
            // The busy environment surfaces as EAGAIN on Linux and as ENOMEM ("Cannot allocate
            // memory") on macOS, so retry on ANY open failure until the deadline, then fail loudly.
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => panic!("reopen over the same datadir failed after 10 s: {e}"),
        }
    }
}

/// Tail-only replay (measured) and part of the restart contract: seal 50 full
/// blocks + 5 sub-blocks into a partial 51st, "crash" (drop, no clean shutdown), reopen a fresh
/// `RethExecutor` over the SAME datadir and replay the SAME log — the tail beyond the persisted head
/// is exactly those 5 sub-blocks (never the other 1,000), and the resulting head is identical to the
/// live run's.

#[tokio::test(flavor = "multi_thread")]
async fn restart_replays_only_the_tail_beyond_the_persisted_head() {
    let log_dir = tempdir().unwrap();
    let reth_dir = tempdir().unwrap();
    let signer = PrivateKeySigner::random();
    let sequencer_key = PrivateKeySigner::random();
    let sequencer_address = sequencer_key.address();

    let genesis_path = write_genesis(reth_dir.path(), signer.address());
    let reth_config = || RethConfig {
        datadir: reth_dir.path().join("db"),
        genesis_path: genesis_path.clone(),
        block_gas_limit: u64::from_str_radix(GAS_LIMIT_HEX.trim_start_matches("0x"), 16).unwrap(),
    };

    const FULL_BLOCKS: u64 = 50;
    const TAIL_SUB_BLOCKS: u16 = 5;

    let live_head = {
        let executor = RethExecutor::new(reth_config()).unwrap();
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
        // This test is about tail-replay/persistence mechanics, not idle behaviour — a
        // nonzero interval (the minimum legal one at this profile's 1s block time) keeps every
        // sub-block sealing exactly as it did before this knob existed.
        .with_empty_block_interval_secs(1);

        let mut nonce = 0u64;
        let base_ts = 1_757_000_000_000_000u64;
        let live_started = Instant::now();
        let total_sub_blocks = FULL_BLOCKS * SUB_BLOCKS_PER_BLOCK as u64 + TAIL_SUB_BLOCKS as u64;
        for i in 0..total_sub_blocks {
            let txs = if i % 4 == 0 {
                let tx = signed_raw_tx(&signer, CHAIN_ID, nonce);
                nonce += 1;
                vec![tx]
            } else {
                vec![]
            };
            sealer
                .seal_sub_block(txs, base_ts + i * 50_000, unbounded())
                .await
                .unwrap();
        }
        eprintln!(
            "MEASURED: live run of {total_sub_blocks} sub-blocks ({FULL_BLOCKS} full blocks + {TAIL_SUB_BLOCKS} tail) took {:?}",
            live_started.elapsed()
        );
        sealer.executor.head()
        // sealer (and its RethExecutor) drop here — no clean shutdown, simulating a crash.
    };

    // Reopen over the SAME datadir: must read back the real persisted head with zero execution.
    // The dropped executor's LAST background persist may still hold the MDBX environment lock for a
    // few hundred ms (persistence is asynchronous by design); a real restart would see the same
    // EAGAIN/EWOULDBLOCK until the old process exits, so reopen with a bounded retry rather than
    // asserting the lock is already free (CI hit "Resource temporarily unavailable (11)" here).
    let reopened = reopen_with_retry(reth_config());
    // Persistence is asynchronous by design — a crash right
    // after a seal may leave the LAST sealed block's MDBX write in flight. The ordered log is the
    // truth, so the durable head must be within one block of the live head; anything beyond is
    // repaired by tail replay below (proven separately by the kill -9 test).
    let persisted = reopened
        .last_persisted_block()
        .expect("at least one block must be durable after 50 sealed blocks");
    // A fresh chain's first sealed block is design 1, so the last
    // of the FULL_BLOCKS fully-sealed blocks is design block FULL_BLOCKS itself, not FULL_BLOCKS - 1.
    assert!(
        persisted + 1 >= FULL_BLOCKS,
        "durable head {persisted} must be within one block of the last sealed block {}",
        FULL_BLOCKS
    );
    let unpersisted_blocks = (FULL_BLOCKS - persisted) as usize;

    let sub_block_calls = Arc::new(AtomicUsize::new(0));
    let mut counting = CountingRethExecutor {
        inner: reopened,
        sub_block_calls: sub_block_calls.clone(),
    };

    let replay_started = Instant::now();
    let resume = replay_into_executor(
        log_dir.path(),
        &mut counting,
        sequencer_address,
        false,
        DEFAULT_BLOCK_GAS_LIMIT,
        Address::ZERO,
        SUB_BLOCKS_PER_BLOCK,
    )
    .await
    .unwrap();
    let replay_elapsed = replay_started.elapsed();

    eprintln!(
        "MEASURED: tail-only replay ({} execute_sub_block calls) took {replay_elapsed:?}",
        sub_block_calls.load(Ordering::SeqCst)
    );

    let expected_replayed =
        TAIL_SUB_BLOCKS as usize + unpersisted_blocks * SUB_BLOCKS_PER_BLOCK as usize;
    assert!(
        sub_block_calls.load(Ordering::SeqCst) <= 2 * SUB_BLOCKS_PER_BLOCK as usize,
        "replay must execute at most the in-flight block plus the partial tail ({}), got {}",
        2 * SUB_BLOCKS_PER_BLOCK,
        sub_block_calls.load(Ordering::SeqCst)
    );
    assert_eq!(
        sub_block_calls.load(Ordering::SeqCst),
        expected_replayed,
        "replay must execute exactly the {unpersisted_blocks} unpersisted block(s) plus the partial 51st block's {TAIL_SUB_BLOCKS} sub-blocks — no more, no less"
    );
    assert_eq!(
        counting.inner.head(),
        live_head,
        "tail-only replay must reach the identical live head"
    );
    assert_eq!(resume.next_block, FULL_BLOCKS + 1);
    assert_eq!(resume.next_index, TAIL_SUB_BLOCKS);
}

/// Named fatal error: the executor's own MDBX claims sequencer block 2 is durably
/// persisted, but the log itself is truncated to only cover block 1 — the database is ahead of a
/// log that is supposed to be authoritative. Refuses to start rather than trusting the database.
#[tokio::test(flavor = "multi_thread")]
async fn log_shorter_than_persisted_head_is_a_named_fatal_error() {
    let log_dir = tempdir().unwrap();
    let reth_dir = tempdir().unwrap();
    let signer = PrivateKeySigner::random();
    let sequencer_key = PrivateKeySigner::random();
    let sequencer_address = sequencer_key.address();

    let genesis_path = write_genesis(reth_dir.path(), signer.address());
    let reth_config = || RethConfig {
        datadir: reth_dir.path().join("db"),
        genesis_path: genesis_path.clone(),
        block_gas_limit: u64::from_str_radix(GAS_LIMIT_HEX.trim_start_matches("0x"), 16).unwrap(),
    };

    {
        let executor = RethExecutor::new(reth_config()).unwrap();
        let mut sealer = SealerState::new(
            executor,
            rome_zk_sequencer::log::LogWriter::open(log_dir.path(), 1_000).unwrap(),
            sequencer_key,
            ChannelSink::new(16),
            CHAIN_ID,
            DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            SUB_BLOCKS_PER_BLOCK,
            rome_zk_sequencer::sealer::ResumePoint::default(),
        )
        // This test is about tail-replay/persistence mechanics, not idle behaviour — a
        // nonzero interval (the minimum legal one at this profile's 1s block time) keeps every
        // sub-block sealing exactly as it did before this knob existed.
        .with_empty_block_interval_secs(1);
        // 2 full blocks (40 sub-blocks) — both sealed and persisted to the SAME reth datadir.
        for i in 0..(2 * SUB_BLOCKS_PER_BLOCK as u64) {
            sealer
                .seal_sub_block(vec![], 1_757_000_000_000_000 + i * 50_000, unbounded())
                .await
                .unwrap();
        }
        // `seal_block`'s MDBX write now runs in the background (joined at the
        // next `open_block`, or here via `flush` on a graceful shutdown — see that method's doc) — this
        // test's own intent is the log/database divergence check below, which needs the SECOND block's
        // write durably landed before this scope ends and a fresh `RethExecutor` reopens the SAME
        // datadir; an in-process drop without this flush would race the still-running background write
        // (unlike a real process kill, which takes the write's own thread down with it — see
        // `rome-zk-sequencer`'s `kill_9_right_after_a_block_seals_then_restart_reaches_the_same_state`
        // for that scenario).
        sealer.executor.flush().await.unwrap();
    }

    // Truncate the log's (only) segment file so it no longer contains block 2's records at all —
    // simulating a log that lost records the database already committed (e.g. an operator restoring
    // a stale log backup next to a live datadir). (A fresh chain's first sealed block is design 1,
    // so the two full blocks sealed above are design blocks 1 and 2.)
    let entries: Vec<_> = std::fs::read_dir(log_dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(entries.len(), 1, "expected a single segment file");
    let segment_path = entries[0].path();
    let full_len = std::fs::metadata(&segment_path).unwrap().len();
    // Truncate to slightly under half — well within block 1's own 20 records, so the file stays
    // well-formed (each remaining record's frame is still intact) but block 2's closing record is
    // gone entirely.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&segment_path)
        .unwrap()
        .set_len(full_len / 4)
        .unwrap();

    let reopened = RethExecutor::new(reth_config()).unwrap();
    assert_eq!(reopened.last_persisted_block(), Some(2));

    let mut executor = reopened;
    let err = replay_into_executor(
        log_dir.path(),
        &mut executor,
        sequencer_address,
        // truncate_torn: the shortened file's tail record is itself torn (cut mid-frame) — allow
        // truncating that so this test isolates the LogShorterThanPersistedHead check, not the
        // (already-covered) torn-tail-without-the-flag path.
        true,
        DEFAULT_BLOCK_GAS_LIMIT,
        Address::ZERO,
        SUB_BLOCKS_PER_BLOCK,
    )
    .await
    .unwrap_err();

    assert!(
        matches!(
            err,
            RecoveryError::LogShorterThanPersistedHead {
                persisted_block: 2,
                ..
            }
        ),
        "expected LogShorterThanPersistedHead{{persisted_block: 2, ..}}, got {err:?}"
    );
}

/// Seals four blocks on a real `RethExecutor` the way the sealer will once it reads the deposit queue: blocks 2 and 4
/// credit deposits (queue indices 0..3 and 3..4), the others none. Each block's records go to the ordered log, and
/// the index-0 record of a block that credits deposits carries that block's withdrawals and its `deposits_end`.
/// Returns the executor's head after block 4.
async fn seal_four_blocks_into_a_log(
    reth_dir: &std::path::Path,
    log_dir: &std::path::Path,
    signer: &PrivateKeySigner,
    sequencer_key: &PrivateKeySigner,
    withdrawals_for: impl Fn(u64) -> Vec<alloy_eips::eip4895::Withdrawal>,
) -> Head {
    use rome_zk_sequencer::header::SubBlockHeader;
    use rome_zk_sequencer::signing::sign_header;

    let genesis_path = write_genesis(reth_dir, signer.address());
    let mut ex = RethExecutor::new(RethConfig {
        datadir: reth_dir.join("db"),
        genesis_path,
        block_gas_limit: u64::from_str_radix(GAS_LIMIT_HEX.trim_start_matches("0x"), 16).unwrap(),
    })
    .unwrap();
    let mut writer = rome_zk_sequencer::log::LogWriter::open(log_dir, 1_000).unwrap();
    let mut prev_hash = alloy::primitives::B256::ZERO;
    let mut nonce = 0u64;
    for number in 1..=4u64 {
        let first_ts_us = (1_757_000_000 + number) * 1_000_000;
        let withdrawals = withdrawals_for(number);
        let env = BlockEnv {
            number,
            timestamp_secs: first_ts_us / 1_000_000,
            gas_limit: DEFAULT_BLOCK_GAS_LIMIT,
            coinbase: Address::ZERO,
            prev_randao: rome_zk_executor_api::prev_randao(CHAIN_ID, number),
            base_fee: None,
            withdrawals: withdrawals.clone(),
        };
        ex.open_block(env.clone()).await.unwrap();
        let mut hashes = Vec::new();
        let mut gas_in_block = 0u64;
        for index in 0..SUB_BLOCKS_PER_BLOCK {
            let txs = if index == 0 {
                nonce += 1;
                vec![signed_raw_tx(signer, CHAIN_ID, nonce - 1)]
            } else {
                vec![]
            };
            let outcome = ex.execute_sub_block(&txs, unbounded()).await.unwrap();
            let credited = index == 0 && !withdrawals.is_empty();
            let header = SubBlockHeader {
                chain_id: CHAIN_ID,
                block: number,
                index,
                timestamp_us: first_ts_us + index as u64 * 50_000,
                tx_root: rome_zk_sequencer::merkle::root(&outcome.included),
                receipts_root: outcome.receipts_root,
                gas_used: outcome.gas_used,
                prev_hash,
                deposits_end: credited.then(|| withdrawals.last().unwrap().index + 1),
            };
            let signature = sign_header(sequencer_key, &header);
            let list: &[alloy_eips::eip4895::Withdrawal] =
                if credited { &withdrawals } else { &[] };
            writer
                .append_with_withdrawals(&header, &signature, &txs, list)
                .unwrap();
            prev_hash = header.hash();
            hashes.push(prev_hash);
            gas_in_block += outcome.gas_used;
        }
        ex.seal_block(BlockSealInputs {
            block: number,
            timestamp_secs: env.timestamp_secs,
            sub_block_header_hashes: hashes,
            total_gas_used: gas_in_block,
        })
        .await
        .unwrap();
    }
    ex.head()
}

/// Recovery replays the logged withdrawals: a fresh executor replaying a log whose blocks credit deposits reaches the
/// very head the live run reached (the credits are in the state root and so in the block hash), and the same chain
/// without the withdrawals has a different head, so the equality is not an accident of nothing being credited.
#[tokio::test(flavor = "multi_thread")]
async fn recovery_replays_the_logged_withdrawals_and_reproduces_the_head() {
    use rome_zk_executor_api::deposit_withdrawal;
    let signer = PrivateKeySigner::random();
    let sequencer_key = PrivateKeySigner::random();
    let withdrawals_for = |number: u64| match number {
        2 => (0..3u64)
            .map(|i| deposit_withdrawal(i, Address::repeat_byte(0xD0 + i as u8), 1_000 * (i + 1)))
            .collect(),
        4 => vec![deposit_withdrawal(3, Address::repeat_byte(0xD3), 77)],
        _ => vec![],
    };

    let live_log = tempdir().unwrap();
    let live_reth = tempdir().unwrap();
    let live_head = seal_four_blocks_into_a_log(
        live_reth.path(),
        live_log.path(),
        &signer,
        &sequencer_key,
        withdrawals_for,
    )
    .await;

    // The same four blocks with no deposits: a different chain.
    let plain_log = tempdir().unwrap();
    let plain_reth = tempdir().unwrap();
    let plain_head = seal_four_blocks_into_a_log(
        plain_reth.path(),
        plain_log.path(),
        &signer,
        &sequencer_key,
        |_| vec![],
    )
    .await;
    assert_ne!(live_head.state_root, plain_head.state_root);
    assert_ne!(live_head.block_hash, plain_head.block_hash);

    // A fresh executor replays the log with withdrawals.
    let fresh_reth = tempdir().unwrap();
    let genesis_path = write_genesis(fresh_reth.path(), signer.address());
    let mut fresh = RethExecutor::new(RethConfig {
        datadir: fresh_reth.path().join("db"),
        genesis_path,
        block_gas_limit: u64::from_str_radix(GAS_LIMIT_HEX.trim_start_matches("0x"), 16).unwrap(),
    })
    .unwrap();
    replay_into_executor(
        live_log.path(),
        &mut fresh,
        sequencer_key.address(),
        false,
        DEFAULT_BLOCK_GAS_LIMIT,
        Address::ZERO,
        SUB_BLOCKS_PER_BLOCK,
    )
    .await
    .unwrap();
    assert_eq!(
        fresh.head(),
        live_head,
        "replay must credit the logged withdrawals and reach the live head"
    );
}
