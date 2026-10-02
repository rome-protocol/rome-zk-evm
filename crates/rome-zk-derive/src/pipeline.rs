//! `DerivePipeline`: wires every stage end to end and drives it either once
//! (`--once`: derive everything final and exit) or continuously (follow the inbox forever).

use std::sync::Arc;
use std::time::Instant;

use alloy_primitives::Address;

use crate::batch_queue::{self, DriftBound};
use crate::channel_bank::ChannelBank;
use crate::engine::{BlockOutcome, EngineApi, EngineController};
use crate::frame_queue;
use crate::inbox::InboxRetrieval;
use crate::metrics::Metrics;
use crate::reader::AccountReader;
use crate::traversal::{BatchRef, SolanaTraversal};
use crate::{attributes, PipelineError};

/// One [`DerivePipeline::step`]'s result.
#[derive(Debug)]
pub enum StepOutcome {
    /// No new finalized batch beyond the cursor — nothing to do (a Temporary condition upstream, but
    /// not an error: this is the pipeline's ordinary idle state).
    Idle,
    /// One batch was fully derived through the engine, in block order.
    Derived {
        batch: u64,
        blocks: Vec<BlockOutcome>,
    },
}

pub struct DerivePipeline<R, E> {
    traversal: SolanaTraversal<R>,
    inbox: InboxRetrieval<R>,
    bank: ChannelBank,
    engine: EngineController<E>,
    chain_id: u64,
    /// The chain's own fee recipient (genesis `coinbase` — `Address::ZERO`
    /// on Tiber), threaded into `attributes::attributes_for_block` for every block this pipeline derives
    /// — the SAME value the guest embeds at compile time and the sequencer reads from its own
    /// loaded genesis, never a hardcoded literal here.
    fee_recipient: Address,
    /// The cap a batch's block count must not exceed (config/profile value, default
    /// [`crate::config::DEFAULT_BLOCKS_PER_BATCH`]). Threaded into [`batch_queue::decode_batch`].
    blocks_per_batch: u64,
    /// The design-numbered `number` of the last block this pipeline has successfully derived, `None`
    /// before the first batch this pipeline instance has EVER derived (also `None` when
    /// resuming right at the settlement genesis sentinel — see [`Self::with_last_design_block`]) —
    /// [`batch_queue::decode_batch`]'s cross-batch continuity check is driven from this, not
    /// from any per-batch arithmetic. Committed only once a whole batch derives successfully (see
    /// [`Self::step`]) — a failed attempt must never poison the next retry's continuity check.
    last_design_block: Option<u64>,
    /// The one-sided timestamp drift bound applied to every batch's declared
    /// block timestamps, against that batch's own committed `open_unix_ts` anchor
    /// (`batch_queue::enforce_drift_bound`). Defaults to [`DriftBound::unbounded`] — a no-op — until
    /// [`Self::with_drift_bound`] wires in a real `max_drift_secs`; the shipped binary wires the chain's own
    /// `chain_config.max_drift_secs` (`chain_bound::chain_drift_bound`, reconciled against an optional
    /// TOML assertion by `config::reconcile_drift_bound`) — see `bin/rome_zk_derive.rs`.
    drift: DriftBound,
    /// Served on `Config.metrics_addr` by the binary — see [`Self::with_metrics`].
    /// Defaults to a private, unshared registry (`Metrics::new()`) so every existing caller keeps working
    /// unchanged; the binary wires in the SAME instance it serves `/metrics` from.
    metrics: Arc<Metrics>,
}

impl<R: AccountReader, E: EngineApi> DerivePipeline<R, E> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        traversal: SolanaTraversal<R>,
        inbox: InboxRetrieval<R>,
        engine: EngineController<E>,
        chain_id: u64,
        fee_recipient: Address,
        max_open_channels: usize,
        blocks_per_batch: u64,
    ) -> Self {
        Self {
            traversal,
            inbox,
            bank: ChannelBank::new(max_open_channels),
            engine,
            chain_id,
            fee_recipient,
            blocks_per_batch,
            last_design_block: None,
            drift: DriftBound::unbounded(),
            metrics: Metrics::new(),
        }
    }

    /// Wires in the SAME `Metrics` instance the binary serves `GET /metrics` from, so a
    /// caller reading the rendered text sees the live counters this pipeline updates. Meaningful only
    /// before the first [`Self::step`] call, same as [`Self::with_drift_bound`].
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = metrics;
        self
    }

    /// Wires in the one-sided drift bound: how many seconds a block's
    /// declared timestamp may exceed this batch's own committed `open_unix_ts` anchor by. Meaningful
    /// only before the first [`Self::step`] call, same as [`Self::with_last_design_block`].
    pub fn with_drift_bound(mut self, max_drift_secs: u64) -> Self {
        self.drift = DriftBound { max_drift_secs };
        self
    }

    /// The next batch id this pipeline will attempt to derive — where traversal resumes on the next
    /// `step`.
    pub fn next_batch(&self) -> u64 {
        self.traversal.next_batch()
    }

    /// Sets the `last_design_block` this pipeline resumes with — the settlement-root anchor's own
    /// continuity starting point (`crate::resume::resume_anchor`), or `None` for a
    /// genuinely fresh start. Meaningful only before the first [`Self::step`] call; a call afterward
    /// would silently discard whatever continuity this pipeline has already established.
    pub fn with_last_design_block(mut self, last_design_block: Option<u64>) -> Self {
        self.last_design_block = last_design_block;
        self
    }

    /// The underlying [`EngineController`] — mainly for tests to inspect a
    /// [`crate::engine::mock::MockEngineApi`]'s call log (a Critical batch must
    /// post nothing to the engine).
    pub fn engine_controller(&self) -> &EngineController<E> {
        &self.engine
    }

    /// The `Metrics` instance this pipeline observes into — for a test to read back the rendered text, or
    /// for a caller that built the pipeline without [`Self::with_metrics`] to still find the private
    /// default it is using.
    pub fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }

    /// Pulls and fully derives the next finalized batch, if any. An engine that disagrees with what this
    /// node is deriving is [`PipelineError::Critical`] (`PipelineError::Reset` and this
    /// method's former reaction to it — rewinding [`SolanaTraversal`] back to batch 0 and re-seeding the
    /// engine in-process — are deleted; nothing in this crate ever raised `Reset` for a live reason, see
    /// `crate::PipelineError`'s own doc). A restart re-seeds instead, from the settlement-root anchor
    /// (`crate::resume`) or the engine's own genesis
    /// (`crate::engine::EngineController::from_engine_head`).
    ///
    /// **A batch attempt is atomic w.r.t. the engine's own position.**
    /// [`Self::derive_one_batch`] calls [`EngineController::advance`] once per block, and `advance`
    /// commits its position (`next_height`/`parent_hash`) forward on every block that succeeds — so a
    /// non-Critical failure partway through a multi-block batch (exactly the conditions that are
    /// [`PipelineError::Temporary`]: an engine timeout, `newPayload` `Syncing`/`Accepted`)
    /// used to leave the controller sitting at `batch_start + k` blocks in, while nothing else about the
    /// attempt (the traversal cursor, `last_design_block`) had moved at all. The *next* `step` call then
    /// re-decodes the SAME batch from its own first block again — but the controller, unrewound, expects
    /// the height it was already `k` blocks past, and the `target_height == env.number`
    /// assertion fires as a spurious [`PipelineError::Critical`]. Snapshotting
    /// the engine's position before the attempt and restoring it on any non-Critical error makes the
    /// retry start the batch from the same expected height every time; every block that already
    /// succeeded then consolidates (a cheap read + compare — [`EngineController::advance`]'s own
    /// consolidation branch) instead of being rebuilt, and the rest builds for real.
    pub async fn step(&mut self) -> Result<StepOutcome, PipelineError> {
        // `SolanaTraversal::next` raises `PipelineError::Critical` directly too (e.g. a
        // v1-shaped batch account, `BatchDecode`) — an explicit match here, rather than `?`, so that raise
        // site is covered by the same `critical_total` choke point as `derive_one_batch`'s own errors
        // below, instead of bypassing it on the way out of this function.
        let batch_ref = match self.traversal.next().await {
            Ok(Some(b)) => b,
            Ok(None) => return Ok(StepOutcome::Idle),
            Err(e @ PipelineError::Critical(_)) => {
                self.metrics.critical_total.inc();
                return Err(e);
            }
            Err(e @ PipelineError::Temporary(_)) => return Err(e),
        };
        let batch_id = batch_ref.batch;
        let expected_first_block = self.last_design_block.map(|n| n + 1);
        let engine_position = self.engine.position();
        let started = Instant::now();

        match self.derive_one_batch(batch_ref, expected_first_block).await {
            Ok((last_block, blocks)) => {
                self.traversal.advance();
                self.last_design_block = Some(last_block);
                // One derived batch, once — never per-block, per-attempt, or
                // on a retried/consolidated re-derivation (this arm only runs on the FIRST successful
                // attempt; a `Temporary` retry rewinds below and does not reach here twice for the same
                // batch's own metrics).
                self.metrics.batches_derived_total.inc();
                self.metrics
                    .last_batch
                    .set(i64::try_from(batch_id).unwrap_or(i64::MAX));
                self.metrics
                    .head_block
                    .set(i64::try_from(last_block).unwrap_or(i64::MAX));
                self.metrics
                    .batch_seconds
                    .observe(started.elapsed().as_secs_f64());
                Ok(StepOutcome::Derived {
                    batch: batch_id,
                    blocks,
                })
            }
            Err(PipelineError::Temporary(msg)) => {
                // Undo whatever partial progress this attempt's engine calls made — the batch
                // itself was not committed (traversal did not advance, `last_design_block` did not
                // change), so the engine's own position must not have moved either.
                self.engine.rewind_to(engine_position);
                Err(PipelineError::Temporary(msg))
            }
            Err(e @ PipelineError::Critical(_)) => {
                // The one choke point every Critical raise passes through — see this
                // field's own doc. Never at each individual raise site (channel decode, drift bound,
                // engine disagreement, ...): a new Critical site anywhere in the pipeline is covered here
                // for free.
                self.metrics.critical_total.inc();
                Err(e)
            }
        }
    }

    /// Returns `(last block's design number, per-block outcomes)` on success — the caller commits
    /// `last_design_block` from the first element only once this whole batch (and nothing after it)
    /// has succeeded (see [`Self::step`]'s doc on why that ordering matters for a retried batch).
    async fn derive_one_batch(
        &mut self,
        batch_ref: BatchRef,
        expected_first_block: Option<u64>,
    ) -> Result<(u64, Vec<BlockOutcome>), PipelineError> {
        let chunk_bodies = self.inbox.chunks(batch_ref).await?;
        let frames = frame_queue::parse_frames(chunk_bodies, self.chain_id, batch_ref.batch)?;
        let channel_id = rome_zk_channel::channel_id(self.chain_id, batch_ref.batch);
        for frame in frames {
            self.bank.ingest(frame);
        }
        let compressed = self.bank.take_complete(channel_id)?.ok_or_else(|| {
            PipelineError::Critical(format!(
                "batch {}: channel did not reassemble even though every sealed chunk was read",
                batch_ref.batch
            ))
        })?;
        let blocks = batch_queue::decode_batch(
            &compressed,
            self.chain_id,
            batch_ref.batch,
            self.blocks_per_batch,
            expected_first_block,
        )?;

        // When there is no continuity anchor to check against (the very first batch this
        // pipeline instance ever derives — `expected_first_block == None`), this batch's first block
        // must still start exactly where the engine controller itself is seeded to build next. Checked
        // here, by name, so a genesis/anchor mismatch is never reported as `EngineController::advance`'s
        // unrelated "design premise violated" message (the fresh-engine-behind-the-anchor
        // case) — the two conditions are numerically the same check, but this one names
        // the actual cause. The design number equals the real height, with no offset.
        if expected_first_block.is_none() {
            let engine_next_height = self.engine.next_height();
            if blocks[0].number != engine_next_height {
                return Err(PipelineError::Critical(format!(
                    "batch {}: first design block {} does not match this engine controller's own \
                     next height {engine_next_height} — this engine was not seeded at the point this \
                     pipeline is about to start deriving from",
                    batch_ref.batch,
                    blocks[0].number,
                )));
            }
        }

        // The one-sided drift bound, checked against this batch's own
        // committed `open_unix_ts` anchor (a no-op under the default `DriftBound::unbounded()` until
        // `Self::with_drift_bound` wires in a real `max_drift_secs` — see `drift`'s field doc).
        // `OpenBatch` itself now refuses a negative `Clock::unix_timestamp` at the source
        // (`zk-inbox`'s `BatchError::NegativeUnixTimestamp`), so this is defense in depth for any account
        // this pipeline reads that predates that refusal (or that reached it by some other route) — it
        // is named as what it is ("negative — not a real Clock reading") rather than folded into anchor
        // 0, which used to surface as a drift-bound violation an operator would misdiagnose as a real
        // clock-drift incident.
        let anchor_unix_ts = u64::try_from(batch_ref.open_unix_ts).map_err(|_| {
            PipelineError::Critical(format!(
                "batch {}: committed open_unix_ts {} is negative — not a real Clock reading; refusing",
                batch_ref.batch, batch_ref.open_unix_ts
            ))
        })?;
        batch_queue::enforce_drift_bound(&blocks, anchor_unix_ts, self.drift)?;
        let last_block = blocks
            .last()
            .expect("decode_batch rejects an empty block list")
            .number;

        let mut outcomes = Vec::with_capacity(blocks.len());
        for block in &blocks {
            let attrs = attributes::attributes_for_block(self.chain_id, self.fee_recipient, block);
            outcomes.push(self.engine.advance(&attrs).await?);
        }
        Ok((last_block, outcomes))
    }

    /// `--once`: derive everything final right now, then return the number of batches
    /// derived — never blocks waiting for more.
    pub async fn run_once(&mut self) -> Result<u64, PipelineError> {
        let mut derived = 0u64;
        loop {
            match self.step().await {
                Ok(StepOutcome::Idle) => return Ok(derived),
                Ok(StepOutcome::Derived { .. }) => derived += 1,
                Err(e) => return Err(e),
            }
        }
    }

    /// Follows the inbox forever: backs off on [`StepOutcome::Idle`] and [`PipelineError::Temporary`]
    /// alike (no hot loop on either), and stops — the strict policy — on the
    /// first [`PipelineError::Critical`] (`PipelineError::Reset` is deleted; see
    /// `crate::PipelineError`'s own doc for why a restart is the only reaction this crate ever needed).
    pub async fn run_forever(&mut self) -> Result<(), PipelineError> {
        loop {
            match self.step().await {
                Ok(StepOutcome::Idle) => {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                Ok(StepOutcome::Derived { batch, blocks }) => {
                    tracing::info!(batch, blocks = blocks.len(), "derived batch");
                }
                Err(PipelineError::Temporary(msg)) => {
                    tracing::warn!(%msg, "temporary — retrying");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
                Err(critical @ PipelineError::Critical(_)) => return Err(critical),
            }
        }
    }
}
