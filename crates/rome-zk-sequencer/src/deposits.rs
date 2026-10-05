//! Deposits: reading the chain's deposit queue from Solana and handing the sealer the deposits a block credits.
//!
//! A depositor locks tokens in the bridge program, which appends a record to the chain's deposit queue. This
//! module reads that queue and its records at **finalized** commitment, through the one configured Solana RPC
//! (the sequencer reads Solana and sends nothing), and keeps the records the sealer has not yet put in a block.
//!
//! * [`DepositPoller`] runs beside the sequencer. Each poll reads `exit_config` (which names the bridge
//!   program), then the queue, then the records the sealer still needs, one `getMultipleAccounts` per 100
//!   records, the way derive's account reader does.
//! * [`DepositFeed`] is the shared view the poller writes and the sealer reads. At the first sub-block of a block
//!   the sealer asks it for the next deposits: every finalized deposit not yet included, oldest first, up to
//!   the per-block cap (the stricter of the queue's current and pending `max_per_block`), and never one at or past
//!   the finalized count. Each credit is a withdrawal built with [`rome_zk_executor_api::deposit_withdrawal`].
//!
//! A sequencer started without a `[deposits]` section has no feed and no poller, and runs as it always did.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::primitives::Address;
use alloy_eips::eip4895::Withdrawal;
use rome_zk_executor_api::deposit_withdrawal;
use rome_zk_layouts::cursor;
use rome_zk_layouts::deposit;
use rome_zk_layouts::deposit_queue::{bridge_config, deposit_queue, deposit_record};
use rome_zk_layouts::exit::exit_config;
use solana_program::pubkey::Pubkey;

use crate::metrics::{DepositMetrics, Metrics};

/// `getMultipleAccounts`' own limit on keys per call.
pub const MAX_ACCOUNTS_PER_CALL: usize = 100;

/// How many records past the last included one the poller keeps ready. A queue longer than this is read in
/// further polls as blocks consume it, so memory stays bounded however long the queue grows.
pub const MAX_BUFFERED_RECORDS: u64 = 1_000;

/// Why a poll failed. Every one is temporary: the sequencer keeps sealing and the next poll tries again.
#[derive(Debug, thiserror::Error)]
pub enum DepositError {
    #[error("solana rpc: {0}")]
    Rpc(String),
    #[error("{account} account does not decode: {reason}")]
    Layout {
        account: &'static str,
        reason: String,
    },
    #[error("deposit record {index}: {reason}")]
    Record { index: u64, reason: String },
}

/// An RPC failure as a [`DepositError`], with the request URL (and any key in it) stripped: the error is logged on
/// every failed poll, and an RPC URL often carries a key.
fn rpc_error(call: &str, e: &impl std::fmt::Display) -> DepositError {
    DepositError::Rpc(format!(
        "{call}: {}",
        rome_zk_solana_sender::describe_rpc_error(e)
    ))
}

/// The keccak the shared deposit chain functions take.
fn keccak(parts: &[&[u8]]) -> [u8; 32] {
    alloy::primitives::keccak256(parts.concat()).0
}

/// Reads Solana accounts. The one seam between the poller and the network: [`RpcAccountReader`] in production,
/// a map of accounts in tests.
pub trait AccountReader: Send {
    /// The accounts' data in the order given, `None` for an account that does not exist. Callers pass at most
    /// [`MAX_ACCOUNTS_PER_CALL`] keys.
    fn get_multiple_account_data(
        &mut self,
        keys: &[Pubkey],
    ) -> impl std::future::Future<Output = Result<Vec<Option<Vec<u8>>>, DepositError>> + Send;
}

/// A real Solana RPC node, always read at finalized commitment.
pub struct RpcAccountReader {
    client: solana_client::nonblocking::rpc_client::RpcClient,
}

impl RpcAccountReader {
    pub fn new(url: String) -> Self {
        Self {
            client: solana_client::nonblocking::rpc_client::RpcClient::new_with_commitment(
                url,
                solana_commitment_config::CommitmentConfig::finalized(),
            ),
        }
    }
}

impl AccountReader for RpcAccountReader {
    async fn get_multiple_account_data(
        &mut self,
        keys: &[Pubkey],
    ) -> Result<Vec<Option<Vec<u8>>>, DepositError> {
        let resp = self
            .client
            .get_multiple_accounts_with_commitment(
                keys,
                solana_commitment_config::CommitmentConfig::finalized(),
            )
            .await
            .map_err(|e| rpc_error("getMultipleAccounts", &e))?;
        // A short answer would shift every later record, so it is an error, never a partial result.
        if resp.value.len() != keys.len() {
            return Err(DepositError::Rpc(format!(
                "getMultipleAccounts: {} of {} entries",
                resp.value.len(),
                keys.len()
            )));
        }
        Ok(resp.value.into_iter().map(|a| a.map(|a| a.data)).collect())
    }
}

/// One finalized deposit, as the queue recorded it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deposit {
    /// Its position in the queue.
    pub index: u64,
    pub recipient: [u8; 20],
    pub amount_gwei: u64,
    /// When the bridge enqueued it, Unix seconds.
    pub enqueue_unix_ts: i64,
}

impl Deposit {
    /// The block's withdrawal for this deposit, built the one way every deposit credit is built.
    pub fn withdrawal(&self) -> Withdrawal {
        deposit_withdrawal(self.index, Address::from(self.recipient), self.amount_gwei)
    }
}

#[derive(Default)]
struct State {
    /// One past the last deposit a block has credited: the sealer's `deposits_end`.
    included: u64,
    /// The queue's finalized count, as last read. No deposit at or past it is ever handed out.
    finalized_count: u64,
    /// The per-block cap, the stricter of the queue's current and pending values.
    cap: u16,
    /// Index of `buffer[0]`.
    base: u64,
    /// Contiguous finalized records from `base`, oldest first.
    buffer: VecDeque<Deposit>,
}

impl State {
    /// The next index the poller has not read yet.
    fn fetch_from(&self) -> u64 {
        self.base + self.buffer.len() as u64
    }
}

/// The poller's and the sealer's shared view of the queue. Cheap to clone.
#[derive(Clone)]
pub struct DepositFeed {
    state: Arc<Mutex<State>>,
    metrics: DepositMetrics,
}

impl DepositFeed {
    /// A feed with nothing finalized yet. Registers the deposit metrics on `metrics`, so they exist only for a
    /// sequencer that runs with deposits.
    pub fn new(metrics: &Metrics) -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            metrics: metrics.register_deposit_metrics(),
        }
    }

    pub fn metrics(&self) -> &DepositMetrics {
        &self.metrics
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // A poisoned lock only means another thread panicked mid-update of plain data; the data is still a
        // valid (if stale) view, and the sealer must keep sealing.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The sealer's position: deposits below `end` have been credited. Called at startup with the value the
    /// log resumed at, and after every block that credits.
    pub fn set_included(&self, end: u64) {
        let mut s = self.lock();
        s.included = end;
        while s.base < end && s.buffer.pop_front().is_some() {
            s.base += 1;
        }
        if s.base < end {
            // The buffer held nothing up to `end`: start reading there.
            s.base = end;
        }
    }

    /// The deposits the next block credits: every finalized deposit not yet included, oldest first, at most
    /// the per-block cap, and never one at or past the finalized count. Empty when none waits.
    pub fn next_block(&self) -> Vec<Deposit> {
        let s = self.lock();
        let end = s
            .included
            .saturating_add(u64::from(s.cap))
            .min(s.finalized_count);
        let mut out = Vec::new();
        for index in s.included..end {
            match s
                .buffer
                .get((index - s.base) as usize)
                .filter(|d| d.index == index)
            {
                Some(d) => out.push(*d),
                // Not read yet: credit what is contiguous, never skip a deposit.
                None => break,
            }
        }
        out
    }

    /// When the oldest deposit waiting for a block was enqueued, Unix seconds.
    pub fn oldest_waiting_enqueue_ts(&self) -> Option<i64> {
        let s = self.lock();
        if s.included >= s.finalized_count {
            return None;
        }
        s.buffer.front().map(|d| d.enqueue_unix_ts)
    }

    /// The poller's window onto the queue: what it has to read this poll.
    fn fetch_from(&self) -> u64 {
        self.lock().fetch_from()
    }

    fn publish_queue(&self, finalized_count: u64, cap: u16) {
        let mut s = self.lock();
        s.finalized_count = finalized_count;
        s.cap = cap;
    }

    /// Appends `records`, whose first index is `first`, but only when `first` is where the buffer ends right
    /// now. A page whose position moved while it was being read (the sealer's start moved `base`) would
    /// otherwise land at the wrong index and stall every credit after it. Returns whether it was appended.
    fn extend(&self, first: u64, records: &[Deposit]) -> bool {
        let mut s = self.lock();
        if first != s.fetch_from() {
            return false;
        }
        s.buffer.extend(records.iter().copied());
        true
    }
}

/// Polls the queue and publishes into a [`DepositFeed`].
pub struct DepositPoller<R: AccountReader> {
    reader: R,
    feed: DepositFeed,
    settlement_program: Pubkey,
    chain_id: u64,
    interval: Duration,
    /// How far the records have been checked against the queue's hash chain. `None` until the first poll that
    /// finds the queue reads where to start.
    chain: Option<ChainCheck>,
}

/// The hash chain checked so far: every record below `next` chains from the start, and `hash` is the chain's
/// value before record `next`.
#[derive(Clone, Copy)]
struct ChainCheck {
    next: u64,
    hash: [u8; 32],
}

impl<R: AccountReader> DepositPoller<R> {
    pub fn new(
        reader: R,
        feed: DepositFeed,
        settlement_program: Pubkey,
        chain_id: u64,
        interval: Duration,
    ) -> Self {
        Self {
            reader,
            feed,
            settlement_program,
            chain_id,
            interval,
            chain: None,
        }
    }

    /// Polls forever. A failed poll is counted and logged, and the next one retries: Solana being slow or
    /// unreachable never stops the sequencer from sealing.
    pub async fn run(mut self) {
        loop {
            if let Err(e) = self.poll_once().await {
                self.feed.metrics.poll_errors_total.inc();
                tracing::warn!("deposit queue poll failed: {e}");
            }
            tokio::time::sleep(self.interval).await;
        }
    }

    /// One poll: `exit_config` (for the bridge program), the queue, then the records the sealer still needs.
    pub async fn poll_once(&mut self) -> Result<(), DepositError> {
        let (exit_config_key, _) = exit_config::pda(&self.settlement_program, self.chain_id);
        let Some(exit_data) = self.read_one(exit_config_key).await? else {
            // The chain has no exit config yet, so no bridge to read.
            return Ok(());
        };
        let exit = exit_config::read(&exit_data).map_err(|e| DepositError::Layout {
            account: "exit_config",
            reason: format!("{e:?}"),
        })?;
        if exit.bridge_program == [0u8; 32] {
            return Ok(());
        }
        let bridge = Pubkey::new_from_array(exit.bridge_program);
        let settlement = self.settlement_program.to_bytes();

        let (queue_key, _) = deposit_queue::pda(&bridge, &settlement, self.chain_id);
        let Some(queue_data) = self.read_one(queue_key).await? else {
            // The queue is not initialised: nothing has been deposited.
            return Ok(());
        };
        let queue = deposit_queue::read(&queue_data).map_err(|e| DepositError::Layout {
            account: "deposit_queue",
            reason: format!("{e:?}"),
        })?;
        // The stricter of the live and the pending proposal's cap: a block never credits more than either allows.
        let mut cap = queue.params.max_per_block;
        if queue.activation_slot != 0 {
            cap = cap.min(queue.pending.max_per_block);
        }
        let count = queue.count;
        self.feed.publish_queue(count, cap);
        self.feed
            .metrics
            .finalized_count
            .set(i64::try_from(count).unwrap_or(i64::MAX));

        let mut chain = match self.chain {
            Some(c) => c,
            None => {
                let c = self.chain_start(&bridge, &settlement).await?;
                self.chain = Some(c);
                c
            }
        };
        let resume_at = self.feed.fetch_from();
        if chain.next > resume_at {
            // The settled part of the queue is past what this log has credited: the log is behind the chain.
            // Records in between cannot be checked, so none is credited.
            return Err(DepositError::Record {
                index: resume_at,
                reason: format!(
                    "the settlement cursor is already at deposit {}, past what the log has credited",
                    chain.next
                ),
            });
        }

        // Only records below the finalized count are ever read, and only a bounded window of them. Records the
        // sealer already credited are read again only to carry the hash chain up to where it resumes.
        let included = self.feed.lock().included;
        let to = count.min(included.saturating_add(MAX_BUFFERED_RECORDS));
        while chain.next < to {
            let page_start = chain.next;
            let page_end = (page_start + MAX_ACCOUNTS_PER_CALL as u64).min(to);
            let keys: Vec<Pubkey> = (page_start..page_end)
                .map(|index| deposit_record::pda(&bridge, &settlement, self.chain_id, index).0)
                .collect();
            let accounts = self.reader.get_multiple_account_data(&keys).await?;
            if accounts.len() != keys.len() {
                return Err(DepositError::Rpc(format!(
                    "{} of {} records returned",
                    accounts.len(),
                    keys.len()
                )));
            }
            let mut page = Vec::with_capacity(accounts.len());
            let mut hash = chain.hash;
            let mut failure = None;
            for (index, data) in (page_start..page_end).zip(accounts) {
                match decode_record(index, data)
                    .and_then(|r| r.chained_after(&hash, &settlement, self.chain_id))
                {
                    Ok((d, next_hash)) => {
                        page.push(d);
                        hash = next_hash;
                    }
                    Err(e) => {
                        failure = Some(e);
                        break;
                    }
                }
            }
            // The records before a bad one are still good and in order: publish them, then report the failure.
            // Nothing after it is published, so a later deposit is never credited ahead of an earlier one.
            let verified_to = page_start + page.len() as u64;
            if !page.is_empty() {
                // Only records the feed has not got yet are handed over; the position is checked again under
                // the feed's lock, so a page that went stale while it was read is dropped, never misplaced.
                let skip = self.feed.fetch_from().saturating_sub(page_start) as usize;
                if skip < page.len() && !self.feed.extend(page_start + skip as u64, &page[skip..]) {
                    // Not appended: leave the check where it was and read the page again next poll.
                    return failure.map_or(Ok(()), Err);
                }
                chain = ChainCheck {
                    next: verified_to,
                    hash,
                };
                self.chain = Some(chain);
            }
            if let Some(e) = failure {
                return Err(e);
            }
        }
        Ok(())
    }

    /// Where the hash chain check starts: the settlement cursor's deposit position and hash, or the queue's
    /// first hash when no batch has taken a deposit yet (no cursor, or a cursor that does not carry deposits).
    async fn chain_start(
        &mut self,
        bridge: &Pubkey,
        settlement: &[u8; 32],
    ) -> Result<ChainCheck, DepositError> {
        let (config_key, _) = bridge_config::pda(bridge);
        let config_data = self
            .read_one(config_key)
            .await?
            .ok_or_else(|| DepositError::Layout {
                account: "bridge_config",
                reason: "does not exist".into(),
            })?;
        let config = bridge_config::read(&config_data).map_err(|e| DepositError::Layout {
            account: "bridge_config",
            reason: format!("{e:?}"),
        })?;
        let inbox = Pubkey::new_from_array(config.inbox_program);
        let (cursor_key, _) = cursor::pda(&inbox, &self.settlement_program, self.chain_id);
        if let Some(data) = self.read_one(cursor_key).await? {
            let c = cursor::read(&data).map_err(|e| DepositError::Layout {
                account: "batch_cursor",
                reason: format!("{e:?}"),
            })?;
            if let Some(d) = c.deposit {
                return Ok(ChainCheck {
                    next: d.next,
                    hash: d.hash,
                });
            }
        }
        Ok(ChainCheck {
            next: 0,
            hash: deposit::queue_seed_hash(&keccak, settlement, self.chain_id),
        })
    }

    async fn read_one(&mut self, key: Pubkey) -> Result<Option<Vec<u8>>, DepositError> {
        Ok(self
            .reader
            .get_multiple_account_data(&[key])
            .await?
            .into_iter()
            .next()
            .flatten())
    }
}

/// A record as read from its account, before its place in the hash chain is checked.
struct DecodedRecord {
    deposit: Deposit,
    sender: [u8; 32],
    hash_after: [u8; 32],
}

impl DecodedRecord {
    /// The deposit and the chain's value after it, when `hash_after` is what the chain gives for this record
    /// after `prev`: `chain_next(prev, leaf)` over the record's own leaf, the same two functions the program,
    /// derive and the prover use. A record that does not chain was not produced by the queue.
    fn chained_after(
        self,
        prev: &[u8; 32],
        settlement: &[u8; 32],
        chain_id: u64,
    ) -> Result<(Deposit, [u8; 32]), DepositError> {
        let d = self.deposit;
        let leaf = deposit::leaf(
            &keccak,
            settlement,
            chain_id,
            d.index,
            &self.sender,
            &d.recipient,
            d.amount_gwei,
        );
        let next = deposit::chain_next(&keccak, prev, &leaf);
        if next != self.hash_after {
            return Err(DepositError::Record {
                index: d.index,
                reason:
                    "hash_after does not follow from the previous record's hash and this record"
                        .into(),
            });
        }
        Ok((d, next))
    }
}

/// One record account's data. A record below the finalized count must exist at finalized commitment; if it does
/// not, the caller waits, because skipping it would credit a later deposit ahead of it.
fn decode_record(index: u64, data: Option<Vec<u8>>) -> Result<DecodedRecord, DepositError> {
    let data = data.ok_or_else(|| DepositError::Record {
        index,
        reason: "missing below the finalized count".into(),
    })?;
    let r = deposit_record::read(&data).map_err(|e| DepositError::Record {
        index,
        reason: format!("does not decode: {e:?}"),
    })?;
    if r.index != index {
        return Err(DepositError::Record {
            index,
            reason: format!("holds index {}", r.index),
        });
    }
    Ok(DecodedRecord {
        deposit: Deposit {
            index,
            recipient: r.recipient,
            amount_gwei: r.amount_gwei,
            enqueue_unix_ts: r.enqueue_unix_ts,
        },
        sender: r.sender,
        hash_after: r.hash_after,
    })
}

/// Starts deposits for a sequencer whose config has a `[deposits]` section: the feed the sealer reads, and the
/// poller task that fills it from the configured Solana RPC. `resume_deposits_end` is where the log resumed (one
/// past the last deposit a block credited). Call inside the tokio runtime.
pub fn start(
    settings: &crate::config::DepositSettings,
    chain_id: u64,
    metrics: &Metrics,
    resume_deposits_end: u64,
) -> Result<DepositFeed, crate::config::ConfigError> {
    let settlement_program = settings.settlement_program_id()?;
    let feed = DepositFeed::new(metrics);
    // Where the log resumed, set before the poller exists: its first page must be read from the right place.
    feed.set_included(resume_deposits_end);
    let poller = DepositPoller::new(
        RpcAccountReader::new(settings.solana_rpc_url.clone()),
        feed.clone(),
        settlement_program,
        chain_id,
        settings.poll_interval(),
    );
    tokio::spawn(poller.run());
    Ok(feed)
}

#[cfg(test)]
pub(crate) mod test_support {
    //! A mock Solana: a map of accounts the tests fill in, and a reader over it that records every call.

    use super::*;
    use rome_zk_layouts::deposit_queue::deposit_queue::{DepositParams, DepositQueueFields};
    use rome_zk_layouts::deposit_queue::deposit_record::DepositRecordFields;
    use std::collections::HashMap;

    pub const CHAIN_ID: u64 = 7_000_001;

    pub type PageReadHook = Box<dyn FnOnce() + Send>;

    #[derive(Clone, Default)]
    pub struct MockReader {
        pub accounts: Arc<Mutex<HashMap<Pubkey, Vec<u8>>>>,
        /// The key count of every call made.
        pub calls: Arc<Mutex<Vec<Vec<Pubkey>>>>,
        /// Runs once, while a read of several accounts is in flight (after the request, before the answer).
        pub during_page_read: Arc<Mutex<Option<PageReadHook>>>,
    }

    impl AccountReader for MockReader {
        async fn get_multiple_account_data(
            &mut self,
            keys: &[Pubkey],
        ) -> Result<Vec<Option<Vec<u8>>>, DepositError> {
            self.calls.lock().unwrap().push(keys.to_vec());
            if keys.len() > 1 {
                let hook = self.during_page_read.lock().unwrap().take();
                if let Some(hook) = hook {
                    hook();
                }
            }
            let accounts = self.accounts.lock().unwrap();
            Ok(keys.iter().map(|k| accounts.get(k).cloned()).collect())
        }
    }

    /// A chain on the mock: settlement program, bridge and the accounts under them.
    pub struct MockChain {
        pub reader: MockReader,
        pub settlement: Pubkey,
        pub bridge: Pubkey,
        pub inbox: Pubkey,
    }

    pub fn params(max_per_block: u16) -> DepositParams {
        DepositParams {
            inclusion_deadline_secs: 43_200,
            max_per_batch: 1_000,
            max_per_block,
            min_amount: 1,
            fee_lamports: 0,
            fee_recipient: [0u8; 32],
        }
    }

    impl MockChain {
        /// A chain whose `exit_config` names the bridge, with an empty queue capped at `max_per_block`.
        pub fn new(max_per_block: u16) -> Self {
            let chain = Self {
                reader: MockReader::default(),
                settlement: Pubkey::new_from_array([0x51; 32]),
                bridge: Pubkey::new_from_array([0x62; 32]),
                inbox: Pubkey::new_from_array([0x73; 32]),
            };
            let (key, _) = exit_config::pda(&chain.settlement, CHAIN_ID);
            let data = exit_config::write(&exit_config::ExitConfigFields {
                chain_id: CHAIN_ID,
                exit_portal: [0u8; 20],
                bridge_program: chain.bridge.to_bytes(),
                pending_exit_portal: [0u8; 20],
                pending_bridge_program: [0u8; 32],
                pending_exit_cap: 0,
                pending_poster_bond: 0,
                activation_slot: 0,
                pending_mask: 0,
            });
            chain
                .reader
                .accounts
                .lock()
                .unwrap()
                .insert(key, data.to_vec());
            let mut config = vec![0u8; bridge_config::LEN];
            bridge_config::write(
                &mut config,
                &bridge_config::BridgeConfigFields {
                    settlement_program: chain.settlement.to_bytes(),
                    inbox_program: chain.inbox.to_bytes(),
                },
            );
            chain
                .reader
                .accounts
                .lock()
                .unwrap()
                .insert(bridge_config::pda(&chain.bridge).0, config);
            chain.set_queue(0, params(max_per_block), params(0), 0);
            chain
        }

        /// A chain whose `exit_config` names the bridge but whose deposit queue was never created: what a chain
        /// looks like when the operator has turned deposits on in the sequencer before running `deposit-queue init`.
        pub fn without_queue() -> Self {
            let chain = Self::new(0);
            let (queue_key, _) =
                deposit_queue::pda(&chain.bridge, &chain.settlement.to_bytes(), CHAIN_ID);
            chain.reader.accounts.lock().unwrap().remove(&queue_key);
            chain
        }

        /// The queue's hash-chain value before deposit `index`, for the records `put_record` writes.
        pub fn hash_before(&self, index: u64) -> [u8; 32] {
            let settlement = self.settlement.to_bytes();
            if index == 0 {
                return deposit::queue_seed_hash(&keccak, &settlement, CHAIN_ID);
            }
            // The record before it is already on the mock in the usual in-order case; otherwise work it out.
            let prev_key = deposit_record::pda(&self.bridge, &settlement, CHAIN_ID, index - 1).0;
            let stored = self.reader.accounts.lock().unwrap().get(&prev_key).cloned();
            if let Some(r) = stored.and_then(|d| deposit_record::read(&d).ok()) {
                return r.hash_after;
            }
            let before_prev = self.hash_before(index - 1);
            self.hash_after_for(index - 1, &before_prev)
        }

        fn hash_after_for(&self, index: u64, prev: &[u8; 32]) -> [u8; 32] {
            let leaf = deposit::leaf(
                &keccak,
                &self.settlement.to_bytes(),
                CHAIN_ID,
                index,
                &[9u8; 32],
                &[(index as u8).wrapping_add(1); 20],
                100 + index,
            );
            deposit::chain_next(&keccak, prev, &leaf)
        }

        /// A settlement cursor that says the next batch takes deposit `next`, whose chain value before it is `hash`.
        pub fn set_cursor(&self, next: u64, hash: [u8; 32]) {
            let data = cursor::write_v2(&cursor::CursorFields {
                chain_id: CHAIN_ID,
                next_batch: 1,
                deposit: Some(cursor::CursorDeposit {
                    next,
                    hash,
                    final_: 0,
                }),
            })
            .unwrap();
            self.reader.accounts.lock().unwrap().insert(
                cursor::pda(&self.inbox, &self.settlement, CHAIN_ID).0,
                data.to_vec(),
            );
        }

        pub fn set_queue(
            &self,
            count: u64,
            live: DepositParams,
            pending: DepositParams,
            activation_slot: u64,
        ) {
            let (key, _) = deposit_queue::pda(&self.bridge, &self.settlement.to_bytes(), CHAIN_ID);
            let mut data = vec![0u8; deposit_queue::LEN];
            deposit_queue::write(
                &mut data,
                &DepositQueueFields {
                    count,
                    head_hash: [0u8; 32],
                    params: live,
                    pending,
                    activation_slot,
                },
            );
            self.reader.accounts.lock().unwrap().insert(key, data);
        }

        /// Sets the queue's count, keeping the live cap and no pending proposal.
        pub fn set_count(&self, count: u64, max_per_block: u16) {
            self.set_queue(count, params(max_per_block), params(0), 0);
        }

        /// A record for deposit `index`: recipient `[index as u8 + 1; 20]`, amount `100 + index`, enqueued at `ts`,
        /// its `hash_after` the real chain value after it.
        pub fn put_record(&self, index: u64, ts: i64) {
            let hash_after = self.hash_after_for(index, &self.hash_before(index));
            self.put_record_with_hash(index, ts, hash_after);
        }

        /// As [`Self::put_record`], with `hash_after` as given.
        pub fn put_record_with_hash(&self, index: u64, ts: i64, hash_after: [u8; 32]) {
            let (key, _) =
                deposit_record::pda(&self.bridge, &self.settlement.to_bytes(), CHAIN_ID, index);
            let mut data = vec![0u8; deposit_record::LEN];
            deposit_record::write(
                &mut data,
                &DepositRecordFields {
                    index,
                    enqueue_unix_ts: ts,
                    sender: [9u8; 32],
                    recipient: [(index as u8).wrapping_add(1); 20],
                    amount_gwei: 100 + index,
                    hash_after,
                },
            );
            self.reader.accounts.lock().unwrap().insert(key, data);
        }

        /// Records `0..count`, enqueued at `ts`, and the queue's count set to match.
        pub fn deposit_up_to(&self, count: u64, max_per_block: u16, ts: i64) {
            for i in 0..count {
                self.put_record(i, ts);
            }
            self.set_count(count, max_per_block);
        }

        pub fn poller(&self, feed: &DepositFeed) -> DepositPoller<MockReader> {
            DepositPoller::new(
                self.reader.clone(),
                feed.clone(),
                self.settlement,
                CHAIN_ID,
                Duration::from_millis(10),
            )
        }
    }

    /// The account key of deposit record `index` on `chain`.
    pub fn record_key(chain: &MockChain, index: u64) -> Pubkey {
        deposit_record::pda(&chain.bridge, &chain.settlement.to_bytes(), CHAIN_ID, index).0
    }

    pub fn feed() -> DepositFeed {
        DepositFeed::new(&Metrics::new())
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    fn indices(d: &[Deposit]) -> Vec<u64> {
        d.iter().map(|d| d.index).collect()
    }

    #[tokio::test]
    async fn blocks_credit_oldest_first_up_to_the_cap() {
        let chain = MockChain::new(3);
        chain.deposit_up_to(8, 3, 1_000);
        let feed = feed();
        let mut poller = chain.poller(&feed);
        poller.poll_once().await.unwrap();

        // Block after block, oldest first, three at a time, then the remainder.
        let mut seen = Vec::new();
        for want in [vec![0, 1, 2], vec![3, 4, 5], vec![6, 7], vec![]] {
            let got = feed.next_block();
            assert_eq!(indices(&got), want);
            if let Some(last) = got.last() {
                feed.set_included(last.index + 1);
            }
            seen.extend(got);
        }
        // Each credit is the withdrawal `deposit_withdrawal` builds for the record, nothing else.
        let w = seen[4].withdrawal();
        assert_eq!(w, deposit_withdrawal(4, Address::from([5u8; 20]), 104));
    }

    #[tokio::test]
    async fn the_stricter_of_current_and_pending_cap_applies() {
        let chain = MockChain::new(5);
        for i in 0..6 {
            chain.put_record(i, 1);
        }
        // A pending proposal of 2 per block, waiting to activate, is already the cap.
        chain.set_queue(6, params(5), params(2), 99);
        let feed = feed();
        chain.poller(&feed).poll_once().await.unwrap();
        assert_eq!(indices(&feed.next_block()), vec![0, 1]);

        // A looser pending proposal does not loosen the live cap.
        chain.set_queue(6, params(3), params(10), 99);
        chain.poller(&feed).poll_once().await.unwrap();
        assert_eq!(indices(&feed.next_block()), vec![0, 1, 2]);

        // No proposal pending: the pending block is ignored even though it holds a smaller number.
        chain.set_queue(6, params(4), params(1), 0);
        chain.poller(&feed).poll_once().await.unwrap();
        assert_eq!(indices(&feed.next_block()), vec![0, 1, 2, 3]);
    }

    #[tokio::test]
    async fn never_past_the_finalized_count() {
        let chain = MockChain::new(10);
        // Accounts exist for deposits 0..6, but only 4 are finalized.
        for i in 0..6 {
            chain.put_record(i, 1);
        }
        chain.set_count(4, 10);
        let feed = feed();
        let mut poller = chain.poller(&feed);
        poller.poll_once().await.unwrap();

        assert_eq!(indices(&feed.next_block()), vec![0, 1, 2, 3]);
        // The poller never asked for a record at or past the count.
        let asked_for_record_4_or_later = chain.reader.calls.lock().unwrap().iter().any(|keys| {
            (4..6).any(|i| {
                keys.contains(
                    &deposit_record::pda(&chain.bridge, &chain.settlement.to_bytes(), CHAIN_ID, i)
                        .0,
                )
            })
        });
        assert!(!asked_for_record_4_or_later);

        feed.set_included(4);
        assert!(feed.next_block().is_empty(), "nothing is past the count");

        // The queue grows to 6 and the next poll makes 4 and 5 creditable.
        chain.set_count(6, 10);
        poller.poll_once().await.unwrap();
        assert_eq!(indices(&feed.next_block()), vec![4, 5]);
    }

    #[tokio::test]
    async fn records_are_read_one_get_multiple_accounts_per_100() {
        let chain = MockChain::new(10);
        chain.deposit_up_to(250, 10, 1);
        let feed = feed();
        chain.poller(&feed).poll_once().await.unwrap();

        let calls = chain.reader.calls.lock().unwrap();
        let record_calls: Vec<usize> = calls
            .iter()
            .filter(|k| k.len() > 1)
            .map(|k| k.len())
            .collect();
        assert_eq!(record_calls, vec![100, 100, 50]);
        assert!(calls.iter().all(|k| k.len() <= MAX_ACCOUNTS_PER_CALL));
    }

    #[tokio::test]
    async fn the_poller_keeps_a_bounded_window_and_reads_on_as_blocks_consume() {
        let chain = MockChain::new(500);
        let total = MAX_BUFFERED_RECORDS + 300;
        chain.deposit_up_to(total, 500, 1);
        let feed = feed();
        let mut poller = chain.poller(&feed);
        poller.poll_once().await.unwrap();
        assert_eq!(feed.lock().buffer.len() as u64, MAX_BUFFERED_RECORDS);

        feed.set_included(500);
        poller.poll_once().await.unwrap();
        // The window now reaches the end of the queue: records 500..1300.
        assert_eq!(feed.lock().buffer.len() as u64, total - 500);
        assert_eq!(feed.next_block().first().map(|d| d.index), Some(500));
    }

    #[tokio::test]
    async fn an_unreadable_record_is_never_skipped() {
        let chain = MockChain::new(10);
        chain.deposit_up_to(3, 10, 1);
        // Remove record 1: crediting 2 ahead of it would break the queue's order.
        let (key, _) =
            deposit_record::pda(&chain.bridge, &chain.settlement.to_bytes(), CHAIN_ID, 1);
        chain.reader.accounts.lock().unwrap().remove(&key);
        let feed = feed();
        let mut poller = chain.poller(&feed);
        assert!(matches!(
            poller.poll_once().await,
            Err(DepositError::Record { index: 1, .. })
        ));
        assert_eq!(indices(&feed.next_block()), vec![0]);
    }

    #[tokio::test]
    async fn no_exit_config_or_no_queue_means_no_deposits() {
        let chain = MockChain::new(10);
        chain.reader.accounts.lock().unwrap().clear();
        let feed = feed();
        let mut poller = chain.poller(&feed);
        poller.poll_once().await.unwrap();
        assert!(feed.next_block().is_empty());
        assert_eq!(feed.oldest_waiting_enqueue_ts(), None);
    }

    /// The chain has an exit config that names the bridge, but no queue was created: a poll succeeds, reads
    /// nothing else, and leaves the feed exactly as it was. A queue created later is picked up by the next poll.
    #[tokio::test]
    async fn a_chain_with_no_queue_yet_leaves_the_feed_untouched() {
        let chain = MockChain::without_queue();
        let feed = feed();
        let mut poller = chain.poller(&feed);
        for _ in 0..3 {
            poller.poll_once().await.unwrap();
        }
        assert!(feed.next_block().is_empty());
        assert_eq!(feed.oldest_waiting_enqueue_ts(), None);
        assert_eq!(feed.metrics().finalized_count.get(), 0);
        // Each poll read the exit config and the queue, one account each, and nothing else: no record, no cursor.
        {
            let calls = chain.reader.calls.lock().unwrap();
            assert_eq!(calls.len(), 6);
            assert!(calls.iter().all(|keys| keys.len() == 1));
        }
        // The operator creates the queue and one deposit lands: the same poller now credits it.
        chain.set_queue(0, params(4), params(0), 0);
        chain.deposit_up_to(1, 4, 1_757_000_000);
        poller.poll_once().await.unwrap();
        assert_eq!(indices(&feed.next_block()), vec![0]);
    }

    #[tokio::test]
    async fn oldest_waiting_is_the_first_not_yet_included() {
        let chain = MockChain::new(10);
        chain.put_record(0, 100);
        chain.put_record(1, 200);
        chain.set_count(2, 10);
        let feed = feed();
        chain.poller(&feed).poll_once().await.unwrap();
        assert_eq!(feed.oldest_waiting_enqueue_ts(), Some(100));
        feed.set_included(1);
        assert_eq!(feed.oldest_waiting_enqueue_ts(), Some(200));
        feed.set_included(2);
        assert_eq!(feed.oldest_waiting_enqueue_ts(), None);
    }

    #[tokio::test]
    async fn resuming_at_a_later_index_reads_from_there() {
        let chain = MockChain::new(10);
        chain.deposit_up_to(5, 10, 1);
        // The settlement cursor is at the resume point, so the chain check starts there too.
        chain.set_cursor(3, chain.hash_before(3));
        let feed = feed();
        feed.set_included(3);
        chain.poller(&feed).poll_once().await.unwrap();
        assert_eq!(indices(&feed.next_block()), vec![3, 4]);
        // Nothing before the resume point was read again.
        let first_record =
            deposit_record::pda(&chain.bridge, &chain.settlement.to_bytes(), CHAIN_ID, 0).0;
        assert!(!chain
            .reader
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|k| k.contains(&first_record)));
    }

    #[tokio::test]
    async fn a_page_read_while_the_log_start_moved_is_never_misplaced() {
        // The poller asked for records from 0; while the answer was in flight the sealer's start moved to 4.
        let chain = MockChain::new(10);
        chain.deposit_up_to(8, 10, 1);
        let feed = feed();
        let moved = feed.clone();
        *chain.reader.during_page_read.lock().unwrap() =
            Some(Box::new(move || moved.set_included(4)));
        let mut poller = chain.poller(&feed);
        poller.poll_once().await.unwrap();
        assert_eq!(indices(&feed.next_block()), vec![4, 5, 6, 7]);
    }

    #[test]
    fn a_page_that_does_not_start_where_the_buffer_ends_is_dropped() {
        let feed = feed();
        let record = |index| Deposit {
            index,
            recipient: [1; 20],
            amount_gwei: 1,
            enqueue_unix_ts: 1,
        };
        // Read from 0, but the log resumed at 4 in the meantime: the buffer now ends at 4.
        feed.set_included(4);
        feed.publish_queue(8, 10);
        assert!(!feed.extend(0, &[record(0), record(1), record(2), record(3), record(4)]));
        assert!(feed.next_block().is_empty());
        assert!(feed.extend(4, &[record(4), record(5)]));
        assert_eq!(indices(&feed.next_block()), vec![4, 5]);
    }

    #[tokio::test]
    async fn a_record_whose_hash_after_does_not_chain_is_refused_and_nothing_after_it_is_credited()
    {
        let chain = MockChain::new(10);
        chain.deposit_up_to(5, 10, 1);
        // Record 2 claims a hash the chain does not give; 3 and 4 are as the queue wrote them.
        chain.put_record_with_hash(2, 1, [0xee; 32]);
        let feed = feed();
        let mut poller = chain.poller(&feed);
        match poller.poll_once().await {
            Err(DepositError::Record { index: 2, reason }) => {
                assert!(reason.contains("hash_after"))
            }
            other => panic!("expected record 2 to be refused, got {other:?}"),
        }
        assert_eq!(indices(&feed.next_block()), vec![0, 1]);
        feed.set_included(2);
        assert!(
            feed.next_block().is_empty(),
            "nothing at or after the bad record is credited"
        );
        // The poll stays stopped at the bad record on every later poll, and never skips it.
        assert!(poller.poll_once().await.is_err());
        assert!(feed.next_block().is_empty());
    }

    #[tokio::test]
    async fn a_record_that_chains_from_a_different_record_is_refused() {
        // Record 1 carries record 2's valid hash_after: each record must chain from the one before it.
        let chain = MockChain::new(10);
        chain.deposit_up_to(3, 10, 1);
        let h2 = chain.hash_before(3);
        chain.put_record_with_hash(1, 1, h2);
        let feed = feed();
        let mut poller = chain.poller(&feed);
        assert!(matches!(
            poller.poll_once().await,
            Err(DepositError::Record { index: 1, .. })
        ));
        assert_eq!(indices(&feed.next_block()), vec![0]);
    }

    #[tokio::test]
    async fn the_chain_check_starts_from_the_settlement_cursor() {
        let chain = MockChain::new(10);
        chain.deposit_up_to(6, 10, 1);
        // The cursor says deposits below 2 are settled; the log resumed at 4.
        chain.set_cursor(2, chain.hash_before(2));
        let feed = feed();
        feed.set_included(4);
        chain.poller(&feed).poll_once().await.unwrap();
        assert_eq!(indices(&feed.next_block()), vec![4, 5]);
        // Records 2 and 3 were read to carry the chain to 4, and 0 and 1 were not read at all.
        let calls = chain.reader.calls.lock().unwrap();
        let read = |i| calls.iter().any(|k| k.contains(&record_key(&chain, i)));
        assert!(!read(0) && !read(1) && read(2) && read(3));
    }

    #[tokio::test]
    async fn a_cursor_hash_that_is_wrong_refuses_every_record() {
        let chain = MockChain::new(10);
        chain.deposit_up_to(4, 10, 1);
        chain.set_cursor(2, [0x11; 32]);
        let feed = feed();
        feed.set_included(2);
        assert!(matches!(
            chain.poller(&feed).poll_once().await,
            Err(DepositError::Record { index: 2, .. })
        ));
        assert!(feed.next_block().is_empty());
    }

    #[tokio::test]
    async fn a_cursor_ahead_of_the_log_credits_nothing() {
        let chain = MockChain::new(10);
        chain.deposit_up_to(6, 10, 1);
        chain.set_cursor(4, chain.hash_before(4));
        let feed = feed();
        feed.set_included(1);
        assert!(chain.poller(&feed).poll_once().await.is_err());
        assert!(feed.next_block().is_empty());
    }

    /// An error whose text carries the request URL, as reqwest's does.
    struct UrlCarryingError;
    impl std::fmt::Display for UrlCarryingError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                f,
                "error sending request for url (https://rpc.example/?api-key=SECRET): connection refused"
            )
        }
    }

    #[test]
    fn an_rpc_error_strips_the_url_and_any_secret_it_carries() {
        let e = rpc_error("getMultipleAccounts", &UrlCarryingError);
        let shown = e.to_string();
        assert!(
            !shown.contains("api-key")
                && !shown.contains("SECRET")
                && !shown.contains("rpc.example"),
            "the URL and the secret in it must not survive, got: {shown:?}"
        );
        assert!(shown.contains("getMultipleAccounts") && shown.contains("error sending request"));
    }
}
