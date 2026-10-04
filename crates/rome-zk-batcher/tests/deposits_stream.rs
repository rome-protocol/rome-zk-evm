//! The deposit cursor in the channel stream, end to end: a real ordered log (written with the log's own writer,
//! credits built by `deposit_withdrawal`) read by `BlockSource`, grouped by `SizeCappedGrouper`, encoded and
//! decoded again by the channel codec.
//!
//! The first test pins the other half of the rule: a log without deposits gives the same stream and frame bytes
//! it gave before the fifth field existed. Its literals are the channel crate's own pinned fixture for two blocks
//! (`golden_vector_channel_stream_bytes_are_pinned`), reached here through a log, the source and the grouper.

use alloy::primitives::{Address, Bytes, B256};
use alloy::signers::local::PrivateKeySigner;
use rome_zk_batcher::channel::{
    cut_frames, decode_stream, encode_stream, resolve_deposits_end, Block,
    DEFAULT_MAX_FRAME_BODY_LEN,
};
use rome_zk_batcher::grouping::{CloseReason, DepositCap, PushOutcome, SizeCappedGrouper};
use rome_zk_batcher::source::BlockSource;
use rome_zk_executor_api::deposit_withdrawal;
use rome_zk_sequencer::header::SubBlockHeader;
use rome_zk_sequencer::log::LogWriter;
use rome_zk_sequencer::signing::sign_header;
use std::time::Instant;
use tempfile::tempdir;

const CHAIN_ID: u64 = 200_198;
const GAS_LIMIT: u64 = 100_000_000;

/// One record per block (so `sub_blocks_per_block` is 1): the block's txs, and its credits on the same record.
fn append(
    writer: &mut LogWriter,
    block: u64,
    timestamp_secs: u64,
    txs: &[&'static [u8]],
    indices: &[u64],
) {
    let header = SubBlockHeader {
        chain_id: CHAIN_ID,
        block,
        index: 0,
        timestamp_us: timestamp_secs * 1_000_000,
        tx_root: B256::ZERO,
        receipts_root: B256::ZERO,
        gas_used: 21_000,
        prev_hash: B256::ZERO,
        deposits_end: indices.last().map(|i| i + 1),
    };
    let signature = sign_header(&PrivateKeySigner::random(), &header);
    let txs: Vec<Bytes> = txs.iter().map(|t| Bytes::from_static(t)).collect();
    let withdrawals: Vec<_> = indices
        .iter()
        .map(|&i| deposit_withdrawal(i, Address::repeat_byte(0x33), 10_000 + i))
        .collect();
    writer
        .append_with_withdrawals(&header, &signature, &txs, &withdrawals)
        .unwrap();
}

/// One closed group: its blocks, its `(deposit_from, deposit_to)`, and why it closed (`None` for the final partial group).
type ClosedGroup = (Vec<Block>, (u64, u64), Option<CloseReason>);

/// Reads every block off the log and groups them (no age close); returns each closed group with its
/// `(deposit_from, deposit_to)`.
fn group_log(
    dir: &std::path::Path,
    from_block: u64,
    deposit_start: u64,
    cap: Option<DepositCap>,
    cap_blocks: u64,
) -> Vec<ClosedGroup> {
    let mut source = BlockSource::open(dir, CHAIN_ID, GAS_LIMIT, 1, from_block, 0)
        .unwrap()
        .with_deposit_start(deposit_start);
    let mut grouper = SizeCappedGrouper::new(cap_blocks, 900, DEFAULT_MAX_FRAME_BODY_LEN, None)
        .with_deposits(deposit_start, cap);
    let mut groups = Vec::new();
    while let Some(sourced) = source.next_block().unwrap() {
        let mut block = sourced.block;
        loop {
            match grouper.push(block.clone(), Instant::now()).unwrap() {
                PushOutcome::Accepted => break,
                PushOutcome::Closed { reason, carry_over } => {
                    let range = grouper.deposit_range();
                    groups.push((grouper.take_group(), range, Some(reason)));
                    match carry_over {
                        Some(carried) => block = carried,
                        None => break,
                    }
                }
            }
        }
    }
    if !grouper.is_empty() {
        let range = grouper.deposit_range();
        groups.push((grouper.take_group(), range, None));
    }
    groups
}

/// A log without deposits: the stream and the frames are the bytes the codec pinned before the fifth field
/// existed.
#[test]
fn a_log_without_deposits_gives_the_pinned_stream_and_frame_bytes() {
    let dir = tempdir().unwrap();
    let mut writer = LogWriter::open(dir.path(), 10_000).unwrap();
    append(&mut writer, 0, 1_757_000_000, &[b"tx-a", b"tx-b"], &[]);
    append(&mut writer, 1, 1_757_000_001, &[b"tx-c"], &[]);
    drop(writer);

    let groups = group_log(dir.path(), 0, 0, None, 10);
    assert_eq!(groups.len(), 1);
    let (blocks, range, _) = &groups[0];
    assert_eq!(*range, (0, 0));
    assert!(blocks.iter().all(|b| b.deposits_end.is_none()));

    let compressed = encode_stream(blocks);
    assert_eq!(
        hex::encode(&compressed),
        "28b52ffd0068510100e9d6808468b9b1408405f5e100ca8474782d618474782d62d1018468b9b1418405f5e100c58474782d63"
    );
    let frames = cut_frames(11, 22, &compressed, DEFAULT_MAX_FRAME_BODY_LEN);
    let all: Vec<u8> = frames.iter().flat_map(|f| f.to_bytes()).collect();
    assert_eq!(
        hex::encode(&all),
        "c0cefee3d28cf77eaea6a4b9bf48a4a400000128b52ffd0068510100e9d6808468b9b1408405f5e100ca8474782d618474782d62d1018468b9b1418405f5e100c58474782d63"
    );
}

/// A log with credits: the fifth field is on exactly the blocks that credit, the stream decodes and resolves to
/// the cumulative value after every block, and the encoded bytes equal those of the same blocks built by hand.
#[test]
fn credits_in_the_log_become_the_fifth_field_where_the_value_changes() {
    let dir = tempdir().unwrap();
    let mut writer = LogWriter::open(dir.path(), 10_000).unwrap();
    append(&mut writer, 1, 1_757_000_000, &[b"a"], &[5, 6]);
    append(&mut writer, 2, 1_757_000_001, &[b"b"], &[]);
    append(&mut writer, 3, 1_757_000_002, &[b"c"], &[7]);
    append(&mut writer, 4, 1_757_000_003, &[b"d"], &[]);
    drop(writer);

    let groups = group_log(dir.path(), 1, 5, None, 10);
    assert_eq!(groups.len(), 1);
    let (blocks, range, _) = &groups[0];
    assert_eq!(*range, (5, 8));
    let fifth: Vec<Option<u64>> = blocks.iter().map(|b| b.deposits_end).collect();
    assert_eq!(fifth, [Some(7), None, Some(8), None]);

    let by_hand: Vec<Block> = [
        (1u64, &b"a"[..], Some(7u64)),
        (2, &b"b"[..], None),
        (3, &b"c"[..], Some(8)),
        (4, &b"d"[..], None),
    ]
    .iter()
    .map(|&(number, tx, deposits_end)| Block {
        number,
        timestamp: 1_757_000_000 + number - 1,
        gas_limit: GAS_LIMIT,
        txs: vec![Bytes::copy_from_slice(tx)],
        deposits_end,
    })
    .collect();
    assert_eq!(blocks, &by_hand);
    let compressed = encode_stream(blocks);
    assert_eq!(compressed, encode_stream(&by_hand));

    let decoded = decode_stream(&compressed).unwrap();
    assert_eq!(resolve_deposits_end(&decoded, 5).unwrap(), [7, 7, 8, 8]);
}

/// The cap closes a batch before the block that would pass it, and the next batch's range starts at the end of
/// the one before: indices run on across the two batches.
#[test]
fn the_cap_splits_the_log_into_batches_that_run_on() {
    let dir = tempdir().unwrap();
    let mut writer = LogWriter::open(dir.path(), 10_000).unwrap();
    append(&mut writer, 1, 1_757_000_000, &[b"a"], &[0, 1]);
    append(&mut writer, 2, 1_757_000_001, &[b"b"], &[2, 3]);
    append(&mut writer, 3, 1_757_000_002, &[b"c"], &[4, 5]);
    drop(writer);

    let cap = DepositCap {
        active: 8,
        pending: Some(4),
    };
    let groups = group_log(dir.path(), 1, 0, Some(cap), 10);
    assert_eq!(groups.len(), 2);

    let (first, first_range, reason) = &groups[0];
    assert_eq!(*reason, Some(CloseReason::Deposits));
    assert_eq!(first.iter().map(|b| b.number).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(*first_range, (0, 4));
    assert_eq!(resolve_deposits_end(first, 0).unwrap(), [2, 4]);

    let (second, second_range, _) = &groups[1];
    assert_eq!(second.iter().map(|b| b.number).collect::<Vec<_>>(), [3]);
    assert_eq!(second_range.0, first_range.1);
    assert_eq!(*second_range, (4, 6));
    assert_eq!(resolve_deposits_end(second, 4).unwrap(), [6]);
}

/// A first index that is not the cursor's, and a gap after it, stop the source with a named error; the batcher
/// binary exits on any source error.
#[test]
fn a_wrong_first_index_and_a_gap_stop_the_source() {
    use rome_zk_batcher::source::SourceError;

    let dir = tempdir().unwrap();
    let mut writer = LogWriter::open(dir.path(), 10_000).unwrap();
    append(&mut writer, 1, 1_757_000_000, &[b"a"], &[3]);
    drop(writer);
    let mut source = BlockSource::open(dir.path(), CHAIN_ID, GAS_LIMIT, 1, 1, 0)
        .unwrap()
        .with_deposit_start(2);
    assert!(matches!(
        source.next_block().unwrap_err(),
        SourceError::FirstDepositIndexMismatch {
            block: 1,
            expected: 2,
            got: 3
        }
    ));

    let dir = tempdir().unwrap();
    let mut writer = LogWriter::open(dir.path(), 10_000).unwrap();
    append(&mut writer, 1, 1_757_000_000, &[b"a"], &[2]);
    append(&mut writer, 2, 1_757_000_001, &[b"b"], &[4]);
    drop(writer);
    let mut source = BlockSource::open(dir.path(), CHAIN_ID, GAS_LIMIT, 1, 1, 0)
        .unwrap()
        .with_deposit_start(2);
    source.next_block().unwrap().unwrap();
    assert!(matches!(
        source.next_block().unwrap_err(),
        SourceError::DepositIndexGap {
            block: 2,
            expected: 3,
            got: 4
        }
    ));
}
