//! Equivalence: a batcher-produced channel stream -> this crate's own
//! [`rome_zk_derive::frame_queue`]/[`rome_zk_derive::channel_bank`]/[`rome_zk_derive::batch_queue`]/
//! [`rome_zk_derive::attributes`]/[`rome_zk_derive::engine::EngineController`] -> a REAL, unmodified
//! reth v2.5.2 node, driven purely through [`rome_zk_derive::engine::JsonRpcEngineApi`] (the actual
//! production `EngineApi` — real JWT-authed `engine_forkchoiceUpdatedV3`/`engine_newPayloadV4` plus
//! `testing_buildBlockV1`, never a mock) — block hash + state root equal to `rome-zk-executor-reth`'s
//! own `RethExecutor` sealing the identical ordered txs under the identical committed `BlockEnv`
//! (the Engine API is the boundary so a stock reth can replace the in-process one — this
//! is exactly that substitution, proven).
//!
//! In-process, via `reth-e2e-test-utils` (the same harness `rome-zk-executor-reth`'s own
//! `derivation_equivalence.rs` uses) — real network I/O to a real Engine API port, so this is
//! deliberately `#[ignore]`d like every other heavy reth-spin-up test in this repo.
//!
//! **Open question (flagged, not silently assumed — see `src/engine.rs`'s module doc):** this exercises
//! [`rome_zk_derive::engine::JsonRpcEngineApi`]'s real `testing_buildBlockV1` dependency — the launched
//! node below enables `RethRpcModule::Testing` explicitly; a production derivation node needs the
//! operational equivalent (`--http.api testing`), which the docs do not currently name.

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::eip2718::Encodable2718;
use alloy_genesis::Genesis;
use alloy_primitives::{Address, Bytes, TxKind, B256, U256};
use alloy_rpc_types_engine::JwtSecret;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use reth_chainspec::{ChainSpec, EthChainSpec};
use reth_node_builder::{NodeBuilder, NodeHandle};
use reth_node_core::{args::RpcServerArgs, node_config::NodeConfig};
use reth_node_ethereum::EthereumNode;
use reth_rpc_server_types::{RethRpcModule, RpcModuleSelection};
use reth_tasks::Runtime;
use rome_zk_channel as channel;
use rome_zk_channel::Block;
use rome_zk_derive::engine::{connect, EngineController};
use rome_zk_derive::{attributes, batch_queue};
use rome_zk_executor_api::{BlockEnv, BlockSealInputs, Executor as _, SubBlockLimits};
use rome_zk_executor_reth::{RethConfig, RethExecutor};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::tempdir;

const CHAIN_ID: u64 = 200_101;
const BLOCKS: u64 = 3;
const TXS_PER_BLOCK: u64 = 3;

fn genesis_json(funded: Address) -> serde_json::Value {
    serde_json::json!({
        "config": {
            "chainId": CHAIN_ID,
            "homesteadBlock": 0, "eip150Block": 0, "eip155Block": 0, "eip158Block": 0,
            "byzantiumBlock": 0, "constantinopleBlock": 0, "petersburgBlock": 0,
            "istanbulBlock": 0, "berlinBlock": 0, "londonBlock": 0,
            "terminalTotalDifficulty": 0, "terminalTotalDifficultyPassed": true,
            "shanghaiTime": 0, "cancunTime": 0, "pragueTime": 0
        },
        "nonce": "0x0", "timestamp": "0x0", "extraData": "0x",
        "gasLimit": "0x2540be400", "difficulty": "0x0",
        "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "coinbase": "0x0000000000000000000000000000000000000000",
        "alloc": { format!("{funded:#x}"): { "balance": format!("{:#x}", U256::from(10u128).pow(U256::from(24u8))) } },
        "number": "0x0", "gasUsed": "0x0",
        "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "baseFeePerGas": "0x3b9aca00"
    })
}

fn signed_transfer(signer: &PrivateKeySigner, nonce: u64) -> Bytes {
    let tx = TxEip1559 {
        chain_id: CHAIN_ID,
        nonce,
        gas_limit: 21_000,
        max_fee_per_gas: 1_000_000_000,
        max_priority_fee_per_gas: 1_000_000_000,
        to: TxKind::Call(Address::ZERO),
        value: U256::ZERO,
        access_list: Default::default(),
        input: Bytes::new(),
    };
    let sig_hash = tx.signature_hash();
    let signature = signer.sign_hash_sync(&sig_hash).unwrap();
    Bytes::from(TxEnvelope::from(tx.into_signed(signature)).encoded_2718())
}

fn unbounded_limits() -> SubBlockLimits {
    SubBlockLimits {
        gas_limit: u64::MAX,
        deadline: Instant::now() + Duration::from_secs(3600),
    }
}

#[ignore = "spins up a real in-process reth node; run explicitly: cargo test -p rome-zk-derive --test engine_equivalence -- --ignored --nocapture"]
#[tokio::test(flavor = "multi_thread")]
async fn a_batch_derived_through_the_real_engine_api_matches_rome_zk_executor_reth() {
    reth_tracing::init_test_tracing();

    let signer = PrivateKeySigner::random();
    let genesis: Genesis = serde_json::from_value(genesis_json(signer.address())).unwrap();
    let chain_spec = Arc::new(ChainSpec::from_genesis(genesis.clone()));

    // 1. Ground truth: `rome-zk-executor-reth`'s own `RethExecutor` seals `BLOCKS` blocks of real
    //    signed transfers, recording the ordered txs + committed env it used for each (exactly
    //    `rome-zk-executor-reth/tests/derivation_equivalence.rs`'s own step 1).
    let mine_dir = tempdir().unwrap();
    let genesis_path = mine_dir.path().join("genesis.json");
    std::fs::write(&genesis_path, genesis_json(signer.address()).to_string()).unwrap();
    let mut mine = RethExecutor::new(RethConfig {
        datadir: mine_dir.path().join("db"),
        genesis_path,
        block_gas_limit: chain_spec.genesis().gas_limit,
    })
    .unwrap();

    let mut blocks = Vec::new();
    let mut nonce = 0u64;
    let base_ts = chain_spec.genesis().timestamp;
    let mut recorded_hashes = Vec::new();
    let mut recorded_state_roots = Vec::new();
    for b in 0..BLOCKS {
        // The sequencer numbers its first sealed block 1 — design
        // number 0 names only the EL's genesis, which is never sealed. `number` is what feeds both
        // `RethExecutor` here AND, via `attributes_for_block` below, `EngineController::advance`'s
        // real-height equality check.
        let number = b + 1;
        let timestamp = base_ts + number;
        let prev_randao = rome_zk_executor_api::prev_randao(CHAIN_ID, number);
        mine.open_block(BlockEnv {
            number,
            timestamp_secs: timestamp,
            gas_limit: chain_spec.genesis().gas_limit,
            coinbase: Address::ZERO,
            prev_randao,
            base_fee: None,
            withdrawals: vec![],
        })
        .await
        .unwrap();

        let mut block_txs = Vec::new();
        for i in 0..20u16 {
            let txs = if i == 0 {
                (0..TXS_PER_BLOCK)
                    .map(|_| {
                        let t = signed_transfer(&signer, nonce);
                        nonce += 1;
                        t
                    })
                    .collect::<Vec<_>>()
            } else {
                vec![]
            };
            block_txs.extend(txs.iter().cloned());
            mine.execute_sub_block(&txs, unbounded_limits())
                .await
                .unwrap();
        }
        let outcome = mine
            .seal_block(BlockSealInputs {
                block: number,
                timestamp_secs: timestamp,
                sub_block_header_hashes: vec![B256::repeat_byte(number as u8 + 1); 20],
                total_gas_used: TXS_PER_BLOCK * 21_000,
            })
            .await
            .unwrap();
        recorded_hashes.push(outcome.block_hash);
        recorded_state_roots.push(outcome.state_root);
        blocks.push(Block {
            number,
            timestamp,
            gas_limit: chain_spec.genesis().gas_limit,
            txs: block_txs,
            deposits_end: None,
        });
    }

    // 2. Through THIS crate's own codec: encode as the batcher would, decode via `batch_queue` (the
    //    strict validity gate) — proves the "ordered log -> batcher frames -> derive" leg, not
    //    only the Engine API leg.
    let compressed = channel::encode_stream(&blocks);
    let decoded = batch_queue::decode_batch(
        &compressed,
        CHAIN_ID,
        0,
        rome_zk_derive::config::DEFAULT_BLOCKS_PER_BATCH,
        None, // the very first (only) batch this test derives — nothing to continue from
    )
    .unwrap();
    assert_eq!(decoded, blocks);

    // 3. A second, real, unmodified reth node, driven purely through `JsonRpcEngineApi` — a
    //    pre-written JWT file (both the node's `--authrpc.jwtsecret` and `connect`'s own read of it),
    //    a plain HTTP port (`testing_buildBlockV1`) and the authenticated engine port.
    let jwt_dir = tempdir().unwrap();
    let jwt_path = jwt_dir.path().join("jwt.hex");
    JwtSecret::try_create_random(&jwt_path).expect("write a real jwt secret file");

    let runtime = Runtime::test();
    let genesis_gas_limit = chain_spec.genesis().gas_limit;
    let node_config = NodeConfig::test()
        .with_chain(chain_spec.clone())
        .with_unused_ports()
        .with_network(reth_node_core::args::NetworkArgs {
            discovery: reth_node_core::args::DiscoveryArgs {
                disable_discovery: true,
                disable_dns_discovery: true,
                ..Default::default()
            },
            ..Default::default()
        })
        .with_rpc({
            let mut rpc = RpcServerArgs::default()
                .with_unused_ports()
                .with_http()
                .with_http_api(RpcModuleSelection::Selection(
                    [RethRpcModule::Eth, RethRpcModule::Testing]
                        .into_iter()
                        .collect(),
                ));
            // `auth_jwtsecret` is a plain public field (no `with_*` builder for it) — set directly so
            // the node reads the SAME secret file `connect` below reads (`RpcServerArgs::auth_jwt_secret`
            // calls `JwtSecret::from_file`, which requires the file to already exist — hence writing it
            // above, before this node ever launches).
            rpc.auth_jwtsecret = Some(jwt_path.clone());
            rpc
        });
    let NodeHandle {
        node,
        node_exit_future: _,
    } = NodeBuilder::new(node_config)
        .testing_node(runtime)
        .node(EthereumNode::default())
        .launch()
        .await
        .unwrap();

    let rpc_url = format!(
        "http://{}",
        node.rpc_server_handle().http_local_addr().unwrap()
    );
    let auth_url = format!("http://{}", node.auth_server_handle().local_addr());
    let engine_api = connect(&rpc_url, &auth_url, &jwt_path).expect("connect to the real node");

    let genesis_hash = chain_spec.genesis_hash();
    let mut ctrl = EngineController::new(engine_api, genesis_hash, 0);

    for (i, block) in decoded.iter().enumerate() {
        let attrs = attributes::attributes_for_block(CHAIN_ID, Address::ZERO, block);
        assert_eq!(attrs.env.gas_limit, genesis_gas_limit);
        let outcome = ctrl.advance(&attrs).await.unwrap_or_else(|e| {
            panic!("block {i}: EngineController::advance against the real node failed: {e}")
        });
        assert!(
            !outcome.consolidated,
            "block {i}: must actually build, not consolidate"
        );
        assert_eq!(
            outcome.block_hash, recorded_hashes[i],
            "block {i}: real-engine block_hash must match rome-zk-executor-reth's"
        );
        assert_eq!(
            outcome.state_root, recorded_state_roots[i],
            "block {i}: real-engine state_root must match rome-zk-executor-reth's"
        );
    }

    // 4. Re-derive pass: a second controller, seeded from this SAME real
    //    node's own genesis exactly as a restart would (`EngineController::from_engine_head`), must
    //    consolidate every block instead of rebuilding — and must report the same non-zero state_root
    //    a first build reported (`advance`'s consolidation branch used to hardcode `B256::ZERO`, which
    //    would have failed the second assertion below on a real node).
    let engine_api2 = connect(&rpc_url, &auth_url, &jwt_path).expect("reconnect to the real node");
    let mut ctrl2 = EngineController::from_engine_head(engine_api2)
        .await
        .unwrap();
    for (i, block) in decoded.iter().enumerate() {
        let attrs = attributes::attributes_for_block(CHAIN_ID, Address::ZERO, block);
        let outcome = ctrl2.advance(&attrs).await.unwrap_or_else(|e| {
            panic!(
                "re-derive block {i}: EngineController::advance against the real node failed: {e}"
            )
        });
        assert!(
            outcome.consolidated,
            "re-derive block {i}: must consolidate, not rebuild"
        );
        assert_eq!(
            outcome.block_hash, recorded_hashes[i],
            "re-derive block {i}: block_hash must match"
        );
        assert_eq!(
            outcome.state_root, recorded_state_roots[i],
            "re-derive block {i}: consolidated state_root must be non-zero and match the original build"
        );
    }
}
