//! Prometheus metrics, served on `/metrics` by `rome-zk-metrics-http` — the same
//! `prometheus::Registry` + `TextEncoder` wiring `rome-zk-prover::metrics`/`rome-zk-batcher::metrics`
//! already use.

use prometheus::{
    Encoder, Histogram, HistogramOpts, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts,
    Registry, TextEncoder,
};
use std::sync::Arc;

pub struct Metrics {
    registry: Registry,
    pub prove_attempts_total: IntCounterVec,
    pub queued_total: IntCounter,
    pub proof_bytes: Histogram,
    pub latency_secs: Histogram,
    pub exits_proved_total: IntCounter,
    /// `crate::follower::Follower::pending.len()` — how many messages the follower is currently tracking
    /// (any `Wait`), sampled once per poll.
    pub exit_pending: IntGauge,
    /// `crate::follower::Follower::stuck`, split by [`crate::follower::StuckReason`] — `"max_send_attempts"`,
    /// `"proof_too_large"`, `"max_window_requeues"`, `"exceeds_window_cap"`, `"proof_invalid"`,
    /// `"unsupported_asset"`.
    pub exit_stuck: IntGaugeVec,
    /// `crate::follower::Follower::scan_from_block` — the next `eth_getLogs` cursor, sampled once per
    /// poll (never a persisted value).
    pub exit_scan_from_block: IntGauge,
    /// Total logs `crate::follower::Follower::ingest` could not decode as `ExitInitiated`, across every
    /// poll — counted, not propagated with `?`.
    pub exit_log_decode_errors_total: IntCounter,
    /// `attempt_exit` returning `Err(_)` (the `Sender` was never touched), split by
    /// which read failed: `kind="settlement"` (`CoreError::Read`), `"verifier"` (`CoreError::Verifier`),
    /// `"build"` (`CoreError::Build`). Distinct from `exit_stuck`/`exit_pending`: a read error never moves
    /// a message between those maps on its own.
    pub exit_read_errors_total: IntCounterVec,
    /// RPC calls `run::poll_once` itself made that failed outright — `method`
    /// `"eth_getLogs"` or `"get_slot"`. Both skip the current poll rather than propagating (never crash the
    /// loop, never fall back to a wrong value like slot `0`).
    pub exit_rpc_errors_total: IntCounterVec,
    /// `1` while the chain's exit config names a portal and the exit cap is above zero (the prover is
    /// scanning), `0` while it is idle waiting for exits to be switched on.
    pub exit_active: IntGauge,
    /// `rome_zk_exit_gate_read_ok`: 1 when the last read of the chain accounts the exit prover needs (the exit
    /// config, the root and the program accounts) worked, 0 when it failed. `exit_active` keeps its last value across a failed read, so this is what tells "exits are off" from
    /// "the chain could not be read".
    pub exit_gate_read_ok: IntGauge,
    /// `rome_zk_exits_released_total`: payouts this process sent with `ReleaseExit` (an exit someone else released
    /// is not counted).
    pub exits_released_total: IntCounter,
    /// `rome_zk_exit_release_waiting`: proved exits left for a manual `release-exit` because the payout is below the
    /// token-account minimum and the recipient has no token account.
    pub exit_release_waiting: IntGauge,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        let registry = Registry::new();

        let prove_attempts_total = IntCounterVec::new(
            Opts::new(
                "rome_zk_exit_prove_attempts_total",
                "Total attempt_exit outcomes, by result",
            ),
            &["result"],
        )
        .unwrap();
        let queued_total = IntCounter::new(
            "rome_zk_exit_queued_total",
            "Exits refused ExitCapExceeded and re-queued to their next window",
        )
        .unwrap();
        let proof_bytes = Histogram::with_opts(HistogramOpts::new(
            "rome_zk_exit_proof_bytes",
            "Signed ProveExit V1 transaction size (bytes)",
        ))
        .unwrap();
        let latency_secs = Histogram::with_opts(HistogramOpts::new(
            "rome_zk_exit_latency_secs",
            "L2 initiate to release, sampled at Sent",
        ))
        .unwrap();
        let exits_proved_total = IntCounter::new(
            "rome_zk_exits_proved_total",
            "Total exits sent (and confirmed) via ProveExit",
        )
        .unwrap();
        let exit_pending = IntGauge::new(
            "rome_zk_exit_pending",
            "Messages the follower is currently tracking (any wait state)",
        )
        .unwrap();
        let exit_stuck = IntGaugeVec::new(
            Opts::new(
                "rome_zk_exit_stuck",
                "Messages parked as stuck, by reason (max_send_attempts, proof_too_large, max_window_requeues, exceeds_window_cap, proof_invalid, unsupported_asset)",
            ),
            &["reason"],
        )
        .unwrap();
        let exit_scan_from_block = IntGauge::new(
            "rome_zk_exit_scan_from_block",
            "The follower's next eth_getLogs cursor (never persisted)",
        )
        .unwrap();
        let exit_log_decode_errors_total = IntCounter::new(
            "rome_zk_exit_log_decode_errors_total",
            "Total ExitInitiated logs that failed to decode, across every poll",
        )
        .unwrap();
        let exit_read_errors_total = IntCounterVec::new(
            Opts::new(
                "rome_zk_exit_read_errors_total",
                "attempt_exit Err(_) outcomes (the Sender was never touched), by which read failed",
            ),
            &["kind"],
        )
        .unwrap();
        let exit_rpc_errors_total = IntCounterVec::new(
            Opts::new(
                "rome_zk_exit_rpc_errors_total",
                "RPC calls run::poll_once made that failed outright, by method",
            ),
            &["method"],
        )
        .unwrap();

        let exit_active = IntGauge::new(
            "rome_zk_exit_active",
            "1 when the chain's exit config names a portal and a cap and the prover is scanning, 0 while idle",
        )
        .unwrap();
        registry.register(Box::new(exit_active.clone())).unwrap();
        let exit_gate_read_ok = IntGauge::new(
            "rome_zk_exit_gate_read_ok",
            "1 when the last read of the chain accounts the exit prover needs succeeded, 0 when it failed",
        )
        .unwrap();
        registry
            .register(Box::new(exit_gate_read_ok.clone()))
            .unwrap();
        let exits_released_total = IntCounter::new(
            "rome_zk_exits_released_total",
            "Payouts this process sent with ReleaseExit",
        )
        .unwrap();
        registry
            .register(Box::new(exits_released_total.clone()))
            .unwrap();
        let exit_release_waiting = IntGauge::new(
            "rome_zk_exit_release_waiting",
            "Proved exits left for a manual release-exit: the payout is below the token-account minimum and the recipient has no token account",
        )
        .unwrap();
        registry
            .register(Box::new(exit_release_waiting.clone()))
            .unwrap();
        registry
            .register(Box::new(prove_attempts_total.clone()))
            .unwrap();
        registry.register(Box::new(queued_total.clone())).unwrap();
        registry.register(Box::new(proof_bytes.clone())).unwrap();
        registry.register(Box::new(latency_secs.clone())).unwrap();
        registry
            .register(Box::new(exits_proved_total.clone()))
            .unwrap();
        registry.register(Box::new(exit_pending.clone())).unwrap();
        registry.register(Box::new(exit_stuck.clone())).unwrap();
        registry
            .register(Box::new(exit_scan_from_block.clone()))
            .unwrap();
        registry
            .register(Box::new(exit_log_decode_errors_total.clone()))
            .unwrap();
        registry
            .register(Box::new(exit_read_errors_total.clone()))
            .unwrap();
        registry
            .register(Box::new(exit_rpc_errors_total.clone()))
            .unwrap();

        Arc::new(Self {
            registry,
            prove_attempts_total,
            queued_total,
            proof_bytes,
            latency_secs,
            exits_proved_total,
            exit_pending,
            exit_stuck,
            exit_scan_from_block,
            exit_log_decode_errors_total,
            exit_read_errors_total,
            exit_rpc_errors_total,
            exit_active,
            exit_gate_read_ok,
            exits_released_total,
            exit_release_waiting,
        })
    }

    /// Samples the follower's own current state into the gauges — called once
    /// per poll from the bin, never from inside `Follower` itself (metrics are an observer, not a
    /// dependency the pure state machine carries).
    pub fn record_follower(&self, follower: &crate::follower::Follower) {
        self.exit_pending.set(follower.pending.len() as i64);
        self.exit_scan_from_block
            .set(follower.scan_from_block as i64);
        let mut max_send_attempts = 0i64;
        let mut proof_too_large = 0i64;
        let mut max_window_requeues = 0i64;
        let mut exceeds_window_cap = 0i64;
        let mut proof_invalid = 0i64;
        let mut unsupported_asset = 0i64;
        for s in follower.stuck.values() {
            match s.reason {
                crate::follower::StuckReason::MaxSendAttempts => max_send_attempts += 1,
                crate::follower::StuckReason::ProofTooLarge { .. } => proof_too_large += 1,
                crate::follower::StuckReason::MaxWindowRequeues { .. } => max_window_requeues += 1,
                crate::follower::StuckReason::ExceedsWindowCap { .. } => exceeds_window_cap += 1,
                crate::follower::StuckReason::ProofInvalid { .. } => proof_invalid += 1,
                crate::follower::StuckReason::UnsupportedAsset => unsupported_asset += 1,
            }
        }
        self.exit_stuck
            .with_label_values(&["max_send_attempts"])
            .set(max_send_attempts);
        self.exit_stuck
            .with_label_values(&["proof_too_large"])
            .set(proof_too_large);
        self.exit_stuck
            .with_label_values(&["max_window_requeues"])
            .set(max_window_requeues);
        self.exit_stuck
            .with_label_values(&["exceeds_window_cap"])
            .set(exceeds_window_cap);
        self.exit_stuck
            .with_label_values(&["proof_invalid"])
            .set(proof_invalid);
        self.exit_stuck
            .with_label_values(&["unsupported_asset"])
            .set(unsupported_asset);
    }

    /// `attempt_exit` returned `Err(_)` — the `Sender` was never touched. `kind` is
    /// `"settlement"` (`CoreError::Read`), `"verifier"` (`CoreError::Verifier`), or `"build"`
    /// (`CoreError::Build`).
    pub fn record_read_error(&self, kind: &str) {
        self.exit_read_errors_total.with_label_values(&[kind]).inc();
    }

    /// An RPC call `run::poll_once` made itself failed outright (`method`
    /// `"eth_getLogs"` or `"get_slot"`) — the poll was skipped, never crashed the loop.
    pub fn record_rpc_error(&self, method: &str) {
        self.exit_rpc_errors_total
            .with_label_values(&[method])
            .inc();
    }

    /// Adds one poll's [`crate::follower::IngestReport`] to the running decode-error counter.
    pub fn record_ingest(&self, report: &crate::follower::IngestReport) {
        self.exit_log_decode_errors_total
            .inc_by(report.decode_errors as u64);
    }

    pub fn render(&self) -> Vec<u8> {
        let metric_families = self.registry.gather();
        let mut buf = Vec::new();
        TextEncoder::new()
            .encode(&metric_families, &mut buf)
            .unwrap();
        buf
    }

    /// Records one [`crate::core::Outcome`] under its named `result` label, plus the derived counters
    /// (`exit_queued_total`, `exits_proved_total`) kept as counters of their own.
    pub fn record_outcome(&self, outcome: &crate::core::Outcome) {
        let label = match outcome {
            crate::core::Outcome::Sent { .. } => "sent",
            crate::core::Outcome::AlreadyProved => "already_proved",
            crate::core::Outcome::QueuedForWindow { .. } => "queued_for_window",
            crate::core::Outcome::Refused(r) => match r {
                crate::core::Refusal::StateUnavailableAtRoot { .. } => "state_unavailable_at_root",
                crate::core::Refusal::Verify(_) => "verify_refused",
                crate::core::Refusal::ProofTooLarge { .. } => "proof_too_large",
                crate::core::Refusal::ExitConfigUnset => "exit_config_unset",
                crate::core::Refusal::ExitCapUnset => "exit_cap_unset",
                crate::core::Refusal::ChallengeWindowZero => "challenge_window_zero",
                crate::core::Refusal::NoFinalBatch => "no_final_batch",
                crate::core::Refusal::ExceedsWindowCap { .. } => "exceeds_window_cap",
                crate::core::Refusal::UnsupportedAsset => "unsupported_asset",
            },
            crate::core::Outcome::SendFailed(_) => "send_failed",
        };
        self.prove_attempts_total.with_label_values(&[label]).inc();
        if matches!(outcome, crate::core::Outcome::QueuedForWindow { .. }) {
            self.queued_total.inc();
        }
        if matches!(outcome, crate::core::Outcome::Sent { .. }) {
            self.exits_proved_total.inc();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_outcome_labels_and_derived_counters() {
        let m = Metrics::new();
        m.record_outcome(&crate::core::Outcome::Sent {
            window_index: 0,
            units: 1,
        });
        m.record_outcome(&crate::core::Outcome::QueuedForWindow {
            next_index: 1,
            retry_slot: 100,
        });
        assert_eq!(m.exits_proved_total.get(), 1);
        assert_eq!(m.queued_total.get(), 1);
        let rendered = String::from_utf8(m.render()).unwrap();
        assert!(rendered.contains("rome_zk_exit_prove_attempts_total"));
        assert!(rendered.contains(r#"result="sent""#));
        assert!(rendered.contains(r#"result="queued_for_window""#));
    }

    /// A render test asserting every new metric name appears: the follower's four metrics.
    #[test]
    fn every_new_follower_metric_name_is_rendered() {
        let m = Metrics::new();
        let mut follower = crate::follower::Follower::new(208_399, 5, 3);
        follower.pending.insert(
            [0x11; 32],
            crate::follower::Pending {
                message: rome_zk_layouts::exit::ExitMessage {
                    nonce: 0,
                    l2_sender: [0; 20],
                    sol_recipient: [0; 32],
                    asset: [0; 20],
                    amount: 0,
                },
                seen_block: 0,
                wait: crate::follower::Wait::Now,
                send_attempts: 0,
                window_requeues: 0,
            },
        );
        follower.stuck.insert(
            [0x22; 32],
            crate::follower::Stuck {
                message: rome_zk_layouts::exit::ExitMessage {
                    nonce: 1,
                    l2_sender: [0; 20],
                    sol_recipient: [0; 32],
                    asset: [0; 20],
                    amount: 0,
                },
                seen_block: 0,
                reason: crate::follower::StuckReason::MaxSendAttempts,
                send_attempts: 5,
                window_requeues: 0,
            },
        );
        m.record_follower(&follower);
        m.record_read_error("settlement");
        m.record_rpc_error("eth_getLogs");
        m.record_ingest(&crate::follower::IngestReport {
            added: 0,
            duplicates: 0,
            decode_errors: 2,
        });

        let rendered = String::from_utf8(m.render()).unwrap();
        assert!(rendered.contains("rome_zk_exit_pending"));
        assert!(rendered.contains("rome_zk_exit_stuck"));
        assert!(rendered.contains(r#"reason="max_send_attempts""#));
        assert!(rendered.contains("rome_zk_exit_scan_from_block"));
        assert!(rendered.contains("rome_zk_exit_log_decode_errors_total"));
        // The two read/RPC error counters and every stuck reason's label render.
        assert!(rendered.contains("rome_zk_exit_read_errors_total"));
        assert!(rendered.contains("rome_zk_exit_rpc_errors_total"));
        for reason in [
            "max_send_attempts",
            "proof_too_large",
            "max_window_requeues",
            "exceeds_window_cap",
            "proof_invalid",
            "unsupported_asset",
        ] {
            assert!(
                rendered.contains(&format!("reason=\"{reason}\"")),
                "stuck reason label {reason} must render"
            );
        }
        assert_eq!(m.exit_pending.get(), 1);
        assert_eq!(m.exit_scan_from_block.get(), 208_399);
        assert_eq!(m.exit_log_decode_errors_total.get(), 2);
    }
}
