//! **Chain anchor** (replacing the local `batcher-cursor.json`): both `--once` and
//! `--follow` resume from the same on-chain fact — the last finalized inbox batch's own posted blocks,
//! re-derived from its sealed chunks — instead of a local file this process wrote about itself.
//!
//! ## Why not a local file
//! The old `batcher-cursor.json` recorded `next_from_block`/`last_block_timestamp` first-hand, but nothing
//! on chain could ever verify *those specific numbers* — only that the file's referenced batch id agreed
//! with the chain's own `batch_cursor` and was finalized. Any crash between an `OpenBatch` confirming and
//! the file being written (or a hand-edited/restored/copied-between-hosts file) left a resume point that
//! looked "verified" but was not: the "no mis-mappable cursor" rule, applied to this consumer,
//! rules it out. `rome-zk-derive`'s own resume anchor is engine-hash-verified (`resume.rs` there); this
//! module gives the batcher an equivalent: **re-derive** the last finalized batch's blocks from its own
//! sealed chunks (the same bytes `rome-zk-derive::inbox::InboxRetrieval` reads on the read side — the
//! parsing here mirrors that approach against this crate's own [`crate::resolve::AccountOps`], never a
//! normal dependency on `rome-zk-derive`) rather than trust anything this process wrote about itself.
//!
//! ## The walk
//! 1. Read `cursor.next_batch = n` (`zk_inbox_client::decode_batch_cursor`). `n == 0` → genesis:
//!    [`Anchor::from_block`] **`1`** (the ordered log starts at block 1 — genesis 0
//!    is never sealed), timestamp seed `0`.
//! 2. Else walk every ever-opened batch id `b = n-1` down to `0`, newest first, **paged** (
//!    `AccountOps::get_multiple_account_data`, up to 100 ids per read — the walk is cheap on Tiber today
//!    (0 probes: the newest id is finalized), but a worst case where every recent id was abandoned would
//!    otherwise be one sequential round trip per id):
//!    - `Finalized` → decode its blocks from its own (still-intact) chunks — see
//!      [`decode_anchor_from_finalized_batch`]. If that batch's chunks are already gone (rent-recycled by
//!      `Close`, which requires `head_final_batch >= batch`, so a batch strictly ahead of
//!      `head_final_batch` is *guaranteed* to still have an intact account) **and**
//!      `head_final_batch >= batch` (that is what "recycled" means — the chunks were legitimately
//!      reclaimed, not lost), fall through to the settlement-root fallback below instead of
//!      refusing. A batch *below* `head_final_batch` whose chunks are genuinely missing without that
//!      being true is a different, refused condition ([`AnchorError::ChunkMissing`]) — `Close` could not
//!      have legitimately run yet.
//!    - `OpenNotFinalized` → [`AnchorError::UnexpectedOpenBatch`]: this is the job of
//!      `crate::pipeline::startup_recover`, which finishes every open batch *before* this resolution so
//!      the walk never actually observes this state at `next_batch - 1`; seeing it here is a caller-order
//!      bug, not a runtime condition to route around.
//!    - `Missing` (abandoned, or never opened, or already rent-recycled) → keep walking.
//! 3. If the walk reaches batch `0` with nothing finalized-and-intact found, every ever-opened batch was
//!    abandoned or already rent-recycled — anchor on the settlement root's own `number`
//!    (`OFF_NUMBER` already equals the corresponding block index — the sequencer numbers its first sealed
//!    block 1, so `header.number == BlockEnv.number` directly, no offset). The **next** block to post is
//!    therefore `root.number + 1`: the settlement sentinel `OFF_NUMBER == 0` means nothing has ever
//!    settled, giving `from_block = 1` — the same fresh-chain value as step 1, not `0`.
//!    The timestamp seed is not on the root account, so it is recomputed exactly by replaying the ordered
//!    log's own monotonic-timestamp recurrence from block 1 up to `from_block - 1`
//!    (`O(log)`, a restart cost only).
//!
//! **Numbering-origin check.** Before any of the above runs, [`resolve_anchor`] calls
//! `rome_zk_log::log_numbering_origin` on `log_dir` directly — a named `AnchorError::LogNumbering`
//! refusal unless the log's own first record is `(block 1, index 0)` (or the log is empty). This is
//! independent of `profile.json`, deliberately: this batcher never relies on that file to know the log's
//! true origin, it reads the log itself, the same way [`replay_prev_timestamp_secs`] does for the
//! timestamp seed — so a 0-based log is unconstructable as this batcher's input whatever the file next
//! to it claims (this crate's `config::read_profile_identity` also checks `first_block` by value, as a
//! second, independent guard on the same field).

use std::path::{Path, PathBuf};

use solana_program::pubkey::Pubkey;

use crate::channel::{self, Frame};
use crate::pipeline::{self, PipelineError};
use crate::resolve::{self, AccountOps, ResolveError};
use crate::resume::BatchAccountState;
use crate::source::{BlockSource, SourceError};

/// How many batch ids the anchor walk probes per `getMultipleAccounts`-style page — the one shared constant
/// `resolve::probe_batch_states_paged`, the startup recovery's own paging, and this walk all use;
/// mirrors `resolve::AccountOps::get_multiple_account_data`'s own real-RPC page size.
const ANCHOR_WALK_PAGE_SIZE: usize = resolve::PROBE_PAGE_SIZE;

/// Decodes one already-fetched batch account's raw bytes into a [`BatchAccountState`] — the walk's own
/// per-id decode, applied to a page [`AccountOps::get_multiple_account_data`] already read. Wraps
/// [`resolve::decode_batch_probe`] (shared with the startup recovery — "one home" for the
/// per-entry decode) into this module's own [`AnchorError`] instead of `ResolveError`, so this walk's own
/// error shape (and the regression test pinning it) is unchanged.
fn decode_probe(data: Option<Vec<u8>>, batch: u64) -> Result<BatchAccountState, AnchorError> {
    resolve::decode_batch_probe(data, batch).map_err(|e| match e {
        ResolveError::BatchAccountDecode { batch, source } => {
            AnchorError::BatchDecode { batch, source }
        }
        other => AnchorError::ChainRead(other),
    })
}

/// What `--once` and `--follow` both resume from: the next block [`crate::source::BlockSource`] should
/// open at, and the `prev_block_timestamp_secs` seed the monotonic-timestamp floor needs
/// alongside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    pub from_block: u64,
    pub prev_block_timestamp_secs: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum AnchorError {
    #[error("reading on-chain state to resolve the resume anchor: {0}")]
    ChainRead(#[from] ResolveError),
    #[error("decoding batch_cursor account for chain {chain_id}: {source}")]
    CursorDecode {
        chain_id: u64,
        #[source]
        source: zk_inbox_client::DecodeError,
    },
    #[error("decoding settlement root account for chain {chain_id}: {source}")]
    RootDecode {
        chain_id: u64,
        #[source]
        source: zk_settlement_client::DecodeError,
    },
    #[error("decoding batch account for batch {batch}: {source}")]
    BatchDecode {
        batch: u64,
        #[source]
        source: zk_inbox_client::DecodeError,
    },
    #[error("batch {batch}'s account vanished between reads (raced by a concurrent process?)")]
    BatchAccountVanished { batch: u64 },
    #[error("batch {batch} chunk {idx} is missing but the batch is finalized")]
    ChunkMissing { batch: u64, idx: u32 },
    #[error("batch {batch} chunk {idx}: {source}")]
    ChunkHeaderDecode {
        batch: u64,
        idx: u32,
        #[source]
        source: zk_inbox_client::DecodeError,
    },
    #[error(
        "batch {batch} chunk {idx}: header reports (chain_id={header_chain_id}, batch={header_batch}, \
         idx={header_idx}), expected (chain_id={expected_chain_id}, batch={batch}, idx={idx})"
    )]
    ChunkHeaderMismatch {
        batch: u64,
        idx: u32,
        header_chain_id: u64,
        header_batch: u64,
        header_idx: u32,
        expected_chain_id: u64,
    },
    #[error("batch {batch} chunk {idx} is not sealed but the batch is finalized")]
    ChunkNotSealed { batch: u64, idx: u32 },
    #[error("batch {batch} chunk {idx}: frame header says frame_no {frame_no} (frame_no must equal the chunk idx)")]
    FrameNoMismatch { batch: u64, idx: u32, frame_no: u16 },
    #[error("batch {batch}: {source}")]
    Channel {
        batch: u64,
        #[source]
        source: channel::ChannelError,
    },
    #[error(
        "batch {batch}'s on-chain acc did not verify against its own re-derived chunks: {source}"
    )]
    Verify {
        batch: u64,
        #[source]
        source: PipelineError,
    },
    #[error("batch {batch} decoded to zero blocks — a batch can never be empty")]
    EmptyBlocks { batch: u64 },
    #[error(
        "batch_cursor.next_batch={n} names batch {batch} (in the pending window, > \
         head_final_batch={head_final_batch}) as open-not-finalized — it must be finished \
         (`pipeline::startup_recover`) before the resume anchor is \
         resolved; seeing it here is a caller-ordering bug"
    )]
    UnexpectedOpenBatch {
        n: u64,
        batch: u64,
        head_final_batch: u64,
    },
    #[error(
        "replaying the ordered log at {log_dir:?} to recompute the closed-batch timestamp seed up to \
         block {anchor_block}: {source}"
    )]
    LogReplayIo {
        log_dir: PathBuf,
        anchor_block: u64,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "replaying the ordered log at {log_dir:?} to recompute the closed-batch timestamp seed up to \
         block {anchor_block}: {source}"
    )]
    LogReplaySource {
        log_dir: PathBuf,
        anchor_block: u64,
        #[source]
        source: SourceError,
    },
    #[error(
        "replaying the ordered log at {log_dir:?} to recompute the closed-batch timestamp seed: the log \
         ends after block {got}, but block {anchor_block} was expected to already be settled on chain — \
         log and chain have diverged"
    )]
    LogReplayShort {
        log_dir: PathBuf,
        anchor_block: u64,
        got: u64,
    },
    #[error(
        "--from-block {given} does not match the resolved on-chain anchor {anchor} — refusing to guess; \
         pass --from-block {anchor} (the verified value) or omit --from-block to use the anchor directly"
    )]
    FromBlockMismatch { given: u64, anchor: u64 },
    /// Both `--once` and `--follow` open the log AT the anchor and must see the anchor's
    /// own block as the very first thing the log hands back — anything else is a gap in the log (the log
    /// does not hold the anchor's block: pruned, rotated, or the log and chain have genuinely diverged).
    /// Checked once, right after the log is opened, before any grouping happens (`bin/rome-zk-batcher.rs`).
    #[error(
        "the log's first block read is {first_block}, but the on-chain anchor expects block {anchor} — \
         gap in the log, or the log and chain have diverged"
    )]
    Gap { first_block: u64, anchor: u64 },
    /// The ordered log is premised to start at block 1 (genesis 0 is never
    /// sealed) and never be pruned — [`replay_prev_timestamp_secs`]'s own walk from block 1 up to the
    /// anchor must see exactly the block numbers it expects at every step, or that premise has been
    /// violated (a log rotated/truncated out from under a chain whose anchor still refers to blocks
    /// before the log's own start, or an older 0-based log). A named, release-build refusal — no
    /// longer a `debug_assert` a release binary would silently skip.
    #[error(
        "replaying the ordered log at {log_dir:?} to recompute the closed-batch timestamp seed: expected \
         block {expected} next, got block {got} — the ordered log does not start at block 1 (or has been \
         pruned), violating the premise this replay depends on"
    )]
    LogReplayMisaligned {
        log_dir: PathBuf,
        expected: u64,
        got: u64,
    },
    /// The log's numbering origin, checked independent of `profile.json` and before
    /// any on-chain state is even read — a 0-based (older) log must be refused here regardless of
    /// which anchor path (fresh chain, finalized-batch decode, or settlement-root fallback) would
    /// otherwise be taken, and regardless of whether `profile.json`'s own `first_block` field was ever
    /// stamped over it (`rome-zk-sequencer::recovery::reconcile_profile_identity`'s V0/no-file migration
    /// branches can still do that — see that crate's own doc). Named `first_block`/
    /// `first_index` exactly as `rome_zk_log::LogNumberingError::WrongOrigin` does.
    #[error(
        "the ordered log at {log_dir:?}'s first record is (block {first_block}, index {first_index}), \
         not (block 1, index 0) — this log was written under 0-based numbering by a pre-C.4d sequencer \
         and cannot be posted by this batcher"
    )]
    LogNumbering {
        log_dir: PathBuf,
        first_block: u64,
        first_index: u16,
    },
    /// A log I/O error surfaced while checking the numbering origin above (not a numbering mismatch
    /// itself — e.g. an unreadable segment file) — named distinctly rather than folded into a replay
    /// error that implies the timestamp-recompute walk actually started.
    #[error("checking the ordered log's numbering origin at {log_dir:?}: {source}")]
    LogNumberingIo {
        log_dir: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// The stateless resume, driven live against on-chain state (this module's own doc has the full
/// walk). `sub_blocks_per_block`/`block_gas_limit` are only used on the closed-batch fallback path, to
/// replay the ordered log's own timestamp recurrence — see [`replay_prev_timestamp_secs`].
pub async fn resolve_anchor<A: AccountOps>(
    accounts: &A,
    inbox_program_id: &Pubkey,
    settlement_program_id: &Pubkey,
    chain_id: u64,
    log_dir: &Path,
    sub_blocks_per_block: u16,
    block_gas_limit: u64,
) -> Result<Anchor, AnchorError> {
    resolve_anchor_below(
        accounts,
        inbox_program_id,
        settlement_program_id,
        chain_id,
        log_dir,
        sub_blocks_per_block,
        block_gas_limit,
        None,
    )
    .await
}

/// [`resolve_anchor`] over only the batch ids strictly below `below_batch` (`None` = every id the cursor has
/// issued): the start block of the first batch a startup resume finishes (`recover.rs`), computed as if that
/// batch and everything above it did not exist yet. With `Some(n)` the walk starts at `min(n, cursor)`.
#[allow(clippy::too_many_arguments)]
pub async fn resolve_anchor_below<A: AccountOps>(
    accounts: &A,
    inbox_program_id: &Pubkey,
    settlement_program_id: &Pubkey,
    chain_id: u64,
    log_dir: &Path,
    sub_blocks_per_block: u16,
    block_gas_limit: u64,
    below_batch: Option<u64>,
) -> Result<Anchor, AnchorError> {
    // The numbering-origin check runs before any on-chain state is read or any
    // block is grouped — independent of `profile.json` and of which of the three paths below (fresh
    // chain, finalized-batch decode, settlement-root fallback) this call ends up taking, so a 0-based
    // log is unconstructable as this batcher's input whatever the file next to it says.
    match rome_zk_log::log_numbering_origin(log_dir) {
        Ok(()) => {}
        Err(rome_zk_log::LogNumberingError::WrongOrigin {
            first_block,
            first_index,
        }) => {
            return Err(AnchorError::LogNumbering {
                log_dir: log_dir.to_path_buf(),
                first_block,
                first_index,
            });
        }
        Err(rome_zk_log::LogNumberingError::Io(source)) => {
            return Err(AnchorError::LogNumberingIo {
                log_dir: log_dir.to_path_buf(),
                source,
            });
        }
    }

    let (cursor_pda, _) =
        zk_inbox_client::cursor_pda(inbox_program_id, settlement_program_id, chain_id);
    let n = match accounts.get_account(&cursor_pda).await? {
        None => 0, // InitBatchCursor never run for this chain — nothing has ever been posted.
        Some(data) => {
            zk_inbox_client::decode_batch_cursor(&data)
                .map_err(|source| AnchorError::CursorDecode { chain_id, source })?
                .next_batch
        }
    };
    let n = below_batch.map_or(n, |limit| n.min(limit));
    if n == 0 {
        // The ordered log starts at block 1 (genesis 0 is never sealed) — a fresh
        // chain's very first block to post is 1, not 0.
        return Ok(Anchor {
            from_block: 1,
            prev_block_timestamp_secs: 0,
        });
    }

    let (root_pda, _) = zk_settlement_client::root_pda(settlement_program_id, chain_id);
    let root = match accounts.get_account(&root_pda).await? {
        None => None,
        Some(data) => Some(
            zk_settlement_client::decode_root_account(&data)
                .map_err(|source| AnchorError::RootDecode { chain_id, source })?,
        ),
    };
    let head_final_batch = root.as_ref().map(|r| r.head_final_batch).unwrap_or(0);

    // Walk every ever-opened batch id, newest first, paged (an unpaged walk is one round trip per id in the worst
    // all-abandoned case). `CloseBatch`/chunk `Close` require `head_final_batch >= batch` before reclaiming rent,
    // so any batch strictly ahead of `head_final_batch` is *guaranteed* to still have an intact account if
    // `Finalized` — the walk simply trusts each individual probe rather than pre-filtering on `head_final_batch`
    // itself. Batch ids are 1-based, so `head_final_batch == 0` unambiguously means "nothing has settled yet" —
    // real batch 0 never exists to be confused with it (the old sentinel ambiguity the derivation-node resume
    // anchor used to have no longer arises). The walk still doesn't special-case it — probing every id in range
    // costs nothing extra a real chain wouldn't otherwise pay.
    let mut hi = n;
    'walk: while hi > 0 {
        let page_len = ANCHOR_WALK_PAGE_SIZE.min(hi as usize) as u64;
        let lo = hi - page_len;
        let ids: Vec<u64> = (lo..hi).collect();
        let pdas: Vec<Pubkey> = ids
            .iter()
            .map(|&b| {
                zk_inbox_client::batch_pda(inbox_program_id, settlement_program_id, chain_id, b).0
            })
            .collect();
        let datas = accounts.get_multiple_account_data(&pdas).await?;
        for (&b, data) in ids.iter().zip(datas).rev() {
            match decode_probe(data, b)? {
                BatchAccountState::Finalized => {
                    match decode_anchor_from_finalized_batch(
                        accounts,
                        inbox_program_id,
                        settlement_program_id,
                        chain_id,
                        b,
                    )
                    .await
                    {
                        Ok(anchor) => return Ok(anchor),
                        // This batch's chunks were legitimately rent-recycled
                        // (Close requires `head_final_batch >= batch`) — fall through to the settlement-
                        // root fallback below exactly as if every batch had been Missing.
                        Err(AnchorError::ChunkMissing { .. }) if head_final_batch >= b => {
                            break 'walk;
                        }
                        Err(e) => return Err(e),
                    }
                }
                BatchAccountState::OpenNotFinalized { .. } => {
                    return Err(AnchorError::UnexpectedOpenBatch {
                        n,
                        batch: b,
                        head_final_batch,
                    });
                }
                BatchAccountState::Missing => continue,
            }
        }
        hi = lo;
    }

    // Every ever-opened batch is missing, or the newest finalized one's chunks were legitimately recycled — anchor on
    // the settlement root itself. The next block to post is one past the last posted design block (`root.number`) —
    // the settlement sentinel (`root_number == 0`, nothing ever settled) also yields `from_block = 1`, the same
    // fresh-chain value step 1 returns, never `0`.
    let root_number = root.as_ref().map(|r| r.number).unwrap_or(0);
    let from_block = root_number + 1;
    let prev_block_timestamp_secs = if root_number == 0 {
        0
    } else {
        replay_prev_timestamp_secs(
            log_dir,
            chain_id,
            sub_blocks_per_block,
            block_gas_limit,
            from_block,
        )?
    };
    Ok(Anchor {
        from_block,
        prev_block_timestamp_secs,
    })
}

/// Both `--once` and `--follow` open the log AT the anchor (`BlockSource::open`
/// with `anchor.from_block`) and must see the anchor's own block as the very first thing the log actually
/// hands back — checked once, right after the log is opened, before any grouping happens
/// (`bin/rome-zk-batcher.rs`). Anything else is the named `Gap` refusal: a gap in the log (pruned,
/// rotated), or the log and chain have genuinely diverged. Pure (no I/O) so it is tested directly.
pub fn verify_first_block_matches(anchor: &Anchor, first_block: u64) -> Result<(), AnchorError> {
    if first_block != anchor.from_block {
        return Err(AnchorError::Gap {
            first_block,
            anchor: anchor.from_block,
        });
    }
    Ok(())
}

/// Decodes `batch`'s own blocks straight from its sealed chunks: paged reads, header parse + validation,
/// `Frame::from_bytes` -> `reassemble` -> `verify_acc` (defense: the on-chain `acc` must match what these
/// exact chunks reduce to) -> `decode_stream` -> last block. Mirrors
/// `rome-zk-derive::inbox::InboxRetrieval::chunks`' own per-chunk checks against this crate's own
/// `AccountOps`, never a dependency on that crate.
async fn decode_anchor_from_finalized_batch<A: AccountOps>(
    accounts: &A,
    inbox_program_id: &Pubkey,
    settlement_program_id: &Pubkey,
    chain_id: u64,
    batch: u64,
) -> Result<Anchor, AnchorError> {
    let (batch_pda, _) =
        zk_inbox_client::batch_pda(inbox_program_id, settlement_program_id, chain_id, batch);
    let data = accounts
        .get_account(&batch_pda)
        .await?
        .ok_or(AnchorError::BatchAccountVanished { batch })?;
    let decoded = zk_inbox_client::decode_batch_account(&data)
        .map_err(|source| AnchorError::BatchDecode { batch, source })?;

    let frames = read_batch_frames(
        accounts,
        inbox_program_id,
        settlement_program_id,
        chain_id,
        batch,
        decoded.expected_count,
    )
    .await?;
    let compressed =
        channel::reassemble(&frames).map_err(|source| AnchorError::Channel { batch, source })?;
    pipeline::verify_acc(&decoded, &frames)
        .map_err(|source| AnchorError::Verify { batch, source })?;
    let blocks = channel::decode_stream(&compressed)
        .map_err(|source| AnchorError::Channel { batch, source })?;
    let last = blocks.last().ok_or(AnchorError::EmptyBlocks { batch })?;
    Ok(Anchor {
        from_block: last.number + 1,
        prev_block_timestamp_secs: last.timestamp,
    })
}

/// Reads every chunk of `batch` (`0..expected_count`) off chain, in order, and parses each into its frame:
/// paged reads, header parse and validation, `Frame::from_bytes`. Every chunk must exist and be sealed.
/// Shared by the finalized-batch anchor decode above and the startup resume of a batch whose every leaf
/// is already present (`recover.rs`).
pub(crate) async fn read_batch_frames<A: AccountOps>(
    accounts: &A,
    inbox_program_id: &Pubkey,
    settlement_program_id: &Pubkey,
    chain_id: u64,
    batch: u64,
    expected_count: u32,
) -> Result<Vec<Frame>, AnchorError> {
    let chunk_pdas: Vec<Pubkey> = (0..expected_count)
        .map(|idx| {
            zk_inbox_client::chunk_pda(
                inbox_program_id,
                settlement_program_id,
                chain_id,
                batch,
                idx,
            )
            .0
        })
        .collect();
    let chunk_datas = accounts.get_multiple_account_data(&chunk_pdas).await?;

    let mut frames = Vec::with_capacity(expected_count as usize);
    for (idx, data) in chunk_datas.into_iter().enumerate() {
        let idx = idx as u32;
        let Some(data) = data else {
            return Err(AnchorError::ChunkMissing { batch, idx });
        };
        let header = zk_inbox_client::decode_chunk_header(&data)
            .map_err(|source| AnchorError::ChunkHeaderDecode { batch, idx, source })?;
        if header.chain_id != chain_id || header.batch != batch || header.idx != idx {
            return Err(AnchorError::ChunkHeaderMismatch {
                batch,
                idx,
                header_chain_id: header.chain_id,
                header_batch: header.batch,
                header_idx: header.idx,
                expected_chain_id: chain_id,
            });
        }
        if !header.sealed {
            return Err(AnchorError::ChunkNotSealed { batch, idx });
        }
        // Slice exactly `header.len` bytes of body, the same overrun-checked slice
        // `rome-zk-derive::inbox` takes (`HEADER_LEN..HEADER_LEN+len`) — not `data[HEADER_LEN..]` to the
        // end of the account, which silently carries any trailing bytes past `len` into the re-derived
        // frame (harmless today only because `pipeline::plan_chunk` always opens a chunk at exactly its
        // sealed length; a chunk opened larger than sealed, which the program itself allows, would
        // otherwise make this decode disagree with what `SealLeaf` actually hashed). `verify_acc` below is
        // the actual on-chain/off-chain equivalence check this decode must feed correctly.
        let start = zk_inbox_client::CHUNK_HEADER_LEN;
        let end = start
            .checked_add(header.len as usize)
            .ok_or(AnchorError::ChunkHeaderDecode {
                batch,
                idx,
                source: zk_inbox_client::DecodeError::TooShort(data.len()),
            })?;
        let body = data.get(start..end).ok_or(AnchorError::ChunkHeaderDecode {
            batch,
            idx,
            source: zk_inbox_client::DecodeError::TooShort(data.len()),
        })?;
        let frame =
            Frame::from_bytes(body).map_err(|source| AnchorError::Channel { batch, source })?;
        // The same rule derive applies (`frame_queue.rs`) — the frame stored at chunk idx i
        // must say it is frame i, refused by name here rather than as a reassemble surprise later.
        if frame.frame_no as u32 != idx {
            return Err(AnchorError::FrameNoMismatch {
                batch,
                idx,
                frame_no: frame.frame_no,
            });
        }
        frames.push(frame);
    }
    Ok(frames)
}

/// When the last inbox batch's chunks are already rent-recycled, the
/// closed-batch timestamp seed is recomputed *exactly* — not guessed at 0 — by replaying the ordered
/// log's own monotonic-timestamp recurrence (`source.rs`'s `resolve_block_timestamp_secs`, applied here
/// for free by simply running a fresh [`BlockSource`] from block 1, the log's own first block)
/// up to `anchor_block - 1`, returning that block's own computed timestamp.
fn replay_prev_timestamp_secs(
    log_dir: &Path,
    chain_id: u64,
    sub_blocks_per_block: u16,
    block_gas_limit: u64,
    anchor_block: u64,
) -> Result<u64, AnchorError> {
    let mut source = BlockSource::open(
        log_dir,
        chain_id,
        block_gas_limit,
        sub_blocks_per_block,
        1,
        0,
    )
    .map_err(|source| AnchorError::LogReplayIo {
        log_dir: log_dir.to_path_buf(),
        anchor_block,
        source,
    })?;
    let mut prev_ts = 0u64;
    for expected in 1..anchor_block {
        match source
            .next_block()
            .map_err(|source| AnchorError::LogReplaySource {
                log_dir: log_dir.to_path_buf(),
                anchor_block,
                source,
            })? {
            Some(sourced) => {
                if sourced.block.number != expected {
                    return Err(AnchorError::LogReplayMisaligned {
                        log_dir: log_dir.to_path_buf(),
                        expected,
                        got: sourced.block.number,
                    });
                }
                prev_ts = sourced.block.timestamp;
            }
            None => {
                return Err(AnchorError::LogReplayShort {
                    log_dir: log_dir.to_path_buf(),
                    anchor_block,
                    got: expected,
                })
            }
        }
    }
    Ok(prev_ts)
}

/// `--from-block` (both modes) survives only as a **verified** value: it must equal
/// the resolved anchor exactly, or this refuses rather than silently trusting an operator's guess.
pub fn verify_from_block_override(anchor: &Anchor, given: u64) -> Result<(), AnchorError> {
    if given != anchor.from_block {
        return Err(AnchorError::FromBlockMismatch {
            given,
            anchor: anchor.from_block,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::Block;
    use alloy::primitives::B256;
    use alloy::signers::local::PrivateKeySigner;
    use rome_zk_sequencer::executor::{MockExecutor, SubBlockLimits};
    use rome_zk_sequencer::header::SubBlockHeader;
    use rome_zk_sequencer::log::LogWriter;
    use rome_zk_sequencer::preconf::ChannelSink;
    use rome_zk_sequencer::sealer::{ResumePoint, SealerState, SUB_BLOCKS_PER_BLOCK};
    use rome_zk_sequencer::signing::sign_header;
    use rome_zk_sequencer::testutil::signed_raw_tx;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    const PROGRAM: Pubkey = Pubkey::new_from_array([9u8; 32]);
    const SETTLEMENT_PROGRAM: Pubkey = Pubkey::new_from_array([7u8; 32]);
    const CHAIN_ID: u64 = 200_198;

    #[derive(Clone, Default)]
    struct FakeChain(Arc<Mutex<HashMap<Pubkey, Vec<u8>>>>);

    impl FakeChain {
        fn set_account(&self, pubkey: Pubkey, data: Vec<u8>) {
            self.0.lock().unwrap().insert(pubkey, data);
        }
    }

    impl AccountOps for FakeChain {
        async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Vec<u8>>, ResolveError> {
            Ok(self.0.lock().unwrap().get(pubkey).cloned())
        }
        async fn accounts_exist(&self, pubkeys: &[Pubkey]) -> Result<Vec<bool>, ResolveError> {
            let st = self.0.lock().unwrap();
            Ok(pubkeys.iter().map(|p| st.contains_key(p)).collect())
        }
    }

    fn cursor_bytes(chain_id: u64, next_batch: u64) -> Vec<u8> {
        let mut d = vec![0u8; rome_zk_layouts::cursor::LEN];
        d[rome_zk_layouts::cursor::OFF_MAGIC..rome_zk_layouts::cursor::OFF_MAGIC + 4]
            .copy_from_slice(&rome_zk_layouts::cursor::MAGIC.to_le_bytes());
        d[rome_zk_layouts::cursor::OFF_VERSION] = rome_zk_layouts::cursor::VERSION;
        d[rome_zk_layouts::cursor::OFF_CHAIN_ID..rome_zk_layouts::cursor::OFF_CHAIN_ID + 8]
            .copy_from_slice(&chain_id.to_le_bytes());
        d[rome_zk_layouts::cursor::OFF_NEXT_BATCH..rome_zk_layouts::cursor::OFF_NEXT_BATCH + 8]
            .copy_from_slice(&next_batch.to_le_bytes());
        d
    }

    fn root_bytes(chain_id: u64, head_final_batch: u64, number: u64) -> Vec<u8> {
        let mut d = vec![0u8; rome_zk_layouts::root::MIN_LEN];
        d[rome_zk_layouts::root::OFF_MAGIC..rome_zk_layouts::root::OFF_MAGIC + 4]
            .copy_from_slice(&rome_zk_layouts::root::MAGIC.to_le_bytes());
        d[rome_zk_layouts::root::OFF_CHAIN_ID..rome_zk_layouts::root::OFF_CHAIN_ID + 8]
            .copy_from_slice(&chain_id.to_le_bytes());
        d[rome_zk_layouts::root::OFF_NUMBER..rome_zk_layouts::root::OFF_NUMBER + 8]
            .copy_from_slice(&number.to_le_bytes());
        d[rome_zk_layouts::root::OFF_HEAD_FINAL_BATCH
            ..rome_zk_layouts::root::OFF_HEAD_FINAL_BATCH + 8]
            .copy_from_slice(&head_final_batch.to_le_bytes());
        d
    }

    /// Builds a real, finalized batch on the fake chain: real chunk PDAs (real headers + real
    /// `Frame::to_bytes()` bodies) and a real batch account whose `acc` matches
    /// `pipeline::verify_acc`'s own reference computation — the exact shape [`decode_anchor_from_finalized_batch`]
    /// must decode.
    fn seed_finalized_batch(chain: &FakeChain, chain_id: u64, batch: u64, blocks: &[Block]) {
        let compressed = channel::encode_stream(blocks);
        let frames = channel::cut_frames(chain_id, batch, &compressed, 3_200);
        seed_finalized_batch_frames(chain, chain_id, batch, &frames);
    }

    /// Same as [`seed_finalized_batch`] but from explicit frames: chunk `idx` i holds `frames[i]`
    /// whatever its own `frame_no` says — so a test can store a mis-numbered frame at an idx.
    fn seed_finalized_batch_frames(
        chain: &FakeChain,
        chain_id: u64,
        batch: u64,
        frames: &[channel::Frame],
    ) {
        let authority = Pubkey::new_unique();
        for (idx, frame) in frames.iter().enumerate() {
            let (pda, _) = zk_inbox_client::chunk_pda(
                &PROGRAM,
                &SETTLEMENT_PROGRAM,
                chain_id,
                batch,
                idx as u32,
            );
            let mut d = vec![0u8; zk_inbox::HEADER_LEN];
            d[zk_inbox::OFF_MAGIC..zk_inbox::OFF_MAGIC + 4]
                .copy_from_slice(&zk_inbox::MAGIC.to_le_bytes());
            d[zk_inbox::OFF_AUTHORITY..zk_inbox::OFF_AUTHORITY + 32]
                .copy_from_slice(authority.as_ref());
            d[zk_inbox::OFF_CHAIN_ID..zk_inbox::OFF_CHAIN_ID + 8]
                .copy_from_slice(&chain_id.to_le_bytes());
            d[zk_inbox::OFF_BATCH..zk_inbox::OFF_BATCH + 8].copy_from_slice(&batch.to_le_bytes());
            d[zk_inbox::OFF_IDX..zk_inbox::OFF_IDX + 4]
                .copy_from_slice(&(idx as u32).to_le_bytes());
            let body = frame.to_bytes();
            d[zk_inbox::OFF_LEN..zk_inbox::OFF_LEN + 4]
                .copy_from_slice(&(body.len() as u32).to_le_bytes());
            d[zk_inbox::OFF_SEALED] = 1;
            d.extend_from_slice(&body);
            chain.set_account(pda, d);
        }
        let chunk_hashes: Vec<[u8; 32]> = frames
            .iter()
            .map(|f| solana_program::keccak::hashv(&[&f.to_bytes()]).to_bytes())
            .collect();
        let (_, _, acc) = zk_inbox_client::reference_commitment(chain_id, batch, 0, &chunk_hashes);
        let mut d = vec![
            0u8;
            rome_zk_layouts::batch::account_len_for(
                rome_zk_layouts::batch::VERSION,
                frames.len() as u32
            )
            .unwrap()
        ];
        d[0..4].copy_from_slice(&rome_zk_layouts::batch::MAGIC.to_le_bytes());
        d[4] = rome_zk_layouts::batch::VERSION;
        d[rome_zk_layouts::batch::OFF_CHAIN_ID..rome_zk_layouts::batch::OFF_CHAIN_ID + 8]
            .copy_from_slice(&chain_id.to_le_bytes());
        d[rome_zk_layouts::batch::OFF_BATCH..rome_zk_layouts::batch::OFF_BATCH + 8]
            .copy_from_slice(&batch.to_le_bytes());
        d[rome_zk_layouts::batch::OFF_EXPECTED_COUNT
            ..rome_zk_layouts::batch::OFF_EXPECTED_COUNT + 4]
            .copy_from_slice(&(frames.len() as u32).to_le_bytes());
        d[rome_zk_layouts::batch::OFF_LEAVES_PRESENT
            ..rome_zk_layouts::batch::OFF_LEAVES_PRESENT + 4]
            .copy_from_slice(&(frames.len() as u32).to_le_bytes());
        d[rome_zk_layouts::batch::OFF_FINALIZED] = 1;
        d[rome_zk_layouts::batch::OFF_ACC..rome_zk_layouts::batch::OFF_ACC + 32]
            .copy_from_slice(&acc);
        let (batch_pda, _) =
            zk_inbox_client::batch_pda(&PROGRAM, &SETTLEMENT_PROGRAM, chain_id, batch);
        chain.set_account(batch_pda, d);
    }

    /// The batcher's anchor reader applies derive's rule (`frame_queue.rs`): the frame stored
    /// at chunk idx i must carry `frame_no == i`, refused by name BEFORE reassemble — never a
    /// `MissingFrame`/duplicate surprise downstream. Without the check, this test sees
    /// `Channel(DuplicateFrame(0))` instead of `FrameNoMismatch`.
    #[tokio::test]
    async fn a_chunk_whose_frame_no_differs_from_its_idx_is_refused_by_name() {
        let chain = FakeChain::default();
        let chain_id = 7u64;
        let batch = 3u64;
        let blocks: Vec<Block> = (1..=3)
            .map(|n| Block {
                number: n,
                timestamp: 1_000 + n,
                gas_limit: 100_000_000,
                // Pseudo-random bodies (xorshift) so zstd cannot collapse them: repeated bytes compress
                // to a single frame and the test would not reach the multi-frame path.
                txs: (0..40)
                    .map(|i| {
                        let mut x = 0x9E37_79B9_7F4A_7C15u64 ^ (n * 1_000_003) ^ i;
                        let bytes: Vec<u8> = (0..200)
                            .map(|_| {
                                x ^= x << 13;
                                x ^= x >> 7;
                                x ^= x << 17;
                                x as u8
                            })
                            .collect();
                        alloy_primitives::Bytes::from(bytes)
                    })
                    .collect(),
                deposits_end: None,
            })
            .collect();
        let compressed = channel::encode_stream(&blocks);
        let mut frames = channel::cut_frames(chain_id, batch, &compressed, 400);
        assert!(frames.len() >= 3, "fixture needs several frames");
        frames[1].frame_no = 0; // stored at idx 1, claims to be frame 0
        seed_finalized_batch_frames(&chain, chain_id, batch, &frames);
        let err = decode_anchor_from_finalized_batch(
            &chain,
            &PROGRAM,
            &SETTLEMENT_PROGRAM,
            chain_id,
            batch,
        )
        .await
        .unwrap_err();
        match err {
            AnchorError::FrameNoMismatch {
                batch: b,
                idx,
                frame_no,
            } => assert_eq!((b, idx, frame_no), (batch, 1, 0)),
            other => panic!("expected FrameNoMismatch, got {other:?}"),
        }
    }

    fn block(number: u64, timestamp: u64) -> Block {
        Block {
            number,
            timestamp,
            gas_limit: 100_000_000,
            txs: vec![],
            deposits_end: None,
        }
    }

    #[tokio::test]
    async fn a_fresh_chain_with_next_batch_zero_anchors_at_block_one() {
        let chain = FakeChain::default();
        let dir = tempdir().unwrap();
        let anchor = resolve_anchor(
            &chain,
            &PROGRAM,
            &SETTLEMENT_PROGRAM,
            CHAIN_ID,
            dir.path(),
            20,
            100_000_000,
        )
        .await
        .unwrap();
        assert_eq!(
            anchor,
            Anchor {
                // The ordered log starts at block 1 — genesis 0 is never sealed.
                from_block: 1,
                prev_block_timestamp_secs: 0
            }
        );
    }

    /// Regression test: the fixture must match the producer — a log sealed through the REAL `SealerState` from
    /// `ResumePoint::default()` (never a hand-written `LogWriter` fixture standing in for it), then the fresh-chain
    /// anchor resolved and the log opened AT it, proving the first block the log yields really is the anchor's own
    /// block. Reverting `resolve_anchor`'s `n == 0` branch to `from_block: 0` makes this test fail with a named
    /// `Gap` error (`verify_first_block_matches` sees the log's real first block, 1, against the wrong anchor, 0).
    #[tokio::test]
    async fn the_fresh_chain_anchor_matches_a_log_sealed_by_the_real_sealer() {
        let dir = tempdir().unwrap();
        let sequencer_key = PrivateKeySigner::random();
        let mut live = SealerState::new(
            MockExecutor::new(),
            LogWriter::open(dir.path(), 10_000).unwrap(),
            sequencer_key,
            ChannelSink::new(16),
            CHAIN_ID,
            100_000_000,
            alloy_primitives::Address::ZERO,
            SUB_BLOCKS_PER_BLOCK,
            ResumePoint::default(),
        )
        // This fixture is about anchor-resolution mechanics, not idle behaviour — a
        // nonzero interval (the minimum legal one at this profile's 1s block time) keeps every
        // sub-block sealing exactly as it did before this knob existed.
        .with_empty_block_interval_secs(1);
        // Seal 3 whole blocks — nothing else touches this log.
        for i in 0..(3 * SUB_BLOCKS_PER_BLOCK as u64) {
            live.seal_sub_block(
                vec![],
                1_757_000_000_000_000 + i * 50_000,
                SubBlockLimits::unbounded(),
            )
            .await
            .unwrap();
        }
        drop(live);

        // Bind this fixture to the PRODUCER's own numbering origin, not just what
        // the log yields at the anchor — a 0-based sealer makes this test fail.
        let mut records = Vec::new();
        rome_zk_sequencer::log::replay(dir.path(), false, |r| records.push(r.clone())).unwrap();
        let first = records
            .first()
            .expect("the real sealer must have written at least one record");
        assert_eq!(
            (first.header.block, first.header.index),
            (1, 0),
            "the real sealer's log must start at (1, 0)"
        );

        // A genuinely fresh chain: no `batch_cursor` account at all (`n == 0`).
        let chain = FakeChain::default();
        let anchor = resolve_anchor(
            &chain,
            &PROGRAM,
            &SETTLEMENT_PROGRAM,
            CHAIN_ID,
            dir.path(),
            SUB_BLOCKS_PER_BLOCK,
            100_000_000,
        )
        .await
        .unwrap();
        assert_eq!(anchor.from_block, 1);

        let mut source = BlockSource::open(
            dir.path(),
            CHAIN_ID,
            100_000_000,
            SUB_BLOCKS_PER_BLOCK,
            anchor.from_block,
            anchor.prev_block_timestamp_secs,
        )
        .unwrap();
        let first = source
            .next_block()
            .unwrap()
            .expect("the real sealer's log must hold a first block at the anchor");
        assert_eq!(
            first.block.number, 1,
            "the real sealer's own first sealed block is 1, matching the anchor exactly"
        );
        verify_first_block_matches(&anchor, first.block.number)
            .expect("the log's real first block must match the fresh-chain anchor");
    }

    /// Root-fallback half of the regression test: the same real, sealer-written log, but resolved through the
    /// root-fallback path (every ever-opened batch missing) with `root.number = 2` — the anchor must be `3` (one
    /// past the last posted design block) and the replayed timestamp seed must be exactly the real sealer's own
    /// block-2 timestamp, not a value approximated by a hand-written fixture. Reverting the fallback's
    /// `from_block = root_number + 1` to `from_block = root_number` makes this test fail (the log's real
    /// first-yielded block, 1, would mismatch an anchor of 2 with a named `Gap`).
    #[tokio::test]
    async fn the_root_fallback_anchor_matches_a_log_sealed_by_the_real_sealer() {
        let dir = tempdir().unwrap();
        let sequencer_key = PrivateKeySigner::random();
        let mut live = SealerState::new(
            MockExecutor::new(),
            LogWriter::open(dir.path(), 10_000).unwrap(),
            sequencer_key,
            ChannelSink::new(16),
            CHAIN_ID,
            100_000_000,
            alloy_primitives::Address::ZERO,
            SUB_BLOCKS_PER_BLOCK,
            ResumePoint::default(),
        )
        // This fixture is about anchor-resolution mechanics, not idle behaviour — a
        // nonzero interval (the minimum legal one at this profile's 1s block time) keeps every
        // sub-block sealing exactly as it did before this knob existed.
        .with_empty_block_interval_secs(1);
        for i in 0..(3 * SUB_BLOCKS_PER_BLOCK as u64) {
            live.seal_sub_block(
                vec![],
                1_757_000_000_000_000 + i * 50_000,
                SubBlockLimits::unbounded(),
            )
            .await
            .unwrap();
        }
        drop(live);

        // Bind this fixture to the PRODUCER's own numbering origin, not just what
        // the log yields at the anchor — a 0-based sealer makes this test fail.
        let mut records = Vec::new();
        rome_zk_sequencer::log::replay(dir.path(), false, |r| records.push(r.clone())).unwrap();
        let first = records
            .first()
            .expect("the real sealer must have written at least one record");
        assert_eq!(
            (first.header.block, first.header.index),
            (1, 0),
            "the real sealer's log must start at (1, 0)"
        );

        // Every ever-opened batch missing (never opened) -> root fallback. root.number = 2: blocks 1
        // and 2 were already posted; the next block to post is 3.
        let chain = FakeChain::default();
        let (cursor_pda, _) = zk_inbox_client::cursor_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(cursor_pda, cursor_bytes(CHAIN_ID, 1));
        let (root_pda, _) = zk_settlement_client::root_pda(&SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(root_pda, root_bytes(CHAIN_ID, 0, 2));

        let anchor = resolve_anchor(
            &chain,
            &PROGRAM,
            &SETTLEMENT_PROGRAM,
            CHAIN_ID,
            dir.path(),
            SUB_BLOCKS_PER_BLOCK,
            100_000_000,
        )
        .await
        .unwrap();
        assert_eq!(anchor.from_block, 3);

        let mut source = BlockSource::open(
            dir.path(),
            CHAIN_ID,
            100_000_000,
            SUB_BLOCKS_PER_BLOCK,
            anchor.from_block,
            anchor.prev_block_timestamp_secs,
        )
        .unwrap();
        let first = source
            .next_block()
            .unwrap()
            .expect("the real sealer's log must hold block 3 at the root-fallback anchor");
        verify_first_block_matches(&anchor, first.block.number)
            .expect("the log's real block 3 must match the root-fallback anchor");
    }

    /// The primary path: `next_batch - 1` is finalized with real chunks still intact — decode its blocks.
    #[tokio::test]
    async fn a_finalized_batch_in_the_pending_window_is_decoded_for_its_last_block() {
        let chain = FakeChain::default();
        let (cursor_pda, _) = zk_inbox_client::cursor_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(cursor_pda, cursor_bytes(CHAIN_ID, 1));
        let (root_pda, _) = zk_settlement_client::root_pda(&SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(root_pda, root_bytes(CHAIN_ID, 0, 0));
        let blocks = vec![block(1, 1_757_000_000), block(2, 1_757_000_001)];
        seed_finalized_batch(&chain, CHAIN_ID, 0, &blocks);

        let dir = tempdir().unwrap();
        let anchor = resolve_anchor(
            &chain,
            &PROGRAM,
            &SETTLEMENT_PROGRAM,
            CHAIN_ID,
            dir.path(),
            20,
            100_000_000,
        )
        .await
        .unwrap();
        assert_eq!(
            anchor,
            Anchor {
                from_block: 3,
                prev_block_timestamp_secs: 1_757_000_001
            }
        );
    }

    /// Batch ids are 1-based; 0 is the sentinel everywhere. No behaviour change
    /// here — the walk already trusts each individual probe rather than special-casing `head_final_batch
    /// == 0` — but this pins the shape a 1-based inbox actually produces at the settlement genesis
    /// sentinel: cursor `next_batch` 3, batches 1 and 2 finalized, batch 0 never opened (`Missing`). The
    /// walk finds batch 2 (newest) first and decodes it directly — it must never need to reach the
    /// absent batch 0 at all.
    #[tokio::test]
    async fn sentinel_anchor_with_a_1_based_inbox_decodes_the_newest_finalized_batch() {
        let chain = FakeChain::default();
        let (cursor_pda, _) = zk_inbox_client::cursor_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(cursor_pda, cursor_bytes(CHAIN_ID, 3));
        let (root_pda, _) = zk_settlement_client::root_pda(&SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(root_pda, root_bytes(CHAIN_ID, 0, 0));
        let blocks_1 = vec![block(1, 1_757_000_000), block(2, 1_757_000_001)];
        seed_finalized_batch(&chain, CHAIN_ID, 1, &blocks_1);
        let blocks_2 = vec![block(3, 1_757_000_002), block(4, 1_757_000_003)];
        seed_finalized_batch(&chain, CHAIN_ID, 2, &blocks_2);
        // Batch 0 is never opened -> Missing on the fake chain (nothing set at its PDA).

        let dir = tempdir().unwrap();
        let anchor = resolve_anchor(
            &chain,
            &PROGRAM,
            &SETTLEMENT_PROGRAM,
            CHAIN_ID,
            dir.path(),
            20,
            100_000_000,
        )
        .await
        .unwrap();
        assert_eq!(
            anchor,
            Anchor {
                from_block: 5,
                prev_block_timestamp_secs: 1_757_000_003
            },
            "must anchor after batch 2 (the newest finalized batch), never touching the Missing batch 0"
        );
    }

    /// Regression test: the batcher's numbering-origin check must refuse a real 0-based log even on the
    /// finalized-batch decode path, where a `profile.json` field is not involved at all — the log
    /// directory's own first record is the only thing this check ever looks at. Removing
    /// `log_numbering_origin` from the top of `resolve_anchor` makes this test fail.
    #[tokio::test]
    async fn batcher_anchor_over_a_real_0_based_log_via_the_finalized_batch_path_is_refused() {
        let chain = FakeChain::default();
        let (cursor_pda, _) = zk_inbox_client::cursor_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(cursor_pda, cursor_bytes(CHAIN_ID, 1));
        let (root_pda, _) = zk_settlement_client::root_pda(&SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(root_pda, root_bytes(CHAIN_ID, 0, 0));
        let blocks = vec![block(1, 1_757_000_000), block(2, 1_757_000_001)];
        seed_finalized_batch(&chain, CHAIN_ID, 0, &blocks);

        // A real 0-based log: the older sealer's own fresh-start point, sealed by the actual
        // SealerState — never hand-written.
        let dir = tempdir().unwrap();
        let sequencer_key = PrivateKeySigner::random();
        let mut live = SealerState::new(
            MockExecutor::new(),
            LogWriter::open(dir.path(), 10_000).unwrap(),
            sequencer_key,
            ChannelSink::new(16),
            CHAIN_ID,
            100_000_000,
            alloy_primitives::Address::ZERO,
            SUB_BLOCKS_PER_BLOCK,
            ResumePoint {
                next_block: 0,
                ..ResumePoint::default()
            },
        )
        .with_empty_block_interval_secs(1);
        for i in 0..(2 * SUB_BLOCKS_PER_BLOCK as u64) {
            live.seal_sub_block(
                vec![],
                1_757_000_000_000_000 + i * 50_000,
                SubBlockLimits::unbounded(),
            )
            .await
            .unwrap();
        }
        drop(live);

        let err = resolve_anchor(
            &chain,
            &PROGRAM,
            &SETTLEMENT_PROGRAM,
            CHAIN_ID,
            dir.path(),
            SUB_BLOCKS_PER_BLOCK,
            100_000_000,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                err,
                AnchorError::LogNumbering {
                    first_block: 0,
                    first_index: 0,
                    ..
                }
            ),
            "{err:?}"
        );
    }

    /// If the on-chain `acc` does not match what these exact chunks reduce to,
    /// the walk must refuse rather than silently trust the decoded blocks.
    #[tokio::test]
    async fn a_tampered_finalized_batch_fails_acc_verification() {
        let chain = FakeChain::default();
        let (cursor_pda, _) = zk_inbox_client::cursor_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(cursor_pda, cursor_bytes(CHAIN_ID, 1));
        let (root_pda, _) = zk_settlement_client::root_pda(&SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(root_pda, root_bytes(CHAIN_ID, 0, 0));
        let blocks = vec![block(1, 1_757_000_000)];
        seed_finalized_batch(&chain, CHAIN_ID, 0, &blocks);
        // Tamper with the on-chain acc after seeding a genuinely matching batch.
        let (batch_pda, _) = zk_inbox_client::batch_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID, 0);
        let mut data = {
            let map = chain.0.lock().unwrap();
            map.get(&batch_pda).unwrap().clone()
        };
        data[rome_zk_layouts::batch::OFF_ACC] ^= 0xff;
        chain.set_account(batch_pda, data);

        let dir = tempdir().unwrap();
        let err = resolve_anchor(
            &chain,
            &PROGRAM,
            &SETTLEMENT_PROGRAM,
            CHAIN_ID,
            dir.path(),
            20,
            100_000_000,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, AnchorError::Verify { batch: 0, .. }),
            "{err:?}"
        );
    }

    /// A batch in the pending window that is missing (abandoned) is walked past to find the next
    /// (older) finalized one.
    #[tokio::test]
    async fn an_abandoned_batch_in_the_pending_window_is_walked_past() {
        let chain = FakeChain::default();
        let (cursor_pda, _) = zk_inbox_client::cursor_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(cursor_pda, cursor_bytes(CHAIN_ID, 2));
        let (root_pda, _) = zk_settlement_client::root_pda(&SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(root_pda, root_bytes(CHAIN_ID, 0, 0));
        // batch 0 finalized with blocks 1..2; batch 1 abandoned (missing) entirely.
        let blocks = vec![block(1, 1_757_000_000), block(2, 1_757_000_001)];
        seed_finalized_batch(&chain, CHAIN_ID, 0, &blocks);

        let dir = tempdir().unwrap();
        let anchor = resolve_anchor(
            &chain,
            &PROGRAM,
            &SETTLEMENT_PROGRAM,
            CHAIN_ID,
            dir.path(),
            20,
            100_000_000,
        )
        .await
        .unwrap();
        assert_eq!(anchor.from_block, 3);
    }

    /// **Caller-ordering guard:** an `OpenNotFinalized` batch found in the walk is a hard, named error —
    /// `pipeline::startup_recover` must have already run.
    #[tokio::test]
    async fn an_open_not_finalized_batch_in_the_walk_is_a_named_caller_ordering_error() {
        let chain = FakeChain::default();
        let (cursor_pda, _) = zk_inbox_client::cursor_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(cursor_pda, cursor_bytes(CHAIN_ID, 1));
        let (root_pda, _) = zk_settlement_client::root_pda(&SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(root_pda, root_bytes(CHAIN_ID, 0, 0));
        let (batch_pda, _) = zk_inbox_client::batch_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID, 0);
        let mut d = vec![
            0u8;
            rome_zk_layouts::batch::account_len_for(rome_zk_layouts::batch::VERSION, 0)
                .unwrap()
        ];
        d[0..4].copy_from_slice(&rome_zk_layouts::batch::MAGIC.to_le_bytes());
        d[4] = rome_zk_layouts::batch::VERSION;
        chain.set_account(batch_pda, d); // finalized = 0

        let dir = tempdir().unwrap();
        let err = resolve_anchor(
            &chain,
            &PROGRAM,
            &SETTLEMENT_PROGRAM,
            CHAIN_ID,
            dir.path(),
            20,
            100_000_000,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, AnchorError::UnexpectedOpenBatch { batch: 0, .. }),
            "{err:?}"
        );
    }

    /// The closed-batch fallback: every batch in the pending window is missing, so
    /// the anchor comes from the settlement root's own `number`, and the timestamp seed is recomputed by
    /// replaying the ordered log — proven here with a same-second seal boundary (the monotonic floor
    /// actually binds), matching the sequencer's own committed timestamp.
    #[tokio::test]
    async fn the_closed_batch_fallback_replays_the_log_for_an_exact_timestamp_seed() {
        let chain = FakeChain::default();
        let (cursor_pda, _) = zk_inbox_client::cursor_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(cursor_pda, cursor_bytes(CHAIN_ID, 1));
        // head_final_batch = 0, so the pending window (b in 1..=0) is empty — straight to the root
        // fallback. root.number = 2 (the last block actually posted; from_block =
        // root.number + 1 = 3).
        let (root_pda, _) = zk_settlement_client::root_pda(&SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(root_pda, root_bytes(CHAIN_ID, 0, 2));

        let dir = tempdir().unwrap();
        let sender = PrivateKeySigner::random();
        let mut writer = LogWriter::open(dir.path(), 10_000).unwrap();
        let sub_blocks_per_block = 2u16;
        let base = 1_757_000_000_000_000u64;
        // The log's first-ever block is 1, not 0.
        // Block 1: two sub-blocks starting at base.
        let mut prev_hash = B256::ZERO;
        for index in 0..sub_blocks_per_block {
            let header = SubBlockHeader {
                chain_id: CHAIN_ID,
                block: 1,
                index,
                timestamp_us: base + index as u64 * 50_000,
                tx_root: B256::ZERO,
                receipts_root: B256::ZERO,
                gas_used: 21_000,
                prev_hash,
                deposits_end: None,
            };
            let signature = sign_header(&PrivateKeySigner::random(), &header);
            let tx = signed_raw_tx(&sender, CHAIN_ID, index as u64);
            writer.append(&header, &signature, &[tx]).unwrap();
            prev_hash = header.hash();
        }
        // Block 2: same wall-clock second as block 1 — the monotonic floor must bind (timestamp 1 higher).
        for index in 0..sub_blocks_per_block {
            let header = SubBlockHeader {
                chain_id: CHAIN_ID,
                block: 2,
                index,
                timestamp_us: base + 100_000 + index as u64 * 50_000,
                tx_root: B256::ZERO,
                receipts_root: B256::ZERO,
                gas_used: 21_000,
                prev_hash,
                deposits_end: None,
            };
            let signature = sign_header(&PrivateKeySigner::random(), &header);
            let tx = signed_raw_tx(&sender, CHAIN_ID, 100 + index as u64);
            writer.append(&header, &signature, &[tx]).unwrap();
            prev_hash = header.hash();
        }

        let anchor = resolve_anchor(
            &chain,
            &PROGRAM,
            &SETTLEMENT_PROGRAM,
            CHAIN_ID,
            dir.path(),
            sub_blocks_per_block,
            100_000_000,
        )
        .await
        .unwrap();
        assert_eq!(anchor.from_block, 3);
        // Block 1 -> timestamp 1_757_000_000; block 2 (same second) -> forced to 1_757_000_001 by the
        // monotonic floor — exactly what the sequencer's own sealer would have committed.
        assert_eq!(anchor.prev_block_timestamp_secs, 1_757_000_001);
    }

    #[test]
    fn verify_from_block_override_accepts_the_matching_value() {
        let anchor = Anchor {
            from_block: 42,
            prev_block_timestamp_secs: 1,
        };
        assert!(verify_from_block_override(&anchor, 42).is_ok());
    }

    /// The log's first block read matches the anchor exactly — the normal case.
    #[test]
    fn verify_first_block_matches_accepts_the_anchor_itself() {
        let anchor = Anchor {
            from_block: 25,
            prev_block_timestamp_secs: 4,
        };
        assert!(verify_first_block_matches(&anchor, 25).is_ok());
    }

    /// Opening the log directly at the anchor and
    /// checking the very first block read (rather than regrouping from block 0 and dropping already-
    /// covered groups) means there is no "grown log after a partial tail" misalignment left to reproduce —
    /// the log is simply expected to hand back the anchor's own block first.
    #[test]
    fn verify_first_block_matches_refuses_a_gap_against_the_anchor() {
        let anchor = Anchor {
            from_block: 25,
            prev_block_timestamp_secs: 0,
        };
        let err = verify_first_block_matches(&anchor, 30).unwrap_err();
        assert!(
            matches!(
                err,
                AnchorError::Gap {
                    first_block: 30,
                    anchor: 25
                }
            ),
            "{err:?}"
        );
    }

    /// The newest finalized batch's chunks were legitimately rent-recycled
    /// (`head_final_batch >= batch`) — the walk must fall back to the settlement root + log replay exactly
    /// as it would if every batch account were `Missing`, not refuse with `ChunkMissing`.
    #[tokio::test]
    async fn a_finalized_batch_with_recycled_chunks_at_or_below_the_head_falls_back_to_the_root() {
        let chain = FakeChain::default();
        let (cursor_pda, _) = zk_inbox_client::cursor_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(cursor_pda, cursor_bytes(CHAIN_ID, 1));
        // batch 0 is Finalized (real header, finalized=1) but its chunks were never seeded — `Missing`
        // from the walk's point of view — and head_final_batch (1) >= batch (0): the chunks were
        // legitimately reclaimed.
        let mut d = vec![
            0u8;
            rome_zk_layouts::batch::account_len_for(rome_zk_layouts::batch::VERSION, 2)
                .unwrap()
        ];
        d[0..4].copy_from_slice(&rome_zk_layouts::batch::MAGIC.to_le_bytes());
        d[4] = rome_zk_layouts::batch::VERSION;
        d[rome_zk_layouts::batch::OFF_EXPECTED_COUNT
            ..rome_zk_layouts::batch::OFF_EXPECTED_COUNT + 4]
            .copy_from_slice(&2u32.to_le_bytes());
        d[rome_zk_layouts::batch::OFF_FINALIZED] = 1;
        let (batch_pda, _) = zk_inbox_client::batch_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID, 0);
        chain.set_account(batch_pda, d);
        // root.number = 0 (the sentinel) so the fallback's own timestamp seed needs no log replay at all
        // — this test is about the fallback firing, not the replay path (covered separately).
        let (root_pda, _) = zk_settlement_client::root_pda(&SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(root_pda, root_bytes(CHAIN_ID, 1, 0));

        let dir = tempdir().unwrap();
        let anchor = resolve_anchor(
            &chain,
            &PROGRAM,
            &SETTLEMENT_PROGRAM,
            CHAIN_ID,
            dir.path(),
            20,
            100_000_000,
        )
        .await
        .expect("recycled chunks at or below the head must fall back, not refuse");
        assert_eq!(
            anchor.from_block, 1,
            "the root fallback's own from_block (root.number + 1 = 1 at the sentinel here)"
        );
    }

    /// The opposite direction: a batch whose chunks are missing while `head_final_batch < batch` could
    /// never have had its chunks legitimately closed (`Close` requires `head_final_batch >= batch`) — a
    /// hard `ChunkMissing` refusal, not a silent fallback. Batch 1 (newest) is finalized with no chunks
    /// seeded, and the root's `head_final_batch` (0) is strictly below it.
    #[tokio::test]
    async fn a_finalized_batch_with_missing_chunks_above_the_head_is_a_hard_refusal() {
        let chain = FakeChain::default();
        let (cursor_pda, _) = zk_inbox_client::cursor_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(cursor_pda, cursor_bytes(CHAIN_ID, 2));
        let mut d1 =
            vec![
                0u8;
                rome_zk_layouts::batch::account_len_for(rome_zk_layouts::batch::VERSION, 2)
                    .unwrap()
            ];
        d1[0..4].copy_from_slice(&rome_zk_layouts::batch::MAGIC.to_le_bytes());
        d1[4] = rome_zk_layouts::batch::VERSION;
        d1[rome_zk_layouts::batch::OFF_EXPECTED_COUNT
            ..rome_zk_layouts::batch::OFF_EXPECTED_COUNT + 4]
            .copy_from_slice(&2u32.to_le_bytes());
        d1[rome_zk_layouts::batch::OFF_FINALIZED] = 1;
        let (batch1_pda, _) =
            zk_inbox_client::batch_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID, 1);
        chain.set_account(batch1_pda, d1);
        let (root_pda, _) = zk_settlement_client::root_pda(&SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(root_pda, root_bytes(CHAIN_ID, 0, 0)); // head_final_batch = 0 < batch 1

        let dir = tempdir().unwrap();
        let err = resolve_anchor(
            &chain,
            &PROGRAM,
            &SETTLEMENT_PROGRAM,
            CHAIN_ID,
            dir.path(),
            20,
            100_000_000,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, AnchorError::ChunkMissing { batch: 1, idx: 0 }),
            "{err:?}"
        );
    }

    /// The log-replay fallback's own premise check — a hard, named refusal (never a
    /// release-build-silent `debug_assert`) when the log's own numbering disagrees with what the replay
    /// expects at that step.
    #[test]
    fn log_replay_misaligned_is_a_named_error_not_a_debug_assert() {
        // Constructing this directly (rather than writing a log that skips a block) proves the variant
        // itself carries the right fields; the end-to-end trigger is exercised by
        // `the_closed_batch_fallback_replays_the_log_for_an_exact_timestamp_seed`'s sibling coverage of
        // `replay_prev_timestamp_secs`'s call site inside `resolve_anchor`.
        let err = AnchorError::LogReplayMisaligned {
            log_dir: PathBuf::from("/tmp/does-not-matter"),
            expected: 5,
            got: 7,
        };
        assert!(matches!(
            err,
            AnchorError::LogReplayMisaligned {
                expected: 5,
                got: 7,
                ..
            }
        ));
    }

    /// A log that does not start at block 1 (or has a hole before the anchor)
    /// must refuse by name, not silently return a wrong timestamp (a release build has `debug_assert!`
    /// compiled out). This is now caught at the very top of `resolve_anchor` by
    /// `log_numbering_origin` (`AnchorError::LogNumbering`), before the closed-batch fallback's own
    /// timestamp replay ever runs — a strictly earlier, more specific refusal for the exact case this
    /// test's own log shape describes (its first record is block 2, not block 1).
    #[tokio::test]
    async fn the_closed_batch_fallback_refuses_when_the_log_does_not_start_at_block_one() {
        let chain = FakeChain::default();
        let (cursor_pda, _) = zk_inbox_client::cursor_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(cursor_pda, cursor_bytes(CHAIN_ID, 1));
        let (root_pda, _) = zk_settlement_client::root_pda(&SETTLEMENT_PROGRAM, CHAIN_ID);
        // root.number = 2: from_block = 3; the replay must walk blocks 1 and 2 before returning block
        // 2's timestamp.
        chain.set_account(root_pda, root_bytes(CHAIN_ID, 0, 2));

        let dir = tempdir().unwrap();
        let sender = PrivateKeySigner::random();
        let mut writer = LogWriter::open(dir.path(), 10_000).unwrap();
        // The log's first-ever block is 2, not 1 — a hole where block 1 should be, violating the "log
        // starts at block 1" premise.
        let header = SubBlockHeader {
            chain_id: CHAIN_ID,
            block: 2,
            index: 0,
            timestamp_us: 1_757_000_000_000_000,
            tx_root: B256::ZERO,
            receipts_root: B256::ZERO,
            gas_used: 21_000,
            prev_hash: B256::ZERO,
            deposits_end: None,
        };
        let signature = sign_header(&PrivateKeySigner::random(), &header);
        let tx = signed_raw_tx(&sender, CHAIN_ID, 0);
        writer.append(&header, &signature, &[tx]).unwrap();

        let err = resolve_anchor(
            &chain,
            &PROGRAM,
            &SETTLEMENT_PROGRAM,
            CHAIN_ID,
            dir.path(),
            1,
            100_000_000,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                err,
                AnchorError::LogNumbering {
                    first_block: 2,
                    first_index: 0,
                    ..
                }
            ),
            "{err:?}"
        );
    }

    /// The walk pages ids 100 at a time (`ANCHOR_WALK_PAGE_SIZE`) — proves a walk spanning more than one
    /// page still finds the right finalized batch and decodes it, not just the single-page cases every
    /// other test above exercises.
    #[tokio::test]
    async fn the_walk_spans_more_than_one_page_and_still_finds_the_finalized_batch() {
        let chain = FakeChain::default();
        let (cursor_pda, _) = zk_inbox_client::cursor_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID);
        // 150 ever-opened ids: batch 0 finalized with real chunks, 1..149 never opened (Missing) — the
        // walk must cross the 100-id page boundary to find it.
        chain.set_account(cursor_pda, cursor_bytes(CHAIN_ID, 150));
        let (root_pda, _) = zk_settlement_client::root_pda(&SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(root_pda, root_bytes(CHAIN_ID, 0, 0));
        let blocks = vec![block(1, 1_757_000_000), block(2, 1_757_000_001)];
        seed_finalized_batch(&chain, CHAIN_ID, 0, &blocks);

        let dir = tempdir().unwrap();
        let anchor = resolve_anchor(
            &chain,
            &PROGRAM,
            &SETTLEMENT_PROGRAM,
            CHAIN_ID,
            dir.path(),
            20,
            100_000_000,
        )
        .await
        .expect("a walk spanning two pages must still find batch 0's finalized chunks");
        assert_eq!(anchor.from_block, 3);
    }

    /// A chunk opened larger than sealed (the program allows
    /// `Open size > Seal len`) must still anchor correctly — the body slice must stop at `header.len`, not
    /// run to the end of the account and carry trailing bytes into the re-derived frame.
    #[tokio::test]
    async fn a_chunk_with_trailing_bytes_past_its_sealed_len_still_anchors() {
        let chain = FakeChain::default();
        let (cursor_pda, _) = zk_inbox_client::cursor_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(cursor_pda, cursor_bytes(CHAIN_ID, 1));
        let (root_pda, _) = zk_settlement_client::root_pda(&SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(root_pda, root_bytes(CHAIN_ID, 0, 0));
        let blocks = vec![block(1, 1_757_000_000)];
        seed_finalized_batch(&chain, CHAIN_ID, 0, &blocks);

        // Append 8 trailing zero bytes past the one chunk's sealed `len` — exactly as an `Open` sized
        // larger than the eventual `Seal` would leave behind.
        let (chunk_pda, _) =
            zk_inbox_client::chunk_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID, 0, 0);
        let mut data = {
            let map = chain.0.lock().unwrap();
            map.get(&chunk_pda).unwrap().clone()
        };
        data.extend_from_slice(&[0u8; 8]);
        chain.set_account(chunk_pda, data);

        let dir = tempdir().unwrap();
        let anchor = resolve_anchor(
            &chain,
            &PROGRAM,
            &SETTLEMENT_PROGRAM,
            CHAIN_ID,
            dir.path(),
            20,
            100_000_000,
        )
        .await
        .expect("8 trailing bytes past len must not break the anchor decode");
        assert_eq!(anchor.from_block, 2);
    }

    #[test]
    fn verify_from_block_override_refuses_a_mismatched_value() {
        let anchor = Anchor {
            from_block: 42,
            prev_block_timestamp_secs: 1,
        };
        let err = verify_from_block_override(&anchor, 41).unwrap_err();
        assert!(matches!(
            err,
            AnchorError::FromBlockMismatch {
                given: 41,
                anchor: 42
            }
        ));
    }

    /// No migration: a v1-shaped batch account (version
    /// byte 1, the old 202-byte length, no `open_unix_ts`) — exactly what a pre-reset Tiber batch looks
    /// like under a retired program id — must be refused by name, never silently trusted as if it had
    /// decoded. `decode_probe` never falls back to the settlement-root anchor for a batch id that
    /// *exists* but fails to decode (that fallback is only for a genuinely `Missing` account); it
    /// propagates `AnchorError::BatchDecode` with the underlying `DecodeError::BadVersion`, straight out
    /// of `resolve_anchor`.
    #[tokio::test]
    async fn a_v1_shaped_batch_account_is_refused_as_bad_version() {
        let chain = FakeChain::default();
        let (cursor_pda, _) = zk_inbox_client::cursor_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(cursor_pda, cursor_bytes(CHAIN_ID, 1));
        let (batch_pda, _) = zk_inbox_client::batch_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID, 0);
        let mut d = vec![0u8; 202];
        d[rome_zk_layouts::batch::OFF_MAGIC..rome_zk_layouts::batch::OFF_MAGIC + 4]
            .copy_from_slice(&rome_zk_layouts::batch::MAGIC.to_le_bytes());
        d[rome_zk_layouts::batch::OFF_VERSION] = 1;
        d[rome_zk_layouts::batch::OFF_CHAIN_ID..rome_zk_layouts::batch::OFF_CHAIN_ID + 8]
            .copy_from_slice(&CHAIN_ID.to_le_bytes());
        chain.set_account(batch_pda, d);

        let dir = tempdir().unwrap();
        let err = resolve_anchor(
            &chain,
            &PROGRAM,
            &SETTLEMENT_PROGRAM,
            CHAIN_ID,
            dir.path(),
            20,
            100_000_000,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                err,
                AnchorError::BatchDecode {
                    batch: 0,
                    source: zk_inbox_client::DecodeError::BadVersion,
                }
            ),
            "expected AnchorError::BatchDecode{{batch: 0, source: BadVersion}}, got: {err:?}"
        );
    }
}
