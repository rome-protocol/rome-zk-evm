//! Wires `channel.rs` + `source.rs` + `sender.rs` + `resume.rs` into the full per-batch flow:
//! re-derive-before-send, then `OpenBatch` -> parallel per-frame **one V1 transaction each**
//! (`Open`+`Write`+`Seal`+`SealLeaf`) -> poll for `leaves_present == expected_count` ->
//! `FinalizeBatch` -> verify `acc` -> hand off to a [`crate::sink::PostRootSink`].
//!
//! Adopted from `OffchainLabs/nitro` `arbnode/batch_poster.go`'s `checkBatchCorrectness` (derive the
//! posted bytes back into blocks/txs and compare to the source **before** paying to post — see
//! [`re_derive_and_check`]) and `ethereum-optimism/optimism` `op-batcher/batcher/channel_manager.go`'s
//! "one channel, N frames, sent independently, reassembled by id+frame_no" shape (see `channel.rs`).
//!
//! ## One V1 tx per frame — the multi-stage split plan is withdrawn
//! The design has always assumed V1 (SIMD-0385, 4,096-byte envelope); the batcher was built against v0
//! (1,232 bytes) and needed a 3-hop `[Open+Write0] -> remaining Writes -> [Seal+SealLeaf]` split just to
//! fit a 3,681-byte frame body at all. V1 makes the whole chunk lane — `Open` + **one** `Write` of the
//! frame's entire body + `Seal` + `SealLeaf` — fit comfortably in one transaction (measured: the design's
//! own max frame is 4,052 B, `sender.rs`'s own
//! `design_frame_v1_tx_fits_4096_and_carries_both_header_limits` test), so there is nothing left to split.
//! [`WRITE_CHUNK_BYTES`], `ChunkPlan::{Combined,Split}` and [`fits_one_transaction`] (the plan-selector
//! that decided between them) are removed — [`plan_chunk`] now always returns the same four-instruction
//! `Vec<Instruction>`, sent as one V1 transaction by [`crate::sender::RpcSender`].

use futures_util::future::FutureExt;
use futures_util::stream::{FuturesUnordered, StreamExt};
use solana_program::instruction::Instruction;
use solana_program::pubkey::Pubkey;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::channel::{self, Block, Frame, FRAME_HEADER_LEN};
use crate::grouping::{CloseReason, PushOutcome, SizeCappedGrouper};
use crate::metrics::Metrics;
use crate::resolve::{self, AccountOps, ResolveError, ResolveOutcome};
use crate::sender::{FramePlan, SendTuning, Sender, SenderError};
use crate::sink::{FinalizedBatch, PostRootSink};

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("re-derive mismatch: {0}")]
    Rederive(String),
    #[error("channel codec error: {0}")]
    Channel(#[from] channel::ChannelError),
    #[error("send failed: {0}")]
    Send(#[from] SenderError),
    #[error("decode batch account: {0}")]
    Decode(#[from] zk_inbox_client::DecodeError),
    #[error("reading on-chain state: {0}")]
    AccountRead(#[from] ResolveError),
    #[error("batch account never reached leaves_present == expected_count after {polls} polls")]
    LeavesNeverComplete { polls: u32 },
    #[error(
        "on-chain acc {on_chain:02x?} does not match the reference commitment {reference:02x?}"
    )]
    AccMismatch {
        on_chain: [u8; 32],
        reference: [u8; 32],
    },
    #[error(
        "leaf {idx} on chain is {on_chain:02x?} but this run's own frame hashes to {expected:02x?} — \
         refusing before FinalizeBatch (once finalized, this batch can no longer be abandoned)"
    )]
    PreFinalizeLeafMismatch {
        idx: u32,
        on_chain: [u8; 32],
        expected: [u8; 32],
    },
    /// The startup resume found no way to finish batch `batch`: no grouping of the blocks in the log
    /// reproduces its present leaves under the configured frame size and drift bound (the zstd version, the
    /// frame size, `blocks_per_batch` or the log changed since the crash). Nothing was sent and nothing was
    /// abandoned; the chain waits for this id, so rerun with the build and config that opened it.
    #[error(
        "cannot resume batch {batch}: no grouping of the blocks in the log matches its {leaves_present} \
         present leaf(s) of {expected_count} under the configured frame size, blocks_per_batch and drift \
         bound — sent nothing, abandoned nothing; rerun with the build and config that opened it"
    )]
    ResumeImpossible {
        batch: u64,
        leaves_present: u32,
        expected_count: u32,
    },
    /// Reading what the startup resume needs (the batch's chunks, or the ordered log) failed.
    #[error("startup resume of batch {batch}: {reason}")]
    ResumeRead { batch: u64, reason: String },
    #[error("batch {batch}'s account is missing while polling it for finalize progress")]
    BatchVanished { batch: u64 },
    #[error("decoding the settlement root account: {0}")]
    RootDecode(#[from] zk_settlement_client::DecodeError),
    /// A previous batch in this posting window already failed — checked at the top of
    /// every `WindowedPoster::submit_group` call before anything else, so no further `OpenBatch` is ever
    /// sent once any sibling batch's own work has failed.
    #[error("a previous batch in this posting window failed — refusing to open another")]
    WindowPreviouslyFailed,
    /// The batch immediately before this one in the window failed (or vanished)
    /// before it finalized and signalled — this batch must not `FinalizeBatch` either, or an abandoned N
    /// beside a finalized N+1 re-posts N's blocks under a later id at the next start and derive meets
    /// block heights out of order. Left open-not-finalized; the next start resumes it.
    #[error(
        "batch {batch}: the previous batch in this posting window failed before it finalized — refusing \
         to FinalizeBatch out of order (the next start resumes it)"
    )]
    PreviousBatchFailed { batch: u64 },
    /// Defense-in-depth: `FinalizeBatch` is authority-gated on chain, so a third party can no longer finalize
    /// batch N+1 while N is still open. This refusal stays for any batch opened under a program version from
    /// before that gate (or straddling an upgrade): a `Finalized` id above an open-not-finalized one in the
    /// pending window means the open one's blocks sit before blocks already finalized. The startup resume
    /// refuses and sends nothing; the state stays repairable (the authority can finalize the open batch once
    /// its leaves are complete).
    #[error(
        "startup: batch {finalized} is finalized above open-not-finalized batch {open} in the pending \
         window — refusing to resume {open} out of order and sending nothing. Finalize batch {open} (or \
         repair by hand) and restart"
    )]
    FinalizedAboveOpenBatch { open: u64, finalized: u64 },
    /// A continuity break in the log (a missing block, or a single block that alone exceeds the frame
    /// budget) — surfaced by [`crate::grouping::SizeCappedGrouper::push`] and propagated here so
    /// [`push_and_post_until_accepted`] can return one error type; the caller treats this as fatal for
    /// the whole run (nothing further is posted), exactly as it did before this error was folded into
    /// [`PipelineError`].
    #[error("continuity break in the log: {0}")]
    Grouping(#[from] crate::grouping::GroupingError),
}

/// **Re-derive-before-send** (Nitro's `checkBatchCorrectness` shape): decode the exact bytes
/// about to be posted back into blocks and compare to the source, block-by-block and tx-by-tx, before any
/// fee is spent. Never trusts the encoder's own return value — independently walks the *compressed*
/// bytes back through [`channel::decode_stream`] first.
pub fn re_derive_and_check(blocks: &[Block], compressed: &[u8]) -> Result<(), PipelineError> {
    let decoded = channel::decode_stream(compressed)?;
    if decoded.len() != blocks.len() {
        return Err(PipelineError::Rederive(format!(
            "block count mismatch: source has {}, decoded has {}",
            blocks.len(),
            decoded.len()
        )));
    }
    for (i, (source, decoded)) in blocks.iter().zip(decoded.iter()).enumerate() {
        if source.number != decoded.number
            || source.timestamp != decoded.timestamp
            || source.gas_limit != decoded.gas_limit
        {
            return Err(PipelineError::Rederive(format!(
                "block {i}: header mismatch (source {source:?} vs decoded {decoded:?})"
            )));
        }
        if source.txs.len() != decoded.txs.len() {
            return Err(PipelineError::Rederive(format!(
                "block {i}: tx count mismatch: source {}, decoded {}",
                source.txs.len(),
                decoded.txs.len()
            )));
        }
        for (j, (s, d)) in source.txs.iter().zip(decoded.txs.iter()).enumerate() {
            if s != d {
                return Err(PipelineError::Rederive(format!(
                    "block {i} tx {j}: byte mismatch"
                )));
            }
        }
    }
    Ok(())
}

/// The `(program, payer, chain, batch)` quadruple every chunk/leaf/finalize instruction needs — grouped
/// so the send functions below stay under a sane argument count instead of threading four ids separately.
#[derive(Debug, Clone, Copy)]
pub struct BatchTarget {
    pub program_id: Pubkey,
    /// The settlement program this chain is registered under — every inbox account (cursor, batch,
    /// chunk) is keyed by it.
    pub settlement_program: Pubkey,
    pub payer: Pubkey,
    pub chain_id: u64,
    pub batch: u64,
}

/// One frame's on-chain instruction plan: `Open` + one `Write` of the frame's *entire*
/// body (≤ 3,681 B) + `Seal` + `SealLeaf`, sent as a single V1 transaction. Public so the
/// `solana-program-test` integration suite can drive exactly these instructions itself (BanksClient, not
/// the `Sender`/RPC seam `send_frame` uses) and prove they execute correctly.
pub type ChunkPlan = Vec<Instruction>;

/// Wraps a [`ChunkPlan`] into the single-stage, single-transaction [`FramePlan`]
/// `sender::run_send_and_confirm_many` drives (one stage per frame: the DAG executor
/// itself stays fully general; this is simply what every real frame's plan looks like now).
fn into_stages(plan: ChunkPlan) -> FramePlan {
    vec![vec![plan]]
}

/// Clock skew in seconds: `open_unix_ts` (the batch account's own committed Solana
/// clock reading) minus `send_wall_clock_unix_secs` (this batcher's own unix wall clock, captured
/// immediately before the `OpenBatch` send) — signed, so a negative result means this batcher's own clock
/// was already ahead of the moment the chain later stamped. A pure function so the observation itself is
/// testable against a decoded fake account, with no live RPC round trip. **Observability only: never a
/// decision input anywhere in this crate** — the drift bound this measures against lives in
/// `rome-zk-derive`, checked there against the same `open_unix_ts`, unaffected by this reading either way.
pub fn clock_skew_secs(open_unix_ts: i64, send_wall_clock_unix_secs: i64) -> f64 {
    (open_unix_ts - send_wall_clock_unix_secs) as f64
}

/// Sends `OpenBatch` followed by however many `GrowBatch` calls are needed to reach
/// `account_len(expected_count)`, all in **one transaction** (see
/// `zk_inbox_client::open_and_grow_batch_ixs`). Returns once that transaction confirms; the batch account
/// is then guaranteed to be at its full size before the chunk lane starts — `Open`/`SealLeaf`/
/// `FinalizeBatch` all fail closed (`BatchNotGrown`) against an undersized account otherwise.
pub async fn open_and_grow_batch<S: Sender>(
    sender: &S,
    target: BatchTarget,
    expected_count: u32,
    tuning: SendTuning,
) -> Result<solana_signature::Signature, PipelineError> {
    let ixs = zk_inbox_client::open_and_grow_batch_ixs(
        &target.program_id,
        &target.payer,
        target.chain_id,
        target.batch,
        expected_count,
        &target.settlement_program,
    );
    Ok(sender.send_and_confirm(&ixs, tuning).await?)
}

/// Everything the startup recovery needs: the posting window's own configuration (programs, payer, chain,
/// frame size, the send and finalize tuning, the posting-window depth) plus where the ordered log is and how
/// it groups. The binary and the tests build it the same way.
#[derive(Clone, Copy)]
pub struct StartupRecover<'a> {
    pub window: &'a WindowConfig,
    pub log_dir: &'a std::path::Path,
    pub sub_blocks_per_block: u16,
    pub block_gas_limit: u64,
    /// `profile.blocks_per_batch`: the resume search tries every group end up to this many blocks long.
    pub blocks_per_batch: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    #[error("recovering open batches in the pending window at startup failed: {0}")]
    Recover(#[from] PipelineError),
    #[error("failed to resolve the resume anchor: {0}")]
    Anchor(#[from] crate::anchor::AnchorError),
}

/// The batcher's whole startup recovery, in one place so a test can drive it exactly as a restarted
/// process does: first FINISH every open-not-finalized batch in the pending window (never abandon one: an
/// id the cursor has passed can never be opened again, and settlement still needs it — see
/// [`crate::recover`]), then resolve the on-chain anchor both modes resume from. The binary calls this
/// once, before the run loop starts. Finished batches are handed to `sink` like any other.
pub async fn startup_recover<A: AccountOps, S: Sender>(
    accounts: &A,
    sender: &S,
    metrics: &Metrics,
    sink: &dyn PostRootSink,
    cfg: &StartupRecover<'_>,
) -> Result<crate::anchor::Anchor, StartupError> {
    let resumed = crate::recover::resume_open_batches(accounts, sender, metrics, sink, cfg).await?;
    if !resumed.is_empty() {
        tracing::info!(
            "finished {} open batch(es) at startup: {resumed:?}",
            resumed.len()
        );
    }

    let w = cfg.window;
    Ok(crate::anchor::resolve_anchor(
        accounts,
        &w.inbox_program_id,
        &w.settlement_program_id,
        w.chain_id,
        cfg.log_dir,
        cfg.sub_blocks_per_block,
        cfg.block_gas_limit,
    )
    .await?)
}

/// Builds one frame's whole on-chain plan: `Open` + a single `Write` of the frame's
/// entire body (≤ 3,681 B, well inside the V1 envelope — see `sender.rs`'s own size-proof test) + `Seal` +
/// `SealLeaf`. No more per-`Write`-chunking (`WRITE_CHUNK_BYTES` is gone) and no combined-vs-split
/// decision — every frame's plan has exactly this shape.
pub fn plan_chunk(
    program_id: &Pubkey,
    payer: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
    batch: u64,
    idx: u32,
    payload: &[u8],
) -> ChunkPlan {
    let len = payload.len() as u32;
    let body_hash = zk_inbox_client::chunk_body_hash(payload);
    vec![
        zk_inbox_client::open_chunk_ix(
            program_id,
            payer,
            settlement_program,
            chain_id,
            batch,
            idx,
            len,
        ),
        zk_inbox_client::write_chunk_ix(
            program_id,
            payer,
            settlement_program,
            chain_id,
            batch,
            idx,
            0,
            payload.to_vec(),
        ),
        zk_inbox_client::seal_chunk_ix(
            program_id,
            payer,
            settlement_program,
            chain_id,
            batch,
            idx,
            len,
            body_hash,
        ),
        zk_inbox_client::seal_leaf_ix(program_id, settlement_program, chain_id, batch, idx),
    ]
}

/// Sends one frame end to end via the single-shot [`Sender`] seam: `Open`+`Write`+`Seal`+`SealLeaf` as one
/// V1 transaction. Returns once it confirms.
async fn send_frame<S: Sender>(
    sender: &S,
    metrics: &Metrics,
    target: BatchTarget,
    frame: &Frame,
    tuning: SendTuning,
) -> Result<(), PipelineError> {
    let payload = frame.to_bytes();
    debug_assert_eq!(
        payload.len(),
        FRAME_HEADER_LEN + frame.body.len(),
        "a frame's on-chain payload is its header plus its body, nothing else"
    );
    let idx = frame.frame_no as u32;
    let ixs = plan_chunk(
        &target.program_id,
        &target.payer,
        &target.settlement_program,
        target.chain_id,
        target.batch,
        idx,
        &payload,
    );
    sender.send_and_confirm(&ixs, tuning).await?;
    metrics.frames_sent_total.inc();
    Ok(())
}

/// Builds every frame's on-chain instruction plan as the single-stage, single-transaction [`FramePlan`]
/// [`crate::sender::RpcSender::send_and_confirm_many`] takes. Returning `Vec<FramePlan>`
/// (not one flat list across every frame) keeps different frames' plans addressable independently — they
/// are fully independent of each other and run concurrently (seals are order-independent; the pipeline is
/// fully parallel).
pub fn build_frame_jobs(target: BatchTarget, frames: &[Frame]) -> Vec<FramePlan> {
    frames
        .iter()
        .map(|frame| {
            let payload = frame.to_bytes();
            debug_assert_eq!(
                payload.len(),
                FRAME_HEADER_LEN + frame.body.len(),
                "a frame's on-chain payload is its header plus its body, nothing else"
            );
            let idx = frame.frame_no as u32;
            into_stages(plan_chunk(
                &target.program_id,
                &target.payer,
                &target.settlement_program,
                target.chain_id,
                target.batch,
                idx,
                &payload,
            ))
        })
        .collect()
}

/// Drives every frame's send concurrently, bounded by `in_flight` ("N in flight, default 64").
pub async fn send_all_frames<S: Sender + 'static>(
    sender: Arc<S>,
    metrics: Arc<Metrics>,
    target: BatchTarget,
    frames: Vec<Frame>,
    tuning: SendTuning,
    in_flight: usize,
) -> Result<(), PipelineError> {
    let mut in_progress = FuturesUnordered::new();
    let mut remaining = frames.into_iter();
    for frame in remaining.by_ref().take(in_flight.max(1)) {
        let sender = sender.clone();
        let metrics = metrics.clone();
        in_progress.push(tokio::spawn(async move {
            send_frame(&*sender, &metrics, target, &frame, tuning).await
        }));
    }
    let mut result: Result<(), PipelineError> = Ok(());
    while let Some(joined) = in_progress.next().await {
        let outcome = joined.expect("send_frame task panicked");
        if let Err(e) = outcome {
            if result.is_ok() {
                result = Err(e);
            }
            continue;
        }
        if let Some(frame) = remaining.next() {
            let sender = sender.clone();
            let metrics = metrics.clone();
            in_progress.push(tokio::spawn(async move {
                send_frame(&*sender, &metrics, target, &frame, tuning).await
            }));
        }
    }
    result
}

/// Polls the batch account (via a plain `get_account` read, not through `Sender`) until
/// `leaves_present == expected_count`, then submits `FinalizeBatch { step: 0 }` and re-polls until
/// `finalized`. `step = 0` per `zk_inbox_client::finalize_batch_ix`'s own doc means "transform every
/// remaining leaf, then combine, in this call" — the design's own measurement (batch.rs module doc: 900
/// leaves finalize in one call) means a single `FinalizeBatch` suffices for the batch sizes used here
/// (`blocks_per_batch` default 10 -> at most `max_frames_per_batch` = 900 leaves); a batch large enough to
/// need more than one `FinalizeBatch` call is out of scope here (the poll loop below would just need to
/// call it again, which this same loop already does if `finalized` isn't yet true).
#[derive(Debug, Clone, Copy)]
pub struct FinalizePoll {
    pub expected_count: u32,
    pub poll_interval: Duration,
    pub max_polls: u32,
}

pub async fn finalize_and_verify<S: Sender, A: AccountOps>(
    sender: &S,
    accounts: &A,
    metrics: &Metrics,
    target: BatchTarget,
    tuning: SendTuning,
    poll: FinalizePoll,
    frames: &[Frame],
) -> Result<zk_inbox_client::BatchAccount, PipelineError> {
    let (batch_pda, _) = zk_inbox_client::batch_pda(
        &target.program_id,
        &target.settlement_program,
        target.chain_id,
        target.batch,
    );
    let read_batch_account =
        |data: Vec<u8>| -> Result<zk_inbox_client::BatchAccount, PipelineError> {
            Ok(zk_inbox_client::decode_batch_account(&data)?)
        };

    for _ in 0..poll.max_polls {
        let data = accounts
            .get_account(&batch_pda)
            .await?
            .ok_or(PipelineError::BatchVanished {
                batch: target.batch,
            })?;
        let decoded = read_batch_account(data)?;
        if decoded.leaves_present >= poll.expected_count {
            break;
        }
        tokio::time::sleep(poll.poll_interval).await;
    }

    let data = accounts
        .get_account(&batch_pda)
        .await?
        .ok_or(PipelineError::BatchVanished {
            batch: target.batch,
        })?;
    let mut decoded = read_batch_account(data.clone())?;
    if decoded.leaves_present < poll.expected_count {
        return Err(PipelineError::LeavesNeverComplete {
            polls: poll.max_polls,
        });
    }

    // Verify every sealed leaf against this run's own frames strictly before FinalizeBatch — once finalized, the
    // batch can no longer be AbandonBatch'd. Skip this once the batch is *already* finalized: FinalizeBatch is now
    // authority-gated on chain, so only this same `target.payer` can ever have finalized it, but an earlier send of
    // this very call (this poll loop's own retry after e.g. a timeout the caller assumed failed) can still land in
    // the ~0.4-1s window before this read, and FinalizeBatch rewrites `leaf_hashes` in place into a different format
    // (`idx ‖ hash`, not `keccak(body)`) — comparing that to `keccak(frame.to_bytes())` here would refuse a perfectly
    // correct, already-finalized batch. `verify_acc` (called separately by the caller once this function returns) is
    // the content check on that case.
    // A finalize that has already started (`finalize_cursor > 0`, only ever from an earlier resumable step) has
    // transformed some leaves in place; the startup resume verifies those in their transformed form itself.
    if !decoded.finalized && decoded.finalize_cursor == 0 {
        if let Err(e) = verify_presealed_leaves(&data, decoded.expected_count, frames) {
            metrics.batches_failed_total.inc();
            return Err(e);
        }
    }

    for _ in 0..poll.max_polls {
        if decoded.finalized {
            metrics.batches_finalized_total.inc();
            return Ok(decoded);
        }
        let finalize_ix = zk_inbox_client::finalize_batch_ix(
            &target.program_id,
            &target.payer,
            &target.settlement_program,
            target.chain_id,
            target.batch,
            0,
        );
        sender
            .send_and_confirm(std::slice::from_ref(&finalize_ix), tuning)
            .await?;
        let data = accounts
            .get_account(&batch_pda)
            .await?
            .ok_or(PipelineError::BatchVanished {
                batch: target.batch,
            })?;
        decoded = read_batch_account(data)?;
    }
    metrics.batches_failed_total.inc();
    Err(PipelineError::LeavesNeverComplete {
        polls: poll.max_polls,
    })
}

/// Reads a **not-yet-finalized** batch account's raw bytes directly (bitmap + `leaf_hashes`, per
/// `rome_zk_layouts::batch`'s layout) and, for every leaf the presence bitmap marks sealed, compares it to
/// `keccak(frame.to_bytes())` for this run's own frame at that index — refusing on any mismatch.
/// Before `FinalizeBatch` transforms `leaf_hashes` in place into `idx ‖ hash`
/// leaves, each entry is still the plain chunk-body hash `SealLeaf` wrote, so this must run strictly before
/// that call (`finalize_and_verify` calls it exactly once, right after `leaves_present == expected_count`
/// and before ever submitting `FinalizeBatch`).
///
/// `verify_acc` alone (below) is not a substitute for this: it only runs *after* `FinalizeBatch`, by which
/// point the batch can no longer be `AbandonBatch`ed — a tampered or substituted chunk body would already
/// be paid for and DA-committed before anyone notices. This function closes that window; `verify_acc`
/// stays as the final on-chain/off-chain equivalence check once finalized.
///
/// A leaf not yet marked present is skipped, never compared — pre-finalize some leaves may legitimately
/// still be in flight (this function does not require `leaves_present == expected_count`; the caller
/// already establishes that separately).
///
/// This is a client-side guard only: `programs/zk-inbox`'s `Seal`
/// does not itself verify Write coverage against a body hash (a separate
/// change), so a third party's `FinalizeBatch` landing before this check ever runs is not prevented by the
/// program — only by this process happening to check first.
pub fn verify_presealed_leaves(
    raw_batch_account_data: &[u8],
    expected_count: u32,
    frames: &[Frame],
) -> Result<(), PipelineError> {
    let by_idx: std::collections::HashMap<u32, &Frame> =
        frames.iter().map(|f| (f.frame_no as u32, f)).collect();
    let (bitmap_off, leaves_off) =
        rome_zk_layouts::batch::leaf_offsets(raw_batch_account_data, expected_count)
            .map_err(|e| PipelineError::Rederive(format!("batch account header: {e:?}")))?;
    for idx in 0..expected_count {
        let byte = raw_batch_account_data
            .get(bitmap_off + (idx as usize) / 8)
            .copied()
            .unwrap_or(0);
        let present = (byte >> (idx % 8)) & 1 == 1;
        if !present {
            continue;
        }
        let slot = leaves_off + 32 * idx as usize;
        let on_chain: [u8; 32] = raw_batch_account_data
            .get(slot..slot + 32)
            .ok_or_else(|| {
                PipelineError::Rederive(format!(
                    "batch account too short to read leaf {idx} (need bytes {slot}..{})",
                    slot + 32
                ))
            })?
            .try_into()
            .expect("slice from get(range) of exactly 32 is exactly 32 bytes");
        let frame = by_idx.get(&idx).ok_or_else(|| {
            PipelineError::Rederive(format!(
                "leaf {idx} is present on chain but this run holds no frame at that index"
            ))
        })?;
        let expected = solana_program::keccak::hashv(&[&frame.to_bytes()]).to_bytes();
        if on_chain != expected {
            return Err(PipelineError::PreFinalizeLeafMismatch {
                idx,
                on_chain,
                expected,
            });
        }
    }
    Ok(())
}

/// Verifies a finalized batch's on-chain `acc` against `zk_inbox_client::reference_commitment` computed
/// from the chunk bodies actually sent (the on-chain/off-chain equivalence).
pub fn verify_acc(
    decoded: &zk_inbox_client::BatchAccount,
    frames: &[Frame],
) -> Result<[u8; 32], PipelineError> {
    let mut ordered: Vec<&Frame> = frames.iter().collect();
    ordered.sort_by_key(|f| f.frame_no);
    let chunk_hashes: Vec<[u8; 32]> = ordered
        .iter()
        .map(|f| solana_program::keccak::hashv(&[&f.to_bytes()]).to_bytes())
        .collect();
    let (_, _, reference_acc) = zk_inbox_client::reference_commitment(
        decoded.chain_id,
        decoded.batch,
        decoded.open_slot,
        &chunk_hashes,
    );
    if reference_acc != decoded.acc {
        return Err(PipelineError::AccMismatch {
            on_chain: decoded.acc,
            reference: reference_acc,
        });
    }
    Ok(reference_acc)
}

/// Builds a [`FinalizedBatch`] and hands it to the sink — the last step of one successful batch attempt.
pub fn hand_off(
    sink: &dyn PostRootSink,
    decoded: &zk_inbox_client::BatchAccount,
    blocks: &[Block],
) {
    let first_block = blocks.first().map(|b| b.number).unwrap_or_default();
    let last_block = blocks.last().map(|b| b.number).unwrap_or_default();
    sink.publish(FinalizedBatch {
        chain_id: decoded.chain_id,
        batch: decoded.batch,
        acc: decoded.acc,
        first_block,
        last_block,
        // State roots are not computed here: no executor is wired in, and the sequencer's own sub-block
        // header hashes that `source.rs` exposes are not state roots. This field is left empty rather than
        // filled with a value this code cannot compute correctly.
        state_roots: Vec::new(),
    });
}

/// CU sampling (two `getTransaction` reads per batch, informational only)
/// must never sit on the posting path — this wraps `lookup` in a `tokio::time::timeout` bounded by
/// `budget`, logging (never propagating) whichever outcome results. The caller is responsible for
/// spawning this as a background task (`tokio::spawn`) rather than awaiting it inline — this function
/// itself only bounds the WORK, it does not make the caller stop waiting on it; see
/// `WindowedPoster::submit_group`'s own `tokio::spawn` call site.
pub async fn sample_cu_off_path<F>(label: &str, budget: Duration, lookup: F)
where
    F: std::future::Future<Output = ()>,
{
    if tokio::time::timeout(budget, lookup).await.is_err() {
        // Never WARN — a CU sample is informational only, off the posting path, and a slow/timed-out
        // lookup on a busy or degraded cluster is routine, expected noise, not an operational alarm
        // (measured: `getTransaction` lookups saw 3-13 s of null-retries on a degraded devnet — that is
        // the normal case this branch exists for).
        tracing::debug!("CU sample ({label}): budget of {budget:?} exceeded — skipping the rest");
    }
}

/// `getTransaction` can return a null result moments after confirmation (the RPC node's history store
/// lags its own confirmation by a beat) — retries briefly before giving up. Runs only inside
/// [`sample_cu_off_path`]'s own timeout budget, never on the posting path.
async fn print_cu(
    rpc: &solana_client::nonblocking::rpc_client::RpcClient,
    label: &str,
    sig: &solana_signature::Signature,
) {
    let legacy_sig = crate::sender::compat::to_legacy_signature(sig);
    let config = solana_client::rpc_config::RpcTransactionConfig {
        encoding: Some(solana_transaction_status_client_types::UiTransactionEncoding::Json),
        // Every chunk-lane tx this crate sends is a V1 (versioned) transaction; without
        // this the RPC node refuses with "Transaction version (1) is not supported by the requesting client".
        max_supported_transaction_version: Some(1),
        ..Default::default()
    };
    for attempt in 0..5 {
        match rpc.get_transaction_with_config(&legacy_sig, config).await {
            Ok(t) => {
                let cu = t
                    .transaction
                    .meta
                    .as_ref()
                    .and_then(|m| Into::<Option<u64>>::into(m.compute_units_consumed.clone()));
                tracing::info!("{label}: sig={sig} cu={cu:?}");
                return;
            }
            Err(_) if attempt < 4 => tokio::time::sleep(Duration::from_millis(500)).await,
            Err(e) => {
                // Never log a raw `ClientError` Display — reqwest's own appends
                // " for url (<URL>)", which can carry a secret (an API key in the query string).
                // Never WARN — this is the same "informational CU sample, off the
                // posting path" routine-noise case `sample_cu_off_path`'s own timeout branch documents.
                tracing::debug!(
                    "{label}: sig={sig} (could not fetch CU after retries: {})",
                    rome_zk_solana_sender::describe_rpc_error(&e)
                );
                return;
            }
        }
    }
}

/// Configures a spawned, budget-bounded CU sample after a batch finalizes —
/// `None` disables sampling entirely (every fake-driven test in this crate passes `None`: there is no
/// fake `RpcClient`-compatible transport for `get_transaction_with_config` in this crate's own test
/// doubles, only real `RpcClient`s — production or `BanksClient`-bridged — can serve it).
#[derive(Clone)]
pub struct CuSampleConfig {
    pub rpc: Arc<solana_client::nonblocking::rpc_client::RpcClient>,
    pub budget: Duration,
}

/// Everything [`WindowedPoster`] needs beyond the sender/accounts/metrics/sink references themselves —
/// grouped so its constructor stays under a sane argument count.
#[derive(Clone)]
pub struct WindowConfig {
    pub inbox_program_id: Pubkey,
    pub settlement_program_id: Pubkey,
    pub payer: Pubkey,
    pub chain_id: u64,
    pub max_frame_body_len: usize,
    pub chunk_tuning: SendTuning,
    /// The open-and-grow transaction's own tuning (its compute-unit limit is `open_compute_unit_limit`).
    pub open_tuning: SendTuning,
    /// A chunk-lane frame that runs out of compute units at `chunk_tuning`'s limit is resent once at this one.
    pub chunk_retry_compute_unit_limit: u32,
    pub finalize_tuning: SendTuning,
    pub finalize_poll_interval: Duration,
    pub finalize_max_polls: u32,
    pub in_flight_frames: usize,
    pub confirm_poll_interval: Duration,
    pub signature_status_batch_size: usize,
    /// At most this many batches post concurrently.
    pub batches_in_flight: usize,
    pub cu_sample: Option<CuSampleConfig>,
    /// Sample only every Nth finalized batch — see
    /// `config::default_cu_sample_every`'s own doc. Refused nonzero at config load
    /// (`config::ConfigError::ZeroCuSampleEvery`); this struct trusts that and never re-checks (a `0`
    /// reaching here would panic on the modulo below, which is exactly the point — unconstructable, not
    /// silently tolerated).
    pub cu_sample_every: u32,
}

type SettleFuture = Pin<Box<dyn std::future::Future<Output = Result<u64, PipelineError>> + Send>>;

/// Posts a **bounded window** of at most `cfg.batches_in_flight` batches concurrently.
///
/// **`OpenBatch(N+1)` is sent once `OpenBatch(N)` has confirmed.** [`Self::submit_group`] resolves the
/// batch id and sends `OpenBatch`(+`GrowBatch`), *awaiting its confirmation*, entirely on the caller's own
/// task before returning — there is no concurrency at all in this step, so the on-chain cursor's own
/// sequential order is trivially preserved and `expected_next_batch` advances in strict
/// batch-id order, in lockstep with real `OpenBatch` sends.
///
/// **The chunk lanes of different batches overlap freely; `FinalizeBatch` stays ordered across batches.**
/// Everything after `OpenBatch` confirms — the chunk-lane sends, the `FinalizeBatch` wait/send/verify, and
/// the hand-off — runs as one `tokio::spawn`ed task per batch (real background progress, not merely
/// cooperative: `--follow`'s own tail-wait loop does not otherwise poll this struct at all, so a batch's
/// settlement must be able to advance on its own while the main loop is blocked waiting for new blocks).
/// Each spawned task waits on a oneshot gate the *previous* batch's own task only signals once ITS
/// `FinalizeBatch` has confirmed and its hand-off is done — a simple, generalizes-to-any-window-size
/// linked chain, one link per batch, rather than a fixed-size slot array.
///
/// **A failure anywhere stops the whole run.** The moment any spawned task's work fails, it stores the
/// error and sets a shared flag *before* doing anything else — [`Self::submit_group`] checks that flag
/// first, ahead of anything else (including draining), so a batch already known to have failed can never
/// let a further `OpenBatch` be sent because of a queue-draining race; [`Self::submit_group`]/
/// [`Self::finish`] also drain every already-completed batch to surface the first real error's own value.
pub struct WindowedPoster<S: Sender + 'static, A: AccountOps + 'static> {
    sender: Arc<S>,
    accounts: Arc<A>,
    metrics: Arc<Metrics>,
    sink: Arc<dyn PostRootSink>,
    cfg: WindowConfig,
    in_flight: FuturesUnordered<SettleFuture>,
    /// The oneshot receiver the *next* spawned task must wait on before sending its own `FinalizeBatch` —
    /// `None` only before the very first batch this poster has ever opened.
    prev_settle_gate: Option<tokio::sync::oneshot::Receiver<()>>,
    /// Set by a spawned task the instant its own work fails, before it does anything else — checked at the
    /// very top of [`Self::submit_group`] so a sibling's already-known failure can never be raced past by
    /// a new `OpenBatch`, regardless of which order `FuturesUnordered` happens to resolve completions in.
    failed: Arc<std::sync::atomic::AtomicBool>,
    /// The newest block number this run has actually seen appended to the
    /// ordered log — updated by the caller (`bin/rome-zk-batcher.rs`'s own `run_once`/`run_follow`, on
    /// every `source.next_block()` that returns a block) on every tick, independent of whether a group is
    /// being submitted right now. Each batch's own settle task reads this **at its own hand-off time**
    /// (`settle_one_batch`), not at submission time — a batch can sit finalizing for seconds while the log
    /// keeps advancing, so a value captured at `submit_group` time would understate the lag by construction
    /// (read at submission time, `lag_blocks` would be structurally 0 or 1).
    newest_block: Arc<AtomicU64>,
}

impl<S: Sender + 'static, A: AccountOps + 'static> WindowedPoster<S, A> {
    pub fn new(
        sender: Arc<S>,
        accounts: Arc<A>,
        metrics: Arc<Metrics>,
        sink: Arc<dyn PostRootSink>,
        cfg: WindowConfig,
        newest_block: Arc<AtomicU64>,
    ) -> Self {
        Self {
            sender,
            accounts,
            metrics,
            sink,
            cfg,
            in_flight: FuturesUnordered::new(),
            prev_settle_gate: None,
            failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            newest_block,
        }
    }

    /// Drains every batch that has *already* completed, without blocking — catches a sibling's failure
    /// (or success) that has already resolved before this call ever does anything else. Never blocks: a
    /// batch that has not finished yet is simply left in the set for a later call to pick up.
    async fn drain_ready(&mut self) -> Result<(), PipelineError> {
        while let Some(outcome) = self.in_flight.next().now_or_never().flatten() {
            self.metrics
                .batches_in_flight
                .set(self.in_flight.len() as i64);
            outcome?;
        }
        Ok(())
    }

    /// `--follow`'s own idle tail-wait loop (`bin/rome-zk-batcher.rs`'s
    /// `tokio::select!` sleep/refresh arm) has no other way to learn a sibling batch already failed — before
    /// this, that news only ever surfaced inside the next `submit_group`/`finish` call, which on an idle
    /// chain might not happen for a whole batch period (or ever, if the chain has genuinely gone quiet)
    /// while this process sat alive with a dead window. A thin, non-blocking public wrapper around
    /// [`Self::drain_ready`] — safe to call every idle tick; never blocks, never sends anything.
    pub async fn poll_failure(&mut self) -> Result<(), PipelineError> {
        self.drain_ready().await
    }

    /// Blocks until at least one in-flight batch completes, applying its outcome.
    async fn await_one(&mut self) -> Result<(), PipelineError> {
        if let Some(outcome) = self.in_flight.next().await {
            self.metrics
                .batches_in_flight
                .set(self.in_flight.len() as i64);
            outcome?;
        }
        Ok(())
    }

    /// Submits one already-closed group of blocks. Resolves the batch id and sends
    /// `OpenBatch`(+`GrowBatch`), synchronously awaiting confirmation, before this call returns — so a
    /// caller that submits groups one at a time, in order, gets `OpenBatch(N+1)` sent strictly after
    /// `OpenBatch(N)` confirmed, by construction. `expected_next_batch` (in/out) advances by one exactly
    /// when a fresh `OpenBatch` was genuinely sent (never on an `AlreadyPosted` resolve, matching the
    /// earlier behavior). The `lag_blocks` gauge is read from `self.newest_block` at this batch's own
    /// hand-off time (`settle_one_batch`), not captured here at submission time.
    pub async fn submit_group(
        &mut self,
        blocks: Vec<Block>,
        expected_next_batch: &mut u64,
    ) -> Result<(), PipelineError> {
        if self.failed.load(std::sync::atomic::Ordering::SeqCst) {
            // A sibling already failed (observed by a spawned task, possibly before this call's own
            // `drain_ready` would otherwise have collected it) — never open anything further.
            self.drain_ready().await?;
            return Err(PipelineError::WindowPreviouslyFailed);
        }
        self.drain_ready().await?;
        while self.in_flight.len() >= self.cfg.batches_in_flight.max(1) {
            self.await_one().await?;
        }

        let compressed = channel::encode_stream(&blocks);
        re_derive_and_check(&blocks, &compressed)?;

        let batch = match resolve::resolve_batch_id(
            self.accounts.as_ref(),
            &self.cfg.inbox_program_id,
            &self.cfg.settlement_program_id,
            self.cfg.chain_id,
            &compressed,
            self.cfg.max_frame_body_len,
            *expected_next_batch,
        )
        .await?
        {
            ResolveOutcome::PostUnder(b) => b,
            ResolveOutcome::AlreadyPosted(_) => return Ok(()),
        };

        // `resolve_batch_id`'s own account reads are real `.await` points — a
        // window in which a sibling batch's spawned task can fail and set `self.failed` AFTER the
        // top-of-function check above already passed (and, when the window has room, without the
        // `while len() >= batches_in_flight` wait loop ever running either). Re-checking here, immediately
        // before `OpenBatch`, closes that window: no `OpenBatch` is ever sent once a sibling is known to
        // have failed, regardless of when in this call that became true.
        if self.failed.load(std::sync::atomic::Ordering::SeqCst) {
            self.drain_ready().await?;
            return Err(PipelineError::WindowPreviouslyFailed);
        }

        let target = BatchTarget {
            program_id: self.cfg.inbox_program_id,
            settlement_program: self.cfg.settlement_program_id,
            payer: self.cfg.payer,
            chain_id: self.cfg.chain_id,
            batch,
        };
        let frames = channel::cut_frames(
            self.cfg.chain_id,
            batch,
            &compressed,
            self.cfg.max_frame_body_len,
        );

        // `OpenBatch(N+1)` after `OpenBatch(N)` confirmed: this send is awaited
        // right here, on this call's own task, before `submit_group` ever returns.
        let post_started = Instant::now();
        // Captured immediately before the send, on this same call's own task —
        // the closest this process gets to "the moment OpenBatch left".
        let send_wall_clock_unix_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let open_sig = open_and_grow_batch(
            self.sender.as_ref(),
            target,
            frames.len() as u32,
            self.cfg.open_tuning,
        )
        .await?;
        self.metrics
            .open_confirm_seconds
            .observe(post_started.elapsed().as_secs_f64());
        tracing::info!("OpenBatch(+Grow) for batch {batch} confirmed: {open_sig}");

        // Read the just-opened batch account back and compare
        // its own committed open_unix_ts against this send's wall clock. Observability only — a read or
        // decode failure here is logged and skipped, never propagated: it must never affect whether this
        // OpenBatch is considered successful (it already confirmed, above).
        let (batch_pda, _) = zk_inbox_client::batch_pda(
            &target.program_id,
            &target.settlement_program,
            target.chain_id,
            target.batch,
        );
        match self.accounts.get_account(&batch_pda).await {
            Ok(Some(data)) => match zk_inbox_client::decode_batch_account(&data) {
                Ok(account) => self
                    .metrics
                    .solana_clock_skew_seconds
                    .observe(clock_skew_secs(
                        account.open_unix_ts,
                        send_wall_clock_unix_secs,
                    )),
                Err(e) => tracing::warn!(
                    "solana_clock_skew_seconds: decode batch {batch} account after open: {e}"
                ),
            },
            Ok(None) => tracing::warn!(
                "solana_clock_skew_seconds: batch {batch} account missing right after OpenBatch"
            ),
            Err(e) => {
                tracing::warn!(
                    "solana_clock_skew_seconds: read batch {batch} account after open: {e}"
                )
            }
        }

        *expected_next_batch += 1;

        let (tx, rx) = tokio::sync::oneshot::channel();
        let wait_for_prev = self.prev_settle_gate.replace(rx);

        let sender = self.sender.clone();
        let accounts = self.accounts.clone();
        let metrics = self.metrics.clone();
        let sink = self.sink.clone();
        let cfg = self.cfg.clone();
        let failed = self.failed.clone();
        let newest_block = self.newest_block.clone();
        let last_block = blocks.last().map(|b| b.number).unwrap_or_default();

        let handle = tokio::spawn(async move {
            let result = settle_one_batch(
                sender,
                accounts,
                metrics,
                sink,
                cfg,
                target,
                frames,
                blocks,
                open_sig,
                wait_for_prev,
                tx,
                post_started,
                last_block,
                newest_block,
            )
            .await;
            if result.is_err() {
                failed.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            result
        });
        let fut: SettleFuture = Box::pin(async move {
            handle
                .await
                .expect("a batch's own settle task must not panic")
        });
        self.in_flight.push(fut);
        self.metrics
            .batches_in_flight
            .set(self.in_flight.len() as i64);
        Ok(())
    }

    /// Drains every still-in-flight batch to completion — call once no more groups are coming (end of
    /// `--once`, or clean `--follow` shutdown) so the process's own exit code reflects every batch this
    /// window ever opened, not just the ones `submit_group` happened to observe finishing already.
    pub async fn finish(&mut self) -> Result<(), PipelineError> {
        while let Some(outcome) = self.in_flight.next().await {
            self.metrics
                .batches_in_flight
                .set(self.in_flight.len() as i64);
            outcome?;
        }
        Ok(())
    }
}

/// Pushes `block` into `grouper`, submitting to `poster` every group that closes along the way (a size
/// close hands the pushed block back as `carry_over` — [`PushOutcome::Closed`]'s own doc — so this loops
/// until the block is genuinely accepted into a group in progress). `now` is `block`'s own receipt
/// instant — recorded by [`SizeCappedGrouper::push`], never derived from
/// `block.timestamp`. Shared by both `--once` and `--follow` (`bin/rome-zk-batcher.rs`) for the ordinary
/// `Cap`/`Size` close path; **never** the age-close path — see [`follow_tick`], which `--follow` alone
/// drives, for that (no `close_if_stale` is reachable from
/// `--once`).
pub async fn push_and_post_until_accepted<S: Sender + 'static, A: AccountOps + 'static>(
    grouper: &mut SizeCappedGrouper,
    mut block: Block,
    now: Instant,
    expected_next_batch: &mut u64,
    poster: &mut WindowedPoster<S, A>,
    metrics: &Metrics,
) -> Result<(), PipelineError> {
    // Observability only, never a close decision (that is
    // `close_if_stale`'s own receipt clock above) — this batcher's own wall clock at the moment it read
    // `block` off the log, minus `block`'s own `Block.timestamp`. A restart resuming into a backlog
    // legitimately observes a large value here.
    let wall_now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    metrics
        .block_age_on_arrival_seconds
        .observe((wall_now_secs - block.timestamp as i64).max(0) as f64);

    loop {
        match grouper.push(block.clone(), now)? {
            PushOutcome::Accepted => return Ok(()),
            PushOutcome::Closed { reason, carry_over } => {
                let group = grouper.take_group();
                metrics
                    .groups_closed_total
                    .with_label_values(&[reason.as_str()])
                    .inc();
                poster.submit_group(group, expected_next_batch).await?;
                match carry_over {
                    Some(carried) => {
                        block = carried;
                        continue;
                    }
                    None => return Ok(()),
                }
            }
        }
    }
}

/// One `--follow` loop tick — the single library entry point
/// both of `run_follow`'s own loop arms drive (`bin/rome-zk-batcher.rs`): a block read off the log's tail
/// ([`FollowEvent::Block`]), or the log having nothing new this tick ([`FollowEvent::Idle`], after the
/// tail-wait sleep). Either way, the group in progress is checked for an age close — **unconditionally**,
/// not only on the idle arm: a trickle of blocks arriving slower than the cap must still close by age
/// (a chain that never goes idle, but never fills a group either, must not wait forever). An empty group
/// never closes here (`SizeCappedGrouper::close_if_stale`'s own guard).
/// `--once` never calls this function at all — it drives [`push_and_post_until_accepted`] directly and
/// has no age-close path.
#[allow(clippy::too_many_arguments)]
pub async fn follow_tick<S: Sender + 'static, A: AccountOps + 'static>(
    grouper: &mut SizeCappedGrouper,
    event: FollowEvent,
    now: Instant,
    close_after: Duration,
    expected_next_batch: &mut u64,
    poster: &mut WindowedPoster<S, A>,
    metrics: &Metrics,
) -> Result<(), PipelineError> {
    if let FollowEvent::Block(block) = event {
        push_and_post_until_accepted(grouper, block, now, expected_next_batch, poster, metrics)
            .await?;
    }
    if grouper.close_if_stale(now, close_after) {
        let group = grouper.take_group();
        metrics
            .groups_closed_total
            .with_label_values(&[CloseReason::Age.as_str()])
            .inc();
        poster.submit_group(group, expected_next_batch).await?;
    }
    metrics
        .oldest_unposted_block_age_seconds
        .set(grouper.oldest_unposted_age(now).as_secs() as i64);
    Ok(())
}

/// What triggered one [`follow_tick`] call — see that function's own doc.
#[derive(Debug)]
pub enum FollowEvent {
    /// A block was read off the ordered log's tail.
    Block(Block),
    /// The log had nothing new this tick (the tail-wait sleep just elapsed).
    Idle,
}

/// One batch's post-`OpenBatch` work: chunk lane -> the finalize-order gate -> `FinalizeBatch` -> verify ->
/// hand-off -> signal the next batch's own gate -> (fire-and-forget) spawn the CU sample. Factored out of
/// [`WindowedPoster::submit_group`] only so that function's own body stays a manageable read; not part of
/// this module's public surface.
#[allow(clippy::too_many_arguments)]
async fn settle_one_batch<S: Sender, A: AccountOps>(
    sender: Arc<S>,
    accounts: Arc<A>,
    metrics: Arc<Metrics>,
    sink: Arc<dyn PostRootSink>,
    cfg: WindowConfig,
    target: BatchTarget,
    frames: Vec<Frame>,
    blocks: Vec<Block>,
    open_sig: solana_signature::Signature,
    wait_for_prev: Option<tokio::sync::oneshot::Receiver<()>>,
    tx: tokio::sync::oneshot::Sender<()>,
    post_started: Instant,
    last_block: u64,
    newest_block: Arc<AtomicU64>,
) -> Result<u64, PipelineError> {
    let frame_jobs = build_frame_jobs(target, &frames);
    let outcome = sender
        .send_and_confirm_many_retrying_compute(
            &frame_jobs,
            cfg.chunk_tuning,
            cfg.chunk_retry_compute_unit_limit,
            cfg.in_flight_frames,
            cfg.confirm_poll_interval,
            cfg.signature_status_batch_size,
        )
        .await
        .map_err(PipelineError::from)?;
    for f in &outcome.frames {
        metrics
            .chunk_confirm_latency_seconds
            .observe(f.confirm_latency.as_secs_f64());
    }
    for _ in &frames {
        metrics.frames_sent_total.inc();
    }

    // `FinalizeBatch(N+1)` only after `FinalizeBatch(N)` confirmed AND N's hand-off is done.
    // A dropped sender means N failed (or its task vanished) BEFORE it signalled: that is not
    // "go" — finalizing N+1 beside an unfinalized N breaks the block order
    // and derive meets heights out of order. This batch stays open-not-finalized for the next start, which finishes it;
    // `WindowedPoster::submit_group`'s `failed` flag stops any NEW batch from being opened.
    if let Some(prev) = wait_for_prev {
        if prev.await.is_err() {
            return Err(PipelineError::PreviousBatchFailed {
                batch: target.batch,
            });
        }
    }

    let finalize_started = Instant::now();
    let decoded = finalize_and_verify(
        sender.as_ref(),
        accounts.as_ref(),
        &metrics,
        target,
        cfg.finalize_tuning,
        FinalizePoll {
            expected_count: frames.len() as u32,
            poll_interval: cfg.finalize_poll_interval,
            max_polls: cfg.finalize_max_polls,
        },
        &frames,
    )
    .await?;
    metrics
        .finalize_confirm_seconds
        .observe(finalize_started.elapsed().as_secs_f64());

    verify_acc(&decoded, &frames)?;
    hand_off(sink.as_ref(), &decoded, &blocks);
    metrics
        .batch_post_seconds
        .observe(post_started.elapsed().as_secs_f64());
    // Read at hand-off time, never at submit time — `newest_block` keeps advancing
    // while this batch's own chunk lane and FinalizeBatch wait were in flight (seconds, or the whole
    // gated-predecessor wait), so a value captured at `submit_group` would understate the lag by exactly
    // that much (structurally 0 or 1 if read at submit time).
    let newest = newest_block.load(Ordering::Relaxed);
    let lag = (newest as i64 - last_block as i64).max(0);
    metrics.lag_blocks.set(lag);

    // Sample only every Nth finalized batch — `metrics.batches_finalized_total`
    // was already incremented for THIS batch inside `finalize_and_verify` above, so reading it here counts
    // "the Nth batch this process has actually finalized" (1, 2, 3, ...), never this batch's own id (which
    // can skip ids across an abandon/restart and would make "every Nth" meaningless against a fixed id
    // sequence). `cu_samples_triggered_total` is bumped whenever it is this batch's turn, independent of
    // whether `cfg.cu_sample` is configured at all — so the cadence itself is directly observable without
    // a live RPC client (this crate's own fake-driven window tests always pass `cu_sample: None`).
    let finalized_so_far = metrics.batches_finalized_total.get();
    if finalized_so_far.is_multiple_of(cfg.cu_sample_every as u64) {
        metrics.cu_samples_triggered_total.inc();
        // CU sampling never sits on this path — spawned, budget-bounded,
        // fire-and-forget; this future's own completion (and therefore the next batch's finalize gate)
        // never waits on it. Both lookups share ONE `sample_cu_off_path` budget (the
        // config's own `cu_cfg.budget`, 5 s total) rather than each independently getting the full budget
        // (which would let the pair together run up to 2x `budget` in the worst case).
        if let Some(cu_cfg) = cfg.cu_sample.clone() {
            let first_frame_sig = outcome.frames.first().map(|f| f.signature);
            tokio::spawn(async move {
                sample_cu_off_path("OpenBatch+Grow, first frame", cu_cfg.budget, async {
                    print_cu(&cu_cfg.rpc, "OpenBatch+Grow", &open_sig).await;
                    if let Some(sig) = first_frame_sig {
                        print_cu(&cu_cfg.rpc, "first frame (Open+Write+Seal+SealLeaf)", &sig).await;
                    }
                })
                .await;
            });
        }
    }

    let _ = tx.send(());
    Ok(target.batch)
}

// CU sampling never sits on the posting path — a spawned, budget-bounded task
// whose own timeout is what closes it out, never the caller waiting on it.
#[cfg(test)]
mod cu_sampling_off_path {
    use super::*;

    /// A lookup that never resolves (`std::future::pending`) still lets `sample_cu_off_path` return —
    /// within the configured budget, under a paused virtual clock (no real 5-second wait). Removing
    /// the `tokio::time::timeout` wrapper (calling `lookup.await` directly) makes this test fail — it would
    /// hang forever waiting on a future that never completes.
    #[tokio::test(start_paused = true)]
    async fn a_lookup_that_never_resolves_still_completes_within_the_budget() {
        let budget = Duration::from_secs(5);
        let handle = tokio::spawn(sample_cu_off_path(
            "test",
            budget,
            std::future::pending::<()>(),
        ));
        // Let the spawned task actually start (reach its own `tokio::time::timeout` call, which registers
        // a timer against the CURRENT paused clock) before advancing — advancing first would move the
        // clock before that timer even exists, so the timeout would compute its deadline from the
        // already-advanced time instead of firing.
        tokio::task::yield_now().await;
        // Advance the paused virtual clock past the budget — `sample_cu_off_path` must have already
        // returned (its own `tokio::time::timeout` fired), not still be waiting on `lookup`.
        tokio::time::advance(budget + Duration::from_millis(1)).await;
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("sample_cu_off_path must return once its own budget elapses, not hang forever")
            .expect("the spawned task must not panic");
    }

    /// The calling path's own completion is never gated on the sample: spawning it and moving on
    /// immediately must not block, even though the sample itself (spawned) is still bounded by its own
    /// budget and hasn't necessarily finished yet.
    #[tokio::test(start_paused = true)]
    async fn spawning_the_sample_never_blocks_the_caller() {
        let caller_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag_for_sample = caller_done.clone();
        tokio::spawn(sample_cu_off_path(
            "test",
            Duration::from_secs(30),
            async move {
                // A lookup that would take far longer than any reasonable caller should ever wait on
                // inline — the assertion below proves the caller did not.
                tokio::time::sleep(Duration::from_secs(20)).await;
                assert!(
                    flag_for_sample.load(std::sync::atomic::Ordering::SeqCst),
                    "the sample's own lookup must not finish before the caller already moved on"
                );
            },
        ));
        // The caller's own path completes immediately — spawning never awaited the sample at all.
        caller_done.store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(21)).await;
    }

    /// Captures every event a plain `tracing_subscriber::Layer` sees — level plus its own `message`
    /// field — with no format-string parsing, so this test asserts on real `tracing::Level`s, not on a
    /// rendered log line's text.
    #[derive(Default, Clone)]
    struct CapturedEvents(std::sync::Arc<std::sync::Mutex<Vec<(tracing::Level, String)>>>);

    struct MessageVisitor(String);
    impl tracing::field::Visit for MessageVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0 = format!("{value:?}");
            }
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CapturedEvents {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut visitor = MessageVisitor(String::new());
            event.record(&mut visitor);
            self.0
                .lock()
                .unwrap()
                .push((*event.metadata().level(), visitor.0));
        }
    }

    /// A "null" CU sample — a lookup that never resolves in time, `sample_cu_off_path`'s own timeout branch — must
    /// never log at WARN; a slow/absent `getTransaction` result is routine, expected noise on this informational,
    /// off-the-posting-path sample, not an operational alarm. Changing `sample_cu_off_path`'s `tracing::debug!` to
    /// `tracing::warn!` makes the second assertion below fail.
    #[tokio::test(start_paused = true)]
    async fn a_null_lookup_never_logs_at_warn() {
        use tracing_subscriber::layer::SubscriberExt;

        let captured = CapturedEvents::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let _guard = tracing::subscriber::set_default(subscriber);

        // Why the rebuild and the bounded re-run below: `sample_cu_off_path`'s `tracing::debug!` is ONE
        // process-wide callsite whose enabled/disabled answer (its "interest") is cached globally, but this
        // subscriber is scoped to this test's thread. Other tests in this suite run the same function on
        // other threads with no subscriber, in parallel. When one of them registers the callsite at the same
        // moment this test installs its subscriber, the callsite can cache "never interested" from a
        // dispatcher list that does not yet include this subscriber, and the event is then dropped before it
        // reaches `captured` even though the thread-local subscriber is installed. Measured on a self-hosted runner
        // (full rome-zk-batcher lib suite, 8-24 suites in parallel): this test failed about 1 run in 150
        // with "got []"; a diagnostic build that rebuilt the interest cache on a miss and ran the sample
        // again captured the event every time it missed. Staleness always looks the same: no event of ANY
        // level reaches `captured`. So rebuild the cache after installing the subscriber, and re-run the
        // sample (rebuilding first) only while nothing at all was captured. A real regression is not hidden
        // by this: a `warn!` or a missing `debug!` still produces a non-empty or a stable-empty result that
        // the assertions below judge, and the retry is bounded.
        const MAX_RUNS: usize = 5;
        let budget = Duration::from_millis(10);
        for _ in 0..MAX_RUNS {
            tracing::callsite::rebuild_interest_cache();
            let handle = tokio::spawn(sample_cu_off_path(
                "test",
                budget,
                std::future::pending::<()>(),
            ));
            tokio::task::yield_now().await;
            tokio::time::advance(budget + Duration::from_millis(1)).await;
            handle.await.unwrap();
            if !captured.0.lock().unwrap().is_empty() {
                break;
            }
        }

        let events = captured.0.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|(level, msg)| *level == tracing::Level::DEBUG && msg.contains("CU sample")),
            "expected a DEBUG-level CU sample event, got {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|(level, _)| *level == tracing::Level::WARN),
            "a null/timed-out CU sample must never log at WARN, got {events:?}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{cut_frames, encode_stream};
    use alloy_primitives::Bytes;

    /// `solana_clock_skew_seconds` observes decoded
    /// `open_unix_ts` minus the send-time wall clock — proven against a real decode of hand-built account
    /// bytes (`build_batch_account_bytes`, this module's own helper), never a bare pair of integers, so a
    /// decode-layout change would break this test the same way it breaks production.
    #[test]
    fn skew_histogram_observes_open_unix_ts_minus_send_wall_clock() {
        let send_wall_clock_unix_secs: i64 = 1_757_000_000;
        let open_unix_ts = send_wall_clock_unix_secs + 7;

        let mut data = build_batch_account_bytes(0, &[]);
        data[rome_zk_layouts::batch::OFF_OPEN_UNIX_TS
            ..rome_zk_layouts::batch::OFF_OPEN_UNIX_TS + 8]
            .copy_from_slice(&open_unix_ts.to_le_bytes());
        let account = zk_inbox_client::decode_batch_account(&data).unwrap();
        assert_eq!(account.open_unix_ts, open_unix_ts);

        let metrics = Metrics::new();
        metrics.solana_clock_skew_seconds.observe(clock_skew_secs(
            account.open_unix_ts,
            send_wall_clock_unix_secs,
        ));

        let rendered = metrics.render();
        assert!(
            rendered.contains("rome_zk_batcher_solana_clock_skew_seconds_sum 7"),
            "expected one observation of 7.0, got:\n{rendered}"
        );
        assert!(
            rendered.contains("rome_zk_batcher_solana_clock_skew_seconds_count 1"),
            "got:\n{rendered}"
        );
    }

    /// `clock_skew_secs` itself: signed, and negative when the batcher's own clock ran ahead of what the
    /// chain later stamped.
    #[test]
    fn clock_skew_secs_is_signed() {
        assert_eq!(clock_skew_secs(107, 100), 7.0);
        assert_eq!(clock_skew_secs(93, 100), -7.0);
        assert_eq!(clock_skew_secs(100, 100), 0.0);
    }

    fn sample_blocks() -> Vec<Block> {
        vec![
            Block {
                number: 0,
                timestamp: 1,
                gas_limit: 100,
                txs: vec![Bytes::from_static(b"tx-a")],
                deposits_end: None,
            },
            Block {
                number: 1,
                timestamp: 2,
                gas_limit: 100,
                txs: vec![Bytes::from_static(b"tx-b"), Bytes::from_static(b"tx-c")],
                deposits_end: None,
            },
        ]
    }

    #[test]
    fn re_derive_accepts_a_correct_encoding() {
        let blocks = sample_blocks();
        let compressed = encode_stream(&blocks);
        re_derive_and_check(&blocks, &compressed).unwrap();
    }

    /// A corrupted frame (here, the whole compressed stream, standing in for "any byte
    /// of any frame got corrupted before send") must be refused before anything is sent — never silently
    /// posted.
    #[test]
    fn re_derive_rejects_a_corrupted_stream() {
        let blocks = sample_blocks();
        let mut compressed = encode_stream(&blocks);
        let last = compressed.len() - 1;
        compressed[last] ^= 0xFF;
        let err = re_derive_and_check(&blocks, &compressed).unwrap_err();
        assert!(matches!(
            err,
            PipelineError::Channel(_) | PipelineError::Rederive(_)
        ));
    }

    #[test]
    fn re_derive_rejects_a_source_tx_byte_mismatch_even_if_the_stream_itself_decodes() {
        let blocks = sample_blocks();
        let compressed = encode_stream(&blocks);
        let mut tampered_source = blocks.clone();
        tampered_source[1].txs[0] = Bytes::from_static(b"tx-DIFFERENT");
        let err = re_derive_and_check(&tampered_source, &compressed).unwrap_err();
        assert!(matches!(err, PipelineError::Rederive(_)));
    }

    /// `plan_chunk` always returns exactly `Open + Write(whole body) + Seal + SealLeaf`
    /// — no combined-vs-split decision, no per-`Write` chunking. The real wire-size proof for this shape
    /// (a design-max 3,681-B body, real V1 compile + sign, ≤ 4,096 B) lives in `sender.rs`'s own
    /// `design_frame_v1_tx_fits_4096_and_carries_both_header_limits` test — this module only pins the
    /// instruction shape and byte coverage, since `plan_chunk` itself never touches a Solana crate whose
    /// version affects the instruction bytes.
    #[test]
    fn plan_chunk_is_always_open_write_seal_seal_leaf_covering_every_byte() {
        let program_id = Pubkey::new_unique();
        let payer = Pubkey::new_unique();
        let payload: Vec<u8> = (0..(FRAME_HEADER_LEN + channel::DEFAULT_MAX_FRAME_BODY_LEN) as u32)
            .map(|i| i as u8)
            .collect();
        let ixs = plan_chunk(
            &program_id,
            &payer,
            &Pubkey::new_unique(),
            1,
            1,
            0,
            &payload,
        );
        assert_eq!(ixs.len(), 4, "Open + Write + Seal + SealLeaf, always");

        let write: zk_inbox::InboxIx =
            borsh::BorshDeserialize::try_from_slice(&ixs[1].data).unwrap();
        match write {
            zk_inbox::InboxIx::Write { offset, data } => {
                assert_eq!(offset, 0, "a single Write always starts at offset 0");
                assert_eq!(
                    data, payload,
                    "the whole frame body in one Write, no chunking"
                );
            }
            other => panic!("expected a Write instruction, got {other:?}"),
        }
    }

    /// A tiny frame plans identically — no special-casing by size.
    #[test]
    fn plan_chunk_small_frame_is_the_same_four_instruction_shape() {
        let program_id = Pubkey::new_unique();
        let payer = Pubkey::new_unique();
        let payload = vec![0u8; FRAME_HEADER_LEN + 50];
        let ixs = plan_chunk(
            &program_id,
            &payer,
            &Pubkey::new_unique(),
            1,
            1,
            0,
            &payload,
        );
        assert_eq!(ixs.len(), 4);
    }

    /// One entry per input frame (never flattened together); each frame's own plan is exactly
    /// `into_stages(plan_chunk(..))` — one stage of one transaction.
    #[test]
    fn build_frame_jobs_returns_one_single_tx_stage_per_frame() {
        let target = BatchTarget {
            program_id: Pubkey::new_unique(),
            settlement_program: Pubkey::new_unique(),
            payer: Pubkey::new_unique(),
            chain_id: 7,
            batch: 3,
        };
        let small = Frame {
            channel_id: [0u8; 16],
            frame_no: 0,
            is_last: false,
            body: vec![1u8; 50],
        };
        let large = Frame {
            channel_id: [0u8; 16],
            frame_no: 1,
            is_last: true,
            body: vec![2u8; 3_681],
        };
        let frames = build_frame_jobs(target, &[small.clone(), large.clone()]);

        assert_eq!(
            frames.len(),
            2,
            "one entry per input frame, never flattened together"
        );
        for f in &frames {
            assert_eq!(f.len(), 1, "every frame is a single stage");
            assert_eq!(f[0].len(), 1, "that stage holds a single transaction");
            assert_eq!(
                f[0][0].len(),
                4,
                "Open + Write + Seal + SealLeaf, one V1 transaction"
            );
        }
    }

    #[test]
    fn cut_frames_bodies_never_exceed_the_configured_frame_budget_regardless_of_tx_plan() {
        let blocks = sample_blocks();
        let compressed = encode_stream(&blocks);
        let frames = cut_frames(1, 1, &compressed, 32);
        for f in &frames {
            assert!(f.body.len() <= 32);
        }
    }

    // ===== Pre-finalize leaf verification =====

    /// Hand-builds raw batch-account bytes (`rome_zk_layouts::batch` layout) with exactly the given
    /// `(idx, leaf_hash)` pairs marked present in the bitmap — everything else (including any leaf not
    /// listed) left absent, matching a batch mid-flight before every leaf is sealed.
    fn build_batch_account_bytes(expected_count: u32, sealed: &[(u32, [u8; 32])]) -> Vec<u8> {
        build_batch_account_bytes_for(rome_zk_layouts::batch::VERSION, expected_count, sealed)
    }

    /// Like [`build_batch_account_bytes`] for a header of `version` (2 or 3): the version byte is set,
    /// the account is that version's own length, and the bitmap and leaves sit at that version's own
    /// offsets (a v3 header is 80 bytes longer than a v2 one).
    fn build_batch_account_bytes_for(
        version: u8,
        expected_count: u32,
        sealed: &[(u32, [u8; 32])],
    ) -> Vec<u8> {
        let mut d =
            vec![0u8; rome_zk_layouts::batch::account_len_for(version, expected_count).unwrap()];
        d[0..4].copy_from_slice(&rome_zk_layouts::batch::MAGIC.to_le_bytes());
        d[4] = version;
        d[rome_zk_layouts::batch::OFF_EXPECTED_COUNT
            ..rome_zk_layouts::batch::OFF_EXPECTED_COUNT + 4]
            .copy_from_slice(&expected_count.to_le_bytes());
        let bitmap_off = rome_zk_layouts::batch::header_len(version).unwrap();
        let leaves_off =
            rome_zk_layouts::batch::leaves_offset_for(version, expected_count).unwrap();
        for (idx, hash) in sealed {
            d[bitmap_off + (*idx as usize) / 8] |= 1 << (*idx % 8);
            let slot = leaves_off + 32 * (*idx as usize);
            d[slot..slot + 32].copy_from_slice(hash);
        }
        d
    }

    fn frame(frame_no: u16, is_last: bool, body: Vec<u8>) -> Frame {
        Frame {
            channel_id: [0u8; 16],
            frame_no,
            is_last,
            body,
        }
    }

    /// Every leaf the bitmap marks present must match `keccak(frame.to_bytes())` for this run's own
    /// frames — the happy path.
    #[test]
    fn verify_presealed_leaves_accepts_bodies_matching_what_was_actually_sealed() {
        let frames = vec![frame(0, true, vec![1, 2, 3])];
        let hash = solana_program::keccak::hashv(&[&frames[0].to_bytes()]).to_bytes();
        let data = build_batch_account_bytes(1, &[(0, hash)]);
        verify_presealed_leaves(&data, 1, &frames).expect("matching leaf must verify");
    }

    /// A v3 account (290-byte header) has its bitmap and leaves 80 bytes further in than a v2 one; the
    /// check reads them there, so a matching leaf verifies and a tampered one is still caught.
    #[test]
    fn verify_presealed_leaves_reads_a_v3_account_at_the_v3_offsets() {
        let frames = vec![frame(0, false, vec![1, 2, 3]), frame(1, true, vec![4, 5])];
        let hash0 = solana_program::keccak::hashv(&[&frames[0].to_bytes()]).to_bytes();
        let hash1 = solana_program::keccak::hashv(&[&frames[1].to_bytes()]).to_bytes();
        let data = build_batch_account_bytes_for(
            rome_zk_layouts::batch::VERSION_V3,
            2,
            &[(0, hash0), (1, hash1)],
        );
        assert_eq!(
            data.len(),
            rome_zk_layouts::batch::HEADER_LEN_V3 + 1 + 64,
            "the 290-byte v3 header, one bitmap byte and two leaves"
        );
        verify_presealed_leaves(&data, 2, &frames).expect("matching v3 leaves must verify");

        let tampered = vec![frame(0, false, vec![9, 9, 9]), frame(1, true, vec![4, 5])];
        match verify_presealed_leaves(&data, 2, &tampered).unwrap_err() {
            PipelineError::PreFinalizeLeafMismatch { idx, .. } => assert_eq!(idx, 0),
            other => panic!("expected PreFinalizeLeafMismatch, got {other:?}"),
        }
    }

    /// A v3 account whose bitmap and leaf were written at the old v2 offsets reads as nothing present,
    /// so the check cannot be satisfied by a v2-shaped body under a v3 version byte.
    #[test]
    fn verify_presealed_leaves_does_not_read_a_v3_account_at_the_v2_offsets() {
        let frames = vec![frame(0, true, vec![1, 2, 3])];
        let hash = solana_program::keccak::hashv(&[&frames[0].to_bytes()]).to_bytes();
        let mut data = build_batch_account_bytes_for(rome_zk_layouts::batch::VERSION_V3, 1, &[]);
        // The leaf is marked present and written where a v2 header would put them.
        data[rome_zk_layouts::batch::HEADER_LEN_V2] |= 1;
        let v2_leaf = rome_zk_layouts::batch::HEADER_LEN_V2 + 1;
        data[v2_leaf..v2_leaf + 32].copy_from_slice(&[0xEE; 32]);
        // At the v3 offsets nothing is present, so there is nothing to compare and nothing mismatches.
        verify_presealed_leaves(&data, 1, &frames).expect("the v3 bitmap is empty");
        // Marked at the v3 offset with the right hash, it verifies.
        let ok = build_batch_account_bytes_for(rome_zk_layouts::batch::VERSION_V3, 1, &[(0, hash)]);
        verify_presealed_leaves(&ok, 1, &frames)
            .expect("v3 bitmap and leaf are read at v3 offsets");
    }

    /// A version byte that is neither 2 nor 3 is refused by name, never read at a guessed offset.
    #[test]
    fn verify_presealed_leaves_refuses_an_unknown_version_byte() {
        let frames = vec![frame(0, true, vec![1, 2, 3])];
        let mut data = build_batch_account_bytes(1, &[]);
        data[rome_zk_layouts::batch::OFF_VERSION] = 4;
        assert!(matches!(
            verify_presealed_leaves(&data, 1, &frames).unwrap_err(),
            PipelineError::Rederive(_)
        ));
    }

    /// A leaf whose on-chain (pre-finalize) hash does not
    /// match `keccak(frame.to_bytes())` for the frame this run actually holds must refuse — this is the
    /// exact case `verify_acc` alone misses until *after* `FinalizeBatch`, by which point the batch can no
    /// longer be `AbandonBatch`ed.
    #[test]
    fn verify_presealed_leaves_rejects_a_tampered_body_before_finalize() {
        let honest = frame(0, true, vec![1, 2, 3]);
        let honest_hash = solana_program::keccak::hashv(&[&honest.to_bytes()]).to_bytes();
        // On chain, leaf 0's sealed hash is `honest_hash` (as if the real chunk body were `[1,2,3]`).
        let data = build_batch_account_bytes(1, &[(0, honest_hash)]);
        // This run's own frame 0, however, has a different (tampered/substituted) body.
        let tampered = vec![frame(0, true, vec![9, 9, 9])];
        let err = verify_presealed_leaves(&data, 1, &tampered).unwrap_err();
        match err {
            PipelineError::PreFinalizeLeafMismatch { idx, .. } => assert_eq!(idx, 0),
            other => panic!("expected PreFinalizeLeafMismatch, got {other:?}"),
        }
    }

    /// A leaf the bitmap does not yet mark present must never be checked — pre-finalize, some leaves may
    /// legitimately still be in flight.
    #[test]
    fn verify_presealed_leaves_skips_leaves_not_yet_sealed() {
        let frames = vec![
            frame(0, false, vec![1]),
            // Frame 1's on-chain leaf is deliberately absent from `sealed` below (not yet SealLeaf'd) —
            // its body here is irrelevant garbage; must not be checked.
            frame(1, true, vec![0xffu8; 4]),
        ];
        let hash0 = solana_program::keccak::hashv(&[&frames[0].to_bytes()]).to_bytes();
        let data = build_batch_account_bytes(2, &[(0, hash0)]);
        verify_presealed_leaves(&data, 2, &frames)
            .expect("an unsealed leaf must be skipped, not compared");
    }

    // ===== `finalize_and_verify` must not run the pre-finalize leaf check once the batch is already
    // finalized (by this run or a third party) — `FinalizeBatch` rewrites `leaf_hashes` in place into a
    // different format, so comparing them to `keccak(frame.to_bytes())` unconditionally refuses a
    // perfectly correct, already-finalized batch. =====
    mod finalize_and_verify_gate {
        use super::*;
        use crate::resolve::ResolveError;
        use crate::sender::{SendTuning, Sender, SenderError};
        // `Sender::send_and_confirm` now returns the V1-generation `Signature` (`sender.rs`'s own
        // conversion boundary) even though `finalize_and_verify` is generic over `AccountOps` for its own
        // account reads (never a live `RpcClient` in this test module any more).
        use solana_signature::Signature;

        /// Answers every [`AccountOps::get_account`] call with the same canned batch-account bytes — good
        /// enough for `finalize_and_verify`, none of whose several reads of this fake ever see the account
        /// change. Generalising `finalize_and_verify` to `AccountOps` means this test
        /// module no longer needs a live `solana_client::rpc_sender::RpcSender` bridge into a real
        /// `RpcClient` — a plain in-memory fake is the whole seam now.
        struct FixedAccount {
            data: Vec<u8>,
        }

        impl AccountOps for FixedAccount {
            async fn get_account(&self, _pubkey: &Pubkey) -> Result<Option<Vec<u8>>, ResolveError> {
                Ok(Some(self.data.clone()))
            }

            async fn accounts_exist(&self, pubkeys: &[Pubkey]) -> Result<Vec<bool>, ResolveError> {
                Ok(vec![true; pubkeys.len()])
            }
        }

        fn account_reading(data: Vec<u8>) -> FixedAccount {
            FixedAccount { data }
        }

        /// Panics if ever asked to send anything — both this module's scenarios must resolve without
        /// `finalize_and_verify` submitting a single transaction (already finalized, or refused before
        /// ever reaching the `FinalizeBatch` step).
        struct NeverSend;
        impl Sender for NeverSend {
            async fn send_and_confirm(
                &self,
                _instructions: &[Instruction],
                _tuning: SendTuning,
            ) -> Result<Signature, SenderError> {
                panic!("finalize_and_verify must not submit anything in this test's scenario")
            }
        }

        /// Fields for [`full_batch_account_bytes`] — grouped so that helper stays under clippy's
        /// too-many-arguments threshold.
        struct FullBatchAccount<'a> {
            chain_id: u64,
            batch: u64,
            open_slot: u64,
            expected_count: u32,
            leaves_present: u32,
            finalized: bool,
            acc: [u8; 32],
            sealed: &'a [(u32, [u8; 32])],
        }

        /// Like [`build_batch_account_bytes`] but with the full fixed header populated — what
        /// `finalize_and_verify`'s actual `finalized`/not-finalized branch needs (that helper only fills
        /// in `expected_count` and the bitmap/leaf entries, enough for `verify_presealed_leaves` alone).
        fn full_batch_account_bytes(f: FullBatchAccount<'_>) -> Vec<u8> {
            use rome_zk_layouts::batch::*;
            let mut d = build_batch_account_bytes(f.expected_count, f.sealed);
            d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].copy_from_slice(&f.chain_id.to_le_bytes());
            d[OFF_BATCH..OFF_BATCH + 8].copy_from_slice(&f.batch.to_le_bytes());
            d[OFF_OPEN_SLOT..OFF_OPEN_SLOT + 8].copy_from_slice(&f.open_slot.to_le_bytes());
            d[OFF_LEAVES_PRESENT..OFF_LEAVES_PRESENT + 4]
                .copy_from_slice(&f.leaves_present.to_le_bytes());
            d[OFF_FINALIZED] = f.finalized as u8;
            d[OFF_ACC..OFF_ACC + 32].copy_from_slice(&f.acc);
            d
        }

        fn tuning() -> SendTuning {
            SendTuning {
                compute_unit_limit: 200_000,
                loaded_accounts_data_size_limit:
                    crate::config::default_loaded_accounts_data_size_limit(),
                priority_fee_micro_lamports: 1_000,
                max_priority_fee_micro_lamports: 200_000,
                confirm_timeout: std::time::Duration::from_secs(5),
                ..Default::default()
            }
        }

        fn target() -> BatchTarget {
            BatchTarget {
                program_id: Pubkey::new_unique(),
                settlement_program: Pubkey::new_unique(),
                payer: Pubkey::new_unique(),
                chain_id: 7,
                batch: 3,
            }
        }

        /// Scenario (a): a third party's `FinalizeBatch` (no signer required) lands first — the account
        /// this run reads is already `finalized`, with `leaf_hashes` in the post-finalize format (never
        /// `keccak(frame.to_bytes())` any more). `finalize_and_verify` must accept this — never compare,
        /// never submit anything — and hand back a `decoded` whose `acc` a separate `verify_acc` call
        /// then confirms. Before the fix, this refused with `PreFinalizeLeafMismatch` on a perfectly
        /// correct batch.
        #[tokio::test]
        async fn already_finalized_by_someone_else_passes_through_to_verify_acc() {
            let t = target();
            let honest = frame(0, true, vec![1, 2, 3]);
            let frames = vec![honest.clone()];
            let chunk_hash = solana_program::keccak::hashv(&[&honest.to_bytes()]).to_bytes();
            let (_, _, acc) =
                zk_inbox_client::reference_commitment(t.chain_id, t.batch, 0, &[chunk_hash]);
            // Post-finalize leaf_hashes are a different format (`idx ‖ hash`, not `keccak(body)`) — stand
            // in with a value that could never coincidentally equal `keccak(honest.to_bytes())`.
            let post_finalize_leaf = [0xAAu8; 32];
            let data = full_batch_account_bytes(FullBatchAccount {
                chain_id: t.chain_id,
                batch: t.batch,
                open_slot: 0,
                expected_count: 1,
                leaves_present: 1,
                finalized: true,
                acc,
                sealed: &[(0, post_finalize_leaf)],
            });
            let accounts = account_reading(data);
            let metrics = Metrics::new();
            let decoded = finalize_and_verify(
                &NeverSend,
                &accounts,
                &metrics,
                t,
                tuning(),
                FinalizePoll {
                    expected_count: 1,
                    poll_interval: std::time::Duration::from_millis(1),
                    max_polls: 5,
                },
                &frames,
            )
            .await
            .expect(
                "an already-finalized batch (by anyone) must pass through, never \
                 PreFinalizeLeafMismatch on its post-finalize-shaped leaf_hashes",
            );
            assert!(decoded.finalized);
            verify_acc(&decoded, &frames)
                .expect("the post-finalize content check must still confirm this batch is correct");
        }

        /// Scenario (b): the batch is *not yet* finalized, all leaves are present, but this run's own
        /// frame doesn't match what's on chain (a tampered/substituted body) — `finalize_and_verify` must
        /// still refuse. Unwiring `verify_presealed_leaves` from `finalize_and_verify` would otherwise go
        /// unnoticed: the standalone `verify_presealed_leaves_*` tests above never drive `finalize_and_verify`
        /// itself.
        #[tokio::test]
        async fn tampered_pre_finalize_leaf_refuses() {
            let t = target();
            let honest = frame(0, true, vec![1, 2, 3]);
            let honest_hash = solana_program::keccak::hashv(&[&honest.to_bytes()]).to_bytes();
            let tampered = vec![frame(0, true, vec![9, 9, 9])];
            let data = full_batch_account_bytes(FullBatchAccount {
                chain_id: t.chain_id,
                batch: t.batch,
                open_slot: 0,
                expected_count: 1,
                leaves_present: 1,
                finalized: false,
                acc: [0u8; 32],
                sealed: &[(0, honest_hash)],
            });
            let accounts = account_reading(data);
            let metrics = Metrics::new();
            let err = finalize_and_verify(
                &NeverSend,
                &accounts,
                &metrics,
                t,
                tuning(),
                FinalizePoll {
                    expected_count: 1,
                    poll_interval: std::time::Duration::from_millis(1),
                    max_polls: 5,
                },
                &tampered,
            )
            .await
            .unwrap_err();
            assert!(
                matches!(err, PipelineError::PreFinalizeLeafMismatch { idx: 0, .. }),
                "expected PreFinalizeLeafMismatch, got {err:?}"
            );
        }
    }
}
