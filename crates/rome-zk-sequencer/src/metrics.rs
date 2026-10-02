//! Prometheus metrics, served as plain text on `/metrics`.
//!
//! The design names four numbers the sequencer must expose against its own performance budget:
//! pre-confirmation latency, admission queue depth, sub-block seal lateness (against the 50 ms
//! deadline), and txs per sub-block.

use prometheus::{Encoder, Histogram, HistogramOpts, IntCounter, IntGauge, Registry, TextEncoder};
use std::net::SocketAddr;
use std::sync::Arc;

pub struct Metrics {
    registry: Registry,
    /// Time from admission-queue entry to the ack (post-fsync) being sent to the caller.
    pub preconf_latency_seconds: Histogram,
    pub queue_depth: IntGauge,
    /// How late a sub-block sealed past its configured deadline (the chain's
    /// `[profile].sub_block_ms`, design default 50 ms), in seconds (0 when on time).
    pub seal_lateness_seconds: Histogram,
    /// Wall time of `LogWriter::append`'s own `write_all` + `sync_data` call,
    /// per sub-block — isolates the ordered log's fsync cost from the rest of `seal_sub_block` (drain,
    /// execution, signing), to test whether it collides with the reth executor's own periodic MDBX
    /// commit fsync (the suspected I/O contention behind the residual idle-host p99).
    pub log_fsync_duration_seconds: Histogram,
    pub txs_per_sub_block: Histogram,
    pub sub_blocks_sealed_total: IntCounter,
    pub blocks_sealed_total: IntCounter,
    /// Total txs carried forward because the executor didn't reach them before `gas_limit`/`deadline`.
    pub not_executed_carried_total: IntCounter,
    /// Total sealed sub-blocks a WS `rome_subscribe` feed subscriber missed because it fell behind the
    /// broadcast channel's capacity — the subscriber stays connected
    /// (broadcast's standard lagged behavior just skips ahead); this only counts how often it happens.
    pub ws_feed_lagged_total: IntCounter,
    /// Named `reth_blockchain_tree_canonical_chain_height` — the exact metric
    /// Tiber's reth Grafana dashboard already reads. In-process reth (`src/
    /// node.rs`) never runs reth's own `BlockchainTree` component (this crate bypasses it — see
    /// `rome-zk-executor-reth`'s module doc), so nothing emits that metric under reth's own
    /// instrumentation; this gauge is sourced from the sequencer's own head instead (reth block
    /// number = sequencer block number + 1) so the existing dashboard panel keeps working across the
    /// Tiber compose swap, without claiming reth's tree component is actually running.
    pub reth_canonical_chain_height: IntGauge,
    /// Named `reth_transaction_pool_pending_pool_transactions`, for the same dashboard-
    /// continuity reason — always 0 and never updated: this design's `noTxPool` semantics mean there
    /// really is no pool to report a size for (not an approximation of one).
    pub reth_pending_pool_transactions: IntGauge,
    /// Total ticks that touched nothing — no transactions ready, and this chain's
    /// `empty_block_interval_secs` either says "never" (0) or is not yet due. The idle-chain liveness
    /// signal: nonzero and climbing is exactly the expected shape of a quiet chain.
    pub idle_ticks_total: IntCounter,
    /// Gas used per sealed block (empty or not) — the gas/s source for the load
    /// programme (`rate(block_gas_used_sum)`).
    pub block_gas_used: Histogram,
    /// How many seconds a sealed block's resolved EVM timestamp ran ahead of its own first
    /// sub-block's wall-clock reading — the catch-up debt `resolve_block_timestamp_secs`'s `prev + 1`
    /// clamp can introduce. Zero on every ordinary block; the drift-bound measurement this feeds.
    pub block_timestamp_ahead_seconds: Histogram,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        let registry = Registry::new();

        let preconf_latency_seconds = Histogram::with_opts(
            HistogramOpts::new(
                "rome_zk_sequencer_preconf_latency_seconds",
                "Latency from admission to pre-confirmation ack",
            )
            .buckets(vec![0.001, 0.005, 0.01, 0.02, 0.05, 0.075, 0.1, 0.2, 0.5]),
        )
        .unwrap();
        let queue_depth = IntGauge::new(
            "rome_zk_sequencer_admission_queue_depth",
            "Number of txs currently ready in the admission queue",
        )
        .unwrap();
        let seal_lateness_seconds = Histogram::with_opts(
            HistogramOpts::new(
                "rome_zk_sequencer_seal_lateness_seconds",
                "How far a sub-block seal ran past its configured deadline (design default 50ms)",
            )
            .buckets(vec![0.0, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2]),
        )
        .unwrap();
        let log_fsync_duration_seconds = Histogram::with_opts(
            HistogramOpts::new(
                "rome_zk_sequencer_log_fsync_duration_seconds",
                "Wall time of LogWriter::append's own write_all + sync_data call, per sub-block",
            )
            .buckets(vec![
                0.0005, 0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2,
            ]),
        )
        .unwrap();
        let txs_per_sub_block = Histogram::with_opts(HistogramOpts::new(
            "rome_zk_sequencer_txs_per_sub_block",
            "Number of txs included per sealed sub-block",
        ))
        .unwrap();
        let sub_blocks_sealed_total = IntCounter::new(
            "rome_zk_sequencer_sub_blocks_sealed_total",
            "Total sub-blocks sealed",
        )
        .unwrap();
        let blocks_sealed_total = IntCounter::new(
            "rome_zk_sequencer_blocks_sealed_total",
            "Total blocks sealed",
        )
        .unwrap();
        let not_executed_carried_total = IntCounter::new(
            "rome_zk_sequencer_not_executed_carried_total",
            "Total txs carried forward because the executor didn't reach them (gas_limit/deadline)",
        )
        .unwrap();
        let ws_feed_lagged_total = IntCounter::new(
            "rome_zk_sequencer_ws_feed_lagged_total",
            "Total sealed sub-blocks a rome_subscribe WS feed subscriber missed by falling behind",
        )
        .unwrap();
        let reth_canonical_chain_height = IntGauge::new(
            "reth_blockchain_tree_canonical_chain_height",
            "Canonical chain height (sourced from the sequencer's own head — see Metrics field doc)",
        )
        .unwrap();
        let reth_pending_pool_transactions = IntGauge::new(
            "reth_transaction_pool_pending_pool_transactions",
            "Pending pool transactions (always 0 — noTxPool design, see Metrics field doc)",
        )
        .unwrap();
        let idle_ticks_total = IntCounter::new(
            "rome_zk_sequencer_idle_ticks_total",
            "Total ticks that sealed nothing (no transactions, empty_block_interval_secs not due)",
        )
        .unwrap();
        let block_gas_used = Histogram::with_opts(HistogramOpts::new(
            "rome_zk_sequencer_block_gas_used",
            "Gas used per sealed block (empty or not) — the gas/s source for the load programme",
        ))
        .unwrap();
        let block_timestamp_ahead_seconds = Histogram::with_opts(
            HistogramOpts::new(
                "rome_zk_sequencer_block_timestamp_ahead_seconds",
                "Whole seconds a sealed block's EVM timestamp ran ahead of its own first sub-block's \
                 second (the prev+1 catch-up debt; integer-second floor, 0 when the wall \
                 clock won)",
            )
            .buckets(vec![0.0, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0]),
        )
        .unwrap();

        registry
            .register(Box::new(preconf_latency_seconds.clone()))
            .unwrap();
        registry.register(Box::new(queue_depth.clone())).unwrap();
        registry
            .register(Box::new(seal_lateness_seconds.clone()))
            .unwrap();
        registry
            .register(Box::new(log_fsync_duration_seconds.clone()))
            .unwrap();
        registry
            .register(Box::new(txs_per_sub_block.clone()))
            .unwrap();
        registry
            .register(Box::new(sub_blocks_sealed_total.clone()))
            .unwrap();
        registry
            .register(Box::new(blocks_sealed_total.clone()))
            .unwrap();
        registry
            .register(Box::new(not_executed_carried_total.clone()))
            .unwrap();
        registry
            .register(Box::new(ws_feed_lagged_total.clone()))
            .unwrap();
        registry
            .register(Box::new(reth_canonical_chain_height.clone()))
            .unwrap();
        registry
            .register(Box::new(reth_pending_pool_transactions.clone()))
            .unwrap();
        // Always 0, set once — see this field's doc.
        reth_pending_pool_transactions.set(0);
        registry
            .register(Box::new(idle_ticks_total.clone()))
            .unwrap();
        registry.register(Box::new(block_gas_used.clone())).unwrap();
        registry
            .register(Box::new(block_timestamp_ahead_seconds.clone()))
            .unwrap();

        Arc::new(Self {
            registry,
            preconf_latency_seconds,
            queue_depth,
            seal_lateness_seconds,
            log_fsync_duration_seconds,
            txs_per_sub_block,
            sub_blocks_sealed_total,
            blocks_sealed_total,
            not_executed_carried_total,
            ws_feed_lagged_total,
            reth_canonical_chain_height,
            reth_pending_pool_transactions,
            idle_ticks_total,
            block_gas_used,
            block_timestamp_ahead_seconds,
        })
    }

    fn render(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        TextEncoder::new()
            .encode(&self.registry.gather(), &mut buf)
            .expect("prometheus encode");
        buf
    }
}

/// Serve `GET /metrics` on `addr` until the process exits. Any other path gets a 404.
/// The responder itself (the listener loop, the one-request read, the 200/404 framing) moved into
/// `rome-zk-metrics-http` — a reth-free crate `rome-zk-batcher` can depend on too — so this is now a
/// thin call into it; this crate's own behaviour (and the test below) is unchanged.
pub async fn serve_metrics(addr: SocketAddr, metrics: Arc<Metrics>) -> std::io::Result<()> {
    rome_zk_metrics_http::serve(addr, move || metrics.render()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn metrics_endpoint_serves_registered_counters() {
        let metrics = Metrics::new();
        metrics.sub_blocks_sealed_total.inc();
        // Reserve a free ephemeral loopback port without naming the listener type this crate no longer
        // owns (`rome-zk-metrics-http` is the one home for it now) — a throwaway UDP
        // bind on the same address serves the same "find a free port" trick without it.
        let probe = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);
        let m = metrics.clone();
        tokio::spawn(async move { serve_metrics(addr, m).await.unwrap() });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /metrics HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut resp = Vec::new();
        stream.read_to_end(&mut resp).await.unwrap();
        let text = String::from_utf8_lossy(&resp);
        assert!(text.contains("200 OK"));
        assert!(text.contains("rome_zk_sequencer_sub_blocks_sealed_total 1"));
    }

    /// After the interval-5 idle-block scenario (4 empty blocks
    /// close over 20s of idle ticks, `sealer::tests::interval_n_seals_an_empty_block_at_most_every_n_seconds`'s
    /// own scenario), the rendered `/metrics` text shows `idle_ticks_total` > 0 and exactly one
    /// `block_gas_used` observation per sealed block — recording the same metrics
    /// `sequencer::Actor::tick` records for each `Tick` outcome, without spinning up a full actor
    /// against real wall-clock timers (this crate's `now_micros` reads the real system clock, which a
    /// paused tokio test cannot control).
    #[tokio::test]
    async fn idle_ticks_and_block_gas_used_appear_in_the_rendered_metrics_text() {
        use crate::executor::{MockExecutor, SubBlockLimits};
        use crate::preconf::ChannelSink;
        use crate::sealer::{
            ResumePoint, SealerState, Tick, DEFAULT_BLOCK_GAS_LIMIT, SUB_BLOCKS_PER_BLOCK,
        };
        use alloy::primitives::Address;
        use alloy::signers::local::PrivateKeySigner;
        use tempfile::tempdir;

        let metrics = Metrics::new();
        let dir = tempdir().unwrap();
        let mut sealer = SealerState::new(
            MockExecutor::new(),
            crate::log::LogWriter::open(dir.path(), 1_000).unwrap(),
            PrivateKeySigner::random(),
            ChannelSink::new(16),
            1,
            DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            SUB_BLOCKS_PER_BLOCK,
            ResumePoint::default(),
        )
        .with_empty_block_interval_secs(5);

        let base_ts = 1_757_000_000_000_000u64;
        let mut blocks_sealed = 0u32;
        for i in 0..400u64 {
            let tick = sealer
                .seal_sub_block(vec![], base_ts + i * 50_000, SubBlockLimits::unbounded())
                .await
                .unwrap();
            match tick {
                Tick::Idle { .. } => metrics.idle_ticks_total.inc(),
                Tick::Sealed(result) => {
                    if let Some(gas) = result.block_gas_used {
                        metrics.block_gas_used.observe(gas as f64);
                        blocks_sealed += 1;
                    }
                    if let Some(ahead) = result.block_timestamp_ahead_seconds {
                        metrics.block_timestamp_ahead_seconds.observe(ahead);
                    }
                }
            }
        }
        assert_eq!(
            blocks_sealed, 4,
            "the interval-5 scenario's own expected block count"
        );

        let probe = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);
        let m = metrics.clone();
        tokio::spawn(async move { serve_metrics(addr, m).await.unwrap() });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /metrics HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut resp = Vec::new();
        stream.read_to_end(&mut resp).await.unwrap();
        let text = String::from_utf8_lossy(&resp);

        assert!(
            !text.contains("rome_zk_sequencer_idle_ticks_total 0"),
            "idle_ticks_total must be > 0 after an idle-heavy run: {text}"
        );
        assert!(
            text.contains("rome_zk_sequencer_block_gas_used_count 4"),
            "one block_gas_used observation per sealed block (4): {text}"
        );
    }
}
