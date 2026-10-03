//! The startup recovery reads a long pending window in pages, never one account at a time.
//!
//! While no batch has settled, `head_final_batch` stays 0 and the window the startup recovery probes can
//! span every batch the chain has ever opened. Reading it one `get_account` per id would cost one round
//! trip each; the recovery must read it with paged `getMultipleAccounts` calls (100 ids a page).
//!
//! `pipeline::startup_recover` is driven here exactly as the binary drives it, over a scripted chain whose
//! batch ids are only Finalized or Missing (nothing open, so nothing is sent), and the test records every
//! read the recovery makes in order.

use rome_zk_batcher::metrics::Metrics;
use rome_zk_batcher::pipeline::{self, StartupRecover, WindowConfig};
use rome_zk_batcher::resolve::{AccountOps, ResolveError};
use rome_zk_batcher::sender::{SendTuning, Sender, SenderError};
use rome_zk_batcher::sink::ChannelPostRootSink;
use solana_program::{instruction::Instruction, pubkey::Pubkey};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tempfile::tempdir;

const INBOX: Pubkey = Pubkey::new_from_array([9u8; 32]);
const SETTLEMENT: Pubkey = Pubkey::new_from_array([7u8; 32]);
const CHAIN_ID: u64 = 200_198;
const HEAD_FINAL_BATCH: u64 = 1_000;
const WINDOW_LEN: u64 = 250;
const NEXT_BATCH: u64 = HEAD_FINAL_BATCH + WINDOW_LEN;

#[derive(Debug, PartialEq, Eq)]
enum Read {
    /// One `get_account` on a batch account.
    Single(Pubkey),
    /// One `get_multiple_account_data` call, with the number of ids it carried.
    Page(usize),
}

/// A chain scripted as raw account bytes, recording every batch-account read in the order it happens.
/// The cursor and the settlement root are singleton reads and are not recorded.
struct ScriptedChain {
    accounts: HashMap<Pubkey, Vec<u8>>,
    reads: Mutex<Vec<Read>>,
}

fn batch_header(expected_count: u32, finalized: bool) -> Vec<u8> {
    use rome_zk_layouts::batch as b;
    let mut d = vec![0u8; b::account_len(expected_count)];
    d[0..4].copy_from_slice(&b::MAGIC.to_le_bytes());
    d[b::OFF_VERSION] = b::VERSION;
    d[b::OFF_EXPECTED_COUNT..b::OFF_EXPECTED_COUNT + 4]
        .copy_from_slice(&expected_count.to_le_bytes());
    d[b::OFF_FINALIZED] = finalized as u8;
    d
}

fn cursor_bytes(next_batch: u64) -> Vec<u8> {
    use rome_zk_layouts::cursor as c;
    let mut d = vec![0u8; c::LEN];
    d[c::OFF_MAGIC..c::OFF_MAGIC + 4].copy_from_slice(&c::MAGIC.to_le_bytes());
    d[c::OFF_VERSION] = c::VERSION;
    d[c::OFF_NEXT_BATCH..c::OFF_NEXT_BATCH + 8].copy_from_slice(&next_batch.to_le_bytes());
    d
}

fn root_bytes(head_final_batch: u64) -> Vec<u8> {
    use rome_zk_layouts::root as r;
    let mut d = vec![0u8; r::MIN_LEN];
    d[r::OFF_MAGIC..r::OFF_MAGIC + 4].copy_from_slice(&r::MAGIC.to_le_bytes());
    d[r::OFF_HEAD_FINAL_BATCH..r::OFF_HEAD_FINAL_BATCH + 8]
        .copy_from_slice(&head_final_batch.to_le_bytes());
    d
}

impl AccountOps for ScriptedChain {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Vec<u8>>, ResolveError> {
        let (cursor, _) = zk_inbox_client::cursor_pda(&INBOX, &SETTLEMENT, CHAIN_ID);
        let (root, _) = zk_settlement_client::root_pda(&SETTLEMENT, CHAIN_ID);
        if *pubkey != cursor && *pubkey != root {
            self.reads.lock().unwrap().push(Read::Single(*pubkey));
        }
        Ok(self.accounts.get(pubkey).cloned())
    }

    async fn accounts_exist(&self, pubkeys: &[Pubkey]) -> Result<Vec<bool>, ResolveError> {
        Ok(pubkeys
            .iter()
            .map(|p| self.accounts.contains_key(p))
            .collect())
    }

    async fn get_multiple_account_data(
        &self,
        pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Vec<u8>>>, ResolveError> {
        self.reads.lock().unwrap().push(Read::Page(pubkeys.len()));
        Ok(pubkeys
            .iter()
            .map(|p| self.accounts.get(p).cloned())
            .collect())
    }
}

/// Fails the test if the recovery sends anything: with no open batch there is nothing to finish.
struct NoSend;

impl Sender for NoSend {
    async fn send_and_confirm(
        &self,
        instructions: &[Instruction],
        _tuning: SendTuning,
    ) -> Result<solana_signature::Signature, SenderError> {
        panic!("nothing is open, so nothing may be sent: {instructions:?}");
    }
}

fn window() -> WindowConfig {
    let tuning = SendTuning {
        compute_unit_limit: 200_000,
        loaded_accounts_data_size_limit: 131_072,
        priority_fee_micro_lamports: 1_000,
        max_priority_fee_micro_lamports: 200_000,
        confirm_timeout: Duration::from_secs(5),
        ..Default::default()
    };
    WindowConfig {
        inbox_program_id: INBOX,
        settlement_program_id: SETTLEMENT,
        payer: Pubkey::new_unique(),
        chain_id: CHAIN_ID,
        max_frame_body_len: 2_000,
        chunk_tuning: tuning,
        open_tuning: tuning,
        chunk_retry_compute_unit_limit: 1_400_000,
        finalize_tuning: tuning,
        finalize_poll_interval: Duration::from_millis(1),
        finalize_max_polls: 10,
        in_flight_frames: 8,
        confirm_poll_interval: Duration::from_millis(1),
        signature_status_batch_size: 32,
        batches_in_flight: 2,
        cu_sample: None,
        cu_sample_every: 1,
    }
}

/// 250 ids sit between `head_final_batch` and the cursor: the oldest is Finalized (its chunk was recycled,
/// so the anchor walk that follows falls back to the settlement root), every other id is Missing. The
/// recovery's own sweep must read them as three pages (100, 100, 50) and make no single read before them.
#[tokio::test]
async fn a_250_id_window_is_read_in_3_pages_never_one_account_at_a_time() {
    let (cursor, _) = zk_inbox_client::cursor_pda(&INBOX, &SETTLEMENT, CHAIN_ID);
    let (root, _) = zk_settlement_client::root_pda(&SETTLEMENT, CHAIN_ID);
    let (finalized_pda, _) =
        zk_inbox_client::batch_pda(&INBOX, &SETTLEMENT, CHAIN_ID, HEAD_FINAL_BATCH);
    let mut accounts = HashMap::new();
    accounts.insert(cursor, cursor_bytes(NEXT_BATCH));
    accounts.insert(root, root_bytes(HEAD_FINAL_BATCH));
    accounts.insert(finalized_pda, batch_header(1, true));
    let chain = ScriptedChain {
        accounts,
        reads: Mutex::new(Vec::new()),
    };

    let w = window();
    let log_dir = tempdir().unwrap();
    let cfg = StartupRecover {
        window: &w,
        log_dir: log_dir.path(),
        sub_blocks_per_block: 20,
        block_gas_limit: 100_000_000,
        blocks_per_batch: 10,
    };
    let (sink, _rx) = ChannelPostRootSink::new();
    let anchor = pipeline::startup_recover(&chain, &NoSend, &Metrics::new(), &sink, &cfg)
        .await
        .expect("a window with nothing open recovers");
    assert_eq!(anchor.from_block, 1);

    let reads = chain.reads.lock().unwrap();
    assert_eq!(
        reads[..3],
        [Read::Page(100), Read::Page(100), Read::Page(50)],
        "250 ids at 100 a page must be read as exactly 3 pages first: {reads:?}"
    );
    let singles_during_the_sweep = reads[..3]
        .iter()
        .filter(|r| matches!(r, Read::Single(_)))
        .count();
    assert_eq!(singles_during_the_sweep, 0, "no batch read one at a time");
    // After the sweep the anchor walk reads only the one finalized batch it must decode, by itself.
    let later_singles: Vec<_> = reads[3..]
        .iter()
        .filter(|r| matches!(r, Read::Single(_)))
        .collect();
    assert_eq!(
        later_singles,
        vec![&Read::Single(finalized_pda)],
        "only the finalized batch is ever read singly"
    );
}
