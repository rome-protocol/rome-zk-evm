//! Prometheus metrics for `rome-zk-derive`. Copies the batcher's own split
//! (`rome-zk-batcher::metrics`, `rome_zk_metrics_http::serve_on`) exactly: this struct owns a
//! `prometheus::Registry` and renders it; the binary owns the listener.
//!
//! The five names a load run reads from (it expects `critical_total` to stay 0 and head lag to stay at or
//! under 120): [`Metrics::batches_derived_total`], [`Metrics::critical_total`], [`Metrics::last_batch`],
//! [`Metrics::head_block`], [`Metrics::batch_seconds`].

use prometheus::{Encoder, Histogram, HistogramOpts, IntCounter, IntGauge, Registry, TextEncoder};
use std::sync::Arc;

pub struct Metrics {
    registry: Registry,
    /// Total batches this pipeline has fully derived (one increment per [`crate::pipeline::StepOutcome::Derived`]).
    pub batches_derived_total: IntCounter,
    /// Total `PipelineError::Critical` raises — the strict-policy stop — regardless
    /// of which stage raised it (channel decode, drift bound, engine disagreement, ...). One choke point:
    /// [`crate::pipeline::DerivePipeline::step`] increments this on every `Critical` it returns, never at
    /// each individual raise site, so a new Critical site anywhere in the pipeline is covered for free.
    pub critical_total: IntCounter,
    /// The most recently derived batch id (gauge, not a counter: this is a position, not a count).
    pub last_batch: IntGauge,
    /// The most recently derived block's own design number (`header.number == BlockEnv.number`)
    /// — a load run reads this against the settlement root's own head to measure lag.
    pub head_block: IntGauge,
    /// Wall-clock seconds to derive one batch (`derive_one_batch`'s own duration) — buckets span a fast
    /// local batch up to a multi-block batch under load.
    pub batch_seconds: Histogram,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        let registry = Registry::new();

        let batches_derived_total = IntCounter::new(
            "rome_zk_derive_batches_derived_total",
            "Total batches fully derived through the engine",
        )
        .unwrap();
        let critical_total = IntCounter::new(
            "rome_zk_derive_critical_total",
            "Total PipelineError::Critical raises (strict-policy stop)",
        )
        .unwrap();
        let last_batch = IntGauge::new(
            "rome_zk_derive_last_batch",
            "The most recently derived batch id",
        )
        .unwrap();
        let head_block = IntGauge::new(
            "rome_zk_derive_head_block",
            "The most recently derived block's own design number",
        )
        .unwrap();
        let batch_seconds = Histogram::with_opts(
            HistogramOpts::new(
                "rome_zk_derive_batch_seconds",
                "Wall-clock seconds to derive one batch",
            )
            .buckets(vec![
                0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0,
            ]),
        )
        .unwrap();

        registry
            .register(Box::new(batches_derived_total.clone()))
            .unwrap();
        registry.register(Box::new(critical_total.clone())).unwrap();
        registry.register(Box::new(last_batch.clone())).unwrap();
        registry.register(Box::new(head_block.clone())).unwrap();
        registry.register(Box::new(batch_seconds.clone())).unwrap();

        Arc::new(Self {
            registry,
            batches_derived_total,
            critical_total,
            last_batch,
            head_block,
            batch_seconds,
        })
    }

    /// Renders the registry as Prometheus text exposition format — what the binary's `GET /metrics`
    /// handler serves via `rome_zk_metrics_http::serve_on`, mirroring `rome-zk-batcher::metrics::Metrics::render`.
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

    /// Rendered text carries all five names.
    #[test]
    fn rendered_text_contains_all_five_names() {
        let m = Metrics::new();
        m.batches_derived_total.inc();
        m.critical_total.inc();
        m.last_batch.set(3);
        m.head_block.set(30);
        m.batch_seconds.observe(0.5);
        let rendered = m.render();
        for name in [
            "rome_zk_derive_batches_derived_total",
            "rome_zk_derive_critical_total",
            "rome_zk_derive_last_batch",
            "rome_zk_derive_head_block",
            "rome_zk_derive_batch_seconds",
        ] {
            assert!(
                rendered.contains(name),
                "missing metric {name} in:\n{rendered}"
            );
        }
    }
}
