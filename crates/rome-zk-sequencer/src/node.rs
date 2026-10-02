//! The standalone reth JSON-RPC surface — `eth_*`/`net`/`web3`/`debug`/`trace`/`ots`
//! served straight off this executor's own MDBX (`RethExecutor::rpc_provider`), with
//! `eth_sendRawTransaction`/`eth_getTransactionCount("pending", ..)` overridden onto the sequencer
//! and `rome_sendRawTransaction`/`rome_getPreconfirmation`/`rome_subscribe` added alongside them —
//! on the same http/ws servers, exactly the "drop-in replacement for the stock reth container"
//! contract (the stock `reth` service: 8545 http, 8546 ws).
//!
//! ## Why `RpcModuleBuilder` over a `ProviderFactory`, not a launched `NodeBuilder`
//!
//! See `rome-zk-executor-reth`'s module doc for the full citation;
//! in short, this crate's `RethExecutor` is the sole block producer via direct library calls
//! (`ConfigureEvm`/`BlockBuilder`), never reth's own networking/sync/engine actor — so there is no
//! `NodeBuilder`-launched node to attach RPC to. `examples/rpc-db/src/main.rs` (reth v2.5.2 tree) is
//! reth's own template for exactly this shape: `RpcModuleBuilder::default().with_provider(..)
//! .with_noop_pool().with_noop_network().with_executor(..).with_evm_config(..)
//! .with_consensus(..)`, an `EthApiBuilder` built from the same components, and
//! `TransportRpcModuleConfig::default().with_http([RethRpcModule::Eth, ..])` picking namespaces —
//! every namespace (`eth`, `net`, `web3`, `debug`, `trace`, `txpool`, `ots`) is built from that one
//! `EthApi` instance by `RpcModuleBuilder::build`'s `create_transport_rpc_modules`
//! (`crates/rpc/rpc-builder/src/lib.rs` ~line 1014: `RethRpcModule::Ots =>
//! OtterscanApi::new(eth_api.clone())..`), so nothing beyond the citations below is needed to get the
//! full read surface for free, reading whatever `RethExecutor::seal_block` most recently committed.
//! (This module uses `.with_network(..)`/a real chain-id-carrying `NoopNetwork` rather than the
//! example's `.with_noop_network()` — see `serve`'s own comment: `NoopNetwork::default()`
//! hard-codes chain id 1, which several `eth_*` methods, `eth_chainId` included, read straight off
//! the network component.)
//!
//! `examples/node-custom-rpc/src/main.rs` is the override/extend pattern:
//! `TransportRpcModules::merge_configured` adds a brand-new namespace; this module additionally uses
//! `TransportRpcModules::replace_configured` (`crates/rpc/rpc-builder/src/lib.rs`'s
//! `remove-then-merge` composition) to override `eth_sendRawTransaction`/`eth_getTransactionCount`,
//! which already exist in the `eth` module `RpcModuleBuilder` just built.
//!
//! ## The `BlockchainProvider` must be `RethExecutor`'s OWN instance
//!
//! `serve` takes an already-built `BlockchainProvider<RethTypes>` (`RethExecutor::rpc_provider`) —
//! it must NEVER construct its own via `BlockchainProvider::new(factory)` from a bare
//! `ProviderFactory`. `BlockchainProvider::clone` is cheap (`ProviderFactory` is an `Arc<DatabaseEnv>`
//! clone; `CanonicalInMemoryState` is itself `Arc<CanonicalInMemoryStateInner>` — reth's
//! `crates/chain-state/src/in_memory.rs`), but only clones of the SAME instance share the same
//! `Arc<CanonicalInMemoryStateInner>` — `BlockchainProvider::new` on a fresh `ProviderFactory` builds a
//! brand-new, independent one. An earlier attempt tried exactly the wrong shape (a bare factory handed here, a
//! fresh `BlockchainProvider::new` built from it) and `eth_blockNumber`/`eth_getBlockByNumber("latest",
//! ..)` stayed stuck at genesis for the whole 1,300s RSS run: `RethExecutor::seal_block` was advancing
//! ITS OWN `blockchain_provider`'s canonical head, which this module's independent instance never saw.
//!
//! **What "latest"/"safe"/"finalized" resolve to**: all three are set to the SAME header, right after
//! each block's persist completes (`RethExecutor::seal_block`'s spawned task) — this is a single
//! sequencer with no reorgs, so there is nothing today that could make "safe" or "finalized" lag
//! behind "latest". This is a real placeholder, not an oversight: the design intends
//! `finalized` to eventually track the settlement watcher (the point a block's root is actually
//! posted and accepted on Solana) rather than the sequencer's own local head — a client that keys
//! safety on `finalized` today gets no more assurance than `latest` gives it.

use std::net::SocketAddr;
use std::sync::Arc;

use alloy::primitives::{Address, Bytes, TxHash, U256};
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_network::Ethereum;
use jsonrpsee::core::{RpcResult, SubscriptionResult};
use jsonrpsee::server::{PendingSubscriptionSink, ServerConfigBuilder};
use jsonrpsee::types::{ErrorCode, ErrorObjectOwned};
use reth_chainspec::{ChainSpec, EthChainSpec};
use reth_ethereum_consensus::EthBeaconConsensus;
use reth_evm_ethereum::EthEvmConfig;
use reth_network_api::noop::NoopNetwork;
use reth_provider::providers::BlockchainProvider;
use reth_rpc::EthApi;
use reth_rpc::EthApiBuilder;
use reth_rpc_builder::{
    RethRpcModule, RpcModuleBuilder, RpcServerConfig, RpcServerHandle, TransportRpcModuleConfig,
};
use reth_rpc_convert::RpcConverter;
use reth_rpc_eth_api::node::RpcNodeCoreAdapter;
use reth_rpc_eth_api::EthApiServer;
use reth_rpc_eth_types::receipt::EthReceiptConverter;
use reth_transaction_pool::noop::NoopTransactionPool;
use rome_zk_executor_reth::RethTypes;
use tokio::sync::broadcast;

use crate::rpc::RpcConfig;
use crate::sequencer::{SequencerHandle, SubmitOutcome};
use crate::signing::Preconfirmation;

/// The concrete `BlockchainProvider` + `EthApi` this crate's RPC node builds — reth's own
/// live-canonical-state overlay (`CanonicalInMemoryState`) never carries in-memory BLOCKS here (this
/// module's doc: nothing is ever appended to `in_memory_state.blocks`/`numbers` — every sealed block
/// is fully committed to the real MDBX before `seal_block` returns, so by-number/by-hash reads always
/// fall straight through to disk), but its canonical/safe/finalized HEAD pointer is real:
/// `RethExecutor::seal_block` advances it after each commit (see that crate's `blockchain_provider`
/// field doc) — which is exactly why `provider` here must be `RethExecutor::rpc_provider()`'s own
/// clone (see this module's doc above), not a fresh instance built from a bare
/// `ProviderFactory`. Mirrors `EthApiBuilder::new`'s own concrete instantiation
/// (`crates/rpc/rpc/src/eth/builder.rs`) —
/// `RpcNodeCoreAdapter<Provider, Pool, Network, EvmConfig>` for `EthApi`'s node-core parameter,
/// `RpcConverter<Ethereum, EvmConfig, EthReceiptConverter<ChainSpec>>` for its RPC-type converter.
type NodeProvider = BlockchainProvider<RethTypes>;
type NodeRpcNodeCore =
    RpcNodeCoreAdapter<NodeProvider, NoopTransactionPool, NoopNetwork, EthEvmConfig>;
type NodeRpcConvert = RpcConverter<Ethereum, EthEvmConfig, EthReceiptConverter<ChainSpec>>;
type NodeEthApi = EthApi<NodeRpcNodeCore, NodeRpcConvert>;

/// Sockets this node's RPC servers actually bound to (ephemeral-port-friendly: tests pass `:0` and
/// read the real port back off this).
#[derive(Debug, Clone, Copy)]
pub struct NodeRpcAddrs {
    pub http: SocketAddr,
    pub ws: SocketAddr,
}

/// Keeps the running RPC server(s) alive; drop to shut them down.
pub struct NodeRpcHandle {
    pub addrs: NodeRpcAddrs,
    _server: RpcServerHandle,
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

fn to_rpc_error(e: crate::sequencer::SequencerError) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(
        ErrorCode::ServerError(-32000).code(),
        e.to_string(),
        None::<()>,
    )
}

#[derive(serde::Serialize)]
struct PreconfirmationFeedItem {
    chain_id: u64,
    block: u64,
    index: u16,
    header_hash: String,
    signature: String,
    tx_hashes: Vec<String>,
}

#[derive(serde::Serialize, Clone)]
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

#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct RomeSendRawTxResult {
    tx_hash: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    preconfirmation: Option<PreconfirmationJson>,
}

/// The methods this crate overrides/adds onto reth's own transport modules — `eth` overrides plus
/// every `rome_*` extension, all sharing one context so `eth_getTransactionCount`'s non-"pending"
/// path can delegate straight into the real `EthApi` this node already built (rather than
/// reimplementing historical-state lookups).
struct RomeContext {
    handle: SequencerHandle,
    eth_api: NodeEthApi,
}

fn build_override_module(ctx: RomeContext) -> jsonrpsee::server::RpcModule<RomeContext> {
    let mut module = jsonrpsee::server::RpcModule::new(ctx);

    // Overrides reth's own `eth_sendRawTransaction` ("our RPC module REPLACES the node's
    // eth_sendRawTransaction ... must return the tx hash after the sequencer's admission/pre-
    // confirmation exactly as today"). Behavior mirrors `rpc.rs::build_module`'s method of the same
    // name exactly (this is the ported implementation — see that module's doc for the "Parked"
    // semantics rationale).
    module
        .register_async_method("eth_sendRawTransaction", |params, ctx, _| async move {
            let hex_str: String = params.one()?;
            let raw = decode_raw_tx(&hex_str)?;
            let outcome = ctx.handle.submit_raw_tx(raw).await.map_err(to_rpc_error)?;
            RpcResult::Ok(format!("0x{:x}", outcome.tx_hash()))
        })
        .expect("register eth_sendRawTransaction override");

    // Overrides reth's own `eth_getTransactionCount`. The requirement: "pending must include the sequencer's
    // in-block nonce advances: implement pending-tag handling by consulting the executor's pending
    // state, or document precisely what pending returns and why MetaMask still works." This
    // implements the former for the "pending" tag specifically (routed to
    // `SequencerHandle::pending_nonce`, which reads `RethExecutor::nonce`'s live, in-progress-block
    // view — see that method's doc) and delegates every other tag (`latest`, `earliest`, a numbered
    // block, or the parameter omitted, which the real `EthApiServer::transaction_count` already
    // defaults to "latest") straight to the real `EthApi` this node built, so historical-state
    // correctness is reth's own code, not reimplemented here.
    module
        .register_async_method("eth_getTransactionCount", |params, ctx, _| async move {
            let (address, block_id): (Address, Option<BlockId>) = params.parse()?;
            let is_pending = matches!(block_id, Some(BlockId::Number(BlockNumberOrTag::Pending)));
            if is_pending {
                let nonce = ctx
                    .handle
                    .pending_nonce(address)
                    .await
                    .map_err(to_rpc_error)?;
                RpcResult::Ok(U256::from(nonce))
            } else {
                EthApiServer::transaction_count(&ctx.eth_api, address, block_id).await
            }
        })
        .expect("register eth_getTransactionCount override");

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
            let found = ctx.handle.get_preconfirmation(tx_hash);
            RpcResult::Ok(found.as_ref().map(PreconfirmationJson::from))
        })
        .expect("register rome_getPreconfirmation");

    // The preconf WS feed lives on 8546 next to eth_subscribe — merged onto the
    // same transports as everything else here, so it rides the ws server reth's own eth_subscribe
    // uses.
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
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                SubscriptionResult::Ok(())
            },
        )
        .expect("register rome_subscribe");

    module
}

/// Starts the in-process reth RPC node: `http_addr` (eth/net/web3/debug/txpool/trace/ots, plus this
/// crate's overrides/extensions) and `ws_addr` (eth/net/web3/debug/txpool, plus the same
/// overrides/extensions and the preconf feed) — matching the stock
/// `reth` service's own namespace split exactly, so the swap
/// changes nothing about what a client sees.
pub async fn serve(
    provider: NodeProvider,
    chain_spec: Arc<ChainSpec>,
    handle: SequencerHandle,
    http_addr: SocketAddr,
    ws_addr: SocketAddr,
    rpc_config: RpcConfig,
) -> eyre::Result<NodeRpcHandle> {
    // `provider` must be `RethExecutor::rpc_provider()`'s own clone — see
    // this module's doc — never rebuilt here via `BlockchainProvider::new(factory)` from a bare
    // `ProviderFactory`.
    let evm_config = EthEvmConfig::new(chain_spec.clone());
    // `NoopNetwork::default()` hard-codes chain id 1 ("mainnet" — `reth_network_api::noop::
    // NoopNetwork::new`'s own comment) since it's built with no chain spec of its own; several
    // `eth_*` RPC methods (`eth_chainId` among them) read the chain id off the network component,
    // not directly off `chain_spec` — this NoopNetwork must be told the real one explicitly, or
    // every RPC client on this node sees chain id 1 regardless of genesis. Caught by
    // `tests/e2e_reth_node.rs::node_starts_and_answers_identity_methods` asserting the real value,
    // not just that `eth_chainId` answers *something*.
    let network = NoopNetwork::default().with_chain_id(chain_spec.chain_id());
    // `Runtime::test()` is reth's own choice in `examples/rpc-db/src/main.rs` for exactly this
    // standalone-RPC-over-a-provider shape (not itself test-only despite the name — see that
    // example's non-test `main.rs`); sizing this pool for production load is an open question.
    let runtime = reth_tasks::Runtime::test();

    let rpc_builder = RpcModuleBuilder::default()
        .with_provider(provider.clone())
        .with_noop_pool()
        .with_network(network.clone())
        .with_executor(runtime)
        .with_evm_config(evm_config.clone())
        .with_consensus(EthBeaconConsensus::new(chain_spec));

    let eth_api: NodeEthApi = EthApiBuilder::new(
        provider,
        NoopTransactionPool::default(),
        network,
        evm_config,
    )
    .build();

    let http_modules = [
        RethRpcModule::Eth,
        RethRpcModule::Net,
        RethRpcModule::Web3,
        RethRpcModule::Debug,
        RethRpcModule::Txpool,
        RethRpcModule::Trace,
        RethRpcModule::Ots,
    ];
    let ws_modules = [
        RethRpcModule::Eth,
        RethRpcModule::Net,
        RethRpcModule::Web3,
        RethRpcModule::Debug,
        RethRpcModule::Txpool,
    ];
    // `--executor reth` is unconditional now — when `[reth].http_addr`/
    // `ws_addr` are both absent, the caller (the binary's `reth_main`) resolves BOTH to the same
    // `Config::rpc_addr` socket (`RethSettings::resolved_rpc_addrs`), and reth's own
    // `RpcServerConfig::start` combines http+ws onto that one listener when the two addresses match
    // ("If both are configured on the same port, we combine them into one server" —
    // `crates/rpc/rpc-builder/src/lib.rs`) — but only if `ensure_ws_http_identical` sees the SAME
    // module set on both transports, which the Tiber (two-port) shape above deliberately does not
    // have. Use the http (superset) list on both when sharing one port, so the ws client on that
    // one-port fallback loses nothing.
    let module_config = if http_addr == ws_addr {
        TransportRpcModuleConfig::default()
            .with_http(http_modules.clone())
            .with_ws(http_modules)
    } else {
        TransportRpcModuleConfig::default()
            .with_http(http_modules)
            .with_ws(ws_modules)
    };
    let mut transport_modules =
        rpc_builder.build(module_config, eth_api.clone(), Default::default());

    let overrides = build_override_module(RomeContext { handle, eth_api });
    // `replace_configured` removes-then-merges on every configured transport: it overrides
    // `eth_sendRawTransaction`/`eth_getTransactionCount` (already present from the `eth` module
    // above) and adds every `rome_*` method/subscription (not present) in the same call — see this
    // module's doc for the citation on why this composition is safe for both cases at once.
    transport_modules.replace_configured(overrides)?;

    // `RpcServerConfig::http(Default::default())`/`.with_ws(Default::default())`
    // each build a `ServerConfigBuilder` at jsonrpsee's OWN hardcoded default — `MAX_CONNECTIONS: u32 =
    // 100` (jsonrpsee-server-0.26.0's `src/server.rs`) — the exact trap `rpc.rs::RpcConfig`
    // exists to avoid on this crate's OTHER server. reth's own CLI never hits this because
    // `RethRpcServerConfig::http_ws_server_builder` (reth `crates/rpc/rpc-builder/src/config.rs`) always
    // threads its `--rpc.max-connections` etc. through a real `ServerConfigBuilder`; this standalone
    // `RpcModuleBuilder`-over-a-`ProviderFactory` shape (this module's own doc) has no CLI args to do
    // that for it, so it must build the same `ServerConfigBuilder` from `RpcConfig` itself, exactly like
    // `rpc.rs::serve` does for its own server.
    let server_builder = ServerConfigBuilder::new()
        .max_connections(rpc_config.max_connections)
        .max_request_body_size(rpc_config.max_request_body_size)
        .max_subscriptions_per_connection(rpc_config.max_subscriptions_per_connection);
    let server_config = RpcServerConfig::http(server_builder.clone())
        .with_http_address(http_addr)
        .with_ws(server_builder)
        .with_ws_address(ws_addr);
    let server = server_config.start(&transport_modules).await?;

    let addrs = NodeRpcAddrs {
        http: server.http_local_addr().unwrap_or(http_addr),
        ws: server.ws_local_addr().unwrap_or(ws_addr),
    };
    Ok(NodeRpcHandle {
        addrs,
        _server: server,
    })
}
