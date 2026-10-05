//! What the batcher reads about deposits at startup, and how the binary uses it: the cursor's `deposit_next`
//! goes to the log source and the grouper's first range, and the queue's per-batch limit caps the grouper.
//!
//! The chain is an in-memory fake that records each account's owner, because the program counts the exit
//! config as absent unless the settlement program owns it and the queue as absent unless the bridge owns it,
//! and the batcher has to read them the same way. The log is a real ordered log, read by the real
//! `BlockSource` and grouped by the real `SizeCappedGrouper`, wired the way `run_once` and `run_follow` wire
//! them from the `DepositStart` this file reads off the fake chain.

use alloy::primitives::{Address, Bytes, B256};
use alloy::signers::local::PrivateKeySigner;
use rome_zk_batcher::channel::{Block, DEFAULT_MAX_FRAME_BODY_LEN};
use rome_zk_batcher::deposits::{read_deposit_start, DepositStart};
use rome_zk_batcher::grouping::{CloseReason, DepositCap, PushOutcome, SizeCappedGrouper};
use rome_zk_batcher::resolve::{AccountOps, ResolveError};
use rome_zk_batcher::source::{BlockSource, SourceError};
use rome_zk_executor_api::deposit_withdrawal;
use rome_zk_layouts::deposit_queue::deposit_queue as dq;
use rome_zk_layouts::exit::exit_config as xc;
use rome_zk_sequencer::header::SubBlockHeader;
use rome_zk_sequencer::log::LogWriter;
use rome_zk_sequencer::signing::sign_header;
use solana_program::pubkey::Pubkey;
use std::collections::HashMap;
use std::time::Instant;
use tempfile::tempdir;

const CHAIN_ID: u64 = 200_198;
const GAS_LIMIT: u64 = 100_000_000;

fn system_id() -> Pubkey {
    solana_sdk_ids::system_program::id()
}

#[derive(Default)]
struct Chain {
    accounts: HashMap<Pubkey, (Vec<u8>, Pubkey)>,
}

impl AccountOps for Chain {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Vec<u8>>, ResolveError> {
        Ok(self.accounts.get(pubkey).map(|(data, _)| data.clone()))
    }

    async fn get_account_owner(&self, pubkey: &Pubkey) -> Result<Option<Pubkey>, ResolveError> {
        Ok(self.accounts.get(pubkey).map(|(_, owner)| *owner))
    }

    async fn accounts_exist(&self, pubkeys: &[Pubkey]) -> Result<Vec<bool>, ResolveError> {
        Ok(pubkeys
            .iter()
            .map(|k| self.accounts.contains_key(k))
            .collect())
    }
}

struct World {
    chain: Chain,
    inbox: Pubkey,
    settlement: Pubkey,
    bridge: Pubkey,
}

impl World {
    /// A chain whose batch cursor is v2 and stands at `deposit_next`, with no exit config and no queue yet.
    fn with_cursor(deposit_next: u64) -> Self {
        let mut w = World {
            chain: Chain::default(),
            inbox: Pubkey::new_unique(),
            settlement: Pubkey::new_unique(),
            bridge: Pubkey::new_unique(),
        };
        let mut acct = rome_zk_testkit::cursor_account_for(2, w.inbox, CHAIN_ID, 1);
        acct.data[rome_zk_layouts::cursor::OFF_DEPOSIT_NEXT..][..8]
            .copy_from_slice(&deposit_next.to_le_bytes());
        let key = zk_inbox_client::cursor_pda(&w.inbox, &w.settlement, CHAIN_ID).0;
        w.chain.accounts.insert(key, (acct.data, w.inbox));
        w
    }

    fn exit_key(&self) -> Pubkey {
        zk_inbox_client::exit_config_pda(&self.settlement, CHAIN_ID).0
    }

    fn queue_key(&self) -> Pubkey {
        dq::pda(&self.bridge, &self.settlement.to_bytes(), CHAIN_ID).0
    }

    fn exit_config_bytes(&self) -> Vec<u8> {
        xc::write(&xc::ExitConfigFields {
            chain_id: CHAIN_ID,
            exit_portal: [7; 20],
            bridge_program: self.bridge.to_bytes(),
            pending_exit_portal: [0; 20],
            pending_bridge_program: [0; 32],
            pending_exit_cap: 0,
            pending_poster_bond: 0,
            activation_slot: 0,
            pending_mask: 0,
        })
        .to_vec()
    }

    fn put_exit_config(&mut self, owner: Pubkey) {
        let data = self.exit_config_bytes();
        self.chain.accounts.insert(self.exit_key(), (data, owner));
    }

    fn put_queue(&mut self, max_per_batch: u16, owner: Pubkey) {
        let mut q = vec![0u8; dq::LEN];
        dq::write(
            &mut q,
            &dq::DepositQueueFields {
                count: 0,
                head_hash: [0; 32],
                params: dq::DepositParams {
                    inclusion_deadline_secs: 3_600,
                    max_per_batch,
                    max_per_block: 10,
                    min_amount: 1,
                    fee_lamports: 0,
                    fee_recipient: [0; 32],
                },
                pending: dq::DepositParams::default(),
                activation_slot: 0,
            },
        );
        self.chain.accounts.insert(self.queue_key(), (q, owner));
    }

    /// What a plain transfer leaves at an address: an empty account the system program owns.
    fn put_dust(&mut self, key: Pubkey) {
        self.chain.accounts.insert(key, (Vec::new(), system_id()));
    }

    async fn start(&self) -> Result<DepositStart, rome_zk_batcher::pipeline::PipelineError> {
        read_deposit_start(&self.chain, &self.inbox, &self.settlement, CHAIN_ID).await
    }
}

#[tokio::test]
async fn an_empty_system_owned_account_at_the_exit_config_address_is_no_bridge_at_startup() {
    let mut w = World::with_cursor(5);
    w.put_dust(w.exit_key());
    assert_eq!(
        w.start().await.unwrap(),
        DepositStart { next: 5, cap: None }
    );
}

#[tokio::test]
async fn an_empty_system_owned_account_at_the_queue_address_is_no_queue_at_startup() {
    let mut w = World::with_cursor(5);
    w.put_exit_config(w.settlement);
    w.put_dust(w.queue_key());
    assert_eq!(
        w.start().await.unwrap(),
        DepositStart { next: 5, cap: None }
    );
}

#[tokio::test]
async fn well_formed_exit_config_and_queue_bytes_under_the_wrong_owner_are_absent_at_startup() {
    let mut w = World::with_cursor(5);
    w.put_exit_config(system_id());
    w.put_queue(4, w.bridge);
    assert_eq!(
        w.start().await.unwrap(),
        DepositStart { next: 5, cap: None },
        "the exit config is not the settlement program's, so no bridge is named and no queue is read"
    );
    w.put_exit_config(w.settlement);
    w.put_queue(4, system_id());
    assert_eq!(
        w.start().await.unwrap(),
        DepositStart { next: 5, cap: None },
        "the queue is not the bridge's"
    );
}

#[tokio::test]
async fn owned_accounts_give_the_cursor_and_the_limit() {
    let mut w = World::with_cursor(5);
    w.put_exit_config(w.settlement);
    w.put_queue(4, w.bridge);
    assert_eq!(
        w.start().await.unwrap(),
        DepositStart {
            next: 5,
            cap: Some(DepositCap {
                active: 4,
                pending: None
            })
        }
    );
}

/// One record per block: the block's one tx, and its credits on the same record.
fn append(writer: &mut LogWriter, block: u64, indices: &[u64]) {
    let header = SubBlockHeader {
        chain_id: CHAIN_ID,
        block,
        index: 0,
        timestamp_us: (1_757_000_000 + block) * 1_000_000,
        tx_root: B256::ZERO,
        receipts_root: B256::ZERO,
        gas_used: 21_000,
        prev_hash: B256::ZERO,
        deposits_end: indices.last().map(|i| i + 1),
    };
    let signature = sign_header(&PrivateKeySigner::random(), &header);
    let txs = vec![Bytes::from(vec![block as u8; 8])];
    let withdrawals: Vec<_> = indices
        .iter()
        .map(|&i| deposit_withdrawal(i, Address::repeat_byte(0x33), 10_000 + i))
        .collect();
    writer
        .append_with_withdrawals(&header, &signature, &txs, &withdrawals)
        .unwrap();
}

/// The wiring of `run_once` and `run_follow`: the source starts at the cursor's index, the grouper starts
/// its first range there and caps it at the queue's limit.
fn wire(dir: &std::path::Path, start: DepositStart) -> (BlockSource, SizeCappedGrouper) {
    let source = BlockSource::open(dir, CHAIN_ID, GAS_LIMIT, 1, 1, 0)
        .unwrap()
        .with_deposit_start(start.next);
    let grouper = SizeCappedGrouper::new(10, 900, DEFAULT_MAX_FRAME_BODY_LEN, None)
        .with_deposits(start.next, start.cap);
    (source, grouper)
}

#[tokio::test]
async fn the_start_read_off_the_chain_drives_the_source_and_the_grouper() {
    let mut w = World::with_cursor(5);
    w.put_exit_config(w.settlement);
    w.put_queue(4, w.bridge);
    let start = w.start().await.unwrap();

    let dir = tempdir().unwrap();
    let mut writer = LogWriter::open(dir.path(), 10_000).unwrap();
    append(&mut writer, 1, &[5, 6]);
    append(&mut writer, 2, &[7, 8]);
    append(&mut writer, 3, &[9]);
    drop(writer);

    let (mut source, mut grouper) = wire(dir.path(), start);
    let mut closed: Vec<(Vec<u64>, (u64, u64), CloseReason)> = Vec::new();
    while let Some(sourced) = source.next_block().unwrap() {
        let mut block: Block = sourced.block;
        loop {
            match grouper.push(block.clone(), Instant::now()).unwrap() {
                PushOutcome::Accepted => break,
                PushOutcome::Closed { reason, carry_over } => {
                    let range = grouper.deposit_range();
                    let numbers = grouper.take_group().iter().map(|b| b.number).collect();
                    closed.push((numbers, range, reason));
                    match carry_over {
                        Some(carried) => block = carried,
                        None => break,
                    }
                }
            }
        }
    }
    assert_eq!(
        closed,
        [(vec![1, 2], (5, 9), CloseReason::Deposits)],
        "the first batch starts at the cursor's index 5 and closes before the block that would pass 4 deposits"
    );
    assert_eq!(
        grouper.deposit_range(),
        (9, 10),
        "the open group starts where the closed one ended"
    );
}

#[tokio::test]
async fn a_log_whose_first_credit_is_not_the_cursors_index_stops_the_source() {
    let w = World::with_cursor(5);
    let start = w.start().await.unwrap();
    assert_eq!(start.next, 5);

    let dir = tempdir().unwrap();
    let mut writer = LogWriter::open(dir.path(), 10_000).unwrap();
    append(&mut writer, 1, &[6]);
    drop(writer);

    let (mut source, _) = wire(dir.path(), start);
    assert!(matches!(
        source.next_block().unwrap_err(),
        SourceError::FirstDepositIndexMismatch {
            block: 1,
            expected: 5,
            got: 6
        }
    ));
}
