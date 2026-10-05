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
//! `scan_from_block` stays at the end of the last chunk that worked, so nothing is lost); a `read_slot` failure ALSO skips the rest of this
//! poll — never substitutes a value — so no message is ever attempted against a fabricated slot.
//! `read_root`'s own failure keeps its prior, more permissive behaviour (`head_final_batch` stays `0` for
//! this poll, so only `Wait::Now` messages become due).

use solana_program::pubkey::Pubkey;

use crate::core::{attempt_exit, AttemptParams, CoreError};
use crate::follower::{Follower, IngestReport, Now};
use crate::metrics::Metrics;
use crate::release::ProvedExit;
use crate::rpc::VerifierRpc;
use crate::settlement::SettlementReader;

/// Whether the chain has switched exits on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitGate {
    /// Exits are on; the portal address (`0x`-prefixed hex) comes from the chain's exit config.
    Active { portal_hex: String },
    /// Nothing to do yet. The reason is for the log; the process stays up and asks again next poll.
    Idle(&'static str),
}

/// Reads the exit config and the exit cap and says whether the prover has anything to watch. A chain
/// that has not switched exits on has no exit config account, a config without a portal, or a cap of
/// zero; all three are the ordinary state before activation, not errors. A read that fails for any other
/// reason is counted and treated as idle for this poll. Run it on every poll, so an activation on chain
/// is picked up without a restart.
///
/// Two gauges follow it. `rome_zk_exit_gate_read_ok` is 1 when both reads worked and 0 when one failed. `exit_active`
/// follows the chain only after a read that worked: a failed read says nothing about whether exits are on, so it
/// keeps the last value rather than reporting "off" for what may be an outage.
pub fn exit_gate<R: SettlementReader>(settlement: &R, metrics: &Metrics) -> ExitGate {
    let (gate, read_ok) = match settlement.read_exit_config() {
        Err(crate::settlement::ReadError::NotFound(_)) => {
            (ExitGate::Idle("the chain has no exit config yet"), true)
        }
        Err(e) => {
            metrics.record_rpc_error("read_exit_config");
            tracing::warn!(error = %e, "reading the exit config failed, idle for this poll");
            (ExitGate::Idle("the exit config could not be read"), false)
        }
        Ok(cfg) if cfg.exit_portal == [0u8; 20] => {
            (ExitGate::Idle("the exit config names no portal yet"), true)
        }
        Ok(cfg) => match settlement.read_root() {
            Err(e) => {
                metrics.record_rpc_error("read_root");
                tracing::warn!(error = %e, "reading the root failed, idle for this poll");
                (ExitGate::Idle("the root could not be read"), false)
            }
            Ok(root) if root.exit_cap_per_window == 0 => (
                ExitGate::Idle("the exit cap is zero, so exits are off"),
                true,
            ),
            Ok(_) => (
                ExitGate::Active {
                    portal_hex: format!("0x{}", hex::encode(cfg.exit_portal)),
                },
                true,
            ),
        },
    };
    metrics.exit_gate_read_ok.set(read_ok as i64);
    if read_ok {
        metrics
            .exit_active
            .set(matches!(gate, ExitGate::Active { .. }) as i64);
    }
    gate
}

/// [`exit_gate`] for a poll that also has to send: `tuning` is `None` when the program accounts could not be read.
/// Without them nothing can be sent, so the read gauge drops to 0 here too: its meaning is "the last read of the chain
/// accounts the exit prover needs worked". Otherwise the process would sit idle with the active and read gauges both at 1.
pub fn exit_gate_with_tuning<R: SettlementReader>(
    settlement: &R,
    tuning: Option<&rome_zk_solana_sender::SendTuning>,
    metrics: &Metrics,
) -> ExitGate {
    let gate = exit_gate(settlement, metrics);
    if tuning.is_none() {
        metrics.exit_gate_read_ok.set(0);
    }
    gate
}

/// Why the send settings could not be worked out.
#[derive(Debug, thiserror::Error)]
pub enum TuningError {
    /// The live accounts could not be read, or the program is not on the chain. Nothing is sent until they can be;
    /// ask again next poll.
    #[error("the loaded-accounts requirement cannot be worked out yet: {0}")]
    Unavailable(String),
    /// The configured limit is too small. The process must not go on.
    #[error(transparent)]
    LimitTooLow(#[from] crate::config::LoadedAccountsLimitTooLow),
}

/// Reads the settlement program, its ProgramData, the root, the exit config and the system program in one call, and
/// returns the data length of every account a `ProveExit` loads (see [`crate::config::prove_exit_account_lens`]).
/// A missing root, exit config or system program counts as `0` bytes; a missing program or ProgramData is an error,
/// since the send cannot work without them.
pub fn read_prove_exit_account_lens(
    client: &solana_client::rpc_client::RpcClient,
    program_id: &Pubkey,
    chain_id: u64,
) -> Result<Vec<usize>, String> {
    let (root, _) = zk_settlement_client::root_pda(program_id, chain_id);
    let (exit_config, _) = zk_settlement_client::exit_config_pda(program_id, chain_id);
    let program_data = zk_settlement_client::program_data_pda(program_id);
    let keys = [
        root,
        exit_config,
        Pubkey::default(), // the system program
        *program_id,
        program_data,
    ];
    let accounts = client
        .get_multiple_accounts_with_commitment(
            &keys,
            solana_commitment_config::CommitmentConfig::finalized(),
        )
        .map_err(|e| e.to_string())?
        .value;
    if accounts.len() != keys.len() {
        return Err(format!(
            "asked for {} accounts, got {}",
            keys.len(),
            accounts.len()
        ));
    }
    let len = |i: usize| accounts[i].as_ref().map(|a| a.data.len());
    let program_len =
        len(3).ok_or_else(|| format!("the settlement program {program_id} is not on the chain"))?;
    let program_data_len = len(4)
        .ok_or_else(|| format!("the program data account {program_data} is not on the chain"))?;
    Ok(crate::config::prove_exit_account_lens(
        len(0).unwrap_or(0),
        len(1).unwrap_or(0),
        len(2).unwrap_or(0),
        program_len,
        program_data_len,
    ))
}

/// The send settings for this poll: the config's own, with the loaded-accounts limit worked out from the live
/// account sizes (or the configured one, if it covers them). Asked on every poll, so a program upgrade that makes the
/// program bigger is followed without a restart.
pub fn resolve_tuning(
    cfg: &crate::config::Config,
    client: &solana_client::rpc_client::RpcClient,
    program_id: &Pubkey,
) -> Result<rome_zk_solana_sender::SendTuning, TuningError> {
    let lens = read_prove_exit_account_lens(client, program_id, cfg.chain_id)
        .map_err(TuningError::Unavailable)?;
    let limit =
        crate::config::resolve_loaded_accounts_limit(cfg.loaded_accounts_data_size_limit, &lens)?;
    Ok(cfg.send_tuning(limit))
}

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
    /// The exits this poll found proved on chain, either by its own `ProveExit` or already proved: the release step
    /// takes it from here.
    pub proved: Vec<crate::release::ProvedExit>,
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
    max_log_range: u64,
) -> PollReport
where
    R: SettlementReader,
    V: VerifierRpc,
    S: rome_zk_solana_sender::Sender,
{
    let mut report = PollReport::default();

    if follower.follow_portal(portal_hex) {
        tracing::warn!(
            portal = portal_hex,
            "the exit config names a different portal, scanning it from the start block"
        );
    }

    // The node refuses wide log ranges, so the chain is scanned in pieces of at most `max_log_range` blocks, up to
    // the head as it was at the start of this poll. A piece that works moves the cursor to its end; one that
    // fails stops the poll with the cursor where it was, so the next poll asks for the same stretch again.
    let head = match verifier.eth_block_number() {
        Ok(head) => head,
        Err(e) => {
            metrics.record_rpc_error("eth_blockNumber");
            tracing::warn!(error = %e, "eth_blockNumber failed, skipping this poll");
            report.logs_error = true;
            return report;
        }
    };
    let range = max_log_range.max(1);
    while follower.scan_from_block <= head {
        let from = follower.scan_from_block;
        let to = from.saturating_add(range - 1).min(head);
        let logs = match verifier.eth_get_logs(portal_hex, from, to) {
            Ok(logs) => logs,
            Err(e) => {
                metrics.record_rpc_error("eth_getLogs");
                tracing::warn!(error = %e, from, to, "eth_getLogs failed, skipping this poll");
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
        report.ingest.added += ingest.added;
        report.ingest.duplicates += ingest.duplicates;
        report.ingest.decode_errors += ingest.decode_errors;
        follower.mark_scanned_through(to);
        if to >= head {
            break;
        }
    }

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
                match outcome {
                    crate::core::Outcome::Sent { .. } => report.proved.push(ProvedExit {
                        message_hash: hash,
                        just_sent: true,
                    }),
                    crate::core::Outcome::AlreadyProved => report.proved.push(ProvedExit {
                        message_hash: hash,
                        just_sent: false,
                    }),
                    _ => {}
                }
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
