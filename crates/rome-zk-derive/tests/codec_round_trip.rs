//! Codec round-trip: frames produced by the batcher's own `channel.rs` ->
//! [`FrameQueue`]/[`ChannelBank`]/[`BatchQueue`]'s decode -> identical blocks + committed `BlockEnv`.
//! Uses the *exact* golden fixture `rome-zk-batcher/src/channel.rs`'s own
//! `golden_vector_channel_stream_round_trips_and_is_stable` test pins, so a future change to the
//! batcher's wire shape (RLP field order, zstd level, frame cut points) that would silently break this
//! crate's decoder is caught here too, not only in the batcher's own test.

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use rome_zk_channel as channel;
use rome_zk_channel::Block;
use rome_zk_derive::channel_bank::ChannelBank;
use rome_zk_derive::{attributes, batch_queue, frame_queue};

const CHAIN_ID: u64 = 200_101;

fn signed_tx(signer: &PrivateKeySigner, nonce: u64) -> Bytes {
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
    Bytes::from(alloy_eips::eip2718::Encodable2718::encoded_2718(
        &TxEnvelope::from(tx.into_signed(signature)),
    ))
}

/// Same shape as the batcher's own golden fixture (`rome-zk-batcher/src/channel.rs`'s
/// `golden_vector_channel_stream_round_trips_and_is_stable` test: 2 blocks, `1_757_000_000`-based
/// timestamps, `100_000_000` gas limit) — with real signed EIP-2718 txs standing in for that test's
/// opaque `b"tx-a"`/`b"tx-b"` placeholders, since the strict validity gate
/// ([`batch_queue::decode_batch`]) requires every tx to actually decode.
fn golden_blocks() -> Vec<Block> {
    let signer = PrivateKeySigner::random();
    vec![
        Block {
            number: 0,
            timestamp: 1_757_000_000,
            gas_limit: 100_000_000,
            txs: vec![signed_tx(&signer, 0), signed_tx(&signer, 1)],
            deposits_end: None,
        },
        Block {
            number: 1,
            timestamp: 1_757_000_001,
            gas_limit: 100_000_000,
            txs: vec![signed_tx(&signer, 2)],
            deposits_end: None,
        },
    ]
}

#[test]
fn frames_from_the_batchers_own_golden_vector_decode_to_identical_blocks_and_env() {
    let blocks = golden_blocks();
    let compressed = channel::encode_stream(&blocks);
    let frames = channel::cut_frames(
        CHAIN_ID,
        0,
        &compressed,
        channel::DEFAULT_MAX_FRAME_BODY_LEN,
    );

    // Chunk bodies as they would come off Solana: exactly `Frame::to_bytes()`.
    let chunk_bodies: Vec<Vec<u8>> = frames
        .iter()
        .map(rome_zk_channel::Frame::to_bytes)
        .collect();

    // FrameQueue
    let parsed = frame_queue::parse_frames(chunk_bodies, CHAIN_ID, 0).unwrap();
    assert_eq!(parsed, frames);

    // ChannelBank
    let channel_id = parsed[0].channel_id;
    let mut bank = ChannelBank::new(4);
    for f in parsed {
        bank.ingest(f);
    }
    let reassembled = bank.take_complete(channel_id).unwrap().unwrap();
    assert_eq!(reassembled, compressed);

    // BatchQueue
    let decoded_blocks = batch_queue::decode_batch(
        &reassembled,
        CHAIN_ID,
        0,
        rome_zk_derive::config::DEFAULT_BLOCKS_PER_BATCH,
        None,
    )
    .unwrap();
    assert_eq!(
        decoded_blocks, blocks,
        "must reconstruct the exact source blocks"
    );

    // AttributesQueue — the committed BlockEnv for each block, using the shared prev_randao formula.
    for (block, expected_number) in decoded_blocks.iter().zip([0u64, 1]) {
        let attrs = attributes::attributes_for_block(CHAIN_ID, Address::ZERO, block, vec![]);
        assert_eq!(attrs.env.number, expected_number);
        assert_eq!(
            attrs.env.prev_randao,
            rome_zk_executor_api::prev_randao(CHAIN_ID, expected_number)
        );
        assert_eq!(attrs.txs, block.txs);
    }
}

/// The same golden vector split into many small frames, ingested into [`ChannelBank`] out of order (as
/// a parallel-send batcher's confirmations can race in wall-clock time — `channel.rs`'s own
/// `multi_frame_block_and_a_tx_crossing_a_frame_boundary_round_trip` test covers the batcher side of
/// this; this proves the derive side survives the same shuffle). [`frame_queue::parse_frames`] itself
/// still sees chunk bodies in their real on-chain idx order (idx *is* frame_no,
/// by construction — `InboxRetrieval` always reads chunks `0..expected_count` in order); the shuffle
/// this test exercises is [`ChannelBank`]'s own reassembly tolerance, downstream of that check.
#[test]
fn out_of_order_frame_ingestion_still_reassembles_and_decodes_identically() {
    let blocks = golden_blocks();
    let compressed = channel::encode_stream(&blocks);
    let frames = channel::cut_frames(CHAIN_ID, 0, &compressed, 16);
    assert!(
        frames.len() > 2,
        "fixture must need several frames at this size"
    );

    // Chunk bodies as InboxRetrieval would hand them: idx order, one per on-chain chunk.
    let chunk_bodies: Vec<Vec<u8>> = frames
        .iter()
        .map(rome_zk_channel::Frame::to_bytes)
        .collect();
    let parsed = frame_queue::parse_frames(chunk_bodies, CHAIN_ID, 0).unwrap();

    // Ingest into the bank out of order (reversed) — this is the shuffle under test.
    let mut shuffled = parsed;
    shuffled.reverse();
    // Also duplicate one frame — `reassemble`'s own documented tolerance (last-write-wins on a
    // duplicate idx with an identical body).
    shuffled.push(shuffled[0].clone());

    let channel_id = shuffled[0].channel_id;
    let mut bank = ChannelBank::new(4);
    for f in shuffled {
        bank.ingest(f);
    }
    let reassembled = bank.take_complete(channel_id).unwrap().unwrap();
    assert_eq!(reassembled, compressed);
    let decoded = batch_queue::decode_batch(
        &reassembled,
        CHAIN_ID,
        0,
        rome_zk_derive::config::DEFAULT_BLOCKS_PER_BATCH,
        None,
    )
    .unwrap();
    assert_eq!(decoded, blocks);
}
