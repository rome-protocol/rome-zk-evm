//! Wires admission, the sealer, and the batcher/feed hand-off into one running node, behind a single
//! actor task — so admission and sealing never race each other over shared state (a user is never
//! answered before the fsync).
//!
//! The actor owns everything mutable (the admission queue, the sealer, the pending-reply map) and
//! reacts to exactly two events via `tokio::select!`: a new tx to admit, or the sealing timer. This is
//! the "interrupt-driven, not polled" shape adopted from OP's sequencer loop.

use alloy::primitives::{Address, Bytes, TxHash};
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::admission::{Admission, AdmissionConfig, AdmissionError, AdmitOutcome};
use crate::executor::{Executor, Reason, SubBlockLimits};
use crate::log::LogWriter;
use crate::metrics::Metrics;
use crate::preconf::{ChannelSink, SealedSubBlock};
use crate::sealer::{ResumePoint, SealError, SealerState, Tick};
use crate::signing::Preconfirmation;
use std::sync::Arc;

/// Default sealing period: 50 ms signed sub-blocks.
pub const SEAL_PERIOD: Duration = Duration::from_millis(50);

#[derive(Debug, thiserror::Error)]
pub enum SequencerError {
    #[error(transparent)]
    Admission(#[from] AdmissionError),
    #[error("rejected at execution: {0:?}")]
    Rejected(Reason),
    #[error("sequencer is shutting down")]
    Shutdown,
}

/// The result of [`SequencerHandle::submit_raw_tx`]: a `Ready` admission
/// waits for its pre-confirmation, but a `Parked` one (behind a nonce gap) must not hold the caller —
/// standard Ethereum `eth_sendRawTransaction` semantics return the tx hash immediately either way, and
/// the caller can poll [`SequencerHandle::get_preconfirmation`] once the gap fills.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitOutcome {
    Preconfirmed(Preconfirmation),
    Parked(TxHash),
}

impl SubmitOutcome {
    pub fn tx_hash(&self) -> TxHash {
        match self {
            SubmitOutcome::Preconfirmed(p) => p.tx_hash,
            SubmitOutcome::Parked(h) => *h,
        }
    }
}

enum Command {
    SubmitTx {
        raw: Bytes,
        reply: oneshot::Sender<Result<SubmitOutcome, SequencerError>>,
    },
    /// The sender's next nonce as the executor's in-progress (open, not-yet-sealed)
    /// block currently sees it — the same value [`crate::executor::Executor::nonce`] would return
    /// mid-block. Routed through the actor's own command channel (like `SubmitTx`) rather than a
    /// second shared-state structure (the `RecentPreconfs` pattern is
    /// for a value written every sub-block regardless of whether anyone asks; this one only has a
    /// reader — the reth node's `eth_getTransactionCount(_, "pending")` override — and is cheap
    /// enough on the actor's own hot path that a direct round trip is simpler than adding a second
    /// `Arc<RwLock<..>>` that only one caller ever reads).
    PendingNonce {
        addr: Address,
        reply: oneshot::Sender<u64>,
    },
}

/// A bounded two-block dedup-window horizon of recently included pre-confirmations,
/// shared between the actor (which writes it after every seal) and every
/// [`SequencerHandle`] (which reads it directly). `[0]` accumulates the block in progress; `[1]` is the
/// block before it; rotated on each block boundary — the same shape as `Admission`'s own dedup ring.
///
/// This used to live only inside the actor and be served over the same
/// bounded `mpsc` command channel as `eth_sendRawTransaction`/`rome_sendRawTransaction` — a poller
/// competed with every submission for one of the channel's 1,024 slots. Behind an `Arc<RwLock<..>>`
/// instead, a lookup is a direct read with no dependency on the actor's channel or its tick loop at all.
type RecentPreconfs = Arc<std::sync::RwLock<[HashMap<TxHash, Preconfirmation>; 2]>>;

/// A cheap-to-clone handle to a running sequencer. This is what the RPC layer holds.
#[derive(Clone)]
pub struct SequencerHandle {
    tx: mpsc::Sender<Command>,
    sink: ChannelSink,
    chain_id: u64,
    recent_preconfs: RecentPreconfs,
}

impl SequencerHandle {
    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// Submit a raw tx. A `Ready` admission waits for its pre-confirmation (≤ one sub-block — the async
    /// operation whose latency the "< 100 ms p99" budget is measured against); a `Parked`
    /// admission (nonce gap) returns immediately without waiting.
    pub async fn submit_raw_tx(&self, raw: Bytes) -> Result<SubmitOutcome, SequencerError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Command::SubmitTx {
                raw,
                reply: reply_tx,
            })
            .await
            .map_err(|_| SequencerError::Shutdown)?;
        reply_rx.await.map_err(|_| SequencerError::Shutdown)?
    }

    /// Look up a tx's pre-confirmation from the bounded recent-inclusion horizon (the last ~2 blocks).
    /// `None` if the tx was never included, isn't included yet, or aged out of the horizon.
    ///
    /// A direct, synchronous read off the shared [`RecentPreconfs`] —
    /// never a round trip through the actor's own command channel, which also carries every
    /// `eth_sendRawTransaction`/`rome_sendRawTransaction`. A poisoned lock (only possible if a writer
    /// panicked mid-update, which the actor's own code never does) is treated the same as "not found"
    /// rather than propagating a panic to every caller.
    pub fn get_preconfirmation(&self, tx_hash: TxHash) -> Option<Preconfirmation> {
        let guard = self.recent_preconfs.read().ok()?;
        guard[0]
            .get(&tx_hash)
            .or_else(|| guard[1].get(&tx_hash))
            .cloned()
    }

    pub fn subscribe_preconfirmations(&self) -> broadcast::Receiver<SealedSubBlock> {
        self.sink.subscribe()
    }

    /// `addr`'s next nonce as the executor's in-progress block currently sees it (the
    /// same value [`crate::executor::Executor::nonce`] returns mid-block) — what the reth node's
    /// `eth_getTransactionCount(_, "pending")` override answers with, so a sender's own next tx's
    /// nonce is immediately correct after a preconfirmed send, without waiting a full second for the
    /// block that tx lands in to seal ("pending must include the sequencer's in-block nonce
    /// advances"). `Err` only if the actor has shut down.
    pub async fn pending_nonce(&self, addr: Address) -> Result<u64, SequencerError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Command::PendingNonce {
                addr,
                reply: reply_tx,
            })
            .await
            .map_err(|_| SequencerError::Shutdown)?;
        reply_rx.await.map_err(|_| SequencerError::Shutdown)
    }
}

/// Why the sequencer actor task exited: either fatal cause leaves state
/// of unknown shape (the log's durability contract or the executor's in-memory state), so the actor
/// shuts down cleanly rather than continuing — this is what the actor's `JoinHandle` resolves to on
/// failure, and pending senders separately receive [`SequencerError::Shutdown`] when their oneshot reply
/// sender is dropped along with the rest of the actor's state.
#[derive(Debug, thiserror::Error)]
pub enum SequencerFatal {
    #[error(transparent)]
    Seal(#[from] SealError),
    /// The executor's `flush()` (joining any still-outstanding background
    /// persist) failed during a graceful shutdown — the process should not report a clean exit over
    /// a write that never actually landed.
    #[error("executor flush failed during shutdown: {0}")]
    Flush(#[from] crate::executor::ExecutorError),
}

fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_micros() as u64
}

struct Actor<E: Executor> {
    admission: Admission,
    sealer: SealerState<E, ChannelSink>,
    pending: HashMap<TxHash, oneshot::Sender<Result<SubmitOutcome, SequencerError>>>,
    metrics: Arc<Metrics>,
    seal_period: Duration,
    /// The per-sub-block gas budget passed to the executor.
    sub_block_gas_limit: u64,
    /// The actor's write handle onto the same shared structure every
    /// [`SequencerHandle`] reads directly — see [`RecentPreconfs`]'s doc.
    recent_preconfs: RecentPreconfs,
}

impl<E: Executor> Actor<E> {
    async fn run(mut self, mut rx: mpsc::Receiver<Command>) -> Result<(), SequencerFatal> {
        let mut interval = tokio::time::interval(self.seal_period);
        // Skip missed ticks rather than bursting to catch up: a sub-block that overran its deadline
        // seals late, once; the schedule then resumes on the original grid.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            // `biased` so the sealing tick is always polled first: under a saturating stream of
            // submissions, an unbiased `select!` can repeatedly pick the ready `rx.recv()` branch over
            // an equally-ready `interval.tick()`, starving the timer and inflating tail latency far past
            // the 50 ms deadline — measured, not assumed (this crate's `tests/e2e.rs` caught a p99 over
            // budget before this bias was added).
            tokio::select! {
                biased;
                _ = interval.tick() => {
                    // A fatal seal error shuts the actor down cleanly —
                    // dropping `self` here drops every pending reply sender, which resolves each waiting
                    // caller's oneshot to `Err` (mapped to `SequencerError::Shutdown`), rather than
                    // panicking the task.
                    if let Err(e) = self.tick().await {
                        tracing::error!("sequencer actor fatal, shutting down: {e}");
                        return Err(SequencerFatal::from(e));
                    }
                }
                maybe_cmd = rx.recv() => {
                    match maybe_cmd {
                        Some(Command::SubmitTx { raw, reply }) => self.handle_submit(raw, reply),
                        Some(Command::PendingNonce { addr, reply }) => {
                            let _ = reply.send(self.sealer.executor.nonce(addr));
                        }
                        None => break,
                    }
                }
            }
        }
        // A graceful shutdown (every `SequencerHandle` dropped) is the last
        // chance to make the tail durable before the process exits normally — join whatever the
        // executor still has outstanding rather than silently reporting a clean exit over an
        // in-flight write. A fatal shutdown above (the `return Err` inside the tick arm) skips this
        // deliberately: that state is already of unknown shape (the same
        // reasoning as a fatal seal error), so this only ever fires on the one path where "clean" is
        // actually meaningful.
        self.sealer.executor.flush().await?;
        Ok(())
    }

    fn handle_submit(
        &mut self,
        raw: Bytes,
        reply: oneshot::Sender<Result<SubmitOutcome, SequencerError>>,
    ) {
        let executor = &self.sealer.executor;
        let outcome = self
            .admission
            .admit(raw, Instant::now(), |addr| executor.nonce(addr));
        match outcome {
            Ok((AdmitOutcome::Ready, tx_hash)) => {
                self.pending.insert(tx_hash, reply);
            }
            // A Parked tx must not hold the RPC — reply immediately with
            // the tx hash (standard `eth_sendRawTransaction` semantics) and never register a pending
            // oneshot for it at all.
            Ok((AdmitOutcome::Parked, tx_hash)) => {
                let _ = reply.send(Ok(SubmitOutcome::Parked(tx_hash)));
            }
            Err(e) => {
                let _ = reply.send(Err(SequencerError::from(e)));
            }
        }
    }

    /// Returns `Err` only when the seal itself fatally failed — the
    /// caller (`run`) shuts the actor down cleanly rather than panicking.
    async fn tick(&mut self) -> Result<(), SealError> {
        let start = Instant::now();

        for (hash, err) in self.admission.expire(Instant::now()) {
            if let Some(reply) = self.pending.remove(&hash) {
                let _ = reply.send(Err(SequencerError::from(err)));
            }
        }

        // Each drained entry carries its *original* admission time, so
        // a tx later carried forward (not_executed below) can still time out against it.
        let drained = self.admission.drain_ready();
        let tx_count = drained.len();
        let txs: Vec<Bytes> = drained.iter().map(|d| d.parsed.raw.clone()).collect();
        let timestamp_us = now_micros();
        let limits = SubBlockLimits {
            gas_limit: self.sub_block_gas_limit,
            deadline: start + self.seal_period,
        };

        let tick = self
            .sealer
            .seal_sub_block(txs, timestamp_us, limits)
            .await?;
        let result = match tick {
            Tick::Idle { .. } => {
                // Nothing to touch — no log record was written, the executor was
                // never called, and `drained` was necessarily empty (idle requires no transactions
                // ready). Admission's own expiry above still ran; only the idle counter moves.
                self.metrics.idle_ticks_total.inc();
                return Ok(());
            }
            Tick::Sealed(result) => result,
        };
        self.metrics
            .log_fsync_duration_seconds
            .observe(result.log_fsync_seconds);

        let attempted_len = drained.len() - result.outcome.not_executed.len();

        // A lookup from tx_hash to the original admission-queue `enqueued_at`
        // for every ATTEMPTED tx (included or rejected — the not_executed tail is excluded, since it
        // was never actually acked this tick) — needed below to observe `Metrics::
        // preconf_latency_seconds` ("time from admission-queue entry to the ack (post-fsync) being sent
        // to the caller", per that field's own doc) at the exact moment each included tx's ack fires.
        // Built here, before `drained` is consumed by the not_executed branch below.
        let enqueued_at_by_hash: HashMap<TxHash, Instant> = drained[..attempted_len]
            .iter()
            .map(|d| (d.parsed.tx_hash, d.enqueued_at))
            .collect();

        // A lookup from tx_hash to the original `DrainedTx` for every
        // REJECTED hash (a subset of the attempted prefix) — needed below to re-park a `NonceTooHigh`
        // rejection without re-running admission's own checks (it already passed them). Built here,
        // before `drained` is consumed by the not_executed branch below.
        let mut rejected_drained: HashMap<TxHash, crate::admission::DrainedTx> = HashMap::new();
        if !result.outcome.rejected.is_empty() {
            let rejected_hash_set: std::collections::HashSet<TxHash> =
                result.outcome.rejected.iter().map(|r| r.tx_hash).collect();
            for d in &drained[..attempted_len] {
                if rejected_hash_set.contains(&d.parsed.tx_hash) {
                    rejected_drained.insert(
                        d.parsed.tx_hash,
                        crate::admission::DrainedTx {
                            parsed: d.parsed.clone(),
                            enqueued_at: d.enqueued_at,
                        },
                    );
                }
            }
        }

        // Anything the executor didn't reach goes back to the front of
        // admission's ready queue, ahead of whatever else gets admitted before the next tick — with its
        // original enqueued_at preserved, taken from the matching suffix of `drained` (the
        // executor's contract guarantees `not_executed` is exactly that tail, in order).
        if !result.outcome.not_executed.is_empty() {
            self.metrics
                .not_executed_carried_total
                .inc_by(result.outcome.not_executed.len() as u64);
            let carried: Vec<crate::admission::DrainedTx> =
                drained.into_iter().skip(attempted_len).collect();
            self.admission.requeue_front(carried);
        }

        {
            // One write-lock acquisition covers the whole sub-block's
            // insertions, not one per tx.
            let mut recent_preconfs = self.recent_preconfs.write().unwrap();
            let ack_at = Instant::now();
            for (position, tx_hash) in result.outcome.included.iter().enumerate() {
                let preconf = Preconfirmation {
                    tx_hash: *tx_hash,
                    block: result.header.block,
                    sub_block_index: result.header.index,
                    position: position as u32,
                    header_hash: result.header_hash,
                    signature: result.signature,
                };
                // This is the exact "ack" `Metrics::preconf_latency_seconds`'s
                // own doc names — the moment this tx's inclusion (its sub-block already fsynced to the
                // log by the time `seal_sub_block` returned above) is made visible, whether or not a
                // caller happens to be waiting on it right now. `enqueued_at_by_hash` was built above
                // from admission's own timestamp for every tx this tick actually attempted.
                if let Some(enqueued_at) = enqueued_at_by_hash.get(tx_hash) {
                    self.metrics
                        .preconf_latency_seconds
                        .observe(ack_at.duration_since(*enqueued_at).as_secs_f64());
                }
                // Recorded regardless of whether anyone is waiting on it
                // — `rome_getPreconfirmation` can be polled by a caller that only got a `Parked` reply
                // earlier.
                recent_preconfs[0].insert(*tx_hash, preconf.clone());
                if let Some(reply) = self.pending.remove(tx_hash) {
                    let _ = reply.send(Ok(SubmitOutcome::Preconfirmed(preconf)));
                }
            }
        }
        // `NonceTooHigh` is a genuine future nonce from the executor's
        // real perspective (not a tx to bounce back to its sender) — park it exactly like a fresh
        // out-of-gap admission instead, so it releases automatically once the gap fills. Every other
        // reason: admission's own nonce cache advanced `sender` to
        // `expected + 1` on the assumption this tx would succeed; the executor just proved otherwise,
        // so reset admission's view from the executor (the source of truth) and re-evaluate the
        // sender's parked entries against the corrected value — without this, every later tx from
        // `sender` is judged against a nonce that never actually advanced and parks forever.
        let mut terminally_rejected_hashes: Vec<TxHash> =
            Vec::with_capacity(result.outcome.rejected.len());
        for rejected in &result.outcome.rejected {
            if matches!(rejected.reason, Reason::NonceTooHigh { .. }) {
                if let Some(drained_tx) = rejected_drained.remove(&rejected.tx_hash) {
                    match self.admission.park_rejected(drained_tx, Instant::now()) {
                        Ok(()) => continue,
                        Err(_drained_tx) => {
                            // This sender's parked bucket is already at
                            // capacity — fall through to the ordinary terminal-reject path below rather
                            // than silently exceeding the per-sender bound.
                        }
                    }
                }
            }
            if let Some(reply) = self.pending.remove(&rejected.tx_hash) {
                let _ = reply.send(Err(SequencerError::Rejected(rejected.reason.clone())));
            }
            let real_nonce = self.sealer.executor.nonce(rejected.sender);
            self.admission
                .reconcile_sender_nonce(rejected.sender, real_nonce, Instant::now());
            terminally_rejected_hashes.push(rejected.tx_hash);
        }

        // Bound admission's dedup horizon by the actual outcome of this
        // sub-block, and rotate the two-block ring on a block boundary. A hash kept parked above is
        // deliberately excluded — it is still held by admission, not gone.
        self.admission
            .on_sub_block_sealed(&result.outcome.included, &terminally_rejected_hashes);
        if result.block_sealed.is_some() {
            self.admission.on_block_sealed();
            // Rotate the recent-preconfirmations horizon in lockstep with
            // admission's own dedup ring — same two-block shape.
            let mut recent_preconfs = self.recent_preconfs.write().unwrap();
            let current = std::mem::take(&mut recent_preconfs[0]);
            *recent_preconfs = [HashMap::new(), current];
        }

        self.metrics.sub_blocks_sealed_total.inc();
        self.metrics.txs_per_sub_block.observe(tx_count as f64);
        self.metrics
            .queue_depth
            .set(self.admission.ready_len() as i64);
        let lateness = start.elapsed().saturating_sub(self.seal_period);
        self.metrics
            .seal_lateness_seconds
            .observe(lateness.as_secs_f64());
        if result.block_sealed.is_some() {
            self.metrics.blocks_sealed_total.inc();
            // Reth block number == sequencer block number (the sequencer numbers
            // blocks from 1; reth block 0 is genesis, which this sequencer numbering never names —
            // see `RethExecutor`'s module doc for the exact mapping this mirrors).
            self.metrics
                .reth_canonical_chain_height
                .set(result.header.block as i64);
            // The gas/s source for the load programme, and the drift-bound catch-up-debt measurement.
            if let Some(gas) = result.block_gas_used {
                self.metrics.block_gas_used.observe(gas as f64);
            }
            if let Some(ahead) = result.block_timestamp_ahead_seconds {
                self.metrics.block_timestamp_ahead_seconds.observe(ahead);
            }
        }
        Ok(())
    }
}

/// Everything [`spawn`] needs beyond the executor itself.
pub struct SpawnConfig {
    pub admission: AdmissionConfig,
    pub log_dir: std::path::PathBuf,
    pub blocks_per_segment: u64,
    pub signer: alloy::signers::local::PrivateKeySigner,
    pub resume: ResumePoint,
    pub metrics: Arc<Metrics>,
    pub seal_period: Duration,
    pub preconf_feed_capacity: usize,
    /// Per-sub-block gas budget handed to the executor. Default
    /// [`DEFAULT_SUB_BLOCK_GAS_LIMIT`] (100M gas/s executor cap × 50 ms cadence); configurable.
    pub sub_block_gas_limit: u64,
    /// This chain's genesis gas limit, published into every block's
    /// `BlockEnv` (see [`crate::sealer::DEFAULT_BLOCK_GAS_LIMIT`]'s doc).
    pub block_gas_limit: u64,
    /// This chain's own fee recipient (genesis `coinbase` —
    /// `Address::ZERO` on Tiber), published into every block's `BlockEnv`. `--executor mock` has no
    /// genesis file to read (see the binary's own `fee_recipient_from_config`), so it stays
    /// `Address::ZERO` — unchanged from this crate's prior hardcoded behavior.
    pub fee_recipient: Address,
    /// Sub-blocks per block (design default [`crate::sealer::SUB_BLOCKS_PER_BLOCK`],
    /// 20 — a chain's profile may declare a different value, e.g. 40 at a 25 ms sub-block period).
    pub sub_blocks_per_block: u16,
    /// This chain's `[profile].empty_block_interval_secs` — 0 (never seal a block with
    /// no transactions) or a nonzero cadence, in seconds (`rome_zk_profile::Profile`'s own field).
    pub empty_block_interval_secs: u64,
}

/// Spawn a running sequencer: opens (or resumes) the ordered log, spawns the actor task, and returns a
/// handle plus the task's `JoinHandle`.
pub fn spawn<E: Executor + 'static>(
    executor: E,
    config: SpawnConfig,
) -> std::io::Result<(
    SequencerHandle,
    tokio::task::JoinHandle<Result<(), SequencerFatal>>,
)> {
    let log = LogWriter::open(&config.log_dir, config.blocks_per_segment)?;
    let sink = ChannelSink::new(config.preconf_feed_capacity);
    let handle_sink = sink.clone();
    let chain_id = config.admission.chain_id;

    let sealer = SealerState::new(
        executor,
        log,
        config.signer,
        sink,
        chain_id,
        config.block_gas_limit,
        config.fee_recipient,
        config.sub_blocks_per_block,
        config.resume,
    )
    .with_empty_block_interval_secs(config.empty_block_interval_secs);
    let admission = Admission::new(config.admission);
    let (tx, rx) = mpsc::channel(1_024);
    // Shared between the actor (writer) and every `SequencerHandle`
    // (direct reader) — see `RecentPreconfs`'s doc.
    let recent_preconfs: RecentPreconfs =
        Arc::new(std::sync::RwLock::new([HashMap::new(), HashMap::new()]));

    let actor = Actor {
        admission,
        sealer,
        pending: HashMap::new(),
        metrics: config.metrics,
        seal_period: config.seal_period,
        sub_block_gas_limit: config.sub_block_gas_limit,
        recent_preconfs: recent_preconfs.clone(),
    };
    let join = tokio::spawn(actor.run(rx));

    Ok((
        SequencerHandle {
            tx,
            sink: handle_sink,
            chain_id,
            recent_preconfs,
        },
        join,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{MockExecutor, DEFAULT_SUB_BLOCK_GAS_LIMIT};
    use crate::testutil::signed_raw_tx;
    use alloy::signers::local::PrivateKeySigner;
    use tempfile::tempdir;

    /// Waits, bounded, for the sealed sub-block on `feed` whose header hash is `preconf`'s, and proves
    /// everything skipped on the way was a carry. The feed carries every sealed sub-block, and a tick
    /// executes its txs against `start + seal_period`, so on a loaded host the deadline can pass before
    /// the tx is reached (`MockExecutor::execute_sub_block` checks `Instant::now() >= limits.deadline`
    /// before each tx): the tx is carried (`not_executed` -> requeued), the tick still seals an empty
    /// sub-block (an open block completes every sub-block, empty or not), and the tx seals one sub-block
    /// later. So the only sub-blocks that may precede the tx's are empty ones, exactly one per carry,
    /// and a `Lagged` receive counts the sub-blocks it evicted as skipped. Anything else (a non-empty
    /// sub-block before the tx's, or more or fewer skipped than carries) fails.
    async fn wait_for_preconfirmed_sub_block(
        feed: &mut broadcast::Receiver<SealedSubBlock>,
        preconf: &Preconfirmation,
        metrics: &Metrics,
    ) -> SealedSubBlock {
        let mut skipped: u64 = 0;
        let sealed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match feed.recv().await {
                    Ok(item) if item.header_hash == preconf.header_hash => break item,
                    Ok(item) => {
                        assert!(
                            item.included.is_empty(),
                            "a sub-block before the preconfirmed tx's must be an empty carry tick, \
                             got included = {:?}",
                            item.included
                        );
                        skipped += 1;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => skipped += n,
                    Err(broadcast::error::RecvError::Closed) => {
                        panic!("the preconfirmation feed closed before the tx's sub-block")
                    }
                }
            }
        })
        .await
        .expect(
            "no sealed sub-block on the feed matched the preconfirmation's header_hash within 10 s",
        );
        assert_eq!(
            skipped,
            metrics.not_executed_carried_total.get(),
            "every sub-block before the tx's must be explained by exactly one carry of the tx"
        );
        sealed
    }

    /// At the design's own 5,000-tx/block load point (5k
    /// transfers), under the profile's DEFAULT values (50 ms sub-block, 20/block, 5,000,000 sub-block
    /// gas limit — i.e. no cadence override at all), `seal_lateness_seconds` must stay exactly 0 —
    /// every tick completes inside its 50 ms deadline (`Actor::tick`'s `start.elapsed()
    /// .saturating_sub(seal_period)` is exactly `Duration::ZERO` whenever a tick finishes on time, so
    /// this is a real equality, not a fuzzy bound). Performance-sensitive (real ECDSA recovery over
    /// 5,000 txs) like this crate's other 5k-scale measurements — run with `--release --ignored`.
    #[ignore = "performance-sensitive; run with --release --nocapture -- --ignored, see comment above"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn seal_lateness_stays_zero_at_5k_txs_per_block_under_profile_defaults() {
        let profile = crate::profile::Profile::default();
        profile.validate().unwrap();

        let dir = tempdir().unwrap();
        let metrics = Metrics::new();
        let (handle, _join) = spawn(
            MockExecutor::new(),
            SpawnConfig {
                admission: AdmissionConfig {
                    chain_id: 1,
                    queue_capacity: 8_192,
                    ..AdmissionConfig::default()
                },
                log_dir: dir.path().to_path_buf(),
                blocks_per_segment: 1_000,
                signer: PrivateKeySigner::random(),
                resume: ResumePoint::default(),
                metrics: metrics.clone(),
                seal_period: std::time::Duration::from_millis(profile.sub_block_ms),
                preconf_feed_capacity: 1_024,
                sub_block_gas_limit: profile.sub_block_gas_limit,
                block_gas_limit: profile.effective_block_gas_limit(),
                fee_recipient: Address::ZERO,
                sub_blocks_per_block: profile.sub_blocks_per_block,
                empty_block_interval_secs: profile.empty_block_interval_secs,
            },
        )
        .unwrap();

        const SENDERS: usize = 50;
        const TXS_PER_SENDER: usize = 100; // 50 * 100 = 5,000, the design's own load point.
        let mut tasks = Vec::with_capacity(SENDERS * TXS_PER_SENDER);
        for _ in 0..SENDERS {
            let signer = PrivateKeySigner::random();
            for nonce in 0..TXS_PER_SENDER as u64 {
                let handle = handle.clone();
                let raw = signed_raw_tx(&signer, 1, nonce);
                tasks.push(tokio::spawn(async move { handle.submit_raw_tx(raw).await }));
            }
        }
        for t in tasks {
            t.await.unwrap().unwrap();
        }

        assert_eq!(
            metrics.seal_lateness_seconds.get_sample_sum(),
            0.0,
            "every tick at the 5k-tx/block design load point must finish inside its {} ms deadline \
             under the profile's default values",
            profile.sub_block_ms
        );
        assert!(
            metrics.sub_blocks_sealed_total.get() > 0,
            "the run must actually have sealed sub-blocks"
        );
    }

    /// "Off-tick pipelined sealing" measurement. Wraps [`MockExecutor`] with an injectable delay on
    /// `seal_block` only (the once-per-block call, measured 139 ms at 5k txs in an earlier build) —
    /// everything else delegates straight through.
    #[derive(Clone)]
    struct SlowSealExecutor {
        inner: std::sync::Arc<tokio::sync::Mutex<MockExecutor>>,
        seal_delay: Duration,
    }
    impl SlowSealExecutor {
        fn new(seal_delay: Duration) -> Self {
            Self {
                inner: std::sync::Arc::new(tokio::sync::Mutex::new(MockExecutor::new())),
                seal_delay,
            }
        }
    }
    impl Executor for SlowSealExecutor {
        async fn open_block(
            &mut self,
            env: crate::executor::BlockEnv,
        ) -> Result<(), crate::executor::ExecutorError> {
            self.inner.lock().await.open_block(env).await
        }
        async fn execute_sub_block(
            &mut self,
            txs: &[Bytes],
            limits: SubBlockLimits,
        ) -> Result<crate::executor::SubBlockOutcome, crate::executor::ExecutorError> {
            self.inner.lock().await.execute_sub_block(txs, limits).await
        }
        async fn seal_block(
            &mut self,
            inputs: crate::executor::BlockSealInputs,
        ) -> Result<crate::executor::BlockOutcome, crate::executor::ExecutorError> {
            tokio::time::sleep(self.seal_delay).await;
            self.inner.lock().await.seal_block(inputs).await
        }
        fn head(&self) -> crate::executor::Head {
            self.inner.try_lock().map(|g| g.head()).unwrap_or_default()
        }
        fn nonce(&self, addr: Address) -> u64 {
            self.inner
                .try_lock()
                .map(|g| g.nonce(addr))
                .unwrap_or_default()
        }
    }

    /// A measurement, not a pass/fail gate: does a slow `seal_block` (139 ms in an earlier build)
    /// delay the ACTOR from admitting a brand-new submission, given the current single-actor
    /// design (`Actor::run`'s `tick().await` and `rx.recv()` are exclusive `select!` arms — see
    /// `sequencer.rs`'s module doc)? Runs 19 empty sub-blocks (no seal yet), then concurrently: (a)
    /// keeps ticking (the 20th sub-block triggers the slow seal), and (b) submits one fresh tx right
    /// after the 19th sub-block seals — timing how long its own preconfirmation takes end to end.
    /// `rome-zk-executor-reth`'s OWN seal_block (measured separately) already
    /// backgrounds its expensive part (the MDBX write); this measures the ACTOR-level
    /// question, which is independent of which `Executor` is behind it.
    #[ignore = "measurement, not a pass/fail gate; timing-sensitive"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn measure_whether_a_slow_seal_delays_admission_of_a_fresh_submission() {
        let dir = tempdir().unwrap();
        let seal_delay = Duration::from_millis(139);
        let (handle, _join) = spawn(
            SlowSealExecutor::new(seal_delay),
            SpawnConfig {
                admission: AdmissionConfig {
                    chain_id: 1,
                    ..AdmissionConfig::default()
                },
                log_dir: dir.path().to_path_buf(),
                blocks_per_segment: 1_000,
                signer: PrivateKeySigner::random(),
                resume: ResumePoint::default(),
                metrics: Metrics::new(),
                seal_period: Duration::from_millis(50),
                preconf_feed_capacity: 16,
                sub_block_gas_limit: DEFAULT_SUB_BLOCK_GAS_LIMIT,
                block_gas_limit: crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
                fee_recipient: Address::ZERO,
                sub_blocks_per_block: crate::sealer::SUB_BLOCKS_PER_BLOCK,
                empty_block_interval_secs: 0,
            },
        )
        .unwrap();

        // Let 19 sub-blocks tick by (empty, fast — no seal yet).
        tokio::time::sleep(Duration::from_millis(50 * 19 + 25)).await;

        // Submit right as the 20th (slow-seal) sub-block is expected to be in flight, and measure this
        // submission's own end-to-end preconfirmation latency.
        let signer = PrivateKeySigner::random();
        let raw = signed_raw_tx(&signer, 1, 0);
        let started = Instant::now();
        let outcome = handle.submit_raw_tx(raw).await.unwrap();
        let elapsed = started.elapsed();
        assert!(matches!(outcome, SubmitOutcome::Preconfirmed(_)));

        eprintln!(
            "MEASURED: a fresh submission during a {seal_delay:?} seal took {elapsed:?} end to end \
             (50 ms design budget for admission -> preconfirmation; the slow seal is injected once, on \
             the block boundary)"
        );
    }

    #[tokio::test]
    async fn submitted_tx_receives_a_preconfirmation_after_sealing() {
        let dir = tempdir().unwrap();
        let metrics = Metrics::new();
        let (handle, _join) = spawn(
            MockExecutor::new(),
            SpawnConfig {
                admission: AdmissionConfig {
                    chain_id: 1,
                    ..AdmissionConfig::default()
                },
                log_dir: dir.path().to_path_buf(),
                blocks_per_segment: 1_000,
                signer: PrivateKeySigner::random(),
                resume: ResumePoint::default(),
                metrics: metrics.clone(),
                seal_period: Duration::from_millis(10),
                preconf_feed_capacity: 16,
                sub_block_gas_limit: DEFAULT_SUB_BLOCK_GAS_LIMIT,
                block_gas_limit: crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
                fee_recipient: Address::ZERO,
                sub_blocks_per_block: crate::sealer::SUB_BLOCKS_PER_BLOCK,
                empty_block_interval_secs: 0,
            },
        )
        .unwrap();

        let sender = PrivateKeySigner::random();
        let raw = signed_raw_tx(&sender, 1, 0);
        let outcome = handle.submit_raw_tx(raw).await.unwrap();
        let SubmitOutcome::Preconfirmed(preconf) = outcome else {
            panic!("a fresh sender's nonce-0 tx must admit Ready, got {outcome:?}");
        };
        // Strict, but explained by a carry: on a loaded host the sub-block deadline can pass before the
        // tx is reached, so the tx is carried and seals in a later sub-block, one empty sub-block per
        // carry (see `wait_for_preconfirmed_sub_block`). The carry counter is bumped before the tick
        // seals and replies, so it already covers every carry that precedes this preconfirmation.
        assert_eq!(preconf.block, 1);
        assert_eq!(preconf.position, 0);
        assert_eq!(
            u64::from(preconf.sub_block_index),
            metrics.not_executed_carried_total.get(),
            "the tx's sub-block index must equal the number of times it was carried"
        );
    }

    /// `Metrics::preconf_latency_seconds` and `Metrics::
    /// log_fsync_duration_seconds` existed but neither was ever observed — this wires up a
    /// per-sub-block ack-latency histogram and a log-fsync-duration histogram on the metrics that
    /// were already there for exactly this purpose.
    #[tokio::test]
    async fn preconf_ack_and_log_fsync_histograms_record_a_real_observation() {
        let dir = tempdir().unwrap();
        let metrics = Metrics::new();
        let (handle, _join) = spawn(
            MockExecutor::new(),
            SpawnConfig {
                admission: AdmissionConfig {
                    chain_id: 1,
                    ..AdmissionConfig::default()
                },
                log_dir: dir.path().to_path_buf(),
                blocks_per_segment: 1_000,
                signer: PrivateKeySigner::random(),
                resume: ResumePoint::default(),
                metrics: metrics.clone(),
                seal_period: Duration::from_millis(10),
                preconf_feed_capacity: 16,
                sub_block_gas_limit: DEFAULT_SUB_BLOCK_GAS_LIMIT,
                block_gas_limit: crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
                fee_recipient: Address::ZERO,
                sub_blocks_per_block: crate::sealer::SUB_BLOCKS_PER_BLOCK,
                empty_block_interval_secs: 0,
            },
        )
        .unwrap();

        let sender = PrivateKeySigner::random();
        let raw = signed_raw_tx(&sender, 1, 0);
        handle.submit_raw_tx(raw).await.unwrap();

        assert_eq!(
            metrics.preconf_latency_seconds.get_sample_count(),
            1,
            "the one preconfirmed tx must have recorded exactly one ack-latency observation"
        );
        assert!(
            metrics.preconf_latency_seconds.get_sample_sum() > 0.0,
            "a real elapsed duration from admission to ack must be positive"
        );
        assert!(
            metrics.log_fsync_duration_seconds.get_sample_count() >= 1,
            "the sub-block this tx sealed in must have recorded a log-fsync observation"
        );
    }

    /// `rome_getPreconfirmation` must be servable directly off a shared,
    /// actor-independent structure — never by round-tripping through the actor's own bounded command
    /// channel, which also carries `eth_sendRawTransaction`/`rome_sendRawTransaction`. Calling
    /// `get_preconfirmation` here with **no `.await`** is the compile-time proof: the old
    /// `Command::GetPreconfirmation` round trip required an async send + oneshot reply, so this line
    /// would not even compile against that implementation.
    #[tokio::test]
    async fn get_preconfirmation_is_a_synchronous_direct_read() {
        let dir = tempdir().unwrap();
        let (handle, _join) = spawn(
            MockExecutor::new(),
            SpawnConfig {
                admission: AdmissionConfig {
                    chain_id: 1,
                    ..AdmissionConfig::default()
                },
                log_dir: dir.path().to_path_buf(),
                blocks_per_segment: 1_000,
                signer: PrivateKeySigner::random(),
                resume: ResumePoint::default(),
                metrics: Metrics::new(),
                seal_period: Duration::from_millis(10),
                preconf_feed_capacity: 16,
                sub_block_gas_limit: DEFAULT_SUB_BLOCK_GAS_LIMIT,
                block_gas_limit: crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
                fee_recipient: Address::ZERO,
                sub_blocks_per_block: crate::sealer::SUB_BLOCKS_PER_BLOCK,
                empty_block_interval_secs: 0,
            },
        )
        .unwrap();
        // No tx was ever submitted — nothing to find, but the call itself must be synchronous.
        let found = handle.get_preconfirmation(TxHash::ZERO);
        assert!(found.is_none());
    }

    /// End to end: a submitted tx's pre-confirmation must be servable via
    /// `get_preconfirmation` bounded by a generous, scheduler-noise-tolerant budget (50ms) even while the
    /// actor's own command channel is being kept busy by a large burst of concurrent submissions. Under
    /// the old actor-routed implementation, `get_preconfirmation` shared the same bounded `mpsc` channel
    /// (capacity 1,024) as every one of those submissions and could queue up behind them; the direct
    /// shared-state read has no such dependency.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn get_preconfirmation_stays_fast_under_a_burst_of_concurrent_submissions() {
        let dir = tempdir().unwrap();
        let (handle, _join) = spawn(
            MockExecutor::new(),
            SpawnConfig {
                admission: AdmissionConfig {
                    chain_id: 1,
                    queue_capacity: 4_096,
                    ..AdmissionConfig::default()
                },
                log_dir: dir.path().to_path_buf(),
                blocks_per_segment: 1_000,
                signer: PrivateKeySigner::random(),
                resume: ResumePoint::default(),
                metrics: Metrics::new(),
                seal_period: Duration::from_millis(10),
                preconf_feed_capacity: 16,
                sub_block_gas_limit: DEFAULT_SUB_BLOCK_GAS_LIMIT,
                block_gas_limit: crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
                fee_recipient: Address::ZERO,
                sub_blocks_per_block: crate::sealer::SUB_BLOCKS_PER_BLOCK,
                empty_block_interval_secs: 0,
            },
        )
        .unwrap();

        let sender = PrivateKeySigner::random();
        let raw = signed_raw_tx(&sender, 1, 0);
        let outcome = handle.submit_raw_tx(raw).await.unwrap();
        let SubmitOutcome::Preconfirmed(preconf) = outcome else {
            panic!("expected Preconfirmed, got {outcome:?}");
        };

        // Flood the actor's command channel with 2,000 concurrent submissions it hasn't drained yet.
        for _ in 0..2_000u32 {
            let handle = handle.clone();
            tokio::spawn(async move {
                let s = PrivateKeySigner::random();
                let raw = signed_raw_tx(&s, 1, 0);
                let _ = handle.submit_raw_tx(raw).await;
            });
        }

        for _ in 0..200u32 {
            let started = Instant::now();
            let found = handle.get_preconfirmation(preconf.tx_hash);
            let elapsed = started.elapsed();
            assert_eq!(found, Some(preconf.clone()));
            assert!(
                elapsed < Duration::from_millis(50),
                "get_preconfirmation took {elapsed:?}, must stay well under budget even under a burst \
                 of concurrent submissions"
            );
        }
    }

    #[tokio::test]
    async fn wrong_chain_id_is_rejected_without_waiting_for_a_seal() {
        let dir = tempdir().unwrap();
        let (handle, _join) = spawn(
            MockExecutor::new(),
            SpawnConfig {
                admission: AdmissionConfig {
                    chain_id: 1,
                    ..AdmissionConfig::default()
                },
                log_dir: dir.path().to_path_buf(),
                blocks_per_segment: 1_000,
                signer: PrivateKeySigner::random(),
                resume: ResumePoint::default(),
                metrics: Metrics::new(),
                seal_period: Duration::from_millis(50),
                preconf_feed_capacity: 16,
                sub_block_gas_limit: DEFAULT_SUB_BLOCK_GAS_LIMIT,
                block_gas_limit: crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
                fee_recipient: Address::ZERO,
                sub_blocks_per_block: crate::sealer::SUB_BLOCKS_PER_BLOCK,
                empty_block_interval_secs: 0,
            },
        )
        .unwrap();

        let sender = PrivateKeySigner::random();
        let raw = signed_raw_tx(&sender, 999, 0);
        let err = handle.submit_raw_tx(raw).await.unwrap_err();
        assert!(matches!(
            err,
            SequencerError::Admission(AdmissionError::WrongChainId {
                expected: 1,
                got: Some(999)
            })
        ));
    }

    #[tokio::test]
    async fn preconfirmation_feed_streams_sealed_sub_blocks() {
        let dir = tempdir().unwrap();
        let metrics = Metrics::new();
        let (handle, _join) = spawn(
            MockExecutor::new(),
            SpawnConfig {
                admission: AdmissionConfig {
                    chain_id: 1,
                    ..AdmissionConfig::default()
                },
                log_dir: dir.path().to_path_buf(),
                blocks_per_segment: 1_000,
                signer: PrivateKeySigner::random(),
                resume: ResumePoint::default(),
                metrics: metrics.clone(),
                seal_period: Duration::from_millis(10),
                preconf_feed_capacity: 16,
                sub_block_gas_limit: DEFAULT_SUB_BLOCK_GAS_LIMIT,
                block_gas_limit: crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
                fee_recipient: Address::ZERO,
                sub_blocks_per_block: crate::sealer::SUB_BLOCKS_PER_BLOCK,
                empty_block_interval_secs: 0,
            },
        )
        .unwrap();
        let mut feed = handle.subscribe_preconfirmations();

        let sender = PrivateKeySigner::random();
        let raw = signed_raw_tx(&sender, 1, 0);
        let outcome = handle.submit_raw_tx(raw).await.unwrap();
        let SubmitOutcome::Preconfirmed(preconf) = outcome else {
            panic!("a fresh sender's nonce-0 tx must admit Ready, got {outcome:?}");
        };

        let sealed = wait_for_preconfirmed_sub_block(&mut feed, &preconf, &metrics).await;
        assert_eq!(sealed.header_hash, preconf.header_hash);
        assert_eq!(sealed.included, vec![preconf.tx_hash]);
    }

    /// An executor that rejects the very first tx it ever sees for a
    /// sender (simulating a reason admission cannot predict at admit time, e.g. insufficient funds)
    /// diverges from admission's nonce cache, which already advanced the sender to `Ready` on the
    /// assumption the tx would succeed. Once the reconciliation fires, the exact same nonce must be
    /// admittable again and this time reach the executor as `Ready`.
    #[tokio::test]
    async fn rejected_tx_reconciles_admission_nonce_so_resubmission_is_ready() {
        use crate::executor::{
            BlockEnv, BlockOutcome, BlockSealInputs, ExecutorError, Head, MockExecutor, Reason,
            RejectedTx, SubBlockLimits, SubBlockOutcome,
        };
        use crate::tx::parse;
        use alloy::primitives::Address;

        /// Rejects every tx exactly once per sender (by hash), then delegates to a real `MockExecutor`
        /// for everything else — so a resubmission of the same nonce after the first rejection behaves
        /// exactly as a fresh admission would.
        #[derive(Default)]
        struct RejectOnceExecutor {
            inner: MockExecutor,
            already_rejected: std::collections::HashSet<alloy::primitives::TxHash>,
        }
        impl Executor for RejectOnceExecutor {
            async fn open_block(&mut self, env: BlockEnv) -> Result<(), ExecutorError> {
                self.inner.open_block(env).await
            }
            async fn execute_sub_block(
                &mut self,
                txs: &[Bytes],
                limits: SubBlockLimits,
            ) -> Result<SubBlockOutcome, ExecutorError> {
                let mut rejected = Vec::new();
                let mut remaining = Vec::new();
                for tx in txs {
                    let parsed = parse(tx.clone()).unwrap();
                    if self.already_rejected.insert(parsed.tx_hash) {
                        rejected.push(RejectedTx {
                            tx_hash: parsed.tx_hash,
                            sender: parsed.sender,
                            reason: Reason::Other("simulated rejection".to_string()),
                        });
                    } else {
                        remaining.push(tx.clone());
                    }
                }
                let mut outcome = self.inner.execute_sub_block(&remaining, limits).await?;
                outcome.rejected.splice(0..0, rejected);
                Ok(outcome)
            }
            async fn seal_block(
                &mut self,
                inputs: BlockSealInputs,
            ) -> Result<BlockOutcome, ExecutorError> {
                self.inner.seal_block(inputs).await
            }
            fn head(&self) -> Head {
                self.inner.head()
            }
            fn nonce(&self, addr: Address) -> u64 {
                self.inner.nonce(addr)
            }
        }

        let dir = tempdir().unwrap();
        let (handle, _join) = spawn(
            RejectOnceExecutor::default(),
            SpawnConfig {
                admission: AdmissionConfig {
                    chain_id: 1,
                    ..AdmissionConfig::default()
                },
                log_dir: dir.path().to_path_buf(),
                blocks_per_segment: 1_000,
                signer: PrivateKeySigner::random(),
                resume: ResumePoint::default(),
                metrics: Metrics::new(),
                seal_period: Duration::from_millis(10),
                preconf_feed_capacity: 16,
                sub_block_gas_limit: DEFAULT_SUB_BLOCK_GAS_LIMIT,
                block_gas_limit: crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
                fee_recipient: Address::ZERO,
                sub_blocks_per_block: crate::sealer::SUB_BLOCKS_PER_BLOCK,
                empty_block_interval_secs: 0,
            },
        )
        .unwrap();

        let sender = PrivateKeySigner::random();
        let raw = signed_raw_tx(&sender, 1, 0);

        // First submission: admitted Ready (admission's fresh nonce cache seeds at 0, matches), but the
        // executor rejects it — a divergence between admission's advanced nonce and the executor's real
        // (unmoved) nonce.
        let first = handle.submit_raw_tx(raw.clone()).await;
        assert!(
            matches!(first, Err(SequencerError::Rejected(_))),
            "first submission must be rejected at execution: {first:?}"
        );

        // Resubmitting the exact same nonce must be admittable as Ready, not StaleNonce — proving
        // admission's nonce cache was reconciled back to the executor's real (unmoved) value.
        let second = handle.submit_raw_tx(raw).await;
        assert!(
            second.is_ok(),
            "resubmission of the same nonce after reconciliation must succeed as Ready: {second:?}"
        );
    }

    /// An executor error is fatal (state unknown) but must never panic
    /// the actor task. It shuts down cleanly — the `JoinHandle` resolves `Err(SequencerFatal)` — and any
    /// sender still waiting on a pending pre-confirmation gets `SequencerError::Shutdown`, not a hung
    /// oneshot or a crashed process.
    #[tokio::test]
    async fn executor_fatal_error_shuts_down_cleanly_and_signals_pending_senders() {
        use crate::executor::{
            BlockEnv, BlockOutcome, BlockSealInputs, ExecutorError, Head, MockExecutor,
            SubBlockOutcome,
        };
        use alloy::primitives::Address;

        #[derive(Default)]
        struct AlwaysFailingExecutor {
            inner: MockExecutor,
        }
        impl Executor for AlwaysFailingExecutor {
            async fn open_block(&mut self, env: BlockEnv) -> Result<(), ExecutorError> {
                self.inner.open_block(env).await
            }
            async fn execute_sub_block(
                &mut self,
                _txs: &[Bytes],
                _limits: SubBlockLimits,
            ) -> Result<SubBlockOutcome, ExecutorError> {
                Err(ExecutorError::Malformed(
                    "simulated fatal executor error".into(),
                ))
            }
            async fn seal_block(
                &mut self,
                inputs: BlockSealInputs,
            ) -> Result<BlockOutcome, ExecutorError> {
                self.inner.seal_block(inputs).await
            }
            fn head(&self) -> Head {
                self.inner.head()
            }
            fn nonce(&self, addr: Address) -> u64 {
                self.inner.nonce(addr)
            }
        }

        let dir = tempdir().unwrap();
        let (handle, join) = spawn(
            AlwaysFailingExecutor::default(),
            SpawnConfig {
                admission: AdmissionConfig {
                    chain_id: 1,
                    ..AdmissionConfig::default()
                },
                log_dir: dir.path().to_path_buf(),
                blocks_per_segment: 1_000,
                signer: PrivateKeySigner::random(),
                resume: ResumePoint::default(),
                metrics: Metrics::new(),
                seal_period: Duration::from_millis(10),
                preconf_feed_capacity: 16,
                sub_block_gas_limit: DEFAULT_SUB_BLOCK_GAS_LIMIT,
                block_gas_limit: crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
                fee_recipient: Address::ZERO,
                sub_blocks_per_block: crate::sealer::SUB_BLOCKS_PER_BLOCK,
                empty_block_interval_secs: 0,
            },
        )
        .unwrap();

        let sender = PrivateKeySigner::random();
        let raw = signed_raw_tx(&sender, 1, 0);

        // Submitted before the fatal tick; the reply is still pending in the actor when it dies.
        let submit = tokio::spawn(async move { handle.submit_raw_tx(raw).await });

        let join_result = tokio::time::timeout(Duration::from_secs(2), join)
            .await
            .expect("actor task must exit, not hang")
            .expect("actor task must not panic");
        assert!(
            join_result.is_err(),
            "the actor's JoinHandle must resolve Err(SequencerFatal) on a fatal executor error"
        );

        let submit_result = tokio::time::timeout(Duration::from_secs(2), submit)
            .await
            .expect("pending submit must resolve, not hang")
            .expect("submit task must not panic");
        assert!(
            matches!(submit_result, Err(SequencerError::Shutdown)),
            "a pending sender must receive SequencerError::Shutdown, got {submit_result:?}"
        );
    }

    /// An idle actor at interval 0 records idle ticks and seals nothing; the first block after idle
    /// records exactly one `block_gas_used` and one `block_timestamp_ahead_seconds` observation. This
    /// pins `Actor::tick`'s OWN recording sites — the metrics module's render test drives the sealer
    /// directly and would stay green with either site deleted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_quiet_actor_records_idle_ticks_and_the_first_block_after_idle_records_gas_and_debt()
    {
        let dir = tempdir().unwrap();
        let metrics = Metrics::new();
        let (handle, _join) = spawn(
            MockExecutor::new(),
            SpawnConfig {
                admission: AdmissionConfig {
                    chain_id: 1,
                    ..AdmissionConfig::default()
                },
                log_dir: dir.path().to_path_buf(),
                blocks_per_segment: 1_000,
                signer: PrivateKeySigner::random(),
                resume: ResumePoint::default(),
                metrics: metrics.clone(),
                seal_period: Duration::from_millis(5),
                preconf_feed_capacity: 64,
                sub_block_gas_limit: DEFAULT_SUB_BLOCK_GAS_LIMIT,
                block_gas_limit: crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
                fee_recipient: Address::ZERO,
                sub_blocks_per_block: crate::sealer::SUB_BLOCKS_PER_BLOCK,
                empty_block_interval_secs: 0,
            },
        )
        .unwrap();

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            metrics.sub_blocks_sealed_total.get(),
            0,
            "a quiet actor at interval 0 must seal nothing"
        );
        assert!(
            metrics.idle_ticks_total.get() > 0,
            "the actor's own tick must count its idle ticks"
        );
        assert_eq!(metrics.block_gas_used.get_sample_count(), 0);

        let sender = PrivateKeySigner::random();
        handle
            .submit_raw_tx(signed_raw_tx(&sender, 1, 0))
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while metrics.blocks_sealed_total.get() < 1 {
            assert!(
                Instant::now() < deadline,
                "the first block after idle must seal once the tx is in"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            metrics.block_gas_used.get_sample_count(),
            1,
            "exactly one block_gas_used observation per sealed block, recorded by the actor"
        );
        assert_eq!(
            metrics.block_timestamp_ahead_seconds.get_sample_count(),
            1,
            "exactly one catch-up-debt observation per sealed block, recorded by the actor"
        );
    }
}
