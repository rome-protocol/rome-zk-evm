//! Proves `rome-zk-derive`'s `batch_queue::decode_batch` — the real function a
//! derivation node runs against every batch on chain — accepts every batch this crate's own
//! grouping produces, with real signed txs through the real channel codec both crates share
//! (`rome_zk_batcher::channel`). A dev-only dependency on `rome-zk-derive` (which itself depends on this
//! crate normally) — see `Cargo.toml`'s own note on why this is a legitimate dev-dependency cycle, not a
//! real one.

use alloy::consensus::{SignableTransaction, TxEip1559};
use alloy::eips::eip2718::Encodable2718;
use alloy::primitives::{Address, Bytes, TxKind, U256};
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::SignerSync;
use rome_zk_batcher::channel::Block;
use rome_zk_batcher::grouping::{BlockGrouper, PushOutcome, SizeCappedGrouper};
use rome_zk_derive::batch_queue::decode_batch;

const CHAIN_ID: u64 = 200_198;

/// Test-only convenience mirroring what production code now does via [`SizeCappedGrouper`] seeded from the
/// chain anchor — pushes every block through one [`BlockGrouper`] and collects each
/// group as it closes (cap only; this file's tests don't need the size close for the plain-grouping
/// cases). Kept local to this test file rather than exported from the library: the deleted
/// `group_by_blocks_per_batch` was specifically the `--once` from-block-0 regrouping path, which no longer
/// exists in production.
fn group_all(blocks: Vec<Block>, cap: u64) -> Vec<Vec<Block>> {
    let mut grouper = BlockGrouper::new(cap);
    let mut groups = Vec::new();
    for block in blocks {
        if grouper.is_full() {
            groups.push(grouper.take_group());
        }
        grouper.push(block).expect("test data is always contiguous");
    }
    if !grouper.is_empty() {
        groups.push(grouper.take_group());
    }
    groups
}

fn signed_tx_with_input(signer: &PrivateKeySigner, nonce: u64, input: Bytes) -> Bytes {
    let tx = TxEip1559 {
        chain_id: CHAIN_ID,
        nonce,
        gas_limit: 21_000,
        max_fee_per_gas: 1_000_000_000,
        max_priority_fee_per_gas: 1_000_000_000,
        to: TxKind::Call(Address::ZERO),
        value: U256::ZERO,
        access_list: Default::default(),
        input,
    };
    let sig_hash = tx.signature_hash();
    let signature = signer.sign_hash_sync(&sig_hash).unwrap();
    let env = alloy::consensus::TxEnvelope::from(tx.into_signed(signature));
    Bytes::from(env.encoded_2718())
}

fn signed_tx(signer: &PrivateKeySigner, nonce: u64) -> Bytes {
    signed_tx_with_input(signer, nonce, Bytes::new())
}

fn block(number: u64, nonce: u64, signer: &PrivateKeySigner) -> Block {
    Block {
        number,
        timestamp: 1_757_000_000 + number,
        gas_limit: 100_000_000,
        txs: vec![signed_tx(signer, nonce)],
        deposits_end: None,
    }
}

/// First case: 10 blocks at `blocks_per_batch=10` -> one group -> `decode_batch` accepts
/// it whole, with `expected_first_block = None` (the very first batch this pipeline instance would ever
/// derive) and returns exactly the same blocks.
#[test]
fn ten_blocks_grouped_into_one_batch_decodes_cleanly_through_derive() {
    let signer = PrivateKeySigner::random();
    let blocks: Vec<Block> = (0..10).map(|n| block(n, n, &signer)).collect();

    let groups = group_all(blocks.clone(), 10);
    assert_eq!(groups.len(), 1);

    let compressed = rome_zk_batcher::channel::encode_stream(&groups[0]);
    let decoded = decode_batch(&compressed, CHAIN_ID, 0, 10, None)
        .expect("derive's decode_batch must accept a 10-block group at cap 10");
    assert_eq!(decoded, groups[0]);
}

/// Second case: 25 blocks at `blocks_per_batch=10` -> three groups (10/10/5) -> each
/// posted under its own sequential batch id -> `decode_batch` accepts every one of them, threading
/// `expected_first_block` across batches exactly the way `crate::pipeline::DerivePipeline` does in
/// production (each batch's `expected_first_block` is the previous batch's last block + 1).
#[test]
fn twenty_five_blocks_in_three_groups_all_decode_through_derive_with_correct_continuity() {
    let signer = PrivateKeySigner::random();
    let blocks: Vec<Block> = (0..25).map(|n| block(n, n, &signer)).collect();

    let groups = group_all(blocks, 10);
    assert_eq!(groups.len(), 3);
    assert_eq!(
        groups.iter().map(|g| g.len()).collect::<Vec<_>>(),
        vec![10, 10, 5]
    );

    let mut expected_first_block = None;
    for (batch_id, group) in groups.iter().enumerate() {
        let compressed = rome_zk_batcher::channel::encode_stream(group);
        let decoded = decode_batch(
            &compressed,
            CHAIN_ID,
            batch_id as u64,
            10,
            expected_first_block,
        )
        .unwrap_or_else(|e| panic!("batch {batch_id} must decode cleanly: {e}"));
        assert_eq!(&decoded, group);
        expected_first_block = Some(group.last().unwrap().number + 1);
    }
}

/// A group over the cap (constructed directly, bypassing this crate's own grouper, exactly the
/// adversarial-content case `decode_batch` itself must catch) is refused by derive — proving this crate's
/// grouping and derive's decode gate agree on the *same* cap, not two independently-drifting numbers.
#[test]
fn a_group_over_the_cap_is_refused_by_derive_too() {
    let signer = PrivateKeySigner::random();
    let blocks: Vec<Block> = (0..11).map(|n| block(n, n, &signer)).collect();
    let compressed = rome_zk_batcher::channel::encode_stream(&blocks);

    let err = decode_batch(&compressed, CHAIN_ID, 0, 10, None).unwrap_err();
    assert!(
        matches!(err, rome_zk_derive::PipelineError::Critical(_)),
        "{err:?}"
    );
}

/// A `SizeCappedGrouper` closing a group early on size (a
/// synthetic large-tx 10th block, at cap 10, closing at 9 blocks instead) still produces groups
/// `rome-zk-derive`'s `decode_batch` accepts — real signed txs through the real channel codec, exactly
/// the shape a live `--follow` run would post.
#[test]
fn a_size_closed_nine_block_group_decodes_through_derive_and_the_carried_block_starts_the_next() {
    let signer = PrivateKeySigner::random();
    // A budget that 9 small-tx blocks fit inside, but not with a large-tx 10th block added too (measured
    // directly via `encode_stream`, not guessed — mirrors `grouping::tests`' own test setup).
    let small_blocks: Vec<Block> = (0..9).map(|n| block(n, n, &signer)).collect();
    let large_tx = signed_tx_with_input(&signer, 9, Bytes::from(vec![0x42u8; 8_000]));
    let large_block = Block {
        number: 9,
        timestamp: 1_757_000_009,
        gas_limit: 100_000_000,
        txs: vec![large_tx],
        deposits_end: None,
    };
    let small_len = rome_zk_batcher::channel::encode_stream(&small_blocks).len();
    let mut with_large = small_blocks.clone();
    with_large.push(large_block.clone());
    let combined_len = rome_zk_batcher::channel::encode_stream(&with_large).len();
    let large_alone_len =
        rome_zk_batcher::channel::encode_stream(std::slice::from_ref(&large_block)).len();
    let max_frame_body_len = combined_len - 1;
    assert!(max_frame_body_len >= small_len && max_frame_body_len >= large_alone_len);

    let mut grouper = SizeCappedGrouper::new(10, 1, max_frame_body_len, None);
    let now = std::time::Instant::now();
    for b in small_blocks {
        assert!(matches!(
            grouper.push(b, now).unwrap(),
            PushOutcome::Accepted
        ));
    }
    let outcome = grouper.push(large_block.clone(), now).unwrap();
    assert!(matches!(
        outcome,
        PushOutcome::Closed {
            carry_over: Some(_),
            ..
        }
    ));
    let first_group = grouper.take_group();
    assert_eq!(first_group.len(), 9, "must close at 9 blocks, not 10");

    let compressed = rome_zk_batcher::channel::encode_stream(&first_group);
    let decoded = decode_batch(&compressed, CHAIN_ID, 0, 10, None)
        .expect("derive's decode_batch must accept the size-closed 9-block group");
    assert_eq!(decoded, first_group);

    // The carried-over (large) block starts the next group — derive accepts that one too, with
    // continuity threaded from the first batch's own last block.
    assert!(matches!(
        grouper.push(large_block.clone(), now).unwrap(),
        PushOutcome::Accepted
    ));
    let second_group = grouper.take_group();
    assert_eq!(second_group, vec![large_block]);
    let compressed2 = rome_zk_batcher::channel::encode_stream(&second_group);
    let decoded2 = decode_batch(&compressed2, CHAIN_ID, 1, 10, Some(9))
        .expect("derive's decode_batch must accept the carried-over block's own batch");
    assert_eq!(decoded2, second_group);
}
