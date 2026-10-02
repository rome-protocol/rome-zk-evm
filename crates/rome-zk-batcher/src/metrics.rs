//! Prometheus metrics: frames/s, chunk tx confirm latency, resubmits, batches finalized,
//! bytes per tx, compression ratio, and posting-cadence observability: per-batch
//! open/finalize confirm latency, whole-batch post time, the current posting-window occupancy, and how
//! far behind the last finalized block the ordered log's own tail has run.

use prometheus::{
    Encoder, Histogram, HistogramOpts, IntCounter, IntCounterVec, IntGauge, Opts, Registry,
    TextEncoder,
};
use std::sync::Arc;

/// Buckets shared by every cadence histogram (`open_confirm_seconds`,
/// `finalize_confirm_seconds`, `batch_post_seconds`) — 0.25 s .. 60 s, covering everything from a fast
/// local-cluster confirm up past the worst observed inclusion tail (11.5 s) in an earlier
/// measurement, with headroom to spare.
fn cadence_buckets() -> Vec<f64> {
    vec![0.25, 0.5, 1.0, 2.0, 4.0, 8.0, 15.0, 30.0, 60.0]
}

/// Buckets for `solana_clock_skew_seconds` — signed, centred on 0, spanning
/// well past the ±60 s drift bound (`DEFAULT_MAX_DRIFT_SECS`) on both sides so a real out-of-bound skew is
/// visible rather than clipped into the outermost bucket.
fn clock_skew_buckets() -> Vec<f64> {
    vec![
        -120.0, -60.0, -30.0, -10.0, -5.0, -1.0, 0.0, 1.0, 5.0, 10.0, 30.0, 60.0, 120.0,
    ]
}

pub struct Metrics {
    registry: Registry,
    pub frames_sent_total: IntCounter,
    pub chunk_confirm_latency_seconds: Histogram,
    pub resubmits_total: IntCounter,
    pub batches_finalized_total: IntCounter,
    pub batches_failed_total: IntCounter,
    /// Bytes of raw tx payload divided by chunk txs sent, per batch (compare against the 74–177 B/tx measured
    /// earlier).
    pub bytes_per_tx: Histogram,
    /// `compressed_len / rlp_len` for one channel stream — lower is better; design targets zstd-19's
    /// typical ratio on EVM tx corpora.
    pub compression_ratio: Histogram,
    /// Latency from one batch's `OpenBatch`(+`GrowBatch`) send to its own confirmation.
    pub open_confirm_seconds: Histogram,
    /// Latency from one batch's `FinalizeBatch` send to its own confirmation.
    pub finalize_confirm_seconds: Histogram,
    /// Latency from a batch's `OpenBatch` send to its `FinalizeBatch` confirming — the
    /// whole per-batch post time the bounded window (`batches_in_flight`) is meant to overlap.
    pub batch_post_seconds: Histogram,
    /// How many batches are currently posting concurrently (0..=`batches_in_flight`).
    pub batches_in_flight: IntGauge,
    /// Newest block visible in the ordered log minus the last block of the last
    /// finalized batch — the verifier's own cadence lag, in blocks.
    pub lag_blocks: IntGauge,
    /// Bumped every time `cu_sample_every`'s own cadence
    /// gate decides "this finalized batch's turn" — independent of whether a real CU sample actually ran
    /// (`WindowConfig::cu_sample` can be `None`), so the cadence itself is directly observable/testable
    /// without a live RPC client.
    pub cu_samples_triggered_total: IntCounter,
    /// Why a group in progress closed — `cap`, `size`, or `age`
    /// (a partial group that sat unposted for `batch_close_after_secs` on the batcher's own receipt
    /// clock). A quiet chain producing nothing shows no increments here at all (no batch
    /// when idle).
    pub groups_closed_total: IntCounterVec,
    /// How long the group in progress has been waiting to post, on the batcher's
    /// own receipt clock — 0 whenever nothing is unposted (an empty group, or right after any close).
    pub oldest_unposted_block_age_seconds: IntGauge,
    /// This batcher's own wall clock, at the moment it read a block
    /// off the log, minus that block's own `Block.timestamp` — observability only (never a close
    /// decision, see `groups_closed_total`/`oldest_unposted_block_age_seconds` above for that). A restart
    /// that resumes into a backlog legitimately observes a large value here; it is not itself an alarm.
    pub block_age_on_arrival_seconds: Histogram,
    /// Decoded `open_unix_ts` minus this batcher's own unix
    /// wall clock, captured immediately before the `OpenBatch` send — signed, so a negative value means
    /// this batcher's clock was already ahead of the moment the chain later stamped. Observability only:
    /// never a decision input anywhere in this crate (the drift bound itself lives in `rome-zk-derive`,
    /// unaffected by this reading either way).
    pub solana_clock_skew_seconds: Histogram,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        let registry = Registry::new();

        let frames_sent_total = IntCounter::new(
            "rome_zk_batcher_frames_sent_total",
            "Total frames successfully sent (Open+Write+Seal confirmed)",
        )
        .unwrap();
        let chunk_confirm_latency_seconds = Histogram::with_opts(
            HistogramOpts::new(
                "rome_zk_batcher_chunk_confirm_latency_seconds",
                "Latency from a chunk tx's first submit to its confirmation",
            )
            .buckets(vec![
                0.05, 0.1, 0.2, 0.4, 0.8, 1.5, 3.0, 6.0, 12.0, 30.0, 60.0,
            ]),
        )
        .unwrap();
        let resubmits_total = IntCounter::new(
            "rome_zk_batcher_resubmits_total",
            "Total resubmits due to blockhash expiry (fresh blockhash + bumped priority fee)",
        )
        .unwrap();
        let batches_finalized_total = IntCounter::new(
            "rome_zk_batcher_batches_finalized_total",
            "Total batches whose acc matched the client-side reference after FinalizeBatch",
        )
        .unwrap();
        let batches_failed_total = IntCounter::new(
            "rome_zk_batcher_batches_failed_total",
            "Total batch attempts that failed re-derivation, sending, or acc verification",
        )
        .unwrap();
        let bytes_per_tx = Histogram::with_opts(
            HistogramOpts::new(
                "rome_zk_batcher_bytes_per_tx",
                "Raw tx payload bytes divided by chunk-lane Solana txs sent, per batch",
            )
            .buckets(vec![20.0, 40.0, 74.0, 100.0, 150.0, 177.0, 250.0, 500.0]),
        )
        .unwrap();
        let compression_ratio = Histogram::with_opts(HistogramOpts::new(
            "rome_zk_batcher_compression_ratio",
            "compressed_len / rlp_len for one channel stream",
        ))
        .unwrap();
        let open_confirm_seconds = Histogram::with_opts(
            HistogramOpts::new(
                "rome_zk_batcher_open_confirm_seconds",
                "Latency from a batch's OpenBatch(+GrowBatch) send to its own confirmation",
            )
            .buckets(cadence_buckets()),
        )
        .unwrap();
        let finalize_confirm_seconds = Histogram::with_opts(
            HistogramOpts::new(
                "rome_zk_batcher_finalize_confirm_seconds",
                "Latency from a batch's FinalizeBatch send to its own confirmation",
            )
            .buckets(cadence_buckets()),
        )
        .unwrap();
        let batch_post_seconds = Histogram::with_opts(
            HistogramOpts::new(
                "rome_zk_batcher_batch_post_seconds",
                "Latency from a batch's OpenBatch send to its FinalizeBatch confirming",
            )
            .buckets(cadence_buckets()),
        )
        .unwrap();
        let batches_in_flight = IntGauge::new(
            "rome_zk_batcher_batches_in_flight",
            "Batches currently posting concurrently (0..=batches_in_flight)",
        )
        .unwrap();
        let lag_blocks = IntGauge::new(
            "rome_zk_batcher_lag_blocks",
            "Newest block visible in the ordered log minus the last finalized batch's own last block",
        )
        .unwrap();
        let cu_samples_triggered_total = IntCounter::new(
            "rome_zk_batcher_cu_samples_triggered_total",
            "Total finalized batches whose turn it was to CU-sample (cu_sample_every cadence)",
        )
        .unwrap();
        let groups_closed_total = IntCounterVec::new(
            Opts::new(
                "rome_zk_batcher_groups_closed_total",
                "Total groups closed, by reason (cap, size, or age)",
            ),
            &["reason"],
        )
        .unwrap();
        let oldest_unposted_block_age_seconds = IntGauge::new(
            "rome_zk_batcher_oldest_unposted_block_age_seconds",
            "How long the group in progress has held its first block, on the batcher's own receipt \
             clock — 0 when nothing is unposted",
        )
        .unwrap();
        let block_age_on_arrival_seconds = Histogram::with_opts(
            HistogramOpts::new(
                "rome_zk_batcher_block_age_on_arrival_seconds",
                "Batcher wall clock minus Block.timestamp at read time — observability only, a restart \
                 legitimately reads a backlog",
            )
            .buckets(vec![
                0.0, 1.0, 5.0, 15.0, 30.0, 60.0, 300.0, 3_600.0, 86_400.0,
            ]),
        )
        .unwrap();
        let solana_clock_skew_seconds = Histogram::with_opts(
            HistogramOpts::new(
                "rome_zk_batcher_solana_clock_skew_seconds",
                "Decoded open_unix_ts minus this batcher's own wall clock at OpenBatch send time — \
                 observability only, never a decision input",
            )
            .buckets(clock_skew_buckets()),
        )
        .unwrap();

        registry
            .register(Box::new(frames_sent_total.clone()))
            .unwrap();
        registry
            .register(Box::new(chunk_confirm_latency_seconds.clone()))
            .unwrap();
        registry
            .register(Box::new(resubmits_total.clone()))
            .unwrap();
        registry
            .register(Box::new(batches_finalized_total.clone()))
            .unwrap();
        registry
            .register(Box::new(batches_failed_total.clone()))
            .unwrap();
        registry.register(Box::new(bytes_per_tx.clone())).unwrap();
        registry
            .register(Box::new(compression_ratio.clone()))
            .unwrap();
        registry
            .register(Box::new(open_confirm_seconds.clone()))
            .unwrap();
        registry
            .register(Box::new(finalize_confirm_seconds.clone()))
            .unwrap();
        registry
            .register(Box::new(batch_post_seconds.clone()))
            .unwrap();
        registry
            .register(Box::new(batches_in_flight.clone()))
            .unwrap();
        registry.register(Box::new(lag_blocks.clone())).unwrap();
        registry
            .register(Box::new(cu_samples_triggered_total.clone()))
            .unwrap();
        registry
            .register(Box::new(groups_closed_total.clone()))
            .unwrap();
        registry
            .register(Box::new(oldest_unposted_block_age_seconds.clone()))
            .unwrap();
        registry
            .register(Box::new(block_age_on_arrival_seconds.clone()))
            .unwrap();
        registry
            .register(Box::new(solana_clock_skew_seconds.clone()))
            .unwrap();

        Arc::new(Self {
            registry,
            frames_sent_total,
            chunk_confirm_latency_seconds,
            resubmits_total,
            batches_finalized_total,
            batches_failed_total,
            bytes_per_tx,
            compression_ratio,
            open_confirm_seconds,
            finalize_confirm_seconds,
            batch_post_seconds,
            batches_in_flight,
            lag_blocks,
            cu_samples_triggered_total,
            groups_closed_total,
            oldest_unposted_block_age_seconds,
            block_age_on_arrival_seconds,
            solana_clock_skew_seconds,
        })
    }

    /// Renders the registry as Prometheus text exposition format (what a `/metrics` HTTP handler serves —
    /// wiring an actual listener is left to the binary, mirroring
    /// `rome_zk_sequencer::metrics`'s split between the `Metrics` struct and its own listener).
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
    fn metrics_render_without_panicking_and_include_registered_names() {
        let m = Metrics::new();
        m.frames_sent_total.inc();
        m.batches_finalized_total.inc();
        m.chunk_confirm_latency_seconds.observe(0.25);
        let rendered = m.render();
        assert!(rendered.contains("rome_zk_batcher_frames_sent_total"));
        assert!(rendered.contains("rome_zk_batcher_batches_finalized_total"));
        assert!(rendered.contains("rome_zk_batcher_chunk_confirm_latency_seconds"));
    }

    /// Every cadence metric (the five names below) is actually registered and rendered.
    #[test]
    fn cadence_metrics_are_registered_and_rendered() {
        let m = Metrics::new();
        m.open_confirm_seconds.observe(1.0);
        m.finalize_confirm_seconds.observe(2.0);
        m.batch_post_seconds.observe(3.0);
        m.batches_in_flight.set(2);
        m.lag_blocks.set(7);
        let rendered = m.render();
        for name in [
            "rome_zk_batcher_open_confirm_seconds",
            "rome_zk_batcher_finalize_confirm_seconds",
            "rome_zk_batcher_batch_post_seconds",
            "rome_zk_batcher_batches_in_flight",
            "rome_zk_batcher_lag_blocks",
        ] {
            assert!(
                rendered.contains(name),
                "missing metric {name} in:\n{rendered}"
            );
        }
    }

    /// `cu_samples_triggered_total` is registered and rendered.
    #[test]
    fn cu_samples_triggered_total_is_registered_and_rendered() {
        let m = Metrics::new();
        m.cu_samples_triggered_total.inc();
        let rendered = m.render();
        assert!(
            rendered.contains("rome_zk_batcher_cu_samples_triggered_total 1"),
            "missing metric in:\n{rendered}"
        );
    }

    /// `groups_closed_total` is label-keyed by reason; the age scenario's own reason label
    /// renders, and the gauge returns to 0 once nothing is unposted.
    #[test]
    fn age_close_metrics_are_registered_and_rendered() {
        let m = Metrics::new();
        m.groups_closed_total.with_label_values(&["cap"]).inc();
        m.groups_closed_total.with_label_values(&["size"]).inc();
        m.groups_closed_total.with_label_values(&["age"]).inc();
        m.oldest_unposted_block_age_seconds.set(45);
        m.block_age_on_arrival_seconds.observe(2.0);
        let rendered = m.render();
        assert!(
            rendered.contains(r#"rome_zk_batcher_groups_closed_total{reason="age"} 1"#),
            "missing age close in:\n{rendered}"
        );
        assert!(rendered.contains(r#"rome_zk_batcher_groups_closed_total{reason="cap"} 1"#));
        assert!(rendered.contains(r#"rome_zk_batcher_groups_closed_total{reason="size"} 1"#));
        assert!(rendered.contains("rome_zk_batcher_oldest_unposted_block_age_seconds 45"));
        assert!(rendered.contains("rome_zk_batcher_block_age_on_arrival_seconds"));

        // The gauge returns to 0 once nothing is unposted (an empty group after a close).
        m.oldest_unposted_block_age_seconds.set(0);
        let rendered = m.render();
        assert!(rendered.contains("rome_zk_batcher_oldest_unposted_block_age_seconds 0"));
    }

    /// The cadence histograms must actually cover 0.25 s .. 60 s, not just exist.
    #[test]
    fn cadence_buckets_span_a_quarter_second_to_a_minute() {
        let buckets = cadence_buckets();
        assert_eq!(buckets.first().copied(), Some(0.25));
        assert_eq!(buckets.last().copied(), Some(60.0));
    }

    /// `solana_clock_skew_seconds` is registered and rendered,
    /// spanning both signs (this crate's own `pipeline.rs` tests exercise the real observation call).
    #[test]
    fn solana_clock_skew_seconds_is_registered_and_rendered_both_signs() {
        let m = Metrics::new();
        m.solana_clock_skew_seconds.observe(7.0);
        m.solana_clock_skew_seconds.observe(-7.0);
        let rendered = m.render();
        assert!(
            rendered.contains("rome_zk_batcher_solana_clock_skew_seconds_count 2"),
            "got:\n{rendered}"
        );
        assert!(
            rendered.contains("rome_zk_batcher_solana_clock_skew_seconds_sum 0"),
            "the two observations must sum to 0, got:\n{rendered}"
        );
    }

    /// The buckets span past ±60 s on both sides so an out-of-bound skew is visible, not clipped.
    #[test]
    fn clock_skew_buckets_span_past_sixty_seconds_both_signs() {
        let buckets = clock_skew_buckets();
        assert!(buckets.first().copied().unwrap() <= -60.0);
        assert!(buckets.last().copied().unwrap() >= 60.0);
    }
}
