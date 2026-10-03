//! `SolanaTraversal`: the pipeline's outermost pull stage — walks the sequential `batch_cursor`
//! one finalized batch at a time.
//!
//! With finalized-only reads and sequential, never-reused batch ids, there is no reorg for this stage to
//! recover from — the previous `open_slot`-gap heuristic (a
//! batch finalized "too long" after the last one meant "abandoned channel, reset") was **deleted
//! entirely**. It produced a livelock: any batcher pause longer than one `channel_timeout` wedged this
//! node forever (five consecutive `Reset`s, no engine call). `PipelineError::Reset` itself is now deleted
//! crate-wide (`crate::PipelineError`'s own doc) — this stage never raised it for a live reason, and neither did
//! anything else, so this module no longer exposes a `reset()` method either. A settlement-root anchor
//! (`crate::resume`) decides where a restarted node's traversal starts; there is no
//! in-process "walk back" reaction left to trigger.

use solana_program::pubkey::Pubkey;

use crate::reader::AccountReader;
use crate::PipelineError;

/// A finalized batch, ready for [`crate::inbox::InboxRetrieval`] — everything [`SolanaTraversal::next`]
/// already learned from the batch account so later stages never re-read it.
/// Carries the account's own commitment fields too, so [`crate::inbox::InboxRetrieval`] can recompute
/// leaf/root/acc over the bytes it reads and compare, instead of trusting them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchRef {
    pub chain_id: u64,
    pub batch: u64,
    /// The slot the batch was opened at (`open_slot` is committed on the batch account —
    /// the deterministic anchor every block in the batch's timestamp bound refers to).
    pub open_slot: u64,
    /// The committed `Clock::unix_timestamp` `OpenBatch` wrote alongside `open_slot` (header v2) — the anchor
    /// `batch_queue::enforce_drift_bound`'s one-sided drift bound checks
    /// every block's timestamp against.
    pub open_unix_ts: i64,
    pub expected_count: u32,
    /// The accumulator's Merkle root over this batch's indexed chunk leaves.
    pub root: [u8; 32],
    /// The forced lane's root (v1: always the fixed empty-lane constant).
    pub forced_root: [u8; 32],
    /// `keccak(chain_id ‖ batch ‖ open_slot ‖ expected_count ‖ root ‖ forced_root)` — the
    /// one value that pins every other field together.
    pub acc: [u8; 32],
}

/// Walks batch ids `0, 1, 2, ...` (batch ids are sequential per chain, never
/// reused — see `zk-inbox-client`'s `cursor_pda`/`decode_batch_cursor` doc), pulling one finalized batch
/// at a time from the real accumulator (the batch account), never inferring anything about a
/// batch beyond what that one account states.
///
/// Batch ids are DA containers, not block arithmetic — a batch id the
/// batcher opened and then abandoned (`AbandonBatch`) or already closed (`CloseBatch`, post-settlement)
/// never becomes finalized, so a naive "keep waiting for this id" would stall forever even though later
/// ids finalize normally. [`Self::next`] tells an abandoned/absent id apart from "not posted yet" by
/// reading the on-chain `batch_cursor`'s `next_batch` (the ceiling of every id the batcher has ever
/// opened): once the cursor has moved past this traversal's `next_batch`, a still-missing account here
/// can only mean that id was abandoned or closed — skip it, don't wait on it.
pub struct SolanaTraversal<R> {
    reader: R,
    program_id: Pubkey,
    /// The settlement program the chain is registered under — batch and cursor addresses are keyed by it.
    settlement_program: Pubkey,
    chain_id: u64,
    next_batch: u64,
}

impl<R: AccountReader> SolanaTraversal<R> {
    pub fn new(
        reader: R,
        program_id: Pubkey,
        settlement_program: Pubkey,
        chain_id: u64,
        start_at_batch: u64,
    ) -> Self {
        Self {
            reader,
            program_id,
            settlement_program,
            chain_id,
            next_batch: start_at_batch,
        }
    }

    /// The next batch id this traversal will attempt to pull.
    pub fn next_batch(&self) -> u64 {
        self.next_batch
    }

    /// Pulls the next batch, if its account exists and is `finalized` (chunks are read at
    /// Solana FINALIZED commitment — [`AccountReader`] implementations read at that commitment).
    /// `Ok(None)` means "not there yet" — a Temporary condition from the caller's point of view, but
    /// modeled as a plain `None` rather than `Err(Temporary)` so the ordinary polling loop
    /// ([`crate::pipeline::DerivePipeline::run_forever`]) does not need to pattern-match an error for
    /// its single most common outcome. Loops internally (rather than returning to the caller) past every
    /// abandoned/closed id it skips, so a caller always either gets the next real finalized
    /// batch or a genuine "nothing yet".
    pub async fn next(&mut self) -> Result<Option<BatchRef>, PipelineError> {
        loop {
            let (batch_pda, _) = zk_inbox_client::batch_pda(
                &self.program_id,
                &self.settlement_program,
                self.chain_id,
                self.next_batch,
            );
            let Some(data) = self.reader.get_account_data(batch_pda).await? else {
                if self.id_already_passed_by_cursor().await? {
                    // The batcher's own cursor has already moved past this id with no account ever
                    // landing here — abandoned (or closed post-settlement), never coming. Skip it.
                    self.next_batch += 1;
                    continue;
                }
                return Ok(None);
            };
            let decoded = zk_inbox_client::decode_batch_account(&data).map_err(|e| {
                PipelineError::Critical(format!(
                    "batch {} account at {batch_pda} failed to decode: {e}",
                    self.next_batch
                ))
            })?;
            if !decoded.finalized {
                return Ok(None);
            }
            // The PDA is derived from (program_id, chain_id, next_batch), so a decoded account
            // reporting a different (chain_id, batch) can only mean this crate's own PDA derivation
            // drifted from `zk-inbox`'s (a build-time bug, not a Solana-side condition) — fail loudly
            // rather than silently deriving the wrong chain's chunks.
            if decoded.chain_id != self.chain_id || decoded.batch != self.next_batch {
                return Err(PipelineError::Critical(format!(
                    "batch account at the expected PDA for (chain_id={}, batch={}) reports (chain_id={}, batch={}) instead",
                    self.chain_id, self.next_batch, decoded.chain_id, decoded.batch
                )));
            }
            return Ok(Some(BatchRef {
                chain_id: self.chain_id,
                batch: self.next_batch,
                open_slot: decoded.open_slot,
                open_unix_ts: decoded.open_unix_ts,
                expected_count: decoded.expected_count,
                root: decoded.root,
                forced_root: decoded.forced_root,
                acc: decoded.acc,
            }));
        }
    }

    /// `true` once the on-chain `batch_cursor.next_batch` has moved strictly past
    /// `self.next_batch` — proof this id was opened and resolved (abandoned or closed) without this
    /// traversal ever seeing it finalized. `false` if the cursor itself does not exist yet (nothing to
    /// compare against — ordinary "not posted yet") or has not reached this id.
    async fn id_already_passed_by_cursor(&mut self) -> Result<bool, PipelineError> {
        let (cursor_pda, _) =
            zk_inbox_client::cursor_pda(&self.program_id, &self.settlement_program, self.chain_id);
        let Some(data) = self.reader.get_account_data(cursor_pda).await? else {
            return Ok(false);
        };
        let cursor = zk_inbox_client::decode_batch_cursor(&data)
            .map_err(|e| PipelineError::Critical(format!("batch_cursor decode: {e}")))?;
        Ok(cursor.next_batch > self.next_batch)
    }

    /// Call once the batch [`Self::next`] returned has been fully derived through the engine —
    /// advances the cursor to the next sequential batch id.
    pub fn advance(&mut self) {
        self.next_batch += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// An in-memory [`AccountReader`] driven entirely by a test's own script — no Solana process
    /// involved. Used for every test in this module; the real, on-chain-backed path is exercised
    /// separately by `tests/real_program_inbox.rs`.
    #[derive(Default)]
    struct FakeReader {
        accounts: HashMap<Pubkey, Vec<u8>>,
    }
    impl AccountReader for FakeReader {
        async fn get_account_data(
            &mut self,
            pubkey: Pubkey,
        ) -> Result<Option<Vec<u8>>, PipelineError> {
            Ok(self.accounts.get(&pubkey).cloned())
        }
    }

    /// A plausible (not zero) `open_unix_ts` — this module's tests never exercise the drift bound
    /// itself (that lives in `batch_queue`'s own tests), so any real-looking committed clock reading
    /// does; a fixed constant keeps every call site here simple.
    const PLAUSIBLE_OPEN_UNIX_TS: i64 = 1_700_000_000;

    fn batch_account_bytes(
        chain_id: u64,
        batch: u64,
        open_slot: u64,
        expected_count: u32,
        finalized: bool,
    ) -> Vec<u8> {
        let header = rome_zk_layouts::batch::write_header(&rome_zk_layouts::batch::BatchFields {
            chain_id,
            batch,
            open_slot,
            expected_count,
            leaves_present: 0,
            finalized,
            settlement_program: [0u8; 32],
            authority: [0u8; 32],
            root: [0u8; 32],
            forced_root: [0u8; 32],
            acc: [0u8; 32],
            finalize_cursor: 0,
            open_unix_ts: PLAUSIBLE_OPEN_UNIX_TS,
        });
        let mut d = vec![0u8; rome_zk_layouts::batch::account_len(expected_count)];
        d[..rome_zk_layouts::batch::HEADER_LEN].copy_from_slice(&header);
        d
    }

    fn cursor_account_bytes(chain_id: u64, next_batch: u64) -> Vec<u8> {
        rome_zk_layouts::cursor::write(&rome_zk_layouts::cursor::CursorFields {
            chain_id,
            next_batch,
        })
        .to_vec()
    }

    #[tokio::test]
    async fn returns_none_when_the_batch_account_does_not_exist_yet() {
        let program_id = Pubkey::new_unique();
        let settlement_program = Pubkey::new_unique();
        let mut t =
            SolanaTraversal::new(FakeReader::default(), program_id, settlement_program, 7, 0);
        assert_eq!(t.next().await.unwrap(), None);
    }

    #[tokio::test]
    async fn returns_none_when_the_batch_account_exists_but_is_not_finalized_yet() {
        let program_id = Pubkey::new_unique();
        let settlement_program = Pubkey::new_unique();
        let (pda, _) = zk_inbox_client::batch_pda(&program_id, &settlement_program, 7, 0);
        let mut reader = FakeReader::default();
        reader
            .accounts
            .insert(pda, batch_account_bytes(7, 0, 100, 3, false));
        let mut t = SolanaTraversal::new(reader, program_id, settlement_program, 7, 0);
        assert_eq!(t.next().await.unwrap(), None);
    }

    #[tokio::test]
    async fn returns_the_batch_ref_once_finalized_and_advance_moves_to_the_next_id() {
        let program_id = Pubkey::new_unique();
        let settlement_program = Pubkey::new_unique();
        let (pda0, _) = zk_inbox_client::batch_pda(&program_id, &settlement_program, 7, 0);
        let mut reader = FakeReader::default();
        reader
            .accounts
            .insert(pda0, batch_account_bytes(7, 0, 100, 3, true));
        let mut t = SolanaTraversal::new(reader, program_id, settlement_program, 7, 0);

        let got = t.next().await.unwrap().expect("batch 0 is finalized");
        assert_eq!(
            got,
            BatchRef {
                chain_id: 7,
                batch: 0,
                open_slot: 100,
                open_unix_ts: PLAUSIBLE_OPEN_UNIX_TS,
                expected_count: 3,
                root: [0u8; 32],
                forced_root: [0u8; 32],
                acc: [0u8; 32],
            }
        );

        t.advance();
        assert_eq!(t.next_batch(), 1);
        // Batch 1's account was never seeded and no cursor exists — not finalized (indeed, doesn't
        // exist) yet, not an abandoned id.
        assert_eq!(t.next().await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_decode_failure_is_critical_not_temporary() {
        let program_id = Pubkey::new_unique();
        let settlement_program = Pubkey::new_unique();
        let (pda, _) = zk_inbox_client::batch_pda(&program_id, &settlement_program, 7, 0);
        let mut reader = FakeReader::default();
        reader.accounts.insert(pda, vec![0xff; 4]); // too short, bad magic
        let mut t = SolanaTraversal::new(reader, program_id, settlement_program, 7, 0);
        let err = t.next().await.unwrap_err();
        assert!(matches!(err, PipelineError::Critical(_)));
    }

    /// A large `open_slot` gap between two consecutively-finalized batches
    /// is not a Reset condition — both derive. A heuristic that has since been deleted used to raise
    /// `PipelineError::Reset` here, and the pipeline could never make progress past it; the test requires
    /// both batches to derive.
    #[tokio::test]
    async fn a_large_open_slot_gap_between_consecutive_batches_is_not_a_reset() {
        let program_id = Pubkey::new_unique();
        let settlement_program = Pubkey::new_unique();
        let mut reader = FakeReader::default();
        let (pda0, _) = zk_inbox_client::batch_pda(&program_id, &settlement_program, 7, 100);
        let (pda1, _) = zk_inbox_client::batch_pda(&program_id, &settlement_program, 7, 101);
        reader
            .accounts
            .insert(pda0, batch_account_bytes(7, 100, 1_000, 3, true));
        // 5,000 slots later — the exact gap that used to reproduce the livelock.
        reader
            .accounts
            .insert(pda1, batch_account_bytes(7, 101, 6_000, 3, true));

        let mut t = SolanaTraversal::new(reader, program_id, settlement_program, 7, 100);
        let first = t.next().await.unwrap().expect("batch 100 is finalized");
        assert_eq!(first.open_slot, 1_000);
        t.advance();

        let second = t
            .next()
            .await
            .unwrap()
            .expect("batch 101 must also derive, not Reset");
        assert_eq!(second.open_slot, 6_000);
    }

    /// A batch id the batcher abandoned (or closed post-settlement) never gets an account —
    /// once the on-chain `batch_cursor.next_batch` has moved past it, `next` must skip it rather than
    /// wait on it forever, and still find the next real finalized batch beyond it.
    #[tokio::test]
    async fn an_id_the_cursor_has_already_passed_is_skipped_not_waited_on() {
        let program_id = Pubkey::new_unique();
        let settlement_program = Pubkey::new_unique();
        let mut reader = FakeReader::default();
        // Batch 5 was abandoned: no account ever lands at its PDA.
        // Batch 6 finalized normally.
        let (pda6, _) = zk_inbox_client::batch_pda(&program_id, &settlement_program, 7, 6);
        reader
            .accounts
            .insert(pda6, batch_account_bytes(7, 6, 1_000, 2, true));
        // The cursor has moved to 7 (batches 5 and 6 both resolved, one way or another).
        let (cursor_pda, _) = zk_inbox_client::cursor_pda(&program_id, &settlement_program, 7);
        reader
            .accounts
            .insert(cursor_pda, cursor_account_bytes(7, 7));

        let mut t = SolanaTraversal::new(reader, program_id, settlement_program, 7, 5);
        let got = t.next().await.unwrap().expect("must skip 5 and find 6");
        assert_eq!(got.batch, 6);
        assert_eq!(
            t.next_batch(),
            6,
            "cursor position lands on the batch it found"
        );
    }

    /// The companion case: with no cursor account at all yet, a missing batch account is ordinary
    /// "not posted yet" — `next` must not skip ahead speculatively.
    #[tokio::test]
    async fn a_missing_batch_with_no_cursor_account_yet_is_idle_not_skipped() {
        let program_id = Pubkey::new_unique();
        let settlement_program = Pubkey::new_unique();
        let reader = FakeReader::default(); // nothing seeded at all
        let mut t = SolanaTraversal::new(reader, program_id, settlement_program, 7, 5);
        assert_eq!(t.next().await.unwrap(), None);
        assert_eq!(t.next_batch(), 5, "must not have skipped ahead");
    }
}
