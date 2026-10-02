//! `poll_once`: the bin's whole loop body, lifted here so it is testable over the
//! same `Deps`-over-fakes shape [`crate::core::attempt_exit`] and [`crate::follower::Follower`] already
//! use — `src/bin/rome-zk-exit-prover.rs` becomes `loop { poll_once(...); sleep }`, `--once` is one call.
//!
//! Two RPC calls this crate makes are OUTSIDE `Follower`/`attempt_exit`'s own control: `eth_getLogs`
//! itself, and reading the current Solana slot. Before this module, both were handled inline in `main`
//! with the wrong failure mode:
//! - `eth_getLogs`'s `Result` was propagated with `?`, which would abort the WHOLE process on the very
//!   first RPC hiccup — the same class of bug [`crate::follower::Follower::ingest`] already fixed for a
//!   single bad LOG (never the whole call failing).
//! - `get_slot`'s failure was papered over with `.unwrap_or(0)` — silently substituting slot `0` would
//!   make every `Wait::Slot(retry_slot)` message look due immediately, sending a `ProveExit` built for the
//!   WRONG challenge window at a real fee.
//!
//! [`poll_once`] fixes both: an `eth_getLogs` failure counts
//! [`crate::metrics::Metrics::record_rpc_error`] and skips the rest of this poll (the next poll's
//! `scan_from_block` is untouched, so nothing is lost); a `read_slot` failure ALSO skips the rest of this
//! poll — never substitutes a value — so no message is ever attempted against a fabricated slot.
//! `read_root`'s own failure keeps its prior, more permissive behaviour (`head_final_batch` stays `0` for
//! this poll, so only `Wait::Now` messages become due).

use solana_program::pubkey::Pubkey;

use crate::core::{attempt_exit, AttemptParams, CoreError};
use crate::follower::{Follower, IngestReport, Now};
use crate::metrics::Metrics;
use crate::rpc::VerifierRpc;
use crate::settlement::SettlementReader;

/// What one [`poll_once`] call did — for the bin's own logging and for tests to assert against (never
/// consulted by [`poll_once`] itself; it is a report, not state).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PollReport {
    /// `eth_getLogs` itself failed — the poll was skipped entirely (never ingested, never attempted).
    pub logs_error: bool,
    /// [`crate::follower::Follower::ingest`]'s own report, when `eth_getLogs` succeeded.
    pub ingest: IngestReport,
    /// Reading the current slot failed — every due message this poll would otherwise have attempted was
    /// skipped (never substituted slot `0`).
    pub slot_error: bool,
    /// Reading the settlement `root` failed — every due message this poll would otherwise have attempted
    /// was skipped (never substituted `head_final_batch = 0`).
    pub root_error: bool,
    /// How many due messages `attempt_exit` actually ran against, this poll (`0` on either error above).
    pub attempted: usize,
}

/// One full poll: `eth_getLogs` → `ingest` → read the current slot/Final batch → `attempt_exit` on every
/// due message → `apply`. Never propagates an error out — every failure this function can hit is reported
/// in the returned [`PollReport`] (and counted in `metrics`) so the caller's loop never aborts on an RPC
/// hiccup.
#[allow(clippy::too_many_arguments)]
pub async fn poll_once<R, V, S>(
    settlement: &R,
    verifier: &V,
    sender: &S,
    follower: &mut Follower,
    metrics: &Metrics,
    portal_hex: &str,
    program_id: &Pubkey,
    payer: &Pubkey,
    max_tx_bytes: usize,
    tuning: rome_zk_solana_sender::SendTuning,
) -> PollReport
where
    R: SettlementReader,
    V: VerifierRpc,
    S: rome_zk_solana_sender::Sender,
{
    let mut report = PollReport::default();

    let logs = match verifier.eth_get_logs(portal_hex, follower.scan_from_block) {
        Ok(logs) => logs,
        Err(e) => {
            metrics.record_rpc_error("eth_getLogs");
            tracing::warn!(error = %e, "eth_getLogs failed, skipping this poll");
            report.logs_error = true;
            return report;
        }
    };

    let ingest = follower.ingest(&logs);
    metrics.record_ingest(&ingest);
    if ingest.decode_errors > 0 {
        tracing::warn!(
            decode_errors = ingest.decode_errors,
            added = ingest.added,
            duplicates = ingest.duplicates,
            "ExitInitiated logs: some undecodable, skipped and counted"
        );
    }
    report.ingest = ingest;

    let now_slot = match settlement.read_slot() {
        Ok(slot) => slot,
        Err(e) => {
            metrics.record_rpc_error("get_slot");
            tracing::warn!(
                error = %e,
                "get_slot failed, skipping this poll's attempts (never falling back to slot 0)"
            );
            report.slot_error = true;
            return report;
        }
    };
    // A `read_root` failure skips this poll's attempts exactly like `read_slot`'s does:
    // a fabricated `head_final_batch = 0` would make `apply` record `seen_head_final: 0` for everything it
    // parks, re-surfacing it on the very next poll regardless of any real Final root.
    let root = match settlement.read_root() {
        Ok(root) => root,
        Err(e) => {
            metrics.record_rpc_error("read_root");
            tracing::warn!(
                error = %e,
                "read_root failed, skipping this poll's attempts (never falling back to head_final 0)"
            );
            report.root_error = true;
            return report;
        }
    };
    let now = Now {
        slot: now_slot,
        head_final_batch: root.head_final_batch,
    };
    // The window `now_slot` falls in, for the follower's own-send accounting;
    // `attempt_exit` refuses `ChallengeWindowZero` itself when the divisor is 0.
    let window_index = if root.challenge_window_slots == 0 {
        0
    } else {
        now_slot / root.challenge_window_slots as u64
    };

    for hash in follower.due(now) {
        let message = follower
            .pending
            .get(&hash)
            .map(|p| p.message)
            .or_else(|| follower.stuck.get(&hash).map(|s| s.message))
            .expect("due() only returns hashes this follower is tracking");

        let params = AttemptParams {
            program_id: *program_id,
            payer: *payer,
            now_slot,
            max_tx_bytes,
            tuning,
            local_spent_units: follower.sent_units_in(window_index),
        };
        let outcome = attempt_exit(settlement, verifier, sender, message, &params).await;
        match &outcome {
            Ok(outcome) => {
                metrics.record_outcome(outcome);
                tracing::info!(?outcome, nonce = message.nonce, "attempt_exit");
                if let crate::core::Outcome::Refused(refusal) = outcome {
                    tracing::warn!(?refusal, nonce = message.nonce, "exit refused pre-send");
                }
            }
            Err(e) => {
                let kind = match e {
                    CoreError::Read(_) => "settlement",
                    CoreError::Verifier(_) => "verifier",
                    CoreError::Build(_) => "build",
                };
                metrics.record_read_error(kind);
                tracing::warn!(error = %e, nonce = message.nonce, "attempt_exit error, will retry");
            }
        }
        follower.apply(hash, now, outcome);
        report.attempted += 1;
    }

    metrics.record_follower(follower);
    report
}
