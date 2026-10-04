//! JSON-RPC + WebSocket server: `eth_sendRawTransaction`, `eth_chainId`, `rome_sendRawTransaction`,
//! `rome_getPreconfirmation`, and `rome_subscribe(["preconfirmations"])`.
//!
//! `eth_sendRawTransaction` deliberately does not return the moment the tx is parsed (the standard
//! Ethereum client behavior) — for a `Ready` admission it awaits the sequencer's pre-confirmation first
//! ("only then is the pre-confirmation returned to the sender") and returns the standard
//! 32-byte tx hash once that ack lands; this is also the operation `tests/e2e.rs`'s p50/p99 measurement
//! times. A `Parked` admission (behind a nonce gap) is the one exception: it
//! returns the tx hash immediately, exactly like any standard Ethereum client would for a tx that
//! hasn't landed yet — the caller polls `rome_getPreconfirmation` once the gap fills.

use alloy::primitives::{Bytes, TxHash};
use jsonrpsee::core::{RpcResult, SubscriptionResult};
use jsonrpsee::server::{PendingSubscriptionSink, RpcModule, Server, ServerConfig, ServerHandle};
use jsonrpsee::types::{ErrorCode, ErrorObjectOwned};
use serde::Serialize;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::metrics::Metrics;
use crate::sequencer::{SequencerError, SequencerHandle, SubmitOutcome};
use crate::signing::Preconfirmation;

/// RPC server hardening knobs. `max_connections` is deliberately much
/// larger than any realistic sender count — the admission queue's own bounded capacity is the real
/// admission limit, not the connection count.
#[derive(Debug, Clone, Copy)]
pub struct RpcConfig {
    pub max_request_body_size: u32,
    pub max_connections: u32,
    pub max_subscriptions_per_connection: u32,
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            max_request_body_size: 1024 * 1024, // 1 MiB
            max_connections: 10_000,
            max_subscriptions_per_connection: 8,
        }
    }
}

/// Bound to the `RpcModule` as its context: the sequencer handle plus the metrics registry, so the
/// `rome_subscribe` WS feed loop can record a `Lagged` event alongside driving the sequencer.
struct RpcContext {
    handle: SequencerHandle,
    metrics: Arc<Metrics>,
}

fn to_rpc_error(e: SequencerError) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(
        ErrorCode::ServerError(-32000).code(),
        e.to_string(),
        None::<()>,
    )
}

fn decode_raw_tx(hex_str: &str) -> Result<Bytes, ErrorObjectOwned> {
    let stripped = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    hex::decode(stripped).map(Bytes::from).map_err(|e| {
        ErrorObjectOwned::owned(ErrorCode::InvalidParams.code(), e.to_string(), None::<()>)
    })
}

fn decode_tx_hash(hex_str: &str) -> Result<TxHash, ErrorObjectOwned> {
    let stripped = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    let bytes = hex::decode(stripped).map_err(|e| {
        ErrorObjectOwned::owned(ErrorCode::InvalidParams.code(), e.to_string(), None::<()>)
    })?;
    if bytes.len() != 32 {
        return Err(ErrorObjectOwned::owned(
            ErrorCode::InvalidParams.code(),
            format!("tx hash must be 32 bytes, got {}", bytes.len()),
            None::<()>,
        ));
    }
    Ok(TxHash::from_slice(&bytes))
}

#[derive(Serialize)]
struct PreconfirmationFeedItem {
    chain_id: u64,
    block: u64,
    index: u16,
    header_hash: String,
    signature: String,
    tx_hashes: Vec<String>,
}

/// The JSON shape of a [`Preconfirmation`] over RPC — `rome_sendRawTransaction`'s `preconfirmation` field
/// and `rome_getPreconfirmation`'s result.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct PreconfirmationJson {
    block: u64,
    sub_block_index: u16,
    position: u32,
    header_hash: String,
    signature: String,
}

impl From<&Preconfirmation> for PreconfirmationJson {
    fn from(p: &Preconfirmation) -> Self {
        Self {
            block: p.block,
            sub_block_index: p.sub_block_index,
            position: p.position,
            header_hash: format!("0x{:x}", p.header_hash),
            signature: format!("0x{}", hex::encode(p.signature.as_bytes())),
        }
    }
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct RomeSendRawTxResult {
    tx_hash: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    preconfirmation: Option<PreconfirmationJson>,
}

/// Build the RPC module (methods + subscription) bound to a running sequencer.
fn build_module(ctx: RpcContext) -> RpcModule<RpcContext> {
    let mut module = RpcModule::new(ctx);

    module
        .register_async_method("eth_sendRawTransaction", |params, ctx, _| async move {
            let hex_str: String = params.one()?;
            let raw = decode_raw_tx(&hex_str)?;
            let outcome = ctx.handle.submit_raw_tx(raw).await.map_err(to_rpc_error)?;
            RpcResult::Ok(format!("0x{:x}", outcome.tx_hash()))
        })
        .expect("register eth_sendRawTransaction");

    module
        .register_async_method("rome_sendRawTransaction", |params, ctx, _| async move {
            let hex_str: String = params.one()?;
            let raw = decode_raw_tx(&hex_str)?;
            let outcome = ctx.handle.submit_raw_tx(raw).await.map_err(to_rpc_error)?;
            let result = match &outcome {
                SubmitOutcome::Preconfirmed(p) => RomeSendRawTxResult {
                    tx_hash: format!("0x{:x}", p.tx_hash),
                    status: "preconfirmed",
                    preconfirmation: Some(p.into()),
                },
                SubmitOutcome::Parked(hash) => RomeSendRawTxResult {
                    tx_hash: format!("0x{hash:x}"),
                    status: "parked",
                    preconfirmation: None,
                },
            };
            RpcResult::Ok(result)
        })
        .expect("register rome_sendRawTransaction");

    module
        .register_method("rome_getPreconfirmation", |params, ctx, _| {
            let hex_str: String = params.one()?;
            let tx_hash = decode_tx_hash(&hex_str)?;
            // A direct, synchronous read — no actor round trip.
            let found = ctx.handle.get_preconfirmation(tx_hash);
            RpcResult::Ok(found.as_ref().map(PreconfirmationJson::from))
        })
        .expect("register rome_getPreconfirmation");

    module
        .register_method("eth_chainId", |_params, ctx, _| {
            RpcResult::Ok(format!("0x{:x}", ctx.handle.chain_id()))
        })
        .expect("register eth_chainId");

    module
        .register_subscription(
            "rome_subscribe",
            "rome_subscription",
            "rome_unsubscribe",
            |params, pending: PendingSubscriptionSink, ctx, _| async move {
                let topics: Vec<String> = params.one().unwrap_or_default();
                if topics.first().map(String::as_str) != Some("preconfirmations") {
                    pending
                        .reject(ErrorObjectOwned::owned(
                            ErrorCode::InvalidParams.code(),
                            "only the \"preconfirmations\" topic is supported",
                            None::<()>,
                        ))
                        .await;
                    return SubscriptionResult::Ok(());
                }
                let sink = pending.accept().await?;
                let mut feed = ctx.handle.subscribe_preconfirmations();
                let chain_id = ctx.handle.chain_id();
                loop {
                    // A subscriber that falls behind the broadcast
                    // channel's capacity gets `Lagged(n)` — count it and keep going (broadcast already
                    // skipped it ahead to the oldest still-buffered sub-block) rather than dropping the
                    // subscriber, which the old `while let Ok(..) = feed.recv().await` loop did.
                    match feed.recv().await {
                        Ok(sealed) => {
                            let item = PreconfirmationFeedItem {
                                chain_id,
                                block: sealed.header.block,
                                index: sealed.header.index,
                                header_hash: format!("0x{:x}", sealed.header_hash),
                                signature: format!(
                                    "0x{}",
                                    hex::encode(sealed.signature.as_bytes())
                                ),
                                tx_hashes: sealed
                                    .included
                                    .iter()
                                    .map(|h| format!("0x{h:x}"))
                                    .collect(),
                            };
                            let raw = serde_json::value::to_raw_value(&item)
                                .expect("serialize preconf feed item");
                            if sink.send(raw).await.is_err() {
                                break; // subscriber disconnected
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            ctx.metrics.ws_feed_lagged_total.inc_by(n);
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                SubscriptionResult::Ok(())
            },
        )
        .expect("register rome_subscribe");

    module
}

/// Start the JSON-RPC + WebSocket server on `addr`, bound to `handle`. Both HTTP (for
/// `eth_sendRawTransaction`/`eth_chainId`/`rome_sendRawTransaction`/`rome_getPreconfirmation`) and
/// WebSocket (for `rome_subscribe`) are served on the same socket — jsonrpsee negotiates the protocol per
/// connection.
pub async fn serve(
    addr: SocketAddr,
    handle: SequencerHandle,
    rpc_config: RpcConfig,
    metrics: Arc<Metrics>,
) -> std::io::Result<(SocketAddr, ServerHandle)> {
    // Bounded request body size, connection count, and per-connection
    // subscription count — jsonrpsee's own defaults (100 connections, unlimited subscriptions per
    // connection) are sized for a handful of long-lived RPC clients, far below what a sequencer with many
    // independent senders needs (measured: `tests/e2e.rs`'s 50 concurrent senders each opening their own
    // HTTP connections hit `SendRequest` errors at the old hardcoded default of 65,536... no — at
    // jsonrpsee's built-in default of 100).
    let server = Server::builder()
        .set_config(
            ServerConfig::builder()
                .max_request_body_size(rpc_config.max_request_body_size)
                .max_connections(rpc_config.max_connections)
                .max_subscriptions_per_connection(rpc_config.max_subscriptions_per_connection)
                .build(),
        )
        .build(addr)
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let local_addr = server
        .local_addr()
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let module = build_module(RpcContext { handle, metrics });
    let server_handle = server.start(module);
    Ok((local_addr, server_handle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admission::AdmissionConfig;
    use crate::executor::MockExecutor;
    use crate::sealer::ResumePoint;
    use crate::sequencer::{spawn, SpawnConfig};
    use crate::testutil::signed_raw_tx;
    use alloy::primitives::Address;
    use alloy::signers::local::PrivateKeySigner;
    use jsonrpsee::ws_client::WsClientBuilder;
    use std::time::Duration;
    use tempfile::tempdir;

    async fn start_test_server() -> (SocketAddr, ServerHandle, tempfile::TempDir) {
        start_test_server_with_config(RpcConfig::default()).await
    }

    async fn start_test_server_with_config(
        rpc_config: RpcConfig,
    ) -> (SocketAddr, ServerHandle, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let (handle, _join) = spawn(
            MockExecutor::new(),
            SpawnConfig {
                admission: AdmissionConfig {
                    chain_id: 1,
                    ..AdmissionConfig::default()
                },
                log_dir: dir.path().to_path_buf(),
                blocks_per_segment: 1_000,
                signer: PrivateKeySigner::random(),
                resume: ResumePoint::default(),
                metrics: Metrics::new(),
                seal_period: Duration::from_millis(10),
                preconf_feed_capacity: 64,
                sub_block_gas_limit: crate::executor::DEFAULT_SUB_BLOCK_GAS_LIMIT,
                block_gas_limit: crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
                fee_recipient: Address::ZERO,
                sub_blocks_per_block: crate::sealer::SUB_BLOCKS_PER_BLOCK,
                empty_block_interval_secs: 0,
                deposits: None,
            },
        )
        .unwrap();
        let (addr, server_handle) = serve(
            "127.0.0.1:0".parse().unwrap(),
            handle,
            rpc_config,
            Metrics::new(),
        )
        .await
        .unwrap();
        (addr, server_handle, dir)
    }

    #[tokio::test]
    async fn eth_send_raw_transaction_returns_tx_hash_after_preconfirmation() {
        let (addr, _server, _dir) = start_test_server().await;
        let client = WsClientBuilder::default()
            .build(format!("ws://{addr}"))
            .await
            .unwrap();

        let sender = PrivateKeySigner::random();
        let raw = signed_raw_tx(&sender, 1, 0);
        let raw_hex = format!("0x{}", hex::encode(&raw));

        use jsonrpsee::core::client::ClientT;
        let result: String = client
            .request("eth_sendRawTransaction", jsonrpsee::rpc_params![raw_hex])
            .await
            .unwrap();
        assert!(result.starts_with("0x"));
        assert_eq!(result.len(), 66);
    }

    #[tokio::test]
    async fn eth_chain_id_returns_configured_chain() {
        let (addr, _server, _dir) = start_test_server().await;
        let client = WsClientBuilder::default()
            .build(format!("ws://{addr}"))
            .await
            .unwrap();
        use jsonrpsee::core::client::ClientT;
        let result: String = client
            .request("eth_chainId", jsonrpsee::rpc_params![])
            .await
            .unwrap();
        assert_eq!(result, "0x1");
    }

    #[tokio::test]
    async fn rome_subscribe_streams_a_sealed_sub_block_after_a_tx() {
        let (addr, _server, _dir) = start_test_server().await;
        let client = WsClientBuilder::default()
            .build(format!("ws://{addr}"))
            .await
            .unwrap();

        use jsonrpsee::core::client::{ClientT, SubscriptionClientT};
        let mut sub: jsonrpsee::core::client::Subscription<serde_json::Value> = client
            .subscribe(
                "rome_subscribe",
                jsonrpsee::rpc_params![vec!["preconfirmations"]],
                "rome_unsubscribe",
            )
            .await
            .unwrap();

        let sender = PrivateKeySigner::random();
        let raw = signed_raw_tx(&sender, 1, 0);
        let raw_hex = format!("0x{}", hex::encode(&raw));
        let _: String = client
            .request("eth_sendRawTransaction", jsonrpsee::rpc_params![raw_hex])
            .await
            .unwrap();

        // The feed may deliver empty sub-blocks (sealed on the 10ms cadence with nothing queued) before
        // the one carrying our tx — wait for the one that actually includes it.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let found = loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                remaining > Duration::ZERO,
                "subscription never delivered our tx's sub-block"
            );
            let item = tokio::time::timeout(remaining, sub.next())
                .await
                .expect("subscription must deliver the sealed sub-block")
                .unwrap()
                .unwrap();
            if !item["tx_hashes"].as_array().unwrap().is_empty() {
                break item;
            }
        };
        assert_eq!(found["tx_hashes"].as_array().unwrap().len(), 1);
    }

    /// A `Parked` tx (behind a nonce gap) must return from
    /// `rome_sendRawTransaction` immediately — well under one sub-block period — with `status: "parked"`
    /// and no `preconfirmation`, not wait for the gap to fill.
    #[tokio::test]
    async fn rome_send_raw_transaction_returns_immediately_for_a_parked_tx() {
        // A long seal period (2s) rather than the shared helper's 10ms, so the qualitative claim under
        // test — a Parked reply comes back without ever waiting on the sealing timer — has a wide safety
        // margin against scheduler jitter from other tests running in parallel in the same process (this
        // crate's own `tests/e2e.rs` notes the same class of noise for its own latency measurement,
        // hence `#[ignore]`d there under `--release`). If this bug regressed and Parked started waiting
        // for a tick, the round trip would take seconds, not tens of milliseconds — nowhere near the
        // assertion's bound below even under heavy parallel load.
        let dir = tempdir().unwrap();
        let (handle, _join) = spawn(
            MockExecutor::new(),
            SpawnConfig {
                admission: AdmissionConfig {
                    chain_id: 1,
                    ..AdmissionConfig::default()
                },
                log_dir: dir.path().to_path_buf(),
                blocks_per_segment: 1_000,
                signer: PrivateKeySigner::random(),
                resume: ResumePoint::default(),
                metrics: Metrics::new(),
                seal_period: Duration::from_secs(2),
                preconf_feed_capacity: 16,
                sub_block_gas_limit: crate::executor::DEFAULT_SUB_BLOCK_GAS_LIMIT,
                block_gas_limit: crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
                fee_recipient: Address::ZERO,
                sub_blocks_per_block: crate::sealer::SUB_BLOCKS_PER_BLOCK,
                empty_block_interval_secs: 0,
                deposits: None,
            },
        )
        .unwrap();
        let (addr, _server) = serve(
            "127.0.0.1:0".parse().unwrap(),
            handle,
            RpcConfig::default(),
            Metrics::new(),
        )
        .await
        .unwrap();

        let client = WsClientBuilder::default()
            .build(format!("ws://{addr}"))
            .await
            .unwrap();
        use jsonrpsee::core::client::ClientT;

        // Warm the connection up (TCP/WS handshake cost, first-request JIT) before timing, exactly as
        // `tests/e2e.rs` does for its own latency measurement — otherwise a cold first request measures
        // connection setup, not the sequencer's own behavior.
        let _: String = client
            .request("eth_chainId", jsonrpsee::rpc_params![])
            .await
            .unwrap();

        let sender = PrivateKeySigner::random();
        // Nonce 5 with nothing admitted yet for nonce 0..4 — parks behind a gap.
        let raw = signed_raw_tx(&sender, 1, 5);
        let raw_hex = format!("0x{}", hex::encode(&raw));

        let started = std::time::Instant::now();
        let result: serde_json::Value = client
            .request("rome_sendRawTransaction", jsonrpsee::rpc_params![raw_hex])
            .await
            .unwrap();
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(1),
            "a Parked tx must return immediately, without waiting on the {:?} sealing timer — took \
             {elapsed:?}",
            Duration::from_secs(2)
        );
        assert_eq!(result["status"], "parked");
        assert!(result.get("preconfirmation").is_none() || result["preconfirmation"].is_null());
        assert!(result["txHash"].as_str().unwrap().starts_with("0x"));
    }

    /// End to end: a gap tx parks (returned immediately), the gap then
    /// fills, and `rome_getPreconfirmation` serves the now-included tx's pre-confirmation from the
    /// bounded recent-inclusion map — whose signature recovers to the sequencer's own address under the
    /// sequencer's signing domain.
    #[tokio::test]
    async fn parked_tx_preconfirmation_is_servable_once_the_gap_fills() {
        let dir = tempdir().unwrap();
        let signer = PrivateKeySigner::random();
        let sequencer_address = signer.address();
        let (handle, _join) = spawn(
            MockExecutor::new(),
            SpawnConfig {
                admission: AdmissionConfig {
                    chain_id: 1,
                    ..AdmissionConfig::default()
                },
                log_dir: dir.path().to_path_buf(),
                blocks_per_segment: 1_000,
                signer,
                resume: ResumePoint::default(),
                metrics: Metrics::new(),
                seal_period: Duration::from_millis(10),
                preconf_feed_capacity: 64,
                sub_block_gas_limit: crate::executor::DEFAULT_SUB_BLOCK_GAS_LIMIT,
                block_gas_limit: crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
                fee_recipient: Address::ZERO,
                sub_blocks_per_block: crate::sealer::SUB_BLOCKS_PER_BLOCK,
                empty_block_interval_secs: 0,
                deposits: None,
            },
        )
        .unwrap();
        let (addr, _server) = serve(
            "127.0.0.1:0".parse().unwrap(),
            handle,
            RpcConfig::default(),
            Metrics::new(),
        )
        .await
        .unwrap();

        let client = WsClientBuilder::default()
            .build(format!("ws://{addr}"))
            .await
            .unwrap();
        use jsonrpsee::core::client::ClientT;

        let sender = PrivateKeySigner::random();
        let gap_raw = signed_raw_tx(&sender, 1, 1);
        let gap_hex = format!("0x{}", hex::encode(&gap_raw));
        let parked: serde_json::Value = client
            .request("rome_sendRawTransaction", jsonrpsee::rpc_params![gap_hex])
            .await
            .unwrap();
        assert_eq!(parked["status"], "parked");
        let gap_tx_hash = parked["txHash"].as_str().unwrap().to_string();

        // rome_getPreconfirmation must be None while the gap is still open.
        let before: serde_json::Value = client
            .request(
                "rome_getPreconfirmation",
                jsonrpsee::rpc_params![gap_tx_hash.clone()],
            )
            .await
            .unwrap();
        assert!(before.is_null());

        // Fill the gap — nonce 0.
        let fill_raw = signed_raw_tx(&sender, 1, 0);
        let fill_hex = format!("0x{}", hex::encode(&fill_raw));
        let _: serde_json::Value = client
            .request("rome_sendRawTransaction", jsonrpsee::rpc_params![fill_hex])
            .await
            .unwrap();

        // Poll rome_getPreconfirmation until the now-included gap tx's preconf shows up.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let preconf = loop {
            let result: serde_json::Value = client
                .request(
                    "rome_getPreconfirmation",
                    jsonrpsee::rpc_params![gap_tx_hash.clone()],
                )
                .await
                .unwrap();
            if !result.is_null() {
                break result;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "rome_getPreconfirmation never served the filled gap tx's preconfirmation"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        };

        let header_hash: alloy::primitives::B256 =
            preconf["headerHash"].as_str().unwrap().parse().unwrap();
        let sig_hex = preconf["signature"].as_str().unwrap();
        let sig_bytes = hex::decode(sig_hex.strip_prefix("0x").unwrap()).unwrap();
        let mut sig_array = [0u8; 65];
        sig_array.copy_from_slice(&sig_bytes);
        let signature = alloy::primitives::Signature::from_raw_array(&sig_array).unwrap();

        // The domain-separated signing hash — recovering against the bare header hash would not match,
        // proving this test actually exercises the signing domain, not just "some signature".
        let signing_hash = alloy::primitives::keccak256(
            [crate::header::SIGNING_DOMAIN, header_hash.as_slice()].concat(),
        );
        let recovered = crate::signing::recover_header_signer(signing_hash, &signature).unwrap();
        assert_eq!(recovered, sequencer_address);
    }

    /// `eth_sendRawTransaction` must return a proper JSON-RPC error
    /// (never panic the server) for an admission failure, and the server must keep serving subsequent
    /// requests afterward.
    #[tokio::test]
    async fn admission_failure_returns_rpc_error_and_server_keeps_serving() {
        let (addr, _server, _dir) = start_test_server().await;
        let client = WsClientBuilder::default()
            .build(format!("ws://{addr}"))
            .await
            .unwrap();
        use jsonrpsee::core::client::ClientT;

        let sender = PrivateKeySigner::random();
        let wrong_chain_raw = signed_raw_tx(&sender, 999, 0);
        let wrong_chain_hex = format!("0x{}", hex::encode(&wrong_chain_raw));
        let err = client
            .request::<String, _>(
                "eth_sendRawTransaction",
                jsonrpsee::rpc_params![wrong_chain_hex],
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("chain id") || err.to_string().contains("-32000"),
            "expected an admission-failure error, got {err}"
        );

        // The server must still be alive and serving.
        let chain_id: String = client
            .request("eth_chainId", jsonrpsee::rpc_params![])
            .await
            .unwrap();
        assert_eq!(chain_id, "0x1");
    }

    /// A request body over `max_request_body_size` must be rejected, not
    /// crash or hang the server — and the connection/server must remain usable afterward.
    #[tokio::test]
    async fn oversized_request_body_is_rejected_not_crashed() {
        let (addr, _server, _dir) = start_test_server_with_config(RpcConfig {
            max_request_body_size: 256,
            ..RpcConfig::default()
        })
        .await;

        let client = WsClientBuilder::default()
            .build(format!("ws://{addr}"))
            .await
            .unwrap();
        use jsonrpsee::core::client::ClientT;

        // A tx hex string far bigger than the 256-byte cap.
        let oversized_hex = format!("0x{}", "ab".repeat(1024));
        let result = client
            .request::<String, _>(
                "eth_sendRawTransaction",
                jsonrpsee::rpc_params![oversized_hex],
            )
            .await;
        assert!(result.is_err(), "an oversized request must be rejected");

        // A fresh connection must still be served — the server itself did not crash. (jsonrpsee's own
        // WS client can end up in a broken local state after a request the server refused outright at
        // the transport layer, which is a client-library detail, not evidence the server crashed; a new
        // client against the same still-running server is the real proof.)
        let fresh_client = WsClientBuilder::default()
            .build(format!("ws://{addr}"))
            .await
            .unwrap();
        let chain_id: String = fresh_client
            .request("eth_chainId", jsonrpsee::rpc_params![])
            .await
            .unwrap();
        assert_eq!(chain_id, "0x1");
    }
}
