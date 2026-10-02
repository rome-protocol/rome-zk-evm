//! Derivation equivalence. Feeds the exact same ordered tx lists + block env
//! that `RethExecutor` used to seal 20 blocks into a SECOND, real, unmodified reth v2.5.2 node
//! (in-process, via `reth-e2e-test-utils` — the simpler, hermetic option over
//! docker), driven purely through its own Engine API: `testing_buildBlockV1` (reth's own
//! forced-tx-list testing endpoint — `alloy_rpc_types_engine::TestingBuildBlockRequestV1`'s own doc:
//! "Raw signed transactions to force-include in order", exactly this crate's `noTxPool` semantics)
//! followed by `engine_newPayloadV4` + `forkchoiceUpdated`, mirroring `tools/derive/src/main.rs`'s
//! FCU → getPayload → newPayload shape (here collapsed into one build-then-validate call by reth's
//! own testing endpoint). Asserts block hash + state root equality for 20 blocks of real transfers, plus
//! BlockEnv::number == the real header number for every block. The Engine API is the boundary, so a stock reth
//! can replace the in-process one.

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::eip2718::Encodable2718;
use alloy_eips::eip7685::RequestsOrHash;
use alloy_genesis::Genesis;
use alloy_primitives::{Address, Bytes, TxKind, B256, U256};
use alloy_rpc_types_engine::{
    PayloadAttributes, PayloadStatus, PayloadStatusEnum, TestingBuildBlockRequestV1,
};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use jsonrpsee::core::client::ClientT;
use reth_chainspec::{ChainSpec, EthChainSpec};
use reth_e2e_test_utils::node::NodeTestContext;
use reth_node_builder::{NodeBuilder, NodeHandle};
use reth_node_core::{args::RpcServerArgs, node_config::NodeConfig};
use reth_node_ethereum::EthereumNode;
use reth_tasks::Runtime;
use rome_zk_executor_api::{BlockSealInputs, Executor as _, SubBlockLimits};
use rome_zk_executor_reth::{RethConfig, RethExecutor};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::tempdir;

const CHAIN_ID: u64 = 200_101;

fn genesis_json(funded: &[Address]) -> serde_json::Value {
    let mut alloc = serde_json::Map::new();
    for a in funded {
        alloc.insert(
            format!("{a:#x}"),
            serde_json::json!({ "balance": format!("{:#x}", U256::from(10u128).pow(U256::from(24u8))) }),
        );
    }
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
        "alloc": alloc,
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
    let signed = tx.into_signed(signature);
    Bytes::from(TxEnvelope::from(signed).encoded_2718())
}

fn unbounded_limits() -> SubBlockLimits {
    SubBlockLimits {
        gas_limit: u64::MAX,
        deadline: Instant::now() + Duration::from_secs(3600),
    }
}

struct Recorded {
    /// The design number this crate's `RethExecutor` was told (`BlockEnv::number` /
    /// `BlockSealInputs::block`): the sequencer numbers its first
    /// sealed block 1, so this must equal the real reth header number the stock node below assigns.
    number: u64,
    txs: Vec<Bytes>,
    timestamp: u64,
    prev_randao: B256,
    state_root: B256,
    block_hash: B256,
}

#[ignore = "spins up a real in-process reth node; run explicitly: cargo test -p rome-zk-executor-reth --test derivation_equivalence -- --ignored --nocapture"]
#[tokio::test(flavor = "multi_thread")]
async fn twenty_blocks_with_real_transfers_derive_identically_on_a_stock_reth() {
    reth_tracing::init_test_tracing();
    // 20 blocks so the env.number == header.number equivalence check below (the numbering
    // invariant) runs over a real span, not just block 1.
    const BLOCKS: u64 = 20;
    const TXS_PER_BLOCK: u64 = 3;

    let signer = PrivateKeySigner::random();
    let genesis: Genesis = serde_json::from_value(genesis_json(&[signer.address()])).unwrap();
    let chain_spec = Arc::new(ChainSpec::from_genesis(genesis));

    // 1. This crate's own in-process executor seals BLOCKS blocks, remembering the exact ordered tx
    //    list + timestamp it used for each — the inputs the second node gets fed below.
    let mine_dir = tempdir().unwrap();
    let genesis_path = mine_dir.path().join("genesis.json");
    std::fs::write(&genesis_path, genesis_json(&[signer.address()]).to_string()).unwrap();
    let mut mine = RethExecutor::new(RethConfig {
        datadir: mine_dir.path().join("db"),
        genesis_path,
        block_gas_limit: chain_spec.genesis().gas_limit,
    })
    .unwrap();

    let mut recorded = Vec::new();
    let mut nonce = 0u64;
    let base_ts = chain_spec.genesis().timestamp;
    for b in 0..BLOCKS {
        // The sequencer numbers its first sealed block 1 — design
        // number 0 names only the EL's genesis, which is never sealed. `number` is what this test
        // feeds `RethExecutor` AND what it asserts the real reth header number equals below.
        let number = b + 1;
        let timestamp = base_ts + number;
        // Block environment is published, not inferred — open this
        // block with its final, resolved timestamp and its real `prev_randao` before its
        // first sub-block executes, exactly as `rome_zk_sequencer::sealer` does.
        let prev_randao = rome_zk_executor_api::prev_randao(CHAIN_ID, number);
        mine.open_block(rome_zk_executor_api::BlockEnv {
            number,
            timestamp_secs: timestamp,
            gas_limit: chain_spec.genesis().gas_limit,
            coinbase: Address::ZERO,
            prev_randao,
            base_fee: None,
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
        recorded.push(Recorded {
            number,
            txs: block_txs,
            timestamp,
            prev_randao,
            state_root: outcome.state_root,
            block_hash: outcome.block_hash,
        });
    }

    // 2. A second, real, unmodified reth node — same chain spec — driven purely through its own
    //    Engine API.
    let runtime = Runtime::test();
    let genesis_gas_limit = chain_spec.genesis().gas_limit;
    let node_config = NodeConfig::test()
        .with_chain(chain_spec.clone())
        .with_unused_ports()
        // `with_unused_ports()` above only applies to the network args it was called with; this
        // replacement value would otherwise carry reth's default P2P port 30303, so two tests in this
        // binary cannot run concurrently — apply it to the value itself.
        .with_network(
            reth_node_core::args::NetworkArgs {
                discovery: reth_node_core::args::DiscoveryArgs {
                    disable_discovery: true,
                    disable_dns_discovery: true,
                    ..Default::default()
                },
                ..Default::default()
            }
            .with_unused_ports(),
        )
        .with_rpc(
            RpcServerArgs::default()
                .with_unused_ports()
                .with_http()
                .with_http_api(reth_rpc_server_types::RpcModuleSelection::All),
        );
    let NodeHandle {
        node,
        node_exit_future: _,
    } = NodeBuilder::new(node_config)
        .testing_node(runtime)
        .node(EthereumNode::default())
        .launch()
        .await
        .unwrap();
    let node = NodeTestContext::new(node, move |ts| PayloadAttributes {
        timestamp: ts,
        prev_randao: B256::ZERO,
        suggested_fee_recipient: Address::ZERO,
        withdrawals: Some(vec![]),
        parent_beacon_block_root: Some(B256::ZERO),
        slot_number: None,
        target_gas_limit: Some(genesis_gas_limit),
    })
    .await
    .unwrap();

    let mut parent_hash = chain_spec.genesis_hash();
    for (i, rec) in recorded.iter().enumerate() {
        let payload_attributes = PayloadAttributes {
            timestamp: rec.timestamp,
            // The stock node must build under the identical
            // `prev_randao` `RethExecutor` opened this block with, or the two blocks' headers
            // (which embed `prevRandao` directly) would diverge for a reason that has nothing to do
            // with interchangeability.
            prev_randao: rec.prev_randao,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::ZERO),
            slot_number: None,
            // This chain's genesis gas_limit is a fixed constant, not subject to Ethereum L1's
            // miner-voted elastic adjustment (there is no such vote on a single-sequencer rollup) —
            // `RethExecutor` holds it steady by construction (`NextBlockEnvAttributes::gas_limit`
            // is taken as an authoritative value, not an elastic target). Absent this,
            // stock reth's default builder nudges the block's gas_limit toward its own compiled-in
            // default by up to 1/1024 per block (measured: 10_000_000_000 -> 9_990_234_376 on
            // block 0) — a real divergence this test caught, not a bug in RethExecutor.
            target_gas_limit: Some(genesis_gas_limit),
        };
        let envelope = node
            .testing_build_block_v1(TestingBuildBlockRequestV1 {
                parent_block_hash: parent_hash,
                payload_attributes,
                transactions: rec.txs.clone(),
                extra_data: None,
            })
            .await
            .unwrap();

        let payload = envelope.execution_payload;
        let block_hash = payload.payload_inner.payload_inner.block_hash;
        let state_root = payload.payload_inner.payload_inner.state_root;
        let header_number = payload.payload_inner.payload_inner.block_number;

        let engine_client = node.auth_server_handle().http_client();
        let versioned_hashes: Vec<B256> = Vec::new();
        let status: PayloadStatus = engine_client
            .request(
                "engine_newPayloadV4",
                (
                    payload,
                    versioned_hashes,
                    B256::ZERO,
                    RequestsOrHash::Requests(envelope.execution_requests),
                ),
            )
            .await
            .unwrap();
        assert_eq!(
            status.status,
            PayloadStatusEnum::Valid,
            "block {i}: stock reth must accept the newPayload"
        );
        node.update_forkchoice(parent_hash, block_hash)
            .await
            .unwrap();

        assert_eq!(
            block_hash, rec.block_hash,
            "block {i}: stock reth's block_hash must match RethExecutor's"
        );
        assert_eq!(
            state_root, rec.state_root,
            "block {i}: stock reth's state_root must match RethExecutor's"
        );
        // The design number `RethExecutor` was told via
        // `BlockEnv::number`/`BlockSealInputs::block` must equal the real header number a stock,
        // unmodified reth assigns for the identical position in the chain — this is the numbering
        // invariant, checked here against a real reth, not reasoned about from architecture.
        assert_eq!(
            header_number, rec.number,
            "block {i}: BlockEnv::number ({}) must equal the real reth header number ({header_number})",
            rec.number
        );

        parent_hash = block_hash;
    }
}

/// A block sealed a full hour after its parent — the
/// idle-gap shape `rome-zk-sequencer`'s own `empty_block_interval_secs = 0` produces once
/// nothing seals for a while — executes and seals identically on a stock, unmodified reth, exactly like
/// any other block. **No guest change, no derive drift-bound change**:
/// `block.timestamp > parent.timestamp` is reth's own `validate_against_parent_timestamp`
/// inside stateless validation (verified against the reth source, not assumed), which places no upper
/// bound on how far ahead a child's timestamp may run. This test checks that against a REAL reth rather
/// than resting on reading the source alone.
#[ignore = "spins up a real in-process reth node; run explicitly: cargo test -p rome-zk-executor-reth --test derivation_equivalence -- --ignored --nocapture"]
#[tokio::test(flavor = "multi_thread")]
async fn a_block_one_hour_after_its_parent_executes_and_seals_on_a_stock_reth() {
    reth_tracing::init_test_tracing();
    const GAP_SECS: u64 = 3_600;

    let signer = PrivateKeySigner::random();
    let genesis: Genesis = serde_json::from_value(genesis_json(&[signer.address()])).unwrap();
    let chain_spec = Arc::new(ChainSpec::from_genesis(genesis));

    // 1. This crate's own in-process executor seals two blocks: the parent at design number 1, then a
    //    child at design number 2 whose timestamp is GAP_SECS ahead of the parent's own — the idle-gap
    //    shape, not a 1-second cadence.
    let mine_dir = tempdir().unwrap();
    let genesis_path = mine_dir.path().join("genesis.json");
    std::fs::write(&genesis_path, genesis_json(&[signer.address()]).to_string()).unwrap();
    let mut mine = RethExecutor::new(RethConfig {
        datadir: mine_dir.path().join("db"),
        genesis_path,
        block_gas_limit: chain_spec.genesis().gas_limit,
    })
    .unwrap();

    let mut recorded = Vec::new();
    let mut nonce = 0u64;
    let base_ts = chain_spec.genesis().timestamp;
    let timestamps = [base_ts + 1, base_ts + 1 + GAP_SECS];
    for (i, &timestamp) in timestamps.iter().enumerate() {
        let number = i as u64 + 1;
        let prev_randao = rome_zk_executor_api::prev_randao(CHAIN_ID, number);
        mine.open_block(rome_zk_executor_api::BlockEnv {
            number,
            timestamp_secs: timestamp,
            gas_limit: chain_spec.genesis().gas_limit,
            coinbase: Address::ZERO,
            prev_randao,
            base_fee: None,
        })
        .await
        .unwrap();

        let mut block_txs = Vec::new();
        for sub_block in 0..20u16 {
            let txs = if sub_block == 0 {
                let t = signed_transfer(&signer, nonce);
                nonce += 1;
                vec![t]
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
                total_gas_used: 21_000,
            })
            .await
            .unwrap();
        recorded.push(Recorded {
            number,
            txs: block_txs,
            timestamp,
            prev_randao,
            state_root: outcome.state_root,
            block_hash: outcome.block_hash,
        });
    }

    // 2. A second, real, unmodified reth node, driven purely through its own Engine API — identical setup
    //    to the 20-block test above, just with the gapped two-block input recorded above.
    let runtime = Runtime::test();
    let genesis_gas_limit = chain_spec.genesis().gas_limit;
    let node_config = NodeConfig::test()
        .with_chain(chain_spec.clone())
        .with_unused_ports()
        // `with_unused_ports()` above only applies to the network args it was called with; this
        // replacement value would otherwise carry reth's default P2P port 30303, so two tests in this
        // binary cannot run concurrently — apply it to the value itself.
        .with_network(
            reth_node_core::args::NetworkArgs {
                discovery: reth_node_core::args::DiscoveryArgs {
                    disable_discovery: true,
                    disable_dns_discovery: true,
                    ..Default::default()
                },
                ..Default::default()
            }
            .with_unused_ports(),
        )
        .with_rpc(
            RpcServerArgs::default()
                .with_unused_ports()
                .with_http()
                .with_http_api(reth_rpc_server_types::RpcModuleSelection::All),
        );
    let NodeHandle {
        node,
        node_exit_future: _,
    } = NodeBuilder::new(node_config)
        .testing_node(runtime)
        .node(EthereumNode::default())
        .launch()
        .await
        .unwrap();
    let node = NodeTestContext::new(node, move |ts| PayloadAttributes {
        timestamp: ts,
        prev_randao: B256::ZERO,
        suggested_fee_recipient: Address::ZERO,
        withdrawals: Some(vec![]),
        parent_beacon_block_root: Some(B256::ZERO),
        slot_number: None,
        target_gas_limit: Some(genesis_gas_limit),
    })
    .await
    .unwrap();

    let mut parent_hash = chain_spec.genesis_hash();
    for (i, rec) in recorded.iter().enumerate() {
        let payload_attributes = PayloadAttributes {
            timestamp: rec.timestamp,
            prev_randao: rec.prev_randao,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::ZERO),
            slot_number: None,
            target_gas_limit: Some(genesis_gas_limit),
        };
        let envelope = node
            .testing_build_block_v1(TestingBuildBlockRequestV1 {
                parent_block_hash: parent_hash,
                payload_attributes,
                transactions: rec.txs.clone(),
                extra_data: None,
            })
            .await
            .unwrap();

        let payload = envelope.execution_payload;
        let block_hash = payload.payload_inner.payload_inner.block_hash;
        let state_root = payload.payload_inner.payload_inner.state_root;
        let header_number = payload.payload_inner.payload_inner.block_number;
        let header_timestamp = payload.payload_inner.payload_inner.timestamp;

        let engine_client = node.auth_server_handle().http_client();
        let versioned_hashes: Vec<B256> = Vec::new();
        let status: PayloadStatus = engine_client
            .request(
                "engine_newPayloadV4",
                (
                    payload,
                    versioned_hashes,
                    B256::ZERO,
                    RequestsOrHash::Requests(envelope.execution_requests),
                ),
            )
            .await
            .unwrap();
        assert_eq!(
            status.status,
            PayloadStatusEnum::Valid,
            "block {i}: a stock reth must accept a newPayload {GAP_SECS} s after its parent's own \
             timestamp, not treat it as a future block"
        );
        node.update_forkchoice(parent_hash, block_hash)
            .await
            .unwrap();

        assert_eq!(
            block_hash, rec.block_hash,
            "block {i}: stock reth's block_hash must match RethExecutor's"
        );
        assert_eq!(
            state_root, rec.state_root,
            "block {i}: stock reth's state_root must match RethExecutor's"
        );
        assert_eq!(
            header_number, rec.number,
            "block {i}: BlockEnv::number ({}) must equal the real reth header number ({header_number})",
            rec.number
        );
        assert_eq!(
            header_timestamp, rec.timestamp,
            "block {i}: the real header must carry the gapped timestamp verbatim"
        );

        parent_hash = block_hash;
    }

    // The whole point of this test: block 2's real header timestamp really is GAP_SECS ahead of block
    // 1's, and reth accepted it anyway — the drift-bound-across-an-idle-gap claim rests on this, not on
    // the citation alone.
    assert_eq!(
        recorded[1].timestamp - recorded[0].timestamp,
        GAP_SECS,
        "the two recorded blocks must actually be GAP_SECS apart"
    );
}
