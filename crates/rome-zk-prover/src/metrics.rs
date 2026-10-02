//! Prometheus metrics for the follower loop, served on `/metrics` by
//! `rome-zk-metrics-http` — the same `prometheus::Registry` + `TextEncoder` wiring
//! `rome-zk-batcher::metrics`/`rome-zk-sequencer::metrics` already use (copied shape, never a second
//! hand-rolled exposition encoder).

use prometheus::{
    Encoder, Gauge, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec,
    Opts, Registry, TextEncoder,
};
use std::sync::Arc;

/// Every state name `rome_zk_prover_state{state}` can be set to 1 for (all others 0) — the follower's
/// own `Outcome`/in-flight-stage vocabulary.
pub const STATE_NAMES: &[&str] = &[
    "idle",
    "queued",
    "input_built",
    "proving",
    "decoded",
    "locally_verified",
    "posting",
    "relayed",
    "finalized",
    "superseded",
    "failed",
];

/// Stage names for `rome_zk_prover_stage_wall_seconds{stage}`.
pub const STAGE_INPUT: &str = "input";
pub const STAGE_STARK: &str = "stark";
pub const STAGE_PLONK: &str = "plonk";
pub const STAGE_VERIFY: &str = "verify";
pub const STAGE_POST: &str = "post";
pub const STAGE_FINALIZE: &str = "finalize";

pub struct Metrics {
    registry: Registry,
    pub batches_behind: IntGauge,
    pub head: IntGaugeVec,
    pub lag_seconds: Gauge,
    pub stage_wall_seconds: HistogramVec,
    pub batch_gas_used: Gauge,
    pub batch_cost_usd: Gauge,
    pub posts_total: IntCounterVec,
    pub prove_attempts_total: IntCounter,
    pub payer_lamports: IntGauge,
    pub state: IntGaugeVec,
    pub abandoned_batch_alarm: IntCounter,
    pub stale_anchor_alarm: IntCounter,
    pub fetch_alarm: IntCounter,
    /// A `Store::record` call that returned `Err` — counted, never propagated. A
    /// Postgres outage shows up here, not as a halted follower.
    pub store_errors_total: IntCounter,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        let registry = Registry::new();

        let batches_behind = IntGauge::new(
            "rome_zk_prover_batches_behind",
            "cursor_next_batch - 1 - head_pending_batch: finalized batches not yet posted",
        )
        .unwrap();
        let head = IntGaugeVec::new(
            Opts::new("rome_zk_prover_head", "The chain's head, by kind"),
            &["kind"],
        )
        .unwrap();
        let lag_seconds = Gauge::new(
            "rome_zk_prover_lag_seconds",
            "now - the inbox batch's own open_unix_ts, sampled at Relayed",
        )
        .unwrap();
        let stage_wall_seconds = HistogramVec::new(
            HistogramOpts::new(
                "rome_zk_prover_stage_wall_seconds",
                "Wall time per pipeline stage",
            )
            .buckets(vec![
                0.05, 0.1, 0.5, 1.0, 5.0, 15.0, 30.0, 60.0, 300.0, 900.0, 1800.0,
            ]),
            &["stage"],
        )
        .unwrap();
        let batch_gas_used = Gauge::new(
            "rome_zk_prover_batch_gas_used",
            "gas_used of the most recently proved batch",
        )
        .unwrap();
        let batch_cost_usd = Gauge::new(
            "rome_zk_prover_batch_cost_usd",
            "gpu_hourly_usd * (stark + plonk wall seconds) / 3600, reporting only",
        )
        .unwrap();
        let posts_total = IntCounterVec::new(
            Opts::new(
                "rome_zk_prover_posts_total",
                "Total PostRootProved outcomes, by result",
            ),
            &["result"],
        )
        .unwrap();
        let prove_attempts_total = IntCounter::new(
            "rome_zk_prover_prove_attempts_total",
            "Total Prover::prove invocations (never incremented on a resumed/skipped prove)",
        )
        .unwrap();
        let payer_lamports = IntGauge::new(
            "rome_zk_prover_payer_lamports",
            "The poster's own fee-payer balance, sampled opportunistically",
        )
        .unwrap();
        let state = IntGaugeVec::new(
            Opts::new(
                "rome_zk_prover_state",
                "1 for the follower's current state, 0 for every other named state",
            ),
            &["state"],
        )
        .unwrap();
        let abandoned_batch_alarm = IntCounter::new(
            "rome_zk_prover_abandoned_batch_alarm",
            "Total times the follower halted on an AbandonedInboxBatch",
        )
        .unwrap();
        let stale_anchor_alarm = IntCounter::new(
            "rome_zk_prover_stale_anchor_alarm",
            "Total times the follower halted on a StaleAnchor streak that never cleared",
        )
        .unwrap();
        let fetch_alarm = IntCounter::new(
            "rome_zk_prover_fetch_alarm",
            "Total times the follower halted on a TransientFetchError streak that never cleared",
        )
        .unwrap();
        let store_errors_total = IntCounter::new(
            "rome_zk_prover_store_errors_total",
            "Total Store::record calls that returned Err (counted, never allowed to halt the follower)",
        )
        .unwrap();

        registry.register(Box::new(batches_behind.clone())).unwrap();
        registry.register(Box::new(head.clone())).unwrap();
        registry.register(Box::new(lag_seconds.clone())).unwrap();
        registry
            .register(Box::new(stage_wall_seconds.clone()))
            .unwrap();
        registry.register(Box::new(batch_gas_used.clone())).unwrap();
        registry.register(Box::new(batch_cost_usd.clone())).unwrap();
        registry.register(Box::new(posts_total.clone())).unwrap();
        registry
            .register(Box::new(prove_attempts_total.clone()))
            .unwrap();
        registry.register(Box::new(payer_lamports.clone())).unwrap();
        registry.register(Box::new(state.clone())).unwrap();
        registry
            .register(Box::new(abandoned_batch_alarm.clone()))
            .unwrap();
        registry
            .register(Box::new(stale_anchor_alarm.clone()))
            .unwrap();
        registry.register(Box::new(fetch_alarm.clone())).unwrap();
        registry
            .register(Box::new(store_errors_total.clone()))
            .unwrap();

        for name in STATE_NAMES {
            state.with_label_values(&[name]).set(0);
        }

        Arc::new(Self {
            registry,
            batches_behind,
            head,
            lag_seconds,
            stage_wall_seconds,
            batch_gas_used,
            batch_cost_usd,
            posts_total,
            prove_attempts_total,
            payer_lamports,
            state,
            abandoned_batch_alarm,
            stale_anchor_alarm,
            fetch_alarm,
            store_errors_total,
        })
    }

    /// Sets the follower's own current state: 1 for `name`, 0 for every other name in
    /// [`STATE_NAMES`] — never leaves a stale prior state reading 1 alongside the new one.
    pub fn set_state(&self, name: &str) {
        for n in STATE_NAMES {
            self.state
                .with_label_values(&[n])
                .set(if *n == name { 1 } else { 0 });
        }
    }

    pub fn observe_stage_seconds(&self, stage: &str, seconds: f64) {
        self.stage_wall_seconds
            .with_label_values(&[stage])
            .observe(seconds);
    }

    pub fn set_head(&self, kind: &str, value: i64) {
        self.head.with_label_values(&[kind]).set(value);
    }

    pub fn inc_posts(&self, result: &str) {
        self.posts_total.with_label_values(&[result]).inc();
    }

    /// Renders the registry as Prometheus text exposition format.
    pub fn render(&self) -> String {
        let metric_families = self.registry.gather();
        let mut buffer = Vec::new();
        TextEncoder::new()
            .encode(&metric_families, &mut buffer)
            .expect("prometheus text encoding cannot fail for well-formed metrics");
        String::from_utf8(buffer).expect("prometheus text encoder always emits valid utf8")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_named_metric_from_plan_b8_is_registered_and_rendered() {
        let m = Metrics::new();
        m.batches_behind.set(3);
        m.set_head("pending", 1);
        m.lag_seconds.set(2.0);
        m.observe_stage_seconds(STAGE_STARK, 12.0);
        m.batch_gas_used.set(0.0);
        m.batch_cost_usd.set(0.0);
        m.inc_posts("relayed");
        m.prove_attempts_total.inc();
        m.payer_lamports.set(1);
        m.set_state("idle");
        m.abandoned_batch_alarm.inc();
        m.stale_anchor_alarm.inc();
        m.fetch_alarm.inc();
        m.store_errors_total.inc();

        let rendered = m.render();
        for name in [
            "rome_zk_prover_batches_behind",
            "rome_zk_prover_head",
            "rome_zk_prover_lag_seconds",
            "rome_zk_prover_stage_wall_seconds",
            "rome_zk_prover_batch_gas_used",
            "rome_zk_prover_batch_cost_usd",
            "rome_zk_prover_posts_total",
            "rome_zk_prover_prove_attempts_total",
            "rome_zk_prover_payer_lamports",
            "rome_zk_prover_state",
            "rome_zk_prover_abandoned_batch_alarm",
            "rome_zk_prover_stale_anchor_alarm",
            "rome_zk_prover_fetch_alarm",
            "rome_zk_prover_store_errors_total",
        ] {
            assert!(
                rendered.contains(name),
                "missing metric {name} in:\n{rendered}"
            );
        }
    }

    #[test]
    fn set_state_clears_every_other_state_to_zero() {
        let m = Metrics::new();
        m.set_state("proving");
        let rendered = m.render();
        assert!(
            rendered.contains("rome_zk_prover_state{state=\"proving\"} 1"),
            "{rendered}"
        );
        assert!(
            rendered.contains("rome_zk_prover_state{state=\"idle\"} 0"),
            "{rendered}"
        );
        m.set_state("idle");
        let rendered = m.render();
        assert!(
            rendered.contains("rome_zk_prover_state{state=\"idle\"} 1"),
            "{rendered}"
        );
        assert!(
            rendered.contains("rome_zk_prover_state{state=\"proving\"} 0"),
            "{rendered}"
        );
    }
}
