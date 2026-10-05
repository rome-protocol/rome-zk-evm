//! The stateful follower: the CHAIN is this crate's own state (the
//! `exit_nullifier` bit `ProveExit` sets, read pre-send by [`crate::core::attempt_exit`]) — this module
//! is only the in-process CACHE a restart rebuilds for free from `eth_getLogs(portal,
//! portal_from_block)`, never a disk cursor or a database. Mirrors `rome-zk-prover::follower`'s own
//! "cursor derived from chain state, nothing on disk" shape (`follower.rs:231-235`'s `batches_behind`),
//! applied here to exits instead of batches.
//!
//! ## Why this exists (the bug it replaces)
//! Before this module, `src/bin/rome-zk-exit-prover.rs` advanced its one `from_block` cursor for EVERY
//! log BEFORE looking at [`crate::core::attempt_exit`]'s outcome (`:90` ran ahead of `:106-124`) — a
//! `StateUnavailableAtRoot`/`QueuedForWindow` message was then never retried: the next poll's
//! `eth_getLogs(from_block)` no longer returns a log the cursor has already passed. [`Follower`] fixes
//! this by separating three concerns the old loop conflated into one mutable local: the log-scan cursor
//! ([`Follower::scan_from_block`], advanced by [`Follower::ingest`] and [`Follower::mark_scanned_through`] alone, independent of any outcome),
//! the retry state of every message not yet done ([`Follower::pending`], mutated only by
//! [`Follower::apply`]), and the terminal-but-not-yet-abandoned messages ([`Follower::stuck`]).
//!
//! ## The wire loop (bin becomes wire-only)
//! `follower.ingest(eth_get_logs(portal, follower.scan_from_block, chunk_end))` (one bounded chunk at a time, up to
//! the node's head) → for each hash in
//! `follower.due(now)` → `attempt_exit(...)` → `follower.apply(hash, now, outcome)`. `--once` is one such
//! pass.
//!
//! ## State machine
//! A message starts [`Wait::Now`] (attempt on the very next `due` call). [`crate::core::attempt_exit`]'s
//! outcome routes it, via [`Follower::apply`] (exhaustive over every [`Refusal`] variant — no catch-all):
//! - `Sent`/`AlreadyProved` — done; removed from every map.
//! - `QueuedForWindow { retry_slot, .. }` — [`Wait::Slot`]: due again once
//!   `now.slot >= retry_slot`. Bounded by `max_window_requeues`, same shape as
//!   `send_attempts`/`max_send_attempts` below; past the bound, [`StuckReason::MaxWindowRequeues`].
//! - `Refused(ProofTooLarge { .. })` — moves straight to [`stuck`](Follower::stuck) with
//!   [`StuckReason::ProofTooLarge`], recording the head-Final batch at the time; only a staged proof
//!   path can truly free it, but it is re-tried the moment `head_final_batch` advances (in case a
//!   shrinking trie brings a later proof back under the V1 envelope) — cheap, no fee, and never on every
//!   poll in between (that would just re-measure the same doomed proof against the same root).
//! - `Refused(ExceedsWindowCap { .. })` (this message's own amount exceeds the WHOLE
//!   per-window cap, checked in `core::attempt_exit` before any read of a specific window) — also straight
//!   to `stuck`, [`StuckReason::ExceedsWindowCap`]; re-tried once `head_final_batch` advances, since the
//!   cap is governance-changeable.
//! - `Refused(Verify(RootMismatch | ProofInvalid(_)))` (the proof itself failed against the CURRENT root) —
//!   `stuck` with [`StuckReason::ProofInvalid`], distinct from the merely-transient refusals below because a
//!   failed verify is worth surfacing on its own; still re-tried (zero fee) once `head_final_batch` advances.
//! - `Refused(Verify(ExitNotSent))` is transient: the message stays in `pending` with [`Wait::NextFinal`]
//!   (an exclusion proof at an older Final root is the ordinary first outcome for a fresh exit).
//! - `Refused(StateUnavailableAtRoot | NoFinalBatch | ExitConfigUnset | ExitCapUnset | ChallengeWindowZero)`
//!   — TRANSIENT: [`Wait::NextFinal`], recording the current `head_final_batch`; due again once a LATER
//!   Final root exists (a message initiated after the current Final batch's own last block is legitimately
//!   unavailable there — not a bug). Kept in `pending`, not `stuck` — these are waiting on
//!   ordinary chain/config state, not on a failed proof.
//! - `SendFailed(_)` — a send was attempted (a fee may have been spent): `send_attempts` increments and the
//!   message stays [`Wait::Now`] until `max_send_attempts`, then moves to [`stuck`](Follower::stuck) with
//!   [`StuckReason::MaxSendAttempts`] (bounded — an unreachable verifier/RPC or a persistent send failure
//!   does not retry forever).
//! - `Err(_)` — `attempt_exit` returning an error at all means the `Sender` was NEVER
//!   touched (`CoreError::{Read,Verifier,Build}` all originate before the send call): a read failure is
//!   FREE. `send_attempts` is left untouched and the message stays `Wait::Now`, never `stuck` on this
//!   alone — distinct from `SendFailed(_)`, which means a fee-spending send was actually attempted.
//!
//! An undecodable log is counted ([`IngestReport::decode_errors`]) and skipped, never `?`'d out — the
//! loop surviving a malformed or foreign log is the other half of the bug this module closes (the old
//! bin's `decode_exit_initiated(log)?` aborted `main` on the very first bad log).

use std::collections::BTreeMap;

use rome_zk_layouts::exit::ExitMessage;

use crate::core::{CoreError, Outcome, Refusal};
use crate::proof::VerifyRefusal;
use crate::rpc::LogEntry;

/// When a pending message is next eligible for an attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// Attempt on the very next [`Follower::due`] call.
    Now,
    /// Due once `head_final_batch` has advanced past the value recorded here — every transient refusal
    /// (a Final root too old to serve this message's proof, or a root not yet configured/capped).
    NextFinal { seen_head_final: u64 },
    /// Due once the current slot reaches `retry_slot` (`QueuedForWindow`).
    Slot(u64),
}

/// One message not yet done, and its retry state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub message: ExitMessage,
    /// The `eth_getLogs` block this message's `ExitInitiated` log was seen at — provenance only (never
    /// read by `due`/`apply`; kept for diagnostics/metrics).
    pub seen_block: u64,
    pub wait: Wait,
    /// How many `SendFailed` outcomes this message has accumulated — reset never (a message that starts
    /// failing does not get a fresh budget just because a `StateUnavailableAtRoot` interleaves). A read
    /// failure (`Err(_)` — the `Sender` was never touched) does NOT increment this; only
    /// an actual send attempt does (see [`Follower::apply`]).
    pub send_attempts: u32,
    /// How many `QueuedForWindow` outcomes this message has accumulated — bounded by
    /// `Follower::max_window_requeues`, same shape as `send_attempts`/`max_send_attempts`. Reset never.
    pub window_requeues: u32,
}

/// Why a message left [`Follower::pending`] for [`Follower::stuck`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StuckReason {
    /// `send_attempts` reached the configured `max_send_attempts` — a persistent send failure, never
    /// retried automatically again.
    MaxSendAttempts,
    /// The projected `ProveExit` transaction exceeded the V1 envelope — re-tried only once
    /// `head_final_batch` advances past the value recorded here (a later, possibly shrunk trie may fit;
    /// otherwise a human or a staged-proof path is needed).
    ProofTooLarge { seen_head_final: u64 },
    /// `window_requeues` reached the configured `max_window_requeues` — a persistently over-subscribed
    /// window. NOT terminal: a window re-queue is a TIME
    /// condition that clears by itself, so the message is re-tried (zero fee — the pre-send window check
    /// refuses locally) once `head_final_batch` advances past the value recorded here; the bound exists
    /// to raise `rome_zk_exit_stuck{reason="max_window_requeues"}`, never to abandon a valid exit.
    MaxWindowRequeues { seen_head_final: u64 },
    /// `message.asset != [0; 20]` — v1 is native-asset only (`programs/zk-settlement::exit::prove_exit`
    /// refuses `UnsupportedAsset` on chain). Nothing on chain can ever change this message, so it is
    /// TERMINAL: never due again (the local mirror of that on-chain gate).
    UnsupportedAsset,
    /// This message's own amount exceeds the WHOLE per-window cap (`Refusal::ExceedsWindowCap`)
    /// — no window under the CURRENT cap could ever admit it. Re-tried (zero fee) only
    /// once `head_final_batch` advances past the value recorded here, since the cap is
    /// governance-changeable (a later root may raise it).
    ExceedsWindowCap { seen_head_final: u64 },
    /// The local MPT re-verify refused with `RootMismatch` or `ProofInvalid(_)` —
    /// the proof itself failed against the CURRENT root, worth surfacing separately from "just waiting on
    /// infra". Still re-tried (zero fee) once `head_final_batch` advances past the value recorded here.
    /// `Verify(ExitNotSent)` is NOT this: an exclusion proof at a Final root older than the message is
    /// the ORDINARY first outcome of every fresh exit (`Verify(Absent)` is TRANSIENT) and
    /// stays `pending`/`Wait::NextFinal`.
    ProofInvalid { seen_head_final: u64 },
}

/// One message parked as stuck, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stuck {
    pub message: ExitMessage,
    pub seen_block: u64,
    pub reason: StuckReason,
    /// Carried across the `pending → stuck → pending` round trip: a message
    /// returning from `stuck` keeps its budget, never gets a fresh one per Final root.
    pub send_attempts: u32,
    pub window_requeues: u32,
}

/// What one [`Follower::ingest`] call did with the logs it was given — an undecodable log is counted
/// here, never propagated as an error (the loop must survive a malformed/foreign log).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IngestReport {
    /// New messages added to `pending`.
    pub added: u32,
    /// Logs that decoded to a message hash already known (in `pending` or `stuck`) — a message seen in
    /// two overlapping polls is attempted only once.
    pub duplicates: u32,
    /// Logs that failed to decode as `ExitInitiated` — skipped, the loop continues.
    pub decode_errors: u32,
}

/// The live chain facts [`Follower::due`]/[`Follower::apply`] compare `Wait`/`Stuck` state against —
/// read fresh every poll by the bin, never cached inside [`Follower`] itself (the follower's own state is
/// only ever what messages are waiting and on what, never a copy of chain state it could go stale
/// against).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Now {
    pub slot: u64,
    pub head_final_batch: u64,
}

/// The stateful follower: rebuilt from `eth_getLogs(portal, portal_from_block)` on
/// every restart (`scan_from_block` starts at the configured `portal_from_block`, never a persisted
/// cursor) — no disk, no Postgres (a persisted cursor was the rejected alternative).
#[derive(Debug, Clone, Default)]
pub struct Follower {
    pub scan_from_block: u64,
    /// The block a scan of a new portal starts from (`Config::portal_from_block`).
    portal_from_block: u64,
    /// The portal (`0x`-prefixed hex) the cursor and the messages below belong to; `None` until the first poll.
    portal: Option<String>,
    pub pending: BTreeMap<[u8; 32], Pending>,
    pub stuck: BTreeMap<[u8; 32], Stuck>,
    /// `Config::max_send_attempts`, carried on the follower itself so `apply`
    /// needs no extra argument beyond the outcome and the live chain facts.
    max_send_attempts: u32,
    /// `Config::max_window_requeues` — bounds `Pending::window_requeues` the same way
    /// `max_send_attempts` bounds `Pending::send_attempts`.
    max_window_requeues: u32,
    /// `window_index → cap units` of THIS process's own confirmed `Sent` outcomes: the settlement reader
    /// serves `exit_window` at `finalized`, so a send confirmed seconds ago is invisible to the next
    /// attempt's pre-send window check for the cluster's confirmed-to-finalized gap. This map is the local
    /// fact the follower already has; `run::poll_once` passes `sent_units_in(window)` into
    /// `AttemptParams::local_spent_units` and `core::attempt_exit` takes the max of the two. Pruned to
    /// the current and previous window on every `Sent`. Not chain state — a cache of our own sends, as
    /// `pending`/`stuck` are; a restart loses it and the on-chain `ExitCapExceeded` (68) still catches
    /// the race, as before.
    pub sent_units: BTreeMap<u64, u64>,
}

impl Follower {
    pub fn new(portal_from_block: u64, max_send_attempts: u32, max_window_requeues: u32) -> Self {
        Self {
            scan_from_block: portal_from_block,
            portal_from_block,
            portal: None,
            pending: BTreeMap::new(),
            stuck: BTreeMap::new(),
            max_send_attempts,
            max_window_requeues,
            sent_units: BTreeMap::new(),
        }
    }

    /// Cap units this process itself has already sent (and seen confirmed) against `window_index` —
    /// see [`Follower::sent_units`].
    pub fn sent_units_in(&self, window_index: u64) -> u64 {
        self.sent_units.get(&window_index).copied().unwrap_or(0)
    }

    /// Tells the follower which portal the chain's exit config names right now. The first call only remembers it.
    /// A later call with a different portal starts over: the cursor goes back to the configured start block and the
    /// waiting and stuck messages (all from the old portal) are dropped, so the new portal's logs are scanned from the
    /// start. Returns whether it reset. The record of this process's own sends in each window is kept, since the cap
    /// is the chain's and does not depend on the portal.
    pub fn follow_portal(&mut self, portal_hex: &str) -> bool {
        match &self.portal {
            Some(current) if current == portal_hex => false,
            Some(_) => {
                self.portal = Some(portal_hex.to_string());
                self.scan_from_block = self.portal_from_block;
                self.pending.clear();
                self.stuck.clear();
                true
            }
            None => {
                self.portal = Some(portal_hex.to_string());
                false
            }
        }
    }

    /// Records that every block up to and including `to_block` has been scanned, so the next scan starts one
    /// past it. A chunk with no logs in it still moves the cursor: without this the cursor moved only past the
    /// last log found, and a quiet stretch was asked for again on every poll.
    pub fn mark_scanned_through(&mut self, to_block: u64) {
        self.scan_from_block = self.scan_from_block.max(to_block.saturating_add(1));
    }

    /// Ingests every log an `eth_getLogs(portal, scan_from_block, ..)` call returned. `scan_from_block`
    /// advances over EVERY log given here (`max(block_number + 1)`) regardless of whether it decoded —
    /// a bad log's own block is still scanned, so the next poll never re-fetches it. A log whose own
    /// `blockNumber` field is not valid hex (never observed from a real node; only a foreign/corrupt
    /// response could produce it) does not advance the cursor for that one log, but is still counted as
    /// a decode error and does not stop the loop.
    pub fn ingest(&mut self, logs: &[LogEntry]) -> IngestReport {
        let mut report = IngestReport::default();
        for log in logs {
            if let Ok(block_number) =
                u64::from_str_radix(log.block_number.trim_start_matches("0x"), 16)
            {
                self.scan_from_block = self.scan_from_block.max(block_number + 1);
            }
            match crate::logs::decode_exit_initiated(log) {
                Ok((message, _log_index, block_number)) => {
                    let hash = message.message_hash();
                    if self.pending.contains_key(&hash) || self.stuck.contains_key(&hash) {
                        report.duplicates += 1;
                    } else {
                        self.pending.insert(
                            hash,
                            Pending {
                                message,
                                seen_block: block_number,
                                wait: Wait::Now,
                                send_attempts: 0,
                                window_requeues: 0,
                            },
                        );
                        report.added += 1;
                    }
                }
                Err(_) => {
                    report.decode_errors += 1;
                }
            }
        }
        report
    }

    /// Every message hash due for an attempt right now: every `pending` entry whose `wait` has come due,
    /// plus every `stuck` entry whose reason carries a `seen_head_final` the current one has advanced past
    /// (`ProofTooLarge`, `ExceedsWindowCap`, `ProofInvalid`, `MaxWindowRequeues` — each re-tried, fee-free,
    /// in case a later root/shrunk trie/raised cap/emptier window now clears it). Only `MaxSendAttempts`
    /// (fees were spent on a persistent failure) and `UnsupportedAsset` (nothing on chain can ever change
    /// it) are terminal until a human/restart clears them.
    pub fn due(&self, now: Now) -> Vec<[u8; 32]> {
        let mut out = Vec::new();
        for (hash, p) in &self.pending {
            let is_due = match p.wait {
                Wait::Now => true,
                Wait::NextFinal { seen_head_final } => now.head_final_batch > seen_head_final,
                Wait::Slot(retry_slot) => now.slot >= retry_slot,
            };
            if is_due {
                out.push(*hash);
            }
        }
        for (hash, s) in &self.stuck {
            let seen_head_final = match s.reason {
                StuckReason::ProofTooLarge { seen_head_final }
                | StuckReason::ExceedsWindowCap { seen_head_final }
                | StuckReason::ProofInvalid { seen_head_final } => Some(seen_head_final),
                // A window re-queue is a time condition — the bound alarms, the
                // message is re-tried (fee-free: the window pre-check refuses locally) on the next Final root.
                StuckReason::MaxWindowRequeues { seen_head_final } => Some(seen_head_final),
                StuckReason::MaxSendAttempts | StuckReason::UnsupportedAsset => None,
            };
            if let Some(seen_head_final) = seen_head_final {
                if now.head_final_batch > seen_head_final {
                    out.push(*hash);
                }
            }
        }
        out
    }

    /// Routes one [`crate::core::attempt_exit`] outcome for `hash` (see the module doc's
    /// state-machine list). `hash` must have come from a prior [`Follower::due`] call (a hash unknown to
    /// both maps is ignored — nothing to route). Exhaustively matches every [`Refusal`] variant (no
    /// catch-all `_` arm) so a newly-added refusal fails to compile here until it is
    /// deliberately classified, rather than silently falling into whatever the wildcard used to mean.
    pub fn apply(&mut self, hash: [u8; 32], now: Now, result: Result<Outcome, CoreError>) {
        let (message, seen_block, send_attempts, window_requeues) =
            if let Some(p) = self.pending.remove(&hash) {
                (p.message, p.seen_block, p.send_attempts, p.window_requeues)
            } else if let Some(s) = self.stuck.remove(&hash) {
                (s.message, s.seen_block, s.send_attempts, s.window_requeues)
            } else {
                return;
            };

        match result {
            Ok(Outcome::Sent {
                window_index,
                units,
            }) => {
                // Done — already removed from both maps above. Account our own confirmed send against
                // its window and prune anything older than the previous window.
                *self.sent_units.entry(window_index).or_insert(0) += units;
                let keep_from = window_index.saturating_sub(1);
                self.sent_units.retain(|w, _| *w >= keep_from);
            }
            Ok(Outcome::AlreadyProved) => {
                // Done — already removed from both maps above.
            }
            Ok(Outcome::QueuedForWindow { retry_slot, .. }) => {
                let window_requeues = window_requeues + 1;
                if window_requeues >= self.max_window_requeues {
                    self.stuck.insert(
                        hash,
                        Stuck {
                            message,
                            seen_block,
                            reason: StuckReason::MaxWindowRequeues {
                                seen_head_final: now.head_final_batch,
                            },
                            send_attempts,
                            window_requeues,
                        },
                    );
                } else {
                    self.pending.insert(
                        hash,
                        Pending {
                            message,
                            seen_block,
                            wait: Wait::Slot(retry_slot),
                            send_attempts,
                            window_requeues,
                        },
                    );
                }
            }
            Ok(Outcome::Refused(refusal)) => match refusal {
                Refusal::ProofTooLarge { .. } => {
                    self.stuck.insert(
                        hash,
                        Stuck {
                            message,
                            seen_block,
                            reason: StuckReason::ProofTooLarge {
                                seen_head_final: now.head_final_batch,
                            },
                            send_attempts,
                            window_requeues,
                        },
                    );
                }
                Refusal::ExceedsWindowCap { .. } => {
                    self.stuck.insert(
                        hash,
                        Stuck {
                            message,
                            seen_block,
                            reason: StuckReason::ExceedsWindowCap {
                                seen_head_final: now.head_final_batch,
                            },
                            send_attempts,
                            window_requeues,
                        },
                    );
                }
                Refusal::UnsupportedAsset => {
                    self.stuck.insert(
                        hash,
                        Stuck {
                            message,
                            seen_block,
                            reason: StuckReason::UnsupportedAsset,
                            send_attempts,
                            window_requeues,
                        },
                    );
                }
                Refusal::Verify(VerifyRefusal::ExitNotSent) => {
                    // `Verify(Absent)` is TRANSIENT — the exclusion proof every fresh exit
                    // gets at a Final root older than the message.
                    self.pending.insert(
                        hash,
                        Pending {
                            message,
                            seen_block,
                            wait: Wait::NextFinal {
                                seen_head_final: now.head_final_batch,
                            },
                            send_attempts,
                            window_requeues,
                        },
                    );
                }
                Refusal::Verify(VerifyRefusal::RootMismatch | VerifyRefusal::ProofInvalid(_)) => {
                    self.stuck.insert(
                        hash,
                        Stuck {
                            message,
                            seen_block,
                            reason: StuckReason::ProofInvalid {
                                seen_head_final: now.head_final_batch,
                            },
                            send_attempts,
                            window_requeues,
                        },
                    );
                }
                Refusal::StateUnavailableAtRoot { .. }
                | Refusal::NoFinalBatch
                | Refusal::ExitConfigUnset
                | Refusal::ExitCapUnset
                | Refusal::ChallengeWindowZero => {
                    self.pending.insert(
                        hash,
                        Pending {
                            message,
                            seen_block,
                            wait: Wait::NextFinal {
                                seen_head_final: now.head_final_batch,
                            },
                            send_attempts,
                            window_requeues,
                        },
                    );
                }
            },
            Ok(Outcome::SendFailed(_)) => {
                let send_attempts = send_attempts + 1;
                if send_attempts >= self.max_send_attempts {
                    self.stuck.insert(
                        hash,
                        Stuck {
                            message,
                            seen_block,
                            reason: StuckReason::MaxSendAttempts,
                            send_attempts,
                            window_requeues,
                        },
                    );
                } else {
                    self.pending.insert(
                        hash,
                        Pending {
                            message,
                            seen_block,
                            wait: Wait::Now,
                            send_attempts,
                            window_requeues,
                        },
                    );
                }
            }
            Err(_) => {
                // `attempt_exit` returning `Err(_)` means the `Sender` was NEVER touched
                // (`CoreError::{Read,Verifier,Build}` all originate before the send call in `core.rs`) — a
                // read failure is free. `send_attempts` is untouched and the message stays `Wait::Now`
                // (never `stuck`); the caller (`run::poll_once`) records
                // `rome_zk_exit_read_errors_total{kind}` for observability, which this pure state machine
                // does not depend on.
                self.pending.insert(
                    hash,
                    Pending {
                        message,
                        seen_block,
                        wait: Wait::Now,
                        send_attempts,
                        window_requeues,
                    },
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Pure `Follower` state-machine tests (no `attempt_exit`, no fakes beyond hand-built
    //! `Outcome`/`CoreError` values) — `tests/follower.rs` covers the full
    //! `ingest`/`due`/`attempt_exit`/`apply` wire loop against scripted `SettlementReader`/`VerifierRpc`/
    //! `Sender` fakes, including the pre-send nullifier check in `crate::core::attempt_exit` itself.
    use super::*;
    use crate::settlement::ReadError;

    fn msg(nonce: u64) -> ExitMessage {
        ExitMessage {
            nonce,
            l2_sender: [0x11; 20],
            sol_recipient: [0x22; 32],
            asset: [0u8; 20],
            amount: 1_000,
        }
    }

    /// A well-formed `ExitInitiated` log for `message` — `message_preimage()` is already the exact
    /// 160-byte, five-32-byte-word ABI encoding `decode_exit_initiated` parses (the same
    /// bytes both sides hash), so it is used directly as the log's `data`.
    fn log_for(message: &ExitMessage, block_number: u64, log_index: u64) -> LogEntry {
        LogEntry {
            address: "0x0000000000000000000000000000000000000000".to_string(),
            topics: vec![
                "0x0000000000000000000000000000000000000000000000000000000000000000".to_string(),
                format!("0x{}", hex::encode(message.message_hash())),
            ],
            data: format!("0x{}", hex::encode(message.message_preimage())),
            block_number: format!("0x{block_number:x}"),
            transaction_hash: "0x00".to_string(),
            log_index: format!("0x{log_index:x}"),
        }
    }

    fn now(slot: u64, head_final_batch: u64) -> Now {
        Now {
            slot,
            head_final_batch,
        }
    }

    // Mutation target: `ingest`'s `Err(_)` arm returning early (restoring the old bin's `?`) instead of counting
    // and continuing — this test then fails because the second, valid log is never added.
    #[test]
    fn an_undecodable_log_is_counted_and_the_loop_continues() {
        let good = msg(1);
        let mut bad_log = log_for(&good, 10, 0);
        bad_log.topics.push("0x00".to_string()); // now 3 topics -> WrongTopicCount
        let good_log = log_for(&good, 11, 0);

        let mut f = Follower::new(0, 5, 3);
        let report = f.ingest(&[bad_log, good_log]);

        assert_eq!(report.decode_errors, 1);
        assert_eq!(report.added, 1);
        assert_eq!(report.duplicates, 0);
        assert_eq!(
            f.pending.len(),
            1,
            "the valid log after the bad one must still be added"
        );
        assert!(f.pending.contains_key(&good.message_hash()));
        assert_eq!(
            f.scan_from_block, 12,
            "the cursor must advance past BOTH logs' blocks, including the undecodable one's"
        );
    }

    #[test]
    fn a_message_seen_in_two_polls_is_attempted_once() {
        let m = msg(2);
        let log = log_for(&m, 20, 0);

        let mut f = Follower::new(0, 5, 3);
        let r1 = f.ingest(std::slice::from_ref(&log));
        assert_eq!(r1.added, 1);
        assert_eq!(r1.duplicates, 0);

        // A second poll whose `fromBlock` overlapped and returned the same log again.
        let r2 = f.ingest(&[log]);
        assert_eq!(r2.added, 0);
        assert_eq!(r2.duplicates, 1);
        assert_eq!(f.pending.len(), 1);

        let due = f.due(now(0, 0));
        assert_eq!(
            due,
            vec![m.message_hash()],
            "attempted exactly once, not twice"
        );
    }

    // Mutation target: remove the `pending.insert` in the transient-refusal arm of `apply`
    // (the exact bug this module replaces — a message dropped instead of kept for the next Final root).
    #[test]
    fn state_unavailable_keeps_the_message_and_retries_when_head_final_advances() {
        let m = msg(3);
        let hash = m.message_hash();
        let mut f = Follower::new(0, 5, 3);
        f.ingest(&[log_for(&m, 30, 0)]);

        let n1 = now(100, 5);
        assert_eq!(f.due(n1), vec![hash]);
        f.apply(
            hash,
            n1,
            Ok(Outcome::Refused(Refusal::StateUnavailableAtRoot {
                number: 999,
            })),
        );
        assert!(
            f.pending.contains_key(&hash),
            "a transient refusal must keep the message, never drop it"
        );
        assert!(f.stuck.is_empty());

        // Same Final root: not yet due again.
        assert!(f.due(now(101, 5)).is_empty());
        // A LATER Final root: due again.
        assert_eq!(f.due(now(101, 6)), vec![hash]);
    }

    // `Err(CoreError::Read(_))` (and, by the same reasoning, `Verifier`/`Build`) means `attempt_exit` never
    // reached the `Sender` — ten consecutive read errors must leave the message pending with
    // `send_attempts == 0`, never stuck. Folding `Err(_)` back into the `SendFailed(_)` arm makes this test
    // fail once `max_send_attempts` (5, well under 10) is reached: the message would go `stuck` after 5, not
    // stay pending through all 10.
    #[test]
    fn a_read_error_never_counts_as_a_send_attempt() {
        let m = msg(8);
        let hash = m.message_hash();
        let mut f = Follower::new(0, 5, 3);
        f.ingest(&[log_for(&m, 80, 0)]);

        for i in 0..10 {
            assert_eq!(f.due(now(0, 0)), vec![hash], "attempt {i}");
            f.apply(
                hash,
                now(0, 0),
                Err(CoreError::Read(ReadError::Rpc(
                    "connection reset".to_string(),
                ))),
            );
            assert!(
                f.pending.contains_key(&hash),
                "a read error must never move the message to stuck, attempt {i}"
            );
            assert_eq!(
                f.pending.get(&hash).map(|p| p.send_attempts),
                Some(0),
                "a read error must never increment send_attempts, attempt {i}"
            );
            assert!(f.stuck.is_empty(), "attempt {i}");
        }
    }

    // Every `Verify(_)` refusal was once routed to `stuck`/`StuckReason::ProofInvalid`; `ExitNotSent` was later
    // moved back to `pending`, since an exclusion proof at an older Final root is the ordinary first outcome.
    // This test keeps its original name and pins that.
    #[test]
    fn verify_absent_is_transient_until_the_next_final_root() {
        // `Verify(ExitNotSent)` IS the `Verify(Absent)` case — the exclusion proof every fresh exit gets at a
        // Final root older than the message. It stays PENDING on `Wait::NextFinal`, never `stuck`, so the
        // healthy path never raises `rome_zk_exit_stuck{reason="proof_invalid"}`.
        let m = msg(4);
        let hash = m.message_hash();
        let mut f = Follower::new(0, 5, 3);
        f.ingest(&[log_for(&m, 40, 0)]);

        let n1 = now(0, 10);
        f.apply(
            hash,
            n1,
            Ok(Outcome::Refused(Refusal::Verify(
                VerifyRefusal::ExitNotSent,
            ))),
        );
        assert!(
            f.stuck.is_empty(),
            "an exclusion proof at an older Final root is the ORDINARY first outcome — never stuck"
        );
        assert_eq!(
            f.pending.get(&hash).map(|p| p.wait),
            Some(Wait::NextFinal {
                seen_head_final: 10
            })
        );
        assert!(
            f.due(now(0, 10)).is_empty(),
            "same Final root: an absent-at-this-root proof is not yet worth retrying"
        );
        assert_eq!(f.due(now(0, 11)), vec![hash]);
    }

    // The bound alarms, it never abandons a valid exit. Mutation target: `due()` returning `None` for
    // `MaxWindowRequeues` (terminal) — this test then fails on the last assertion.
    #[test]
    fn max_window_requeues_is_retried_when_head_final_advances() {
        let m = msg(19);
        let hash = m.message_hash();
        let mut f = Follower::new(0, 5, 1);
        f.ingest(&[log_for(&m, 190, 0)]);
        f.apply(
            hash,
            now(0, 7),
            Ok(Outcome::QueuedForWindow {
                next_index: 1,
                retry_slot: 100,
            }),
        );
        assert!(matches!(
            f.stuck.get(&hash).map(|s| s.reason),
            Some(StuckReason::MaxWindowRequeues { seen_head_final: 7 })
        ));
        assert!(
            f.due(now(u64::MAX, 7)).is_empty(),
            "same Final root: not yet"
        );
        assert_eq!(
            f.due(now(0, 8)),
            vec![hash],
            "a later Final root re-surfaces a max-requeued exit (zero fee: the window pre-check refuses locally)"
        );
    }

    // Mutation target: drop the `sent_units` accumulation in `apply`'s `Sent` arm.
    #[test]
    fn a_sent_outcome_accounts_its_units_against_the_window() {
        let a = msg(21);
        let b = msg(22);
        let mut f = Follower::new(0, 5, 3);
        f.ingest(&[log_for(&a, 210, 0), log_for(&b, 210, 1)]);
        assert_eq!(f.sent_units_in(4), 0);
        f.apply(
            a.message_hash(),
            now(450, 1),
            Ok(Outcome::Sent {
                window_index: 4,
                units: 600,
            }),
        );
        assert_eq!(f.sent_units_in(4), 600);
        f.apply(
            b.message_hash(),
            now(451, 1),
            Ok(Outcome::Sent {
                window_index: 4,
                units: 300,
            }),
        );
        assert_eq!(f.sent_units_in(4), 900, "own sends accumulate per window");
        assert_eq!(f.sent_units_in(5), 0, "a different window is untouched");
        assert!(f.pending.is_empty() && f.stuck.is_empty());
    }

    // Mutation target: `apply` re-reading a stuck entry with `(…, 0, 0)` counters.
    #[test]
    fn a_message_returning_from_stuck_keeps_its_counters() {
        let m = msg(23);
        let hash = m.message_hash();
        let mut f = Follower::new(0, 5, 3);
        f.ingest(&[log_for(&m, 230, 0)]);
        // Two fee-paying send failures, then a ProofTooLarge parks it stuck.
        for _ in 0..2 {
            f.apply(hash, now(0, 1), Ok(Outcome::SendFailed("x".into())));
        }
        f.apply(
            hash,
            now(0, 1),
            Ok(Outcome::Refused(Refusal::ProofTooLarge {
                len: 5000,
                max: 4096,
            })),
        );
        assert_eq!(f.stuck.get(&hash).map(|s| s.send_attempts), Some(2));
        // Final root advances → due → another send failure: the count continues at 3, not 1.
        assert_eq!(f.due(now(0, 2)), vec![hash]);
        f.apply(hash, now(0, 2), Ok(Outcome::SendFailed("x".into())));
        assert_eq!(
            f.pending.get(&hash).map(|p| p.send_attempts),
            Some(3),
            "the send budget is carried across the stuck round trip, never reset"
        );
    }

    #[test]
    fn an_unsupported_asset_is_stuck_for_good() {
        let m = msg(24);
        let hash = m.message_hash();
        let mut f = Follower::new(0, 5, 3);
        f.ingest(&[log_for(&m, 240, 0)]);
        f.apply(
            hash,
            now(0, 1),
            Ok(Outcome::Refused(Refusal::UnsupportedAsset)),
        );
        assert!(matches!(
            f.stuck.get(&hash).map(|s| s.reason),
            Some(StuckReason::UnsupportedAsset)
        ));
        assert!(
            f.due(now(u64::MAX, u64::MAX)).is_empty(),
            "nothing on chain can ever make a non-native asset provable in v1"
        );
    }

    // Mutation target: restore a
    // catch-all `_` arm in `apply`'s `Refusal` match (folding `Verify(_)` back into the transient
    // `Wait::NextFinal` bucket) — this test then fails on `f.stuck` being empty.
    #[test]
    fn a_root_mismatch_is_stuck_not_transient() {
        let m = msg(40);
        let hash = m.message_hash();
        let mut f = Follower::new(0, 5, 3);
        f.ingest(&[log_for(&m, 400, 0)]);

        let n1 = now(0, 20);
        f.apply(
            hash,
            n1,
            Ok(Outcome::Refused(Refusal::Verify(
                VerifyRefusal::RootMismatch,
            ))),
        );
        assert!(
            !f.pending.contains_key(&hash),
            "a failed local verify is stuck, not merely transient-pending"
        );
        assert!(matches!(
            f.stuck.get(&hash).map(|s| s.reason),
            Some(StuckReason::ProofInvalid {
                seen_head_final: 20
            })
        ));
        assert!(f.due(now(0, 20)).is_empty());
        assert_eq!(f.due(now(0, 21)), vec![hash]);
    }

    #[test]
    fn queued_for_window_waits_for_retry_slot() {
        let m = msg(5);
        let hash = m.message_hash();
        let mut f = Follower::new(0, 5, 3);
        f.ingest(&[log_for(&m, 50, 0)]);

        f.apply(
            hash,
            now(200, 1),
            Ok(Outcome::QueuedForWindow {
                next_index: 3,
                retry_slot: 300,
            }),
        );
        assert!(f.pending.contains_key(&hash));
        assert!(f.due(now(299, 1)).is_empty());
        assert_eq!(f.due(now(300, 1)), vec![hash]);
    }

    // Mutation target: never increment
    // `window_requeues` in `apply`'s `QueuedForWindow` arm — this test then fails: the fourth
    // `QueuedForWindow` never reaches `stuck` (it would requeue forever).
    #[test]
    fn window_requeues_are_bounded_then_stuck() {
        let m = msg(9);
        let hash = m.message_hash();
        let mut f = Follower::new(0, 5, 3);
        f.ingest(&[log_for(&m, 90, 0)]);

        for attempt in 1..3 {
            f.apply(
                hash,
                now(0, 0),
                Ok(Outcome::QueuedForWindow {
                    next_index: attempt as u64,
                    retry_slot: attempt as u64 * 100,
                }),
            );
            assert_eq!(
                f.pending.get(&hash).map(|p| p.window_requeues),
                Some(attempt),
                "window_requeues must accumulate, attempt {attempt}"
            );
            assert!(f.stuck.is_empty(), "not stuck before max_window_requeues");
        }
        // Third requeue reaches max_window_requeues (3) -> stuck.
        f.apply(
            hash,
            now(0, 0),
            Ok(Outcome::QueuedForWindow {
                next_index: 3,
                retry_slot: 300,
            }),
        );
        assert!(!f.pending.contains_key(&hash));
        assert!(matches!(
            f.stuck.get(&hash).map(|s| s.reason),
            Some(StuckReason::MaxWindowRequeues { seen_head_final: 0 })
        ));
        assert!(
            f.due(now(u64::MAX, 0)).is_empty(),
            "the bound holds while the Final root is unchanged — no per-window retry any more"
        );
    }

    #[test]
    fn send_failed_becomes_stuck_after_max_attempts() {
        let m = msg(6);
        let hash = m.message_hash();
        let mut f = Follower::new(0, 3, 3);
        f.ingest(&[log_for(&m, 60, 0)]);

        for attempt in 1..3 {
            assert_eq!(f.due(now(0, 0)), vec![hash]);
            f.apply(
                hash,
                now(0, 0),
                Ok(Outcome::SendFailed("rpc timeout".to_string())),
            );
            assert!(
                f.pending.get(&hash).map(|p| p.send_attempts) == Some(attempt),
                "send_attempts must accumulate, attempt {attempt}"
            );
            assert!(f.stuck.is_empty(), "not stuck before max_send_attempts");
        }
        // Third failure reaches max_send_attempts (3) -> stuck.
        f.apply(
            hash,
            now(0, 0),
            Ok(Outcome::SendFailed("rpc timeout".to_string())),
        );
        assert!(!f.pending.contains_key(&hash));
        assert!(matches!(
            f.stuck.get(&hash).map(|s| s.reason),
            Some(StuckReason::MaxSendAttempts)
        ));
        // Stuck-for-max-attempts is never re-surfaced by `due`, on any Now.
        assert!(f.due(now(u64::MAX, u64::MAX)).is_empty());
    }

    #[test]
    fn proof_too_large_is_stuck_not_retried_every_poll() {
        let m = msg(7);
        let hash = m.message_hash();
        let mut f = Follower::new(0, 5, 3);
        f.ingest(&[log_for(&m, 70, 0)]);

        f.apply(
            hash,
            now(0, 8),
            Ok(Outcome::Refused(Refusal::ProofTooLarge {
                len: 5000,
                max: 4096,
            })),
        );
        assert!(
            !f.pending.contains_key(&hash),
            "ProofTooLarge is stuck at once, never left pending"
        );
        assert!(matches!(
            f.stuck.get(&hash).map(|s| s.reason),
            Some(StuckReason::ProofTooLarge { seen_head_final: 8 })
        ));

        // Same Final root, polled repeatedly: never retried.
        assert!(f.due(now(0, 8)).is_empty());
        assert!(f.due(now(0, 8)).is_empty());
        // A LATER Final root: retried exactly then.
        assert_eq!(f.due(now(0, 9)), vec![hash]);
    }

    /// `Pending`'s own byte size, MEASURED via `std::mem::size_of` (never hand-summed from its field types,
    /// since alignment/padding can differ), plus the real process RSS delta of holding 10,000 synthetic
    /// pending messages — the follower's own cache bound. On a platform without `/proc/self/status` (only
    /// Linux has it; CI runs on Linux) the RSS half is skipped and only the `size_of` ceiling is asserted;
    /// either way this never silently reports 0.
    #[test]
    fn pending_entry_size_and_10k_rss_are_measured() {
        let entry_size = std::mem::size_of::<Pending>();
        println!("size_of::<Pending>() = {entry_size} bytes");
        assert!(
            entry_size <= 200,
            "Pending grew past the documented ceiling ({entry_size} bytes) — update the documented limit \
             if this growth is intentional"
        );

        fn rss_kb() -> Option<u64> {
            let status = std::fs::read_to_string("/proc/self/status").ok()?;
            status.lines().find_map(|line| {
                line.strip_prefix("VmRSS:")
                    .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse().ok())
            })
        }

        let before = rss_kb();
        let mut f = Follower::new(0, 5, 3);
        for i in 0..10_000u64 {
            let mut hash = [0u8; 32];
            hash[..8].copy_from_slice(&i.to_le_bytes());
            f.pending.insert(
                hash,
                Pending {
                    message: msg(i),
                    seen_block: i,
                    wait: Wait::Now,
                    send_attempts: 0,
                    window_requeues: 0,
                },
            );
        }
        assert_eq!(f.pending.len(), 10_000);
        let after = rss_kb();

        match (before, after) {
            (Some(b), Some(a)) => {
                let delta_kb = a.saturating_sub(b);
                println!(
                    "10,000-entry Follower.pending RSS delta: {delta_kb} KB (~{} bytes/entry, incl. the \
                     BTreeMap's own node overhead — a MEASURED figure, not the bare size_of above)",
                    delta_kb.saturating_mul(1024) / 10_000
                );
            }
            _ => println!(
                "RSS measurement unavailable on this platform (no /proc/self/status) — the size_of \
                 ceiling above still holds"
            ),
        }
    }
}
