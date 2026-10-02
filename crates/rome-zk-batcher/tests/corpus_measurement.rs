//! Compression-ratio / bytes-per-tx measurement against the real corpora
//! (`fixtures/corpus/{transfers,swaps,mixed}.rlp`): bytes per tx for a 1,000-tx
//! transfer block and a mixed corpus. Marked `#[ignore]` because it is a measurement, not a gate (it
//! still skips cleanly if the corpus is missing) — run manually with `cargo test -p rome-zk-batcher --test
//! corpus_measurement -- --ignored --nocapture`.

use alloy_primitives::Bytes;
use rome_zk_batcher::channel::{encode_stream, Block};

/// Each corpus file is one `0x`-prefixed hex-encoded raw tx per line (verified by inspection: 300-400
/// lines, `~110-230` hex chars each).
fn load_corpus(path: &std::path::Path) -> Vec<Bytes> {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let hex = l.trim().trim_start_matches("0x");
            Bytes::from(hex::decode(hex).unwrap_or_else(|e| panic!("bad hex line in corpus: {e}")))
        })
        .collect()
}

fn measure(label: &str, txs: Vec<Bytes>) {
    let tx_count = txs.len();
    let raw_bytes: usize = txs.iter().map(|t| t.len()).sum();
    // Group into blocks of 20 (one design "block" = 20 sub-blocks worth), 10 blocks per batch — matching
    // `blocks_per_batch` default; the grouping only affects RLP list-nesting overhead marginally, not the
    // measurement's point (compression ratio / bytes-per-Solana-tx for real tx content).
    let blocks: Vec<Block> = txs
        .chunks(20)
        .enumerate()
        .map(|(i, chunk)| Block {
            number: i as u64,
            timestamp: 1_757_000_000 + i as u64,
            gas_limit: 100_000_000,
            txs: chunk.to_vec(),
        })
        .collect();
    let compressed = encode_stream(&blocks);
    let frames = rome_zk_batcher::channel::cut_frames(
        1,
        1,
        &compressed,
        rome_zk_batcher::channel::DEFAULT_MAX_FRAME_BODY_LEN,
    );
    // "bytes per tx" here is compressed-DA-bytes-per-tx (what actually goes on chain), not raw tx bytes —
    // the 74-177 B/tx figures it is compared against are already-compressed on-chain figures.
    println!(
        "{label}: {tx_count} txs, {raw_bytes} raw bytes ({:.1} raw B/tx), {} compressed bytes \
         ({:.1} compressed B/tx), ratio {:.3}, {} frames of <= {} B",
        raw_bytes as f64 / tx_count as f64,
        compressed.len(),
        compressed.len() as f64 / tx_count as f64,
        compressed.len() as f64 / raw_bytes as f64,
        frames.len(),
        rome_zk_batcher::channel::DEFAULT_MAX_FRAME_BODY_LEN,
    );
}

fn corpus_dir() -> Option<std::path::PathBuf> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/corpus");
    dir.exists().then_some(dir)
}

#[test]
#[ignore = "measurement, not a gate: reads the corpus in fixtures/corpus; run it manually to take the measurement"]
fn compression_ratio_and_bytes_per_tx_against_real_corpora() {
    let Some(dir) = corpus_dir() else {
        eprintln!("corpus not found, skipping (expected at fixtures/corpus)");
        return;
    };
    for name in ["transfers", "swaps", "mixed"] {
        let path = dir.join(format!("{name}.rlp"));
        if !path.exists() {
            eprintln!("missing {}, skipping", path.display());
            continue;
        }
        measure(name, load_corpus(&path));
    }
}
