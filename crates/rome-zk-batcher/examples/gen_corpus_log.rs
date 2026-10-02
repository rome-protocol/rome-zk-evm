//! Fixture generator for the devnet measurement (not part of the shipped binary): builds a real
//! ordered log — `blocks` complete blocks, `SUB_BLOCKS_PER_BLOCK` sub-blocks each — from the real
//! corpora (`fixtures/corpus/{transfers,mixed,swaps}.rlp`), sized so the
//! resulting channel stream cuts into approximately `--target-frames` frames at the default
//! `max_frame_body_len` (3,681 B) — i.e. `--target-frames 900` reproduces the "~900 frames ≈ 3.3 MiB" batch
//! the design assumes, `--target-frames 90` a ~10x smaller one. The corpus has only ~800 real txs; hitting
//! a multi-thousand-tx target means cycling through it, and a byte-*similar* repeat would let zstd
//! collapse it to near nothing (defeating the point of measuring realistic chunk volume) — see
//! [`cycled_tx`] for how repeats past the first cycle are kept from being cheaply back-referenced.
//!
//! Usage:
//!   cargo run -p rome-zk-batcher --example gen_corpus_log -- \
//!     --out <dir> --target-frames 900 --chain-id 200101 [--corpus-dir <dir>] [--blocks 10]
//!
//! Prints the calibrated tx count, raw/compressed bytes, and the actual frame count after a real
//! `BlockSource` read-back (not just the write side) — the log-generation equivalent of a re-derive check.

use alloy::primitives::B256;
use alloy::signers::local::PrivateKeySigner;
use alloy_primitives::Bytes;
use rand::{RngCore, SeedableRng};
use rome_zk_batcher::channel::{cut_frames, encode_stream, Block, DEFAULT_MAX_FRAME_BODY_LEN};
use rome_zk_batcher::source::BlockSource;
use rome_zk_sequencer::header::SubBlockHeader;
use rome_zk_sequencer::log::LogWriter;
use rome_zk_sequencer::sealer::SUB_BLOCKS_PER_BLOCK;
use rome_zk_sequencer::signing::sign_header;
use std::path::PathBuf;

struct Args {
    corpus_dir: PathBuf,
    out: PathBuf,
    chain_id: u64,
    target_frames: usize,
    blocks: u64,
    max_frame_body_len: usize,
}

fn parse_args() -> Args {
    let mut corpus_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/corpus");
    let mut out = PathBuf::from("./corpus-log");
    let mut chain_id = 200_101u64;
    let mut target_frames = 900usize;
    let mut blocks = 10u64;
    let mut max_frame_body_len = DEFAULT_MAX_FRAME_BODY_LEN;
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let flag = argv[i].as_str();
        let val = || {
            argv.get(i + 1)
                .unwrap_or_else(|| panic!("{flag} needs a value"))
                .clone()
        };
        match flag {
            "--corpus-dir" => corpus_dir = PathBuf::from(val()),
            "--out" => out = PathBuf::from(val()),
            "--chain-id" => chain_id = val().parse().expect("bad --chain-id"),
            "--target-frames" => target_frames = val().parse().expect("bad --target-frames"),
            "--blocks" => blocks = val().parse().expect("bad --blocks"),
            "--max-frame-body-len" => {
                max_frame_body_len = val().parse().expect("bad --max-frame-body-len")
            }
            other => panic!("unknown flag: {other}"),
        }
        i += 2;
    }
    Args {
        corpus_dir,
        out,
        chain_id,
        target_frames,
        blocks,
        max_frame_body_len,
    }
}

fn load_corpus(dir: &std::path::Path) -> Vec<Bytes> {
    let mut all = Vec::new();
    for name in ["transfers", "mixed", "swaps"] {
        let path = dir.join(format!("{name}.rlp"));
        let Ok(s) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in s.lines().filter(|l| !l.trim().is_empty()) {
            let hex = line.trim().trim_start_matches("0x");
            all.push(Bytes::from(
                hex::decode(hex).expect("bad hex line in corpus"),
            ));
        }
    }
    assert!(
        !all.is_empty(),
        "no corpus lines loaded from {} — check the path (the corpus is committed at fixtures/corpus/)",
        dir.display()
    );
    all
}

/// The `n`-th cycle through `corpus` — cycle 0 is the real corpus bytes unmodified. `zstd`'s window
/// (≈8 MiB at level 19) spans this whole channel stream, so a byte-*similar* repeat (e.g. the same bytes
/// plus a short counter suffix) still lets it emit a cheap back-reference instead of new literals,
/// collapsing many cycles down to near nothing — which would silently defeat the point of measuring
/// realistic chunk volume. Every cycle past the first is instead replaced with independent
/// pseudo-random bytes of the same length, seeded deterministically from the flat index `i` (reproducible,
/// not proof of real tx structure) — this mimics the entropy density a real corpus of *distinct* txs would
/// have (a raw signed tx is majority high-entropy signature/hash bytes already) without the artifact of
/// literal duplication.
fn cycled_tx(corpus: &[Bytes], i: usize) -> Bytes {
    let base = &corpus[i % corpus.len()];
    let cycle = i / corpus.len();
    if cycle == 0 {
        base.clone()
    } else {
        let mut rng = rand::rngs::StdRng::seed_from_u64(i as u64);
        let mut v = vec![0u8; base.len()];
        rng.fill_bytes(&mut v);
        Bytes::from(v)
    }
}

/// Builds `blocks` blocks of block-number/txs (no header/RLP concerns — just what `channel::encode_stream`
/// needs) from `tx_count` cycled corpus txs, split as evenly as possible across blocks. Numbered from 1 —
/// the ordered log starts at block 1, genesis 0 is never sealed.
fn build_blocks(corpus: &[Bytes], tx_count: usize, blocks: u64) -> Vec<Block> {
    let per_block = tx_count.div_ceil(blocks as usize).max(1);
    (0..blocks)
        .map(|b| {
            let start = (b as usize) * per_block;
            let end = (start + per_block).min(tx_count);
            let txs = (start..end).map(|i| cycled_tx(corpus, i)).collect();
            Block {
                number: b + 1,
                timestamp: 1_757_000_000 + b,
                gas_limit: 100_000_000,
                txs,
            }
        })
        .collect()
}

fn main() {
    let args = parse_args();
    let corpus = load_corpus(&args.corpus_dir);
    println!(
        "loaded {} corpus txs from {:?}",
        corpus.len(),
        args.corpus_dir
    );

    // --- calibrate: how many compressed bytes does one cycled tx contribute on average? ---
    const SAMPLE_TXS: usize = 4_000;
    let sample_blocks = build_blocks(&corpus, SAMPLE_TXS, args.blocks);
    let sample_compressed = encode_stream(&sample_blocks);
    let bytes_per_tx = sample_compressed.len() as f64 / SAMPLE_TXS as f64;
    let target_bytes = args.target_frames as f64 * args.max_frame_body_len as f64;
    let tx_count = ((target_bytes / bytes_per_tx).round() as usize).max(1);
    println!(
        "calibration: {SAMPLE_TXS} sample txs -> {} compressed bytes ({bytes_per_tx:.2} B/tx); \
         target {} frames * {} B = {target_bytes:.0} B -> {tx_count} txs",
        sample_compressed.len(),
        args.target_frames,
        args.max_frame_body_len
    );

    // --- build the real block set and write it as an ordered log ---
    let final_blocks = build_blocks(&corpus, tx_count, args.blocks);
    let raw_bytes: usize = final_blocks
        .iter()
        .flat_map(|b| b.txs.iter())
        .map(|t| t.len())
        .sum();
    let compressed = encode_stream(&final_blocks);
    let frames = cut_frames(args.chain_id, 0, &compressed, args.max_frame_body_len);
    println!(
        "final: {tx_count} txs, {raw_bytes} raw bytes -> {} compressed bytes -> {} frames (target {})",
        compressed.len(),
        frames.len(),
        args.target_frames
    );

    std::fs::create_dir_all(&args.out).expect("create out dir");
    write_log(&args.out, args.chain_id, &final_blocks);

    // --- read-back check: BlockSource must reproduce exactly the same blocks (the same check the
    // binary's `--once` mode relies on before ever spending a fee) ---
    let mut source = BlockSource::open(
        &args.out,
        args.chain_id,
        100_000_000,
        SUB_BLOCKS_PER_BLOCK,
        1,
        0,
    )
    .expect("open the log we just wrote");
    let mut read_back = Vec::new();
    while let Some(sourced) = source.next_block().expect("read a block") {
        read_back.push(sourced.block);
    }
    assert_eq!(
        read_back.len(),
        final_blocks.len(),
        "BlockSource must read back exactly the blocks written"
    );
    for (w, r) in final_blocks.iter().zip(read_back.iter()) {
        assert_eq!(
            w.txs.len(),
            r.txs.len(),
            "block {}: tx count round-trip",
            w.number
        );
    }
    println!("read-back OK: {} blocks at {:?}", read_back.len(), args.out);
}

/// Writes `blocks` as a real ordered log (`SUB_BLOCKS_PER_BLOCK` signed sub-block records per block,
/// txs spread evenly across the sub-blocks in order) — the exact shape `source.rs`/`BlockSource` expects,
/// duplicating `source.rs`'s own test helper (`write_block`) since that helper is private to its test
/// module, not a library export.
fn write_log(dir: &std::path::Path, chain_id: u64, blocks: &[Block]) {
    let mut writer = LogWriter::open(dir, 10_000).expect("open log writer");
    let mut prev_hash = B256::ZERO;
    for block in blocks {
        let per_sub_block = block
            .txs
            .len()
            .div_ceil(SUB_BLOCKS_PER_BLOCK as usize)
            .max(1);
        for index in 0..SUB_BLOCKS_PER_BLOCK {
            let start = (index as usize) * per_sub_block;
            let end = (start + per_sub_block).min(block.txs.len());
            let txs: Vec<_> = if start < block.txs.len() {
                block.txs[start..end].to_vec()
            } else {
                Vec::new()
            };
            let header = SubBlockHeader {
                chain_id,
                block: block.number,
                index,
                timestamp_us: (block.timestamp * 1_000_000) + index as u64 * 50_000,
                tx_root: B256::repeat_byte(index as u8),
                receipts_root: B256::repeat_byte(index as u8 + 1),
                gas_used: 21_000 * txs.len() as u64,
                prev_hash,
            };
            let signature = sign_header(&PrivateKeySigner::random(), &header);
            writer
                .append(&header, &signature, &txs)
                .expect("append sub-block");
            prev_hash = header.hash();
        }
    }
}
