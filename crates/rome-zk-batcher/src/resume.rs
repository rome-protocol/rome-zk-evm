//! Stateless resume: on start, the batcher has no local durable state of its own — it decides where to
//! continue purely from what the inbox program's on-chain batch accounts say. `batch` ids are a plain
//! monotonically-increasing counter this process assigns (unrelated to block numbers). An id is never
//! burned: a batch left half written by a crash is FINISHED under the same id by the next start
//! (`recover.rs`), because settlement posts exactly `head_pending_batch + 1` and an id the inbox cursor
//! has passed can never be opened again.
//!
//! This module is the pure decision table only — no RPC calls, no I/O. `pipeline.rs` supplies the
//! on-chain state (via `zk_inbox_client::decode_batch_account`, or a fake in tests) and drives the loop
//! this module's [`decide`] describes one step of.

/// What the inbox program's batch account for `(chain_id, batch)` currently says, boiled down to the
/// three facts resume needs. Constructed by `pipeline.rs` from
/// [`zk_inbox_client::decode_batch_account`]'s output (or a fake state in tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchAccountState {
    /// No account exists at this batch id's PDA yet — never opened (or abandoned by hand).
    Missing,
    /// The account exists, is not yet finalized: some (possibly zero) leaves sealed, but
    /// `leaves_present < expected_count`. Either a live process is still sending the missing leaves, or the
    /// process that opened this batch died; a stateless batcher restarting here treats it as the latter and
    /// finishes it under the same id (`recover.rs`), never abandoning it.
    OpenNotFinalized {
        leaves_present: u32,
        expected_count: u32,
    },
    /// The account exists and `finalized == true` — this batch's DA is complete and its `acc` is set.
    Finalized,
}

/// What to do about one batch id, given its on-chain state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeAction {
    /// Nothing exists here yet: this is the resume point — `OpenBatch` and post the next unposted block
    /// range under this id.
    PostFresh,
    /// A previous attempt died mid-post: finish this same id (`recover.rs` searches for the grouping that
    /// matches what is on chain, sends the missing frames and finalizes). The id is never abandoned.
    ResumeOpen,
    /// Already finalized on chain, but this session has not yet handed it to the `PostRootSink` — verify
    /// `acc` against the client-side reference and publish, without resending any chunk transaction, then
    /// continue scanning at the next batch id.
    HandOffOnly,
    /// Already finalized on chain **and** already handed off earlier in this same scan (or session) —
    /// nothing left to do here; continue scanning at the next batch id.
    Advance,
}

/// One step of the decision table (the resume logic, tested against fake account states).
///
/// `already_handed_off` is the caller's own note of whether *this run* already published this exact
/// `(chain_id, batch)` to the `PostRootSink` earlier in the same startup scan (never persisted — a fresh
/// process re-verifies and re-publishes every already-finalized batch it walks past once, which the
/// `PostRootSink` consumer must treat as idempotent by `(chain_id, batch)`, the same way
/// `rome_zk_sequencer::preconf::SubBlockSink` consumers must tolerate their own best-effort redelivery).
pub fn decide(state: BatchAccountState, already_handed_off: bool) -> ResumeAction {
    match state {
        BatchAccountState::Missing => ResumeAction::PostFresh,
        BatchAccountState::OpenNotFinalized { .. } => ResumeAction::ResumeOpen,
        BatchAccountState::Finalized => {
            if already_handed_off {
                ResumeAction::Advance
            } else {
                ResumeAction::HandOffOnly
            }
        }
    }
}

/// Scans batch ids `starting_at, starting_at + 1, ...` (via `probe`, which returns this id's on-chain
/// state) until it finds the first [`ResumeAction::PostFresh`] or [`ResumeAction::ResumeOpen`] id — the
/// point production should actually resume at. Every `Finalized` id walked past along the way is reported
/// via `on_finalized` (so the caller can hand it off to the `PostRootSink`) before scanning continues.
///
/// Restated precisely: the batcher resumes at the first batch that is not finalized on chain — `Missing`
/// and `OpenNotFinalized` are exactly "not finalized"; this function stops at the
/// first one of either, in ascending batch-id order, having reported every finalized id seen before it.
pub fn find_resume_point(
    starting_at: u64,
    mut probe: impl FnMut(u64) -> BatchAccountState,
    mut on_finalized: impl FnMut(u64),
) -> ResumePoint {
    let mut batch = starting_at;
    loop {
        let state = probe(batch);
        match decide(state, false) {
            ResumeAction::PostFresh => {
                return ResumePoint {
                    batch,
                    action: ResumeAction::PostFresh,
                }
            }
            ResumeAction::ResumeOpen => {
                return ResumePoint {
                    batch,
                    action: ResumeAction::ResumeOpen,
                }
            }
            ResumeAction::HandOffOnly => {
                on_finalized(batch);
                batch += 1;
            }
            ResumeAction::Advance => unreachable!(
                "decide(_, false) never returns Advance — `already_handed_off` is always false here"
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumePoint {
    pub batch: u64,
    /// Always [`ResumeAction::PostFresh`] or [`ResumeAction::ResumeOpen`] — see [`find_resume_point`]'s doc.
    pub action: ResumeAction,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// The four states each decide correctly on their own.
    #[test]
    fn decision_table_no_batch() {
        assert_eq!(
            decide(BatchAccountState::Missing, false),
            ResumeAction::PostFresh
        );
        assert_eq!(
            decide(BatchAccountState::Missing, true),
            ResumeAction::PostFresh
        );
    }

    #[test]
    fn decision_table_half_written() {
        let half = BatchAccountState::OpenNotFinalized {
            leaves_present: 3,
            expected_count: 10,
        };
        assert_eq!(decide(half, false), ResumeAction::ResumeOpen);
        assert_eq!(decide(half, true), ResumeAction::ResumeOpen);
    }

    #[test]
    fn decision_table_finalized_but_not_posted() {
        assert_eq!(
            decide(BatchAccountState::Finalized, false),
            ResumeAction::HandOffOnly
        );
    }

    #[test]
    fn decision_table_all_posted() {
        assert_eq!(
            decide(BatchAccountState::Finalized, true),
            ResumeAction::Advance
        );
    }

    /// A fresh chain (nothing ever opened) resumes at batch 0, `PostFresh`, with nothing handed off.
    #[test]
    fn find_resume_point_on_a_fresh_chain_starts_at_zero() {
        let mut handed_off = Vec::new();
        let rp = find_resume_point(0, |_| BatchAccountState::Missing, |b| handed_off.push(b));
        assert_eq!(
            rp,
            ResumePoint {
                batch: 0,
                action: ResumeAction::PostFresh
            }
        );
        assert!(handed_off.is_empty());
    }

    /// Batches 0..3 already finalized, batch 3 missing: resume at 3, having handed off 0, 1, 2 in order.
    #[test]
    fn find_resume_point_walks_past_every_finalized_batch_in_order() {
        let mut states = HashMap::new();
        states.insert(0u64, BatchAccountState::Finalized);
        states.insert(1u64, BatchAccountState::Finalized);
        states.insert(2u64, BatchAccountState::Finalized);
        let mut handed_off = Vec::new();
        let rp = find_resume_point(
            0,
            |b| {
                states
                    .get(&b)
                    .copied()
                    .unwrap_or(BatchAccountState::Missing)
            },
            |b| handed_off.push(b),
        );
        assert_eq!(
            rp,
            ResumePoint {
                batch: 3,
                action: ResumeAction::PostFresh
            }
        );
        assert_eq!(handed_off, vec![0, 1, 2]);
    }

    /// A half-written batch stops the scan immediately with `ResumeOpen`, without walking past it (nothing
    /// after an unfinished batch can be trusted, by construction — the scan never even probes batch+1).
    #[test]
    fn find_resume_point_stops_at_a_half_written_batch() {
        let mut probed = Vec::new();
        let rp = find_resume_point(
            5,
            |b| {
                probed.push(b);
                if b == 5 {
                    BatchAccountState::OpenNotFinalized {
                        leaves_present: 4,
                        expected_count: 9,
                    }
                } else {
                    BatchAccountState::Missing
                }
            },
            |_| panic!("must not hand off past a half-written batch"),
        );
        assert_eq!(
            rp,
            ResumePoint {
                batch: 5,
                action: ResumeAction::ResumeOpen
            }
        );
        assert_eq!(probed, vec![5], "must not probe beyond the abandoned id");
    }

    /// Resuming partway through history (`starting_at` > 0) behaves identically to resuming from 0 —
    /// the scan is purely a function of on-chain state, never of where it started.
    #[test]
    fn find_resume_point_honors_a_nonzero_starting_point() {
        let mut states = HashMap::new();
        states.insert(41u64, BatchAccountState::Finalized);
        let mut handed_off = Vec::new();
        let rp = find_resume_point(
            41,
            |b| {
                states
                    .get(&b)
                    .copied()
                    .unwrap_or(BatchAccountState::Missing)
            },
            |b| handed_off.push(b),
        );
        assert_eq!(
            rp,
            ResumePoint {
                batch: 42,
                action: ResumeAction::PostFresh
            }
        );
        assert_eq!(handed_off, vec![41]);
    }
}
