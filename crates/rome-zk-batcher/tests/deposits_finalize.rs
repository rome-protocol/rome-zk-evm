//! `finalize_and_verify` and the deposit range: what it sends, what it refuses before sending, and what it
//! checks after the batch is final.
//!
//! The chain here is an in-memory fake: a map of accounts and a sender that applies a completing
//! `FinalizeBatchV2` the way the inbox does (range into the header, `acc` over it, the cursor moved on, a v1
//! cursor grown to 69 bytes) and applies a plain transfer. The real program runs in
//! `restart_mid_batch_keeps_settlement_live.rs`; here the point is the batcher's own decisions, which a fake
//! can observe exactly: the instructions it sends, and in what order.

use alloy_primitives::Bytes;
use rome_zk_batcher::channel::{cut_frames, encode_stream, Block, Frame};
use rome_zk_batcher::metrics::Metrics;
use rome_zk_batcher::pipeline::{
    finalize_and_verify, verify_acc, BatchTarget, FinalizePoll, PipelineError,
};
use rome_zk_batcher::resolve::{AccountOps, ResolveError};
use rome_zk_batcher::sender::{SendTuning, Sender, SenderError};
use rome_zk_layouts::batch::{
    self as batch_layout, account_len_for, header_len, leaves_offset_for, write_header_v3,
    BatchDeposit, BatchFields,
};
use rome_zk_layouts::cursor;
use rome_zk_layouts::exit::exit_config as xc;
use solana_program::{instruction::Instruction, keccak, pubkey::Pubkey};
use solana_signature::Signature;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

const CHAIN_ID: u64 = 7;
const BATCH: u64 = 3;
const OPEN_SLOT: u64 = 42;

fn system_id() -> Pubkey {
    solana_sdk_ids::system_program::id()
}

struct Stored {
    data: Vec<u8>,
    lamports: u64,
    owner: Pubkey,
}

/// The accounts, plus a record of what was applied.
#[derive(Default)]
struct MemChain {
    accounts: Mutex<HashMap<Pubkey, Stored>>,
    /// Every instruction sent, in order.
    sent: Mutex<Vec<Instruction>>,
    /// When set, a completing finalize writes this range into the header instead of the one it was sent.
    header_range_override: Mutex<Option<(u64, u64)>>,
}

impl MemChain {
    /// An account owned by the system program, which is what a plain transfer to a fresh address leaves.
    fn put(&self, key: Pubkey, data: Vec<u8>, lamports: u64) {
        self.put_owned(key, data, lamports, system_id());
    }

    fn put_owned(&self, key: Pubkey, data: Vec<u8>, lamports: u64, owner: Pubkey) {
        self.accounts.lock().unwrap().insert(
            key,
            Stored {
                data,
                lamports,
                owner,
            },
        );
    }

    fn data(&self, key: &Pubkey) -> Option<Vec<u8>> {
        self.accounts
            .lock()
            .unwrap()
            .get(key)
            .map(|a| a.data.clone())
    }

    fn lamports(&self, key: &Pubkey) -> u64 {
        self.accounts.lock().unwrap()[key].lamports
    }

    fn sent(&self) -> Vec<Instruction> {
        self.sent.lock().unwrap().clone()
    }

    fn transfers(&self) -> Vec<Instruction> {
        self.sent()
            .into_iter()
            .filter(|ix| ix.program_id == system_id())
            .collect()
    }

    fn finalizes(&self, inbox: &Pubkey) -> Vec<Instruction> {
        self.sent()
            .into_iter()
            .filter(|ix| ix.program_id == *inbox)
            .collect()
    }
}

impl AccountOps for MemChain {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Vec<u8>>, ResolveError> {
        Ok(self.data(pubkey))
    }

    async fn get_account_owner(&self, pubkey: &Pubkey) -> Result<Option<Pubkey>, ResolveError> {
        Ok(self.accounts.lock().unwrap().get(pubkey).map(|a| a.owner))
    }

    async fn accounts_exist(&self, pubkeys: &[Pubkey]) -> Result<Vec<bool>, ResolveError> {
        Ok(pubkeys.iter().map(|k| self.data(k).is_some()).collect())
    }

    async fn rent_shortfall(
        &self,
        pubkey: &Pubkey,
        data_len: usize,
    ) -> Result<Option<u64>, ResolveError> {
        Ok(self
            .accounts
            .lock()
            .unwrap()
            .get(pubkey)
            .map(|a| rome_zk_testkit::rent_exempt(data_len).saturating_sub(a.lamports)))
    }
}

/// Applies what the inbox would, to the fake chain.
struct MemSender<'a> {
    chain: &'a MemChain,
    inbox: Pubkey,
    settlement: Pubkey,
    frames: Vec<Frame>,
}

impl Sender for MemSender<'_> {
    async fn send_and_confirm(
        &self,
        instructions: &[Instruction],
        _tuning: SendTuning,
    ) -> Result<Signature, SenderError> {
        for ix in instructions {
            self.chain.sent.lock().unwrap().push(ix.clone());
            if ix.program_id == system_id() {
                // `Transfer`: a u32 discriminant of 2, then the lamports.
                assert_eq!(
                    &ix.data[..4],
                    &2u32.to_le_bytes(),
                    "only transfers go to the system program"
                );
                let lamports = u64::from_le_bytes(ix.data[4..12].try_into().unwrap());
                let to = ix.accounts[1].pubkey;
                self.chain
                    .accounts
                    .lock()
                    .unwrap()
                    .get_mut(&to)
                    .unwrap()
                    .lamports += lamports;
            } else if ix.program_id == self.inbox {
                match zk_inbox_client::decode_instruction(&ix.data).unwrap() {
                    zk_inbox_client::InboxIx::FinalizeBatchV2 { deposit_to, .. } => {
                        self.finalize(deposit_to)
                    }
                    other => panic!("unexpected inbox instruction {other:?}"),
                }
            } else {
                panic!("unexpected program {}", ix.program_id);
            }
        }
        Ok(Signature::default())
    }
}

impl MemSender<'_> {
    fn finalize(&self, deposit_to: u64) {
        let cursor_key = zk_inbox_client::cursor_pda(&self.inbox, &self.settlement, CHAIN_ID).0;
        let batch_key =
            zk_inbox_client::batch_pda(&self.inbox, &self.settlement, CHAIN_ID, BATCH).0;
        let cursor_data = self.chain.data(&cursor_key).unwrap();
        let c = zk_inbox_client::decode_batch_cursor(&cursor_data).unwrap();
        if c.deposit.is_none() {
            assert!(
                self.chain.lamports(&cursor_key) >= rome_zk_testkit::rent_exempt(cursor::LEN_V2),
                "a v1 cursor must hold the 69-byte rent minimum before the first FinalizeBatchV2"
            );
        }
        let from = c.deposit.map_or(0, |d| d.next);
        let mut batch = self.chain.data(&batch_key).unwrap();
        let mut fields = batch_layout::read(&batch).unwrap();
        fields.finalized = true;
        let sent_range = (from, deposit_to);
        let (hf, ht) = self
            .chain
            .header_range_override
            .lock()
            .unwrap()
            .unwrap_or(sent_range);
        if fields.deposit.is_some() {
            fields.deposit = Some(BatchDeposit {
                from: hf,
                to: ht,
                hash_from: [0x0a; 32],
                hash_to: [0x0b; 32],
            });
        }
        let chunk_hashes: Vec<[u8; 32]> = self
            .frames
            .iter()
            .map(|f| keccak::hashv(&[&f.to_bytes()]).to_bytes())
            .collect();
        let range = fields.deposit.unwrap_or(BatchDeposit {
            from: 0,
            to: 0,
            hash_from: [0; 32],
            hash_to: [0; 32],
        });
        let (_, _, acc) = zk_inbox_client::reference_commitment_with_deposits(
            CHAIN_ID,
            BATCH,
            OPEN_SLOT,
            &chunk_hashes,
            &range,
        );
        fields.acc = acc;
        let head = if fields.deposit.is_some() {
            write_header_v3(&fields).unwrap().to_vec()
        } else {
            batch_layout::write_header(&fields).to_vec()
        };
        batch[..head.len()].copy_from_slice(&head);
        self.chain.put(batch_key, batch, 1);

        // The cursor moves on, and a v1 cursor grows to its 69-byte layout.
        let mut new_cursor = vec![0u8; cursor::LEN_V2];
        new_cursor[..cursor::LEN.min(cursor_data.len())]
            .copy_from_slice(&cursor_data[..cursor::LEN.min(cursor_data.len())]);
        new_cursor[cursor::OFF_VERSION] = cursor::VERSION_V2;
        new_cursor[cursor::OFF_NEXT_BATCH..cursor::OFF_NEXT_BATCH + 8]
            .copy_from_slice(&(BATCH + 1).to_le_bytes());
        new_cursor[cursor::OFF_DEPOSIT_NEXT..cursor::OFF_DEPOSIT_NEXT + 8]
            .copy_from_slice(&deposit_to.to_le_bytes());
        let lamports = self.chain.lamports(&cursor_key);
        self.chain.put(cursor_key, new_cursor, lamports);
    }
}

fn tuning() -> SendTuning {
    SendTuning {
        compute_unit_limit: 600_000,
        loaded_accounts_data_size_limit:
            rome_zk_batcher::config::default_loaded_accounts_data_size_limit(),
        priority_fee_micro_lamports: 1_000,
        max_priority_fee_micro_lamports: 200_000,
        confirm_timeout: Duration::from_secs(5),
        ..Default::default()
    }
}

fn poll(frames: &[Frame]) -> FinalizePoll {
    FinalizePoll {
        expected_count: frames.len() as u32,
        poll_interval: Duration::from_millis(1),
        max_polls: 3,
    }
}

fn block(number: u64, deposits_end: Option<u64>) -> Block {
    Block {
        number,
        timestamp: 1_000 + number,
        gas_limit: 30_000_000,
        txs: vec![Bytes::from(vec![number as u8; 40])],
        deposits_end,
    }
}

/// Three blocks taking deposits `from..from + 3`: one credit in the second block, two in the third.
fn blocks_taking_three(from: u64) -> Vec<Block> {
    vec![
        block(10, None),
        block(11, Some(from + 1)),
        block(12, Some(from + 3)),
    ]
}

fn blocks_without_deposits() -> Vec<Block> {
    vec![block(10, None), block(11, None), block(12, None)]
}

struct World {
    chain: MemChain,
    inbox: Pubkey,
    settlement: Pubkey,
    bridge: Pubkey,
    payer: Pubkey,
}

#[derive(Clone, Copy)]
enum Cursor {
    V1 { lamports: u64 },
    V2 { next: u64 },
}

#[derive(Clone, Copy)]
enum Exit {
    Absent,
    Bridge,
}

fn world(cursor_kind: Cursor, exit: Exit, header_v3: bool, frames: &[Frame]) -> World {
    let w = World {
        chain: MemChain::default(),
        inbox: Pubkey::new_unique(),
        settlement: Pubkey::new_unique(),
        bridge: Pubkey::new_unique(),
        payer: Pubkey::new_unique(),
    };
    let cursor_key = zk_inbox_client::cursor_pda(&w.inbox, &w.settlement, CHAIN_ID).0;
    match cursor_kind {
        Cursor::V1 { lamports } => {
            let acct = rome_zk_testkit::cursor_account_for(1, w.inbox, CHAIN_ID, BATCH);
            w.chain.put(cursor_key, acct.data, lamports);
        }
        Cursor::V2 { next } => {
            let mut acct = rome_zk_testkit::cursor_account_for(2, w.inbox, CHAIN_ID, BATCH);
            acct.data[cursor::OFF_DEPOSIT_NEXT..cursor::OFF_DEPOSIT_NEXT + 8]
                .copy_from_slice(&next.to_le_bytes());
            w.chain.put(cursor_key, acct.data, acct.lamports);
        }
    }
    if let Exit::Bridge = exit {
        let data = xc::write(&xc::ExitConfigFields {
            chain_id: CHAIN_ID,
            exit_portal: [7; 20],
            bridge_program: w.bridge.to_bytes(),
            pending_exit_portal: [0; 20],
            pending_bridge_program: [0; 32],
            pending_exit_cap: 0,
            pending_poster_bond: 0,
            activation_slot: 0,
            pending_mask: 0,
        });
        w.chain.put_owned(
            zk_inbox_client::exit_config_pda(&w.settlement, CHAIN_ID).0,
            data.to_vec(),
            1,
            w.settlement,
        );
    }
    let n = frames.len() as u32;
    let version = if header_v3 {
        batch_layout::VERSION_V3
    } else {
        batch_layout::VERSION
    };
    let fields = BatchFields {
        chain_id: CHAIN_ID,
        batch: BATCH,
        open_slot: OPEN_SLOT,
        expected_count: n,
        leaves_present: n,
        finalized: false,
        settlement_program: w.settlement.to_bytes(),
        authority: w.payer.to_bytes(),
        root: [0; 32],
        forced_root: [0; 32],
        acc: [0; 32],
        finalize_cursor: 0,
        open_unix_ts: 1_700_000_000,
        deposit: header_v3.then_some(BatchDeposit {
            from: 0,
            to: 0,
            hash_from: [0; 32],
            hash_to: [0; 32],
        }),
    };
    let head: Vec<u8> = if header_v3 {
        write_header_v3(&fields).unwrap().to_vec()
    } else {
        batch_layout::write_header(&fields).to_vec()
    };
    let mut d = vec![0u8; account_len_for(version, n).unwrap()];
    d[..head.len()].copy_from_slice(&head);
    let bitmap = header_len(version).unwrap();
    let leaves = leaves_offset_for(version, n).unwrap();
    for (i, f) in frames.iter().enumerate() {
        d[bitmap + i / 8] |= 1 << (i % 8);
        d[leaves + 32 * i..leaves + 32 * (i + 1)]
            .copy_from_slice(&keccak::hashv(&[&f.to_bytes()]).to_bytes());
    }
    w.chain.put(
        zk_inbox_client::batch_pda(&w.inbox, &w.settlement, CHAIN_ID, BATCH).0,
        d,
        1,
    );
    w
}

impl World {
    fn target(&self) -> BatchTarget {
        BatchTarget {
            program_id: self.inbox,
            settlement_program: self.settlement,
            payer: self.payer,
            chain_id: CHAIN_ID,
            batch: BATCH,
        }
    }

    fn sender(&self, frames: &[Frame]) -> MemSender<'_> {
        MemSender {
            chain: &self.chain,
            inbox: self.inbox,
            settlement: self.settlement,
            frames: frames.to_vec(),
        }
    }

    async fn finalize(
        &self,
        frames: &[Frame],
        expected: Option<&[Block]>,
    ) -> Result<zk_inbox_client::BatchAccount, PipelineError> {
        finalize_and_verify(
            &self.sender(frames),
            &self.chain,
            &Metrics::new(),
            self.target(),
            tuning(),
            poll(frames),
            frames,
            expected,
        )
        .await
    }

    fn expected_finalize_ix(&self, deposit_to: u64, bridge: Option<&Pubkey>) -> Instruction {
        zk_inbox_client::finalize_batch_v2_ix(
            &self.inbox,
            &self.payer,
            &self.settlement,
            CHAIN_ID,
            BATCH,
            0,
            deposit_to,
            bridge,
        )
    }
}

fn frames_of(blocks: &[Block]) -> Vec<Frame> {
    cut_frames(CHAIN_ID, BATCH, &encode_stream(blocks), 60)
}

#[tokio::test]
async fn a_batch_taking_deposits_finalizes_with_the_range_its_own_stream_carries() {
    let blocks = blocks_taking_three(5);
    let frames = frames_of(&blocks);
    let w = world(Cursor::V2 { next: 5 }, Exit::Bridge, true, &frames);
    let decoded = w.finalize(&frames, Some(&blocks)).await.unwrap();
    let range = decoded.deposit.expect("a v3 header");
    assert_eq!((range.from, range.to), (5, 8));
    verify_acc(&decoded, &frames).unwrap();
    let sent = w.chain.finalizes(&w.inbox);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0],
        w.expected_finalize_ix(8, Some(&w.bridge)),
        "the end comes from the posted stream, from the cursor's deposit_next"
    );
}

#[tokio::test]
async fn the_bridge_program_comes_from_the_exit_config() {
    let blocks = blocks_taking_three(0);
    let frames = frames_of(&blocks);
    let w = world(Cursor::V2 { next: 0 }, Exit::Bridge, true, &frames);
    w.finalize(&frames, Some(&blocks)).await.unwrap();
    let sent = w.chain.finalizes(&w.inbox);
    assert_eq!(sent[0], w.expected_finalize_ix(3, Some(&w.bridge)));
    assert_ne!(
        sent[0],
        w.expected_finalize_ix(3, None),
        "the queue and the record accounts are named under the bridge"
    );
}

#[tokio::test]
async fn a_chain_with_no_exit_config_finalizes_an_empty_range() {
    let blocks = blocks_without_deposits();
    let frames = frames_of(&blocks);
    let w = world(Cursor::V2 { next: 4 }, Exit::Absent, true, &frames);
    let decoded = w.finalize(&frames, Some(&blocks)).await.unwrap();
    let range = decoded.deposit.unwrap();
    assert_eq!((range.from, range.to), (4, 4));
    assert_eq!(
        w.chain.finalizes(&w.inbox),
        vec![w.expected_finalize_ix(4, None)]
    );
}

#[tokio::test]
async fn a_stream_with_deposits_on_a_chain_without_a_bridge_is_refused_before_any_send() {
    let blocks = blocks_taking_three(0);
    let frames = frames_of(&blocks);
    let w = world(Cursor::V2 { next: 0 }, Exit::Absent, true, &frames);
    let err = w.finalize(&frames, Some(&blocks)).await.unwrap_err();
    assert!(matches!(err, PipelineError::Deposits(_)), "{err:?}");
    assert!(w.chain.sent().is_empty());
}

#[tokio::test]
async fn a_posted_stream_that_differs_from_the_blocks_is_refused_before_any_send() {
    let posted = blocks_taking_three(0);
    let frames = frames_of(&posted);
    // The blocks the batcher means to post carry a different cursor in the last block.
    let mut meant = posted.clone();
    meant[2].deposits_end = Some(2);
    let w = world(Cursor::V2 { next: 0 }, Exit::Bridge, true, &frames);
    let err = w.finalize(&frames, Some(&meant)).await.unwrap_err();
    assert!(matches!(err, PipelineError::Rederive(_)), "{err:?}");
    assert!(
        w.chain.sent().is_empty(),
        "nothing was sent: {:?}",
        w.chain.sent()
    );
}

#[tokio::test]
async fn a_stream_whose_first_credit_is_not_the_cursors_next_index_is_refused_before_any_send() {
    // The stream says deposits 0..3 but the live cursor has moved to 5: the first block's end (1) is not
    // above the cursor.
    let blocks = blocks_taking_three(0);
    let frames = frames_of(&blocks);
    let w = world(Cursor::V2 { next: 5 }, Exit::Bridge, true, &frames);
    let err = w.finalize(&frames, Some(&blocks)).await.unwrap_err();
    assert!(matches!(err, PipelineError::Channel(_)), "{err:?}");
    assert!(w.chain.sent().is_empty());
}

#[tokio::test]
async fn bytes_that_do_not_decode_as_a_stream_are_refused_before_any_send() {
    let frames = vec![Frame {
        channel_id: [0; 16],
        frame_no: 0,
        is_last: true,
        body: vec![1, 2, 3],
    }];
    let w = world(Cursor::V2 { next: 0 }, Exit::Bridge, true, &frames);
    let err = w.finalize(&frames, None).await.unwrap_err();
    assert!(matches!(err, PipelineError::Channel(_)), "{err:?}");
    assert!(w.chain.sent().is_empty());
}

#[tokio::test]
async fn a_header_range_other_than_the_one_sent_is_an_error() {
    let blocks = blocks_taking_three(0);
    let frames = frames_of(&blocks);
    let w = world(Cursor::V2 { next: 0 }, Exit::Bridge, true, &frames);
    *w.chain.header_range_override.lock().unwrap() = Some((0, 2));
    let err = w.finalize(&frames, Some(&blocks)).await.unwrap_err();
    assert!(matches!(err, PipelineError::Deposits(_)), "{err:?}");
}

#[tokio::test]
async fn verify_acc_refuses_a_header_range_the_posted_stream_does_not_carry() {
    let blocks = blocks_taking_three(0);
    let frames = frames_of(&blocks);
    let w = world(Cursor::V2 { next: 0 }, Exit::Bridge, true, &frames);
    // The fake writes a header ending at 2 with an `acc` computed over that header, so the acc check passes
    // and only the stream check can catch it.
    *w.chain.header_range_override.lock().unwrap() = Some((0, 2));
    let _ = w.finalize(&frames, None).await;
    let data = w
        .chain
        .data(&zk_inbox_client::batch_pda(&w.inbox, &w.settlement, CHAIN_ID, BATCH).0)
        .unwrap();
    let decoded = zk_inbox_client::decode_batch_account(&data).unwrap();
    let err = verify_acc(&decoded, &frames).unwrap_err();
    assert!(matches!(err, PipelineError::Deposits(_)), "{err:?}");
}

#[tokio::test]
async fn a_v1_cursor_is_topped_up_once_then_the_batch_finalizes() {
    let blocks = blocks_without_deposits();
    let frames = frames_of(&blocks);
    // A v1 cursor with the lamports of its own 21 bytes only.
    let w = world(
        Cursor::V1 {
            lamports: rome_zk_testkit::rent_exempt(cursor::LEN),
        },
        Exit::Absent,
        true,
        &frames,
    );
    let cursor_key = zk_inbox_client::cursor_pda(&w.inbox, &w.settlement, CHAIN_ID).0;
    w.finalize(&frames, Some(&blocks)).await.unwrap();
    let transfers = w.chain.transfers();
    assert_eq!(transfers.len(), 1, "one plain transfer");
    assert_eq!(
        transfers[0],
        solana_system_interface::instruction::transfer(
            &w.payer,
            &cursor_key,
            rome_zk_testkit::rent_exempt(cursor::LEN_V2)
                - rome_zk_testkit::rent_exempt(cursor::LEN),
        )
    );
    let all = w.chain.sent();
    assert_eq!(all[0].program_id, system_id(), "the top-up goes first");
    assert_eq!(all.len(), 2);
    assert_eq!(all[1].program_id, w.inbox);
    assert_eq!(
        w.chain.lamports(&cursor_key),
        rome_zk_testkit::rent_exempt(cursor::LEN_V2)
    );
}

#[tokio::test]
async fn a_second_run_over_a_topped_up_cursor_sends_no_second_transfer() {
    let blocks = blocks_without_deposits();
    let frames = frames_of(&blocks);
    let w = world(
        Cursor::V1 {
            lamports: rome_zk_testkit::rent_exempt(cursor::LEN),
        },
        Exit::Absent,
        true,
        &frames,
    );
    // A run that died after the transfer and before the finalize: the cursor already holds the minimum.
    let cursor_key = zk_inbox_client::cursor_pda(&w.inbox, &w.settlement, CHAIN_ID).0;
    let data = w.chain.data(&cursor_key).unwrap();
    w.chain.put(
        cursor_key,
        data,
        rome_zk_testkit::rent_exempt(cursor::LEN_V2),
    );
    w.finalize(&frames, Some(&blocks)).await.unwrap();
    assert!(w.chain.transfers().is_empty());
}

#[tokio::test]
async fn a_v2_cursor_is_never_topped_up() {
    let blocks = blocks_taking_three(2);
    let frames = frames_of(&blocks);
    let w = world(Cursor::V2 { next: 2 }, Exit::Bridge, true, &frames);
    w.finalize(&frames, Some(&blocks)).await.unwrap();
    assert!(w.chain.transfers().is_empty());
}

#[tokio::test]
async fn a_batch_opened_with_a_v2_header_finalizes_an_empty_range_and_keeps_its_header() {
    let blocks = blocks_without_deposits();
    let frames = frames_of(&blocks);
    let w = world(Cursor::V2 { next: 6 }, Exit::Bridge, false, &frames);
    let decoded = w.finalize(&frames, Some(&blocks)).await.unwrap();
    assert!(decoded.deposit.is_none());
    verify_acc(&decoded, &frames).unwrap();
}

#[tokio::test]
async fn an_already_finalized_batch_is_checked_against_its_header_and_sends_nothing() {
    let blocks = blocks_taking_three(0);
    let frames = frames_of(&blocks);
    let w = world(Cursor::V2 { next: 0 }, Exit::Bridge, true, &frames);
    w.finalize(&frames, Some(&blocks)).await.unwrap();
    let sent_before = w.chain.sent().len();
    // The cursor has moved past the batch now; a second pass (a restart) must not re-plan from it.
    let decoded = w.finalize(&frames, Some(&blocks)).await.unwrap();
    assert_eq!(w.chain.sent().len(), sent_before);
    verify_acc(&decoded, &frames).unwrap();
}

/// The lamports of a plain transfer that leaves an empty account: what anyone can send to any address.
const DUST: u64 = 890_880;

/// Funds an empty, system-owned account at `key`, the way a plain transfer would.
fn fund_empty(w: &World, key: Pubkey) {
    w.chain.put(key, Vec::new(), DUST);
}

#[tokio::test]
async fn an_empty_system_owned_account_at_the_exit_config_address_does_not_stop_the_finalize() {
    // The program counts an exit config as absent unless the settlement program owns it, and accepts the
    // empty range. A third party can fund that address on a chain with no exit config.
    let blocks = blocks_without_deposits();
    let frames = frames_of(&blocks);
    let w = world(Cursor::V2 { next: 4 }, Exit::Absent, true, &frames);
    fund_empty(
        &w,
        zk_inbox_client::exit_config_pda(&w.settlement, CHAIN_ID).0,
    );
    let decoded = w.finalize(&frames, Some(&blocks)).await.unwrap();
    let range = decoded.deposit.unwrap();
    assert_eq!((range.from, range.to), (4, 4));
    assert_eq!(
        w.chain.finalizes(&w.inbox),
        vec![w.expected_finalize_ix(4, None)]
    );
}

#[tokio::test]
async fn an_exit_config_the_settlement_program_does_not_own_names_no_bridge() {
    // Well-formed bytes, wrong owner: the program ignores it, so the bridge is not read from it.
    let blocks = blocks_without_deposits();
    let frames = frames_of(&blocks);
    let w = world(Cursor::V2 { next: 4 }, Exit::Bridge, true, &frames);
    let key = zk_inbox_client::exit_config_pda(&w.settlement, CHAIN_ID).0;
    let data = w.chain.data(&key).unwrap();
    w.chain.put(key, data, DUST);
    w.finalize(&frames, Some(&blocks)).await.unwrap();
    assert_eq!(
        w.chain.finalizes(&w.inbox),
        vec![w.expected_finalize_ix(4, None)]
    );
}

#[tokio::test]
async fn an_empty_system_owned_account_at_the_queue_address_does_not_stop_the_finalize() {
    // A named bridge, and a funded empty account where its queue would be: the program counts the queue as
    // absent unless the bridge owns it, and accepts the empty range.
    let blocks = blocks_without_deposits();
    let frames = frames_of(&blocks);
    let w = world(Cursor::V2 { next: 4 }, Exit::Bridge, true, &frames);
    fund_empty(
        &w,
        rome_zk_layouts::deposit_queue::deposit_queue::pda(
            &w.bridge,
            &w.settlement.to_bytes(),
            CHAIN_ID,
        )
        .0,
    );
    let decoded = w.finalize(&frames, Some(&blocks)).await.unwrap();
    let range = decoded.deposit.unwrap();
    assert_eq!((range.from, range.to), (4, 4));
    assert_eq!(
        w.chain.finalizes(&w.inbox),
        vec![w.expected_finalize_ix(4, Some(&w.bridge))]
    );
}
