//! Startup recovery of half-written batches: finish the open batch under its own id, never abandon it.
//!
//! A crash between `OpenBatch` and `FinalizeBatch` leaves a batch id that settlement still needs: the
//! settlement program posts exactly `head_pending_batch + 1`, that id's inbox account must be finalized,
//! and an id the inbox cursor has passed can never be opened again. So the restarted batcher never
//! abandons such a batch; it **finishes the same id**.
//!
//! What the chain still holds about an open batch: `expected_count`, `open_unix_ts`, the presence bitmap
//! and, per present leaf, `keccak(frame bytes)`. What it does not hold is the frames that never landed;
//! those exist only in the ordered log, and the group the crashed process cut can not be read back (an
//! age close depends on the crashed process's own clock, so a restart finds more blocks in the log than
//! the crashed process had). So for each open batch this module searches for a grouping that explains the
//! chain:
//!
//! - start at the block after the end of the batch below it (the chain anchor walk over the ids below the
//!   first open one, then the previous resumed batch's own last block);
//! - try each end `e` in `start ..= start + blocks_per_batch - 1`, smallest first;
//! - keep `e` only if (1) `cut_frames` gives exactly `expected_count` frames, (2) every present leaf equals
//!   `keccak` of that candidate's frame at that index ([`pipeline::verify_presealed_leaves`], cursor-aware
//!   once a finalize has started), and (3) every block passes derive's drift rule
//!   (`timestamp <= open_unix_ts + max_drift_secs`).
//!
//! Any passing candidate is valid: its present frames are byte-identical to what is on chain, so the
//! finished batch is exactly that candidate's channel and decodes cleanly. With every leaf already present
//! there is no search at all: the frames are read back from the chunk accounts.
//!
//! With a posting window deeper than one, several batches can be open at once; they are planned together,
//! in ascending order, each starting where the previous one ends, and the search backtracks over a lower
//! batch's candidates when a higher one finds none. **Nothing is sent until the whole plan exists.** If no
//! plan passes (zstd version, frame size, `blocks_per_batch` or the log changed since the crash) the result
//! is the named [`PipelineError::ResumeImpossible`]: nothing sent, nothing abandoned, repairable by
//! rerunning the build and config that opened the batch.
//!
//! This relies on the one-batcher-per-authority rule (`CursorAdvanced`): two live batchers picking
//! different candidates for a zero-leaf batch could mix frames.

use solana_program::pubkey::Pubkey;
use std::collections::HashMap;

use crate::anchor;
use crate::channel::{self, Block, Frame};
use crate::metrics::Metrics;
use crate::pipeline::{
    self, BatchTarget, FinalizePoll, PipelineError, StartupError, StartupRecover,
};
use crate::resolve::{self, AccountOps};
use crate::resume::BatchAccountState;
use crate::sender::Sender;
use crate::sink::PostRootSink;
use crate::source::BlockSource;

/// One open-not-finalized batch found in the pending window.
struct OpenBatch {
    batch: u64,
    raw: Vec<u8>,
    account: zk_inbox_client::BatchAccount,
}

/// The frames (and the blocks they carry) a batch will be finished with.
struct Chosen {
    frames: Vec<Frame>,
    blocks: Vec<Block>,
}

/// Blocks read from the ordered log, contiguous from `first`.
struct LogBlocks {
    first: u64,
    blocks: Vec<Block>,
}

fn leaf_present(raw: &[u8], idx: u32) -> bool {
    raw.get(rome_zk_layouts::batch::HEADER_LEN + (idx as usize) / 8)
        .is_some_and(|byte| (byte >> (idx % 8)) & 1 == 1)
}

/// True iff every present leaf of the batch equals the leaf `frames` would produce. Before a finalize has
/// started this is [`pipeline::verify_presealed_leaves`]; once `FinalizeBatch` has transformed the first
/// `finalize_cursor` leaves in place (`indexed_leaf(idx, keccak(frame))`), those are compared in that form.
fn present_leaves_match(open: &OpenBatch, frames: &[Frame]) -> Result<(), PipelineError> {
    let expected_count = open.account.expected_count;
    if open.account.finalize_cursor == 0 {
        return pipeline::verify_presealed_leaves(&open.raw, expected_count, frames);
    }
    let leaves_off = rome_zk_layouts::batch::leaves_offset(expected_count);
    for frame in frames {
        let idx = frame.frame_no as u32;
        if idx >= expected_count || !leaf_present(&open.raw, idx) {
            continue;
        }
        let slot = leaves_off + 32 * idx as usize;
        let on_chain: [u8; 32] = open
            .raw
            .get(slot..slot + 32)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(|| {
                PipelineError::Rederive(format!("batch account too short to read leaf {idx}"))
            })?;
        let raw_leaf = solana_program::keccak::hashv(&[&frame.to_bytes()]).to_bytes();
        let expected = if idx < open.account.finalize_cursor {
            rome_zk_merkle::indexed_leaf(&rome_zk_merkle::keccak256, idx, &raw_leaf)
        } else {
            raw_leaf
        };
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

struct Search<'a> {
    chain_id: u64,
    blocks_per_batch: u64,
    max_frame_body_len: usize,
    max_drift_secs: u64,
    open: &'a [OpenBatch],
    /// `Some(chosen)` for a batch whose every leaf is already present (read back from chain).
    present: &'a [Option<Chosen>],
    log: &'a LogBlocks,
    /// `encode_stream` of `log.blocks[start..=end]`, keyed by `(start, end)`: the encoding does not depend
    /// on the batch id, so backtracking never re-encodes a group.
    encoded: HashMap<(usize, usize), Vec<u8>>,
    /// Deepest open-batch index at which no candidate passed.
    deepest_failure: usize,
}

impl Search<'_> {
    /// Chooses, for `open[i..]`, a consistent set of groupings starting at block `start_number`. Pushes the
    /// choices onto `out` (popped again on backtrack) and returns whether every batch got one.
    fn plan(&mut self, i: usize, start_number: u64, out: &mut Vec<Chosen>) -> bool {
        let Some(open) = self.open.get(i) else {
            return true;
        };
        let consecutive = i == 0 || open.batch == self.open[i - 1].batch + 1;
        if !consecutive {
            // An id between two open ones is gone (abandoned by hand): its blocks are unknown, so
            // the start of this one is too.
            self.deepest_failure = self.deepest_failure.max(i);
            return false;
        }

        if let Some(chosen) = &self.present[i] {
            let first = chosen.blocks.first().map(|b| b.number);
            let last = chosen.blocks.last().map(|b| b.number);
            let (Some(first), Some(last)) = (first, last) else {
                self.deepest_failure = self.deepest_failure.max(i);
                return false;
            };
            if first != start_number {
                self.deepest_failure = self.deepest_failure.max(i);
                return false;
            }
            out.push(Chosen {
                frames: chosen.frames.clone(),
                blocks: chosen.blocks.clone(),
            });
            if self.plan(i + 1, last + 1, out) {
                return true;
            }
            out.pop();
            return false;
        }

        let bound = u64::try_from(open.account.open_unix_ts)
            .unwrap_or(0)
            .saturating_add(self.max_drift_secs);
        if start_number < self.log.first {
            self.deepest_failure = self.deepest_failure.max(i);
            return false;
        }
        let start = (start_number - self.log.first) as usize;
        let end_limit = (start as u64 + self.blocks_per_batch).min(self.log.blocks.len() as u64);
        let mut any = false;
        for end in start..end_limit as usize {
            let group = &self.log.blocks[start..=end];
            if group.iter().any(|b| b.timestamp > bound) {
                // A longer group only adds blocks, so none of the later ends can pass either.
                break;
            }
            let compressed = self
                .encoded
                .entry((start, end))
                .or_insert_with(|| channel::encode_stream(group))
                .clone();
            let frames = channel::cut_frames(
                self.chain_id,
                open.batch,
                &compressed,
                self.max_frame_body_len,
            );
            if frames.len() != open.account.expected_count as usize {
                continue;
            }
            if present_leaves_match(open, &frames).is_err() {
                continue;
            }
            if pipeline::re_derive_and_check(group, &compressed).is_err() {
                continue;
            }
            any = true;
            out.push(Chosen {
                frames,
                blocks: group.to_vec(),
            });
            if self.plan(i + 1, self.log.blocks[end].number + 1, out) {
                return true;
            }
            out.pop();
        }
        if !any {
            self.deepest_failure = self.deepest_failure.max(i);
        }
        false
    }
}

/// The chain's committed drift bound, or `u64::MAX` (no bound) when the chain's `chain_config` is absent
/// or still v1 — derive refuses such a chain by name, so there is nothing to enforce here.
async fn read_max_drift_secs<A: AccountOps>(
    accounts: &A,
    settlement_program_id: &Pubkey,
    chain_id: u64,
) -> Result<u64, PipelineError> {
    let (pda, _) = zk_settlement_client::chain_config_pda(settlement_program_id, chain_id);
    let Some(data) = accounts.get_account(&pda).await? else {
        tracing::warn!(
            "chain_config missing for chain {chain_id}: the resume search checks no drift bound"
        );
        return Ok(u64::MAX);
    };
    match zk_settlement_client::decode_chain_config_account(&data)?.max_drift_secs {
        Some(v) if v > 0 => Ok(v),
        _ => {
            tracing::warn!(
                "chain_config for chain {chain_id} has no drift bound: the resume search checks none"
            );
            Ok(u64::MAX)
        }
    }
}

/// Finishes every open-not-finalized batch in the pending window `[root.head_final_batch,
/// cursor.next_batch)`, in ascending id order, and returns the ids it finished. See the module doc.
/// Sends nothing until the full plan for every open batch exists.
pub async fn resume_open_batches<A: AccountOps, S: Sender>(
    accounts: &A,
    sender: &S,
    metrics: &Metrics,
    sink: &dyn PostRootSink,
    cfg: &StartupRecover<'_>,
) -> Result<Vec<u64>, StartupError> {
    let w = cfg.window;
    let (cursor_pda, _) =
        zk_inbox_client::cursor_pda(&w.inbox_program_id, &w.settlement_program_id, w.chain_id);
    let next_batch = match accounts
        .get_account(&cursor_pda)
        .await
        .map_err(PipelineError::from)?
    {
        None => return Ok(Vec::new()), // InitBatchCursor never run for this chain — nothing open.
        Some(data) => {
            zk_inbox_client::decode_batch_cursor(&data)
                .map_err(PipelineError::from)?
                .next_batch
        }
    };
    let (root_pda, _) = zk_settlement_client::root_pda(&w.settlement_program_id, w.chain_id);
    let head_final_batch = match accounts
        .get_account(&root_pda)
        .await
        .map_err(PipelineError::from)?
    {
        None => 0,
        Some(data) => {
            zk_settlement_client::decode_root_account(&data)
                .map_err(PipelineError::from)?
                .head_final_batch
        }
    };

    // Paged, never one read per id: while nothing has settled this window spans every id ever opened.
    let ids: Vec<u64> = (head_final_batch..next_batch).collect();
    let states = resolve::probe_batch_states_paged(
        accounts,
        &w.inbox_program_id,
        &w.settlement_program_id,
        w.chain_id,
        &ids,
    )
    .await
    .map_err(PipelineError::from)?;

    // Defense in depth (`FinalizeBatch` is authority-gated on chain): a Finalized id ABOVE an open one means
    // the open batch's blocks sit before blocks already finalized, so finishing it could not keep the log
    // contiguous. Refuse by name, send nothing, leave the state repairable.
    let highest_finalized = ids
        .iter()
        .zip(&states)
        .filter(|(_, st)| matches!(st, BatchAccountState::Finalized))
        .map(|(&b, _)| b)
        .max();
    if let Some(finalized) = highest_finalized {
        if let Some((&open, _)) = ids.iter().zip(&states).find(|(&b, st)| {
            b < finalized && matches!(st, BatchAccountState::OpenNotFinalized { .. })
        }) {
            return Err(PipelineError::FinalizedAboveOpenBatch { open, finalized }.into());
        }
    }

    let open_ids: Vec<u64> = ids
        .iter()
        .zip(&states)
        .filter(|(_, st)| matches!(st, BatchAccountState::OpenNotFinalized { .. }))
        .map(|(&b, _)| b)
        .collect();
    if open_ids.is_empty() {
        return Ok(Vec::new());
    }

    // ----- Plan (reads only) -----
    let mut open = Vec::with_capacity(open_ids.len());
    for &batch in &open_ids {
        let (pda, _) = zk_inbox_client::batch_pda(
            &w.inbox_program_id,
            &w.settlement_program_id,
            w.chain_id,
            batch,
        );
        let raw = accounts
            .get_account(&pda)
            .await
            .map_err(PipelineError::from)?
            .ok_or(PipelineError::BatchVanished { batch })?;
        let account = zk_inbox_client::decode_batch_account(&raw).map_err(PipelineError::from)?;
        open.push(OpenBatch {
            batch,
            raw,
            account,
        });
    }

    let mut present: Vec<Option<Chosen>> = Vec::with_capacity(open.len());
    for ob in &open {
        if ob.account.leaves_present < ob.account.expected_count {
            present.push(None);
            continue;
        }
        // Every leaf is already there: read the frames back, no search.
        let read_err = |reason: String| PipelineError::ResumeRead {
            batch: ob.batch,
            reason,
        };
        let frames = anchor::read_batch_frames(
            accounts,
            &w.inbox_program_id,
            &w.settlement_program_id,
            w.chain_id,
            ob.batch,
            ob.account.expected_count,
        )
        .await
        .map_err(|e| read_err(e.to_string()))?;
        present_leaves_match(ob, &frames)?;
        let compressed = channel::reassemble(&frames).map_err(|e| read_err(e.to_string()))?;
        let blocks = channel::decode_stream(&compressed).map_err(|e| read_err(e.to_string()))?;
        present.push(Some(Chosen { frames, blocks }));
    }

    let anchor0 = anchor::resolve_anchor_below(
        accounts,
        &w.inbox_program_id,
        &w.settlement_program_id,
        w.chain_id,
        cfg.log_dir,
        cfg.sub_blocks_per_block,
        cfg.block_gas_limit,
        Some(open[0].batch),
    )
    .await?;

    // Blocks from the log: enough for every batch that still needs a grouping.
    let searching = present.iter().filter(|p| p.is_none()).count() as u64;
    let mut log = LogBlocks {
        first: anchor0.from_block,
        blocks: Vec::new(),
    };
    if searching > 0 {
        let log_err = |batch: u64, reason: String| PipelineError::ResumeRead { batch, reason };
        let wanted = open.len() as u64 * cfg.blocks_per_batch;
        let mut source = BlockSource::open(
            cfg.log_dir,
            w.chain_id,
            cfg.block_gas_limit,
            cfg.sub_blocks_per_block,
            anchor0.from_block,
            anchor0.prev_block_timestamp_secs,
        )
        .map_err(|e| log_err(open[0].batch, format!("opening the ordered log: {e}")))?;
        while (log.blocks.len() as u64) < wanted {
            match source
                .next_block()
                .map_err(|e| log_err(open[0].batch, format!("reading the ordered log: {e}")))?
            {
                Some(sourced) => log.blocks.push(sourced.block),
                None => break,
            }
        }
        // A gap in the numbering ends the usable log there: only the blocks before it can be grouped.
        let first = anchor0.from_block;
        if let Some(k) = log
            .blocks
            .iter()
            .enumerate()
            .position(|(k, b)| b.number != first + k as u64)
        {
            log.blocks.truncate(k);
        }
    }

    let max_drift_secs =
        read_max_drift_secs(accounts, &w.settlement_program_id, w.chain_id).await?;
    let mut search = Search {
        chain_id: w.chain_id,
        blocks_per_batch: cfg.blocks_per_batch,
        max_frame_body_len: w.max_frame_body_len,
        max_drift_secs,
        open: &open,
        present: &present,
        log: &log,
        encoded: HashMap::new(),
        deepest_failure: 0,
    };
    let mut plan = Vec::with_capacity(open.len());
    if !search.plan(0, anchor0.from_block, &mut plan) {
        let failed = &open[search.deepest_failure.min(open.len() - 1)];
        return Err(PipelineError::ResumeImpossible {
            batch: failed.batch,
            leaves_present: failed.account.leaves_present,
            expected_count: failed.account.expected_count,
        }
        .into());
    }

    // ----- Execute, ascending: each batch is finalized before the next one's frames go out. -----
    let mut resumed = Vec::with_capacity(open.len());
    for (ob, chosen) in open.iter().zip(&plan) {
        let target = BatchTarget {
            program_id: w.inbox_program_id,
            settlement_program: w.settlement_program_id,
            payer: w.payer,
            chain_id: w.chain_id,
            batch: ob.batch,
        };
        let missing: Vec<Frame> = chosen
            .frames
            .iter()
            .filter(|f| !leaf_present(&ob.raw, f.frame_no as u32))
            .cloned()
            .collect();
        tracing::warn!(
            "batch {} was open-not-finalized at startup ({}/{} leaves present): resuming it, sending {} \
             missing frame(s) for blocks {}..={}",
            ob.batch,
            ob.account.leaves_present,
            ob.account.expected_count,
            missing.len(),
            chosen.blocks.first().map(|b| b.number).unwrap_or_default(),
            chosen.blocks.last().map(|b| b.number).unwrap_or_default(),
        );
        if !missing.is_empty() {
            let jobs = pipeline::build_frame_jobs(target, &missing);
            sender
                .send_and_confirm_many_retrying_compute(
                    &jobs,
                    w.chunk_tuning,
                    w.chunk_retry_compute_unit_limit,
                    w.in_flight_frames,
                    w.confirm_poll_interval,
                    w.signature_status_batch_size,
                )
                .await
                .map_err(PipelineError::from)?;
            for _ in &missing {
                metrics.frames_sent_total.inc();
            }
        }
        let decoded = pipeline::finalize_and_verify(
            sender,
            accounts,
            metrics,
            target,
            w.finalize_tuning,
            FinalizePoll {
                expected_count: ob.account.expected_count,
                poll_interval: w.finalize_poll_interval,
                max_polls: w.finalize_max_polls,
            },
            &chosen.frames,
        )
        .await?;
        pipeline::verify_acc(&decoded, &chosen.frames)?;
        pipeline::hand_off(sink, &decoded, &chosen.blocks);
        resumed.push(ob.batch);
    }
    Ok(resumed)
}
