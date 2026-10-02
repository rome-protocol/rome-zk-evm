//! Groups the blocks [`crate::source::BlockSource`] hands back into batches of **at most
//! `blocks_per_batch` consecutive blocks** — the exact shape
//! `rome-zk-derive`'s `batch_queue::decode_batch` checks on the read side (a batch holds at most
//! `blocks_per_batch` blocks; a batch's first block continues immediately from the previous batch's
//! last block). [`BlockGrouper`] is the one place this crate enforces both invariants, shared by
//! **both** the `--once` cap (batch every complete block the log holds, but in groups
//! of at most `blocks_per_batch`, not one giant batch) and continuous mode (accumulate
//! blocks off the tailed log until a group is full, then post) — so there is exactly one continuity
//! check to mutate/verify, not two copies that could drift apart.
//!
//! A batch account carries no block-range field — nothing downstream of this module can
//! recover "which blocks batch N held" except by decoding it (`rome-zk-derive`'s job on the read side).
//! This module's job is to make sure the batcher never *produces* a batch that would fail that decode:
//! every batch it hands to [`crate::pipeline`] is ≤ `blocks_per_batch` blocks, and batch N+1's first
//! block is batch N's last block + 1 — checked incrementally, on every block pushed, not just at the
//! end, so a caller can push and post a full group as soon as it fills without buffering the whole log.

use crate::channel::{self, Block, ShadowCompressor};
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GroupingError {
    /// A block number this crate read from the log does not continue immediately from the last one
    /// it saw — either a genuine gap (a missing block) or a reorder. The continuity
    /// invariant ("first block of batch N+1 = last block of N + 1", and *within* one batch blocks are
    /// strictly consecutive) is enforced on every push, so this is raised as soon as the break is seen,
    /// before anything is posted.
    #[error(
        "block {got}: expected block {expected} (blocks read off the log must be strictly consecutive — \
         a gap or reorder was found; nothing from this group has been posted)"
    )]
    NonContiguousBlock { expected: u64, got: u64 },
    /// A single block's own compressed encoding already
    /// needs more than `max_frames` frames of `max_frame_body_len` bytes each — the channel cannot close
    /// early to make room (there is nothing accumulated yet to close), so this is a hard, named
    /// misconfiguration rather than a batch of zero blocks.
    #[error(
        "block {number}: this one block alone needs more than max_frames_per_batch frames \
         (max_frame_body_len={max_frame_body_len}) — no group can ever hold it; raise max_frames_per_batch \
         or max_frame_body_len"
    )]
    SingleBlockExceedsFrameBudget {
        number: u64,
        max_frame_body_len: usize,
    },
}

/// Accumulates [`Block`]s into groups of at most `cap` consecutive blocks each (`cap` is the chain's
/// `blocks_per_batch`, read from the `profile.json` the sequencer writes only after its own profile validation
/// has refused 0 — so `cap == 0` is a caller bug, not a runtime state this type needs to defend against).
///
/// **Continuity across groups:** [`Self::take_group`] does
/// *not* forget the last block it handed back — it remembers it as `last_taken`, so the very next
/// [`Self::push`] (the first block of the *next* group) is continuity-checked against it exactly the
/// same way a push within one group is. A caller that wants a fresh group's first push to be
/// unconstrained (there is no such caller in this crate any more — see [`crate::anchor`]) would need a
/// brand new [`BlockGrouper`] instance.
pub struct BlockGrouper {
    cap: u64,
    pending: Vec<Block>,
    last_taken: Option<u64>,
}

impl BlockGrouper {
    /// A fresh grouper with no continuity constraint on its very first push.
    pub fn new(cap: u64) -> Self {
        Self::new_seeded(cap, None)
    }

    /// A fresh grouper whose first push must continue from `last_taken` (i.e. equal `last_taken + 1`) —
    /// `None` iff there truly is no prior block (design genesis). Used to seed continuity across a
    /// `--follow` restart from [`crate::anchor::Anchor::from_block`].
    pub fn new_seeded(cap: u64, last_taken: Option<u64>) -> Self {
        Self {
            cap,
            pending: Vec::with_capacity(cap as usize),
            last_taken,
        }
    }

    /// The block number a push must supply next to be accepted, or `None` if there is no constraint yet
    /// (a brand new, never-seeded grouper with nothing pushed).
    pub fn expected_next(&self) -> Option<u64> {
        self.pending
            .last()
            .map(|b| b.number + 1)
            .or(self.last_taken.map(|n| n + 1))
    }

    /// Pushes one more block onto the group in progress. Refuses (leaving `self` unchanged) if `block`
    /// does not continue immediately from the last block already pushed, or from `last_taken` if the
    /// group in progress is empty — the caller must treat this as fatal for the whole run
    /// ("continuity break in the log ... nothing posted"), not retry or skip the offending block.
    pub fn push(&mut self, block: Block) -> Result<(), GroupingError> {
        if let Some(expected) = self.expected_next() {
            if block.number != expected {
                return Err(GroupingError::NonContiguousBlock {
                    expected,
                    got: block.number,
                });
            }
        }
        self.pending.push(block);
        Ok(())
    }

    /// `true` once the group in progress holds `cap` blocks — the caller should [`Self::take_group`] and
    /// post it before pushing further (a group is never allowed to grow past `cap`; [`Self::push`] does
    /// not enforce this itself so a caller can always decide exactly when a group closes, e.g. also on
    /// end-of-log or shutdown with a *partial* group still allowed).
    pub fn is_full(&self) -> bool {
        self.pending.len() as u64 >= self.cap
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Takes whatever is accumulated so far (0..=`cap` blocks) and resets the grouper for the next
    /// group — the new group's first push must continue from this group's own last block (see this
    /// type's own doc); a group of zero blocks leaves `last_taken` unchanged.
    pub fn take_group(&mut self) -> Vec<Block> {
        let group = std::mem::take(&mut self.pending);
        if let Some(last) = group.last() {
            self.last_taken = Some(last.number);
        }
        group
    }
}

/// Why [`SizeCappedGrouper::push`] closed the group in progress, or why a caller closed it itself via
/// [`SizeCappedGrouper::close_if_stale`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// The group in progress reached `blocks_per_batch` blocks.
    Cap,
    /// Appending the next block would have pushed the channel's compressed encoding over
    /// `max_frames_per_batch` frames — the group closes *without* that block, which becomes the first
    /// block of the next group instead.
    Size,
    /// The group in progress has held at least one block for `batch_close_after_secs` (the batcher's
    /// own monotonic receipt clock, never `Block.timestamp` vs wall clock) —
    /// [`SizeCappedGrouper::close_if_stale`]'s own doc has the full rationale.
    Age,
}

impl CloseReason {
    /// The `groups_closed_total{reason=...}` label value.
    pub fn as_str(&self) -> &'static str {
        match self {
            CloseReason::Cap => "cap",
            CloseReason::Size => "size",
            CloseReason::Age => "age",
        }
    }
}

/// What [`SizeCappedGrouper::push`] decided.
#[derive(Debug)]
pub enum PushOutcome {
    /// `block` was accepted into the group in progress; it is not yet closable.
    Accepted,
    /// The group in progress just closed — call [`SizeCappedGrouper::take_group`] to retrieve it.
    /// `carry_over` is `Some(block)` when the close reason is [`CloseReason::Size`] (that block was
    /// *not* accepted — the caller must push it again after taking the group); `None` for
    /// [`CloseReason::Cap`] (that block *was* accepted; it is the group's own last block).
    Closed {
        reason: CloseReason,
        carry_over: Option<Block>,
    },
}

/// Closes a group on whichever comes first: `blocks_per_batch` blocks ([`BlockGrouper`]'s own cap), or the channel's
/// compressed encoding would need more than `max_frames_per_batch` frames of `max_frame_body_len` bytes each (the
/// same accounting [`ShadowCompressor`] already implements). Shares [`BlockGrouper`]'s continuity/cap check rather
/// than duplicating it; **the one grouping path both `--once` and `--follow` drive from the chain anchor**: `--once`
/// is `--follow` without the tail wait, so both modes need the size close, not only the tailing one — a full
/// `blocks_per_batch`-block group at the design ceiling can exceed `max_frames_per_batch`, and a `--once` run over
/// such a group would otherwise refuse (`frames.len() > max_frames_per_batch`, `bin/rome-zk-batcher.rs`) on every
/// rerun with no way to make progress.
pub struct SizeCappedGrouper {
    grouper: BlockGrouper,
    shadow: ShadowCompressor,
    max_frames: usize,
    max_frame_body_len: usize,
    /// The batcher's own monotonic instant when the group in progress
    /// received its *first* block off the log — never `Block.timestamp` vs wall clock (see
    /// [`Self::close_if_stale`]'s own doc for why). `None` while the group is empty; set on the first
    /// [`PushOutcome::Accepted`]/[`PushOutcome::Closed`]`{reason: Cap, ..}` push into an empty group (i.e.
    /// exactly once per group, never reset by a later push into the same group); cleared by
    /// [`Self::take_group`].
    first_received_at: Option<Instant>,
}

impl SizeCappedGrouper {
    pub fn new(
        cap: u64,
        max_frames: usize,
        max_frame_body_len: usize,
        last_taken: Option<u64>,
    ) -> Self {
        Self {
            grouper: BlockGrouper::new_seeded(cap, last_taken),
            shadow: ShadowCompressor::new(max_frames, max_frame_body_len),
            max_frames,
            max_frame_body_len,
            first_received_at: None,
        }
    }

    /// Continuity is checked first (non-mutating on failure); then the block is offered to the shadow
    /// compressor *before* it is committed to the grouper, so a [`CloseReason::Size`] close never
    /// mutates the group in progress — `block` is simply handed back via `carry_over` for the caller to
    /// push again once it has taken the now-closed group and started a fresh one.
    ///
    /// `now` is this block's own **receipt instant** — the batcher's own monotonic clock at the moment it
    /// read this block off the log, never `block.timestamp`. It is recorded (once,
    /// see [`Self::first_received_at`]'s own doc) only when this push actually lands a block into the
    /// grouper (i.e. not on a [`CloseReason::Size`] close, which does not accept `block` — the carried-over
    /// block records its own receipt instant on the *next* push, once it is genuinely accepted into the
    /// fresh group).
    pub fn push(&mut self, block: Block, now: Instant) -> Result<PushOutcome, GroupingError> {
        if let Some(expected) = self.grouper.expected_next() {
            if block.number != expected {
                return Err(GroupingError::NonContiguousBlock {
                    expected,
                    got: block.number,
                });
            }
        }
        match self.shadow.try_append(block.clone()) {
            channel::AppendOutcome::Full => {
                if self.grouper.is_empty() {
                    return Err(GroupingError::SingleBlockExceedsFrameBudget {
                        number: block.number,
                        max_frame_body_len: self.max_frame_body_len,
                    });
                }
                Ok(PushOutcome::Closed {
                    reason: CloseReason::Size,
                    carry_over: Some(block),
                })
            }
            channel::AppendOutcome::Added => {
                let was_empty = self.grouper.is_empty();
                self.grouper
                    .push(block)
                    .expect("continuity already checked above");
                // Setting this on every accepted push, rather than only
                // the first into an empty group, would let a fast-arriving trickle keep pushing the age
                // clock forward and never close — the 60 s test in this module's own test suite pins this.
                if was_empty {
                    self.first_received_at = Some(now);
                }
                if self.grouper.is_full() {
                    Ok(PushOutcome::Closed {
                        reason: CloseReason::Cap,
                        carry_over: None,
                    })
                } else {
                    Ok(PushOutcome::Accepted)
                }
            }
        }
    }

    /// Takes the completed group (call after [`Self::push`] returns `Closed`, or after
    /// [`Self::close_if_stale`] returns `true`) and resets the shadow compressor for the next group —
    /// continuity carries forward via [`BlockGrouper::take_group`]'s own seeding.
    pub fn take_group(&mut self) -> Vec<Block> {
        let group = self.grouper.take_group();
        self.shadow = ShadowCompressor::new(self.max_frames, self.max_frame_body_len);
        self.first_received_at = None;
        group
    }

    /// `true` iff the group in progress holds no blocks yet — what a caller checks at end-of-log
    /// (`--once`) to decide whether there is a final partial group left to post before
    /// exiting.
    pub fn is_empty(&self) -> bool {
        self.grouper.is_empty()
    }

    /// How many blocks the group in progress holds so far.
    pub fn len(&self) -> usize {
        self.grouper.len()
    }

    /// `true` iff the group in progress is non-empty AND has held its
    /// first block for at least `close_after` — the caller's cue to [`Self::take_group`] and post the
    /// result with [`CloseReason::Age`], exactly the same way it would post a [`CloseReason::Cap`] or
    /// [`CloseReason::Size`] close.
    ///
    /// **An empty group never closes by age, however long `now` runs ahead** ("no batch when
    /// idle") — there is no `first_received_at` to compare against, so this always returns `false` while
    /// [`Self::is_empty`] holds, regardless of `close_after`.
    ///
    /// `now` is the caller's own receipt-clock reading (`Instant::now()` in production; an injected value
    /// in tests) — **never** derived from any block's `Block.timestamp`. At Tiber's profile
    /// (60 blocks / 60 s / 1 s blocks) chain-time age reaches 60 s while the grouper holds 59 blocks (block
    /// N+59's last sub-block seals at ≈ T+59.95, read ≤ 400 ms later), so every loaded batch would close
    /// `Age` at 59 and never `Cap`; and a batcher wall clock ahead of the sequencer's would shrink groups
    /// toward 1 block (60× the 28 s fixed proof cost, 60× the 0.001 SOL fee). Receipt time involves one
    /// host's clock only: both failure modes are unconstructable, not guarded.
    pub fn close_if_stale(&mut self, now: Instant, close_after: Duration) -> bool {
        match self.first_received_at {
            Some(first) if !self.grouper.is_empty() => {
                now.saturating_duration_since(first) >= close_after
            }
            _ => false,
        }
    }

    /// How long the group in progress has been waiting to post, as of `now` — `Duration::ZERO` while
    /// empty (the `rome_zk_batcher_oldest_unposted_block_age_seconds` gauge's own source: 0 whenever
    /// nothing is unposted).
    pub fn oldest_unposted_age(&self, now: Instant) -> Duration {
        match self.first_received_at {
            Some(first) => now.saturating_duration_since(first),
            None => Duration::ZERO,
        }
    }
}

/// The one place that turns a chain's `profile.json`
/// (`profile_identity.blocks_per_batch`) into this run's grouper cap — a tested library call both the
/// `--once` and `--follow` binary paths call, instead of each inlining
/// `SizeCappedGrouper::new(blocks_per_batch, ..)` against a local variable that could silently drift from
/// what `profile.json` actually says. `blocks_per_batch` is read from `profile_identity` directly — this
/// crate owns no parallel copy of the value and no local default it could
/// silently fall back to instead.
pub fn grouper_from_profile(
    profile_identity: &rome_zk_profile::ProfileIdentity,
    max_frames: usize,
    max_frame_body_len: usize,
    last_taken: Option<u64>,
) -> SizeCappedGrouper {
    SizeCappedGrouper::new(
        profile_identity.blocks_per_batch,
        max_frames,
        max_frame_body_len,
        last_taken,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(number: u64) -> Block {
        Block {
            number,
            timestamp: 1_757_000_000 + number,
            gas_limit: 100_000_000,
            txs: vec![],
        }
    }

    fn blocks(range: std::ops::Range<u64>) -> Vec<Block> {
        range.map(block).collect()
    }

    /// Test-only convenience: pushes every block through one [`BlockGrouper`] and collects each group as
    /// it closes (cap only, no size close) — what the old `group_by_blocks_per_batch` did, kept
    /// here only to exercise [`BlockGrouper`]'s own cap/continuity behavior directly; production code now
    /// drives grouping exclusively through [`SizeCappedGrouper`], seeded from the chain anchor (see
    /// `bin/rome-zk-batcher.rs`).
    fn group_all(blocks: Vec<Block>, cap: u64) -> Result<Vec<Vec<Block>>, GroupingError> {
        let mut grouper = BlockGrouper::new(cap);
        let mut groups = Vec::new();
        for block in blocks {
            if grouper.is_full() {
                groups.push(grouper.take_group());
            }
            grouper.push(block)?;
        }
        if !grouper.is_empty() {
            groups.push(grouper.take_group());
        }
        Ok(groups)
    }

    /// "10 blocks -> one batch".
    #[test]
    fn ten_blocks_at_cap_ten_makes_one_group_of_ten() {
        let groups = group_all(blocks(0..10), 10).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 10);
        assert_eq!(groups[0].first().unwrap().number, 0);
        assert_eq!(groups[0].last().unwrap().number, 9);
    }

    /// "25 -> 3 batches of 10/10/5 with continuity" — every group's first block continues the previous
    /// group's last block + 1.
    #[test]
    fn twenty_five_blocks_at_cap_ten_makes_three_groups_of_ten_ten_five_with_continuity() {
        let groups = group_all(blocks(0..25), 10).unwrap();
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].len(), 10);
        assert_eq!(groups[1].len(), 10);
        assert_eq!(groups[2].len(), 5);
        assert_eq!(groups[0].first().unwrap().number, 0);
        assert_eq!(groups[0].last().unwrap().number, 9);
        assert_eq!(groups[1].first().unwrap().number, 10);
        assert_eq!(groups[1].last().unwrap().number, 19);
        assert_eq!(groups[2].first().unwrap().number, 20);
        assert_eq!(groups[2].last().unwrap().number, 24);
    }

    /// A continuity break in the log (a missing block) is a named error, and nothing is posted.
    #[test]
    fn a_missing_block_is_a_named_error_and_nothing_is_grouped() {
        let mut blocks = blocks(0..5);
        blocks.push(block(7)); // skip 5, 6
        let err = group_all(blocks, 10).unwrap_err();
        assert_eq!(
            err,
            GroupingError::NonContiguousBlock {
                expected: 5,
                got: 7
            }
        );
    }

    /// A group never exceeds `cap`, even when pushed one block at a time (the incremental path
    /// continuous mode uses via [`BlockGrouper`] directly, not [`group_by_blocks_per_batch`]).
    #[test]
    fn block_grouper_never_exceeds_cap_and_reports_full_at_exactly_cap() {
        let mut grouper = BlockGrouper::new(3);
        assert!(!grouper.is_full());
        grouper.push(block(0)).unwrap();
        grouper.push(block(1)).unwrap();
        assert!(!grouper.is_full());
        grouper.push(block(2)).unwrap();
        assert!(grouper.is_full());
        let taken = grouper.take_group();
        assert_eq!(taken.len(), 3);
        assert!(grouper.is_empty());
        assert!(!grouper.is_full());
    }

    /// A fresh group after `take_group` IS constrained by the previous group's last block — a completely unrelated
    /// block number is refused, closing the cross-group continuity gap this test used to pin as "intended". The case:
    /// `push(0)`, `push(1)`, `take_group`, then `push(100)` must refuse, not silently start a new, disconnected
    /// group.
    #[test]
    fn a_fresh_group_after_take_is_continuity_checked_against_the_previous_groups_last_block() {
        let mut grouper = BlockGrouper::new(2);
        grouper.push(block(0)).unwrap();
        grouper.push(block(1)).unwrap();
        let _ = grouper.take_group();
        let err = grouper.push(block(100)).unwrap_err();
        assert_eq!(
            err,
            GroupingError::NonContiguousBlock {
                expected: 2,
                got: 100
            }
        );
        assert!(
            grouper.is_empty(),
            "a refused push must not have been applied"
        );
    }

    /// A grouper seeded with `last_taken` (`BlockGrouper::new_seeded(cap,
    /// anchor.from_block.checked_sub(1))`) requires its very first push to continue from the seed,
    /// exactly like any push after a `take_group` — this is what makes a `--follow` restart's first block
    /// continuity-checked against the on-chain anchor instead of being silently unconstrained.
    #[test]
    fn a_seeded_grouper_requires_its_first_push_to_continue_the_seed() {
        let mut grouper = BlockGrouper::new_seeded(5, Some(41));
        let err = grouper.push(block(100)).unwrap_err();
        assert_eq!(
            err,
            GroupingError::NonContiguousBlock {
                expected: 42,
                got: 100
            }
        );
        grouper.push(block(42)).unwrap();
        assert_eq!(grouper.len(), 1);
    }

    /// A grouper seeded with `last_taken = None` (design genesis, no anchor yet) has no constraint on its
    /// first push — unlike a seeded grouper, and unlike a post-`take_group` grouper, this is the one
    /// legitimately-unconstrained case.
    #[test]
    fn a_grouper_seeded_with_no_prior_block_is_unconstrained_on_its_first_push() {
        let mut grouper = BlockGrouper::new_seeded(5, None);
        grouper.push(block(1_000)).unwrap();
        assert_eq!(grouper.len(), 1);
    }

    #[test]
    fn pushing_a_non_contiguous_block_leaves_the_group_unchanged() {
        let mut grouper = BlockGrouper::new(5);
        grouper.push(block(0)).unwrap();
        let err = grouper.push(block(5)).unwrap_err();
        assert_eq!(
            err,
            GroupingError::NonContiguousBlock {
                expected: 1,
                got: 5
            }
        );
        assert_eq!(
            grouper.len(),
            1,
            "the failed push must not have been applied"
        );
    }

    /// Reusing one `BlockGrouper` across all groups in a whole run
    /// used to lose continuity the moment `take_group` cleared its state — a gap landing exactly at a
    /// group boundary (block 10..14 skipped, cap 10) used to be silently accepted as two disconnected
    /// groups. Now that `take_group` carries `last_taken` forward, this refuses.
    #[test]
    fn a_gap_exactly_at_a_group_boundary_is_refused_not_silently_split_into_two_groups() {
        let mut input = blocks(0..10);
        input.extend(blocks(15..18)); // skip 10..14
        let err = group_all(input, 10).unwrap_err();
        assert_eq!(
            err,
            GroupingError::NonContiguousBlock {
                expected: 10,
                got: 15
            }
        );
    }

    /// Genuinely random (not zstd-compressible, unlike a repeated byte or a short deterministic cycle) tx
    /// payload of `len` bytes, so a "large tx" test actually produces a large *compressed* encoding too.
    fn tx(len: usize) -> alloy_primitives::Bytes {
        use rand::RngCore;
        let mut buf = vec![0u8; len];
        rand::thread_rng().fill_bytes(&mut buf);
        alloy_primitives::Bytes::from(buf)
    }

    /// A normal-size block: appending it never overflows a generous frame budget.
    fn small_block(number: u64) -> Block {
        Block {
            number,
            timestamp: 1_757_000_000 + number,
            gas_limit: 100_000_000,
            txs: vec![tx(4)],
        }
    }

    /// A `SizeCappedGrouper` closes a group before its
    /// `blocks_per_batch` cap when the next block would push the channel over `max_frames` frames — here
    /// a synthetic large-tx 10th block, at cap 10, closes the group at 9 blocks instead, and the large
    /// block starts the next group.
    #[test]
    fn a_synthetic_large_tx_block_closes_the_group_early_at_nine_blocks() {
        let large = Block {
            number: 9,
            timestamp: 1_757_000_009,
            gas_limit: 100_000_000,
            txs: vec![tx(10_000)],
        };
        // A budget picked to sit strictly between "9 small blocks alone" and "9 small blocks + the large
        // one" (both measured directly via `encode_stream`, not guessed) — big enough for the large block
        // on its own (the next group, after this one closes), too small for it to join the first 9.
        let nine_small: Vec<Block> = (0..9).map(small_block).collect();
        let nine_small_len = channel::encode_stream(&nine_small).len();
        let mut with_large = nine_small.clone();
        with_large.push(large.clone());
        let combined_len = channel::encode_stream(&with_large).len();
        let large_alone_len = channel::encode_stream(std::slice::from_ref(&large)).len();
        let max_frame_body_len = combined_len - 1;
        assert!(
            max_frame_body_len >= nine_small_len && max_frame_body_len >= large_alone_len,
            "test setup: the budget must fit the 9 small blocks alone AND the large block alone, just \
             not both together (nine_small_len={nine_small_len}, large_alone_len={large_alone_len}, \
             combined_len={combined_len})"
        );

        let mut g = SizeCappedGrouper::new(10, 1, max_frame_body_len, None);
        let now = Instant::now();
        // Push clones of the SAME nine blocks the budget above was measured against — `small_block(n)`
        // would regenerate fresh random tx bytes on every call (the
        // budget was measured on one random set, then a completely different fresh set was pushed here,
        // so an unlucky compression ratio on the fresh set made this flaky).
        for block in &nine_small {
            let outcome = g.push(block.clone(), now).unwrap();
            assert!(
                matches!(outcome, PushOutcome::Accepted),
                "block {} must not close the group early",
                block.number
            );
        }
        let outcome = g.push(large.clone(), now).unwrap();
        match outcome {
            PushOutcome::Closed {
                reason: CloseReason::Size,
                carry_over,
            } => assert_eq!(carry_over, Some(large.clone())),
            other => panic!("expected a Size close, got {other:?}"),
        }
        let group = g.take_group();
        assert_eq!(group.len(), 9, "the group must close at 9 blocks, not 10");
        assert_eq!(group.first().unwrap().number, 0);
        assert_eq!(group.last().unwrap().number, 8);

        // The carried-over block becomes the next group's own first block.
        let outcome = g.push(large, now).unwrap();
        assert!(matches!(outcome, PushOutcome::Accepted));
        assert_eq!(g.take_group().len(), 1);
    }

    /// The `blocks_per_batch` cap still closes a group when size never binds (the other
    /// close condition, unchanged from `BlockGrouper`'s own behavior).
    #[test]
    fn a_size_capped_grouper_still_closes_on_the_blocks_per_batch_cap() {
        let mut g = SizeCappedGrouper::new(3, 900, 3_681, None);
        let now = Instant::now();
        assert!(matches!(
            g.push(small_block(0), now).unwrap(),
            PushOutcome::Accepted
        ));
        assert!(matches!(
            g.push(small_block(1), now).unwrap(),
            PushOutcome::Accepted
        ));
        let outcome = g.push(small_block(2), now).unwrap();
        assert!(matches!(
            outcome,
            PushOutcome::Closed {
                reason: CloseReason::Cap,
                carry_over: None
            }
        ));
        assert_eq!(g.take_group().len(), 3);
    }

    /// Continuity is still enforced by `SizeCappedGrouper` (it shares `BlockGrouper`'s own check, not a
    /// second copy).
    #[test]
    fn a_size_capped_grouper_refuses_a_non_contiguous_push() {
        let mut g = SizeCappedGrouper::new(10, 900, 3_681, None);
        let now = Instant::now();
        g.push(small_block(0), now).unwrap();
        let err = g.push(small_block(5), now).unwrap_err();
        assert_eq!(
            err,
            GroupingError::NonContiguousBlock {
                expected: 1,
                got: 5
            }
        );
    }

    /// A single block that alone exceeds the frame budget is a named misconfiguration error, never a
    /// zero-block group.
    #[test]
    fn a_single_block_alone_exceeding_the_frame_budget_is_a_named_error() {
        let mut g = SizeCappedGrouper::new(10, 1, 64, None);
        let huge = Block {
            number: 0,
            timestamp: 1_757_000_000,
            gas_limit: 100_000_000,
            txs: vec![tx(10_000)],
        };
        let err = g.push(huge, Instant::now()).unwrap_err();
        assert!(matches!(
            err,
            GroupingError::SingleBlockExceedsFrameBudget { number: 0, .. }
        ));
    }

    fn identity_with_blocks_per_batch(
        blocks_per_batch: u64,
    ) -> rome_zk_sequencer::profile::ProfileIdentity {
        rome_zk_sequencer::profile::ProfileIdentity {
            chain_id: 200_101,
            sub_block_ms: 50,
            sub_blocks_per_block: 20,
            sub_block_gas_limit: 5_000_000,
            block_gas_limit: 100_000_000,
            blocks_per_batch,
            first_block: rome_zk_sequencer::profile::FIRST_BLOCK,
        }
    }

    /// `grouper_from_profile`'s cap comes from
    /// `profile_identity.blocks_per_batch`, the design default (10) — proving the factory actually reads
    /// the profile rather than a hardcoded literal that happens to match it too. This
    /// test builds its identity from `rome_zk_sequencer::profile::Profile::default()` — the real,
    /// shared default — rather than a bare literal `10`, specifically so bumping
    /// `rome_zk_profile::DEFAULT_BLOCKS_PER_BATCH` flows through and makes this test fail too (proving
    /// "one home", not just "one name that happens to also be 10").
    #[test]
    fn grouper_from_profile_at_the_design_default_caps_at_ten() {
        let identity = rome_zk_sequencer::profile::ProfileIdentity::new(
            200_101,
            &rome_zk_sequencer::profile::Profile::default(),
        );
        assert_eq!(
            identity.blocks_per_batch, 10,
            "sanity: the design default is 10"
        );
        let mut g = grouper_from_profile(&identity, 900, 3_681, None);
        let now = Instant::now();
        let mut closed_at = None;
        for (i, b) in blocks(0..12).into_iter().enumerate() {
            if let PushOutcome::Closed { reason, .. } = g.push(b, now).unwrap() {
                assert_eq!(reason, CloseReason::Cap);
                closed_at = Some(i + 1);
                break;
            }
        }
        assert_eq!(closed_at, Some(10));
    }

    /// A `grouper_from_profile` that ignores the profile and
    /// hardcodes 10 makes this test fail — `blocks_per_batch = 7` is a second value the identity could
    /// legally carry (the batcher is not fixed to any one cap; the cap must come
    /// from `profile.json`, never a local copy), so a grouper built from it must close its group at 7
    /// blocks, not 10.
    #[test]
    fn grouper_from_profile_honours_a_non_default_blocks_per_batch() {
        let identity = identity_with_blocks_per_batch(7);
        let mut g = grouper_from_profile(&identity, 900, 3_681, None);
        let now = Instant::now();
        let mut closed_at = None;
        for (i, b) in blocks(0..10).into_iter().enumerate() {
            match g.push(b, now).unwrap() {
                PushOutcome::Accepted => {}
                PushOutcome::Closed { reason, .. } => {
                    assert_eq!(reason, CloseReason::Cap, "closed by cap, not by size");
                    closed_at = Some(i + 1);
                    break;
                }
            }
        }
        assert_eq!(
            closed_at,
            Some(7),
            "the group must close at blocks_per_batch=7 from the profile, not the design default of 10"
        );
    }

    /// `grouper_from_profile` seeds continuity from `last_taken`, exactly like a direct
    /// `SizeCappedGrouper::new` call — a restart resuming mid-chain must still reject a block that does
    /// not continue from the chain anchor.
    #[test]
    fn grouper_from_profile_seeds_continuity_from_last_taken() {
        let identity = identity_with_blocks_per_batch(10);
        let mut g = grouper_from_profile(&identity, 900, 3_681, Some(41));
        let err = g.push(block(100), Instant::now()).unwrap_err();
        assert_eq!(
            err,
            GroupingError::NonContiguousBlock {
                expected: 42,
                got: 100
            }
        );
    }

    // ===================== Batch age close =====================

    /// A partial group that received its first block 60 s ago (by the injected receipt clock) closes by
    /// age at exactly `close_after` — the caller's own cue to `take_group` and post it under
    /// `CloseReason::Age`. Below 60 s it must not close yet.
    #[test]
    fn a_partial_group_received_60s_ago_is_posted_with_age_reason() {
        let mut g = SizeCappedGrouper::new(60, 900, 3_681, None);
        let t0 = Instant::now();
        assert!(matches!(
            g.push(block(0), t0).unwrap(),
            PushOutcome::Accepted
        ));
        assert!(matches!(
            g.push(block(1), t0 + Duration::from_secs(1)).unwrap(),
            PushOutcome::Accepted
        ));

        let close_after = Duration::from_secs(60);
        assert!(
            !g.close_if_stale(t0 + Duration::from_secs(59), close_after),
            "59 s after the first block: not stale yet"
        );
        assert!(
            g.close_if_stale(t0 + Duration::from_secs(60), close_after),
            "60 s after the first block: stale, ready to post as Age"
        );
        let group = g.take_group();
        assert_eq!(group.len(), 2);
        assert_eq!(group.first().unwrap().number, 0);
        assert_eq!(group.last().unwrap().number, 1);
        assert!(g.is_empty(), "take_group must clear the group in progress");
        assert!(
            !g.close_if_stale(t0 + Duration::from_secs(1_000), close_after),
            "an empty group never closes by age, however far now runs ahead"
        );
    }

    /// An empty grouper never closes by age, however long `now` runs ahead — there is
    /// simply no `first_received_at` to compare against yet.
    #[test]
    fn an_empty_grouper_never_closes_by_age_regardless_of_elapsed_time() {
        let mut g = SizeCappedGrouper::new(60, 900, 3_681, None);
        let t0 = Instant::now();
        for secs in [0u64, 1, 60, 3_600, 1_000_000] {
            assert!(
                !g.close_if_stale(t0 + Duration::from_secs(secs), Duration::from_secs(60)),
                "an empty grouper closed by age at +{secs}s"
            );
        }
        assert_eq!(
            g.oldest_unposted_age(t0 + Duration::from_secs(3_600)),
            Duration::ZERO
        );
    }

    /// The cap still wins when it is reached well before `close_after` — block 60
    /// arriving one second after block 1 (t = 59 s from the first receipt, cap 60) closes by `Cap`, never
    /// `Age`. `SizeCappedGrouper::push`'s own cap check has no idea `close_after` even exists; this pins
    /// that the two mechanisms stay independent (the caller decides when to ask about age, `push` never
    /// does on its own).
    #[test]
    fn cap_fires_before_age_when_60_blocks_arrive_one_second_apart() {
        let mut g = SizeCappedGrouper::new(60, 900, 3_681, None);
        let t0 = Instant::now();
        let mut last_outcome = None;
        for i in 0..60u64 {
            let now = t0 + Duration::from_secs(i);
            last_outcome = Some(g.push(block(i), now).unwrap());
        }
        match last_outcome.unwrap() {
            PushOutcome::Closed {
                reason: CloseReason::Cap,
                carry_over: None,
            } => {}
            other => panic!("block 60 (t=59s) must close by Cap, got {other:?}"),
        }
        let group = g.take_group();
        assert_eq!(group.len(), 60);
    }

    /// Age must come from the batcher's own receipt `Instant`,
    /// never from `Block.timestamp` — every block here carries a `Block.timestamp` decades BEHIND the
    /// injected receipt clock (any stale on-chain value serves), and the group must still close by `Cap`
    /// (reaching the cap well inside a real 60 s `close_after`), never early by an `Age` check that
    /// mistakenly read the stale on-chain timestamps instead of the receipt clock.
    #[test]
    fn a_batcher_wall_clock_ahead_of_the_sequencer_never_shortens_a_group() {
        let stale_timestamp_base = 1_000u64; // decades behind the receipt clock: any stale on-chain value
        let far_ahead_block = |number: u64| Block {
            number,
            timestamp: stale_timestamp_base + number, // deliberately stale/irrelevant to the receipt clock
            gas_limit: 100_000_000,
            txs: vec![],
        };
        let mut g = SizeCappedGrouper::new(10, 900, 3_681, None);
        let t0 = Instant::now();
        // 2 s is generous headroom over the ~900 ms of real receipt time 10 blocks span below — large
        // enough that a correct (receipt-clock) implementation never reports stale here, but small
        // enough that an implementation reading `block.timestamp` instead (real wall-clock seconds vs a
        // fake ~1,000-second value) would report stale from the very first push: `close_if_stale` is
        // exercised at every step, not only trusted to exist.
        let close_after = Duration::from_secs(2);
        let mut last_outcome = None;
        for i in 0..10u64 {
            // Blocks arrive 100 ms apart in receipt time — well under close_after — while their own
            // Block.timestamp fields sit a full hour "behind" (i.e. unrelated to) that receipt clock.
            let now = t0 + Duration::from_millis(100 * i);
            last_outcome = Some(g.push(far_ahead_block(i), now).unwrap());
            assert!(
                !g.close_if_stale(now, close_after),
                "block {i}'s own on-chain timestamp looks ancient, but only {} ms of real receipt time \
                 has actually elapsed — must not report stale",
                100 * i
            );
        }
        match last_outcome.unwrap() {
            PushOutcome::Closed {
                reason: CloseReason::Cap,
                carry_over: None,
            } => {}
            other => panic!("expected a Cap close driven by the receipt clock, got {other:?}"),
        }
        assert_eq!(g.take_group().len(), 10);
    }

    /// Continuity carries across an age close exactly as it does across a `Cap`/`Size` close — the next
    /// group's first push must continue from the aged-out group's own last block.
    #[test]
    fn age_close_keeps_continuity_into_the_next_group() {
        let mut g = SizeCappedGrouper::new(60, 900, 3_681, None);
        let t0 = Instant::now();
        g.push(block(0), t0).unwrap();
        g.push(block(1), t0 + Duration::from_secs(1)).unwrap();
        assert!(g.close_if_stale(t0 + Duration::from_secs(60), Duration::from_secs(60)));
        let group = g.take_group();
        assert_eq!(group.last().unwrap().number, 1);

        let err = g
            .push(block(100), t0 + Duration::from_secs(61))
            .unwrap_err();
        assert_eq!(
            err,
            GroupingError::NonContiguousBlock {
                expected: 2,
                got: 100
            },
            "the group after an age close must still be continuity-checked against the aged-out group's \
             own last block"
        );
        assert!(matches!(
            g.push(block(2), t0 + Duration::from_secs(61)).unwrap(),
            PushOutcome::Accepted
        ));
    }
}
