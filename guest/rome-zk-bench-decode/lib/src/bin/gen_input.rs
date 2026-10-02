//! Fixture generator: builds a [`rome_zk_bench_decode_lib::BenchInput`] and writes it to a file in
//! the exact bincode 2.x wire format `ziskos::io::read()` expects (`bincode::serde::encode_to_vec` with
//! `bincode::config::standard()`) — so ziskemu's `-i <file>` reads it straight into the guest.
//!
//! Host-only (uses the real `rome-zk-channel`/`rome-zk-layouts` encoder, so the synthetic fixtures
//! match what the batcher produces); never built for the ZisK target.

use clap::{Parser, Subcommand};
use rome_zk_bench_decode_lib::BenchInput;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// A synthetic batch of `blocks` blocks, each with `txs_per_block` legacy-transfer-shaped
    /// transactions of `tx_len` bytes, encoded with the real batcher encoder
    /// (`rome_zk_channel::encode_stream` at level 19) and cut at the real default frame size.
    Synthetic {
        #[arg(long, default_value_t = 200_101)]
        chain_id: u64,
        #[arg(long, default_value_t = 1)]
        batch: u64,
        #[arg(long, default_value_t = 1_757_000_000)]
        open_slot: u64,
        #[arg(long, default_value_t = 10)]
        blocks: u64,
        #[arg(long, default_value_t = 0)]
        txs_per_block: usize,
        /// Raw (pre-compression) bytes per tx — ~110 B for an unsigned-field-complete legacy ETH
        /// transfer (nonce/gasPrice/gasLimit/to/value/data/v/r/s), matching this crate's own test fixture.
        #[arg(long, default_value_t = 110)]
        tx_len: usize,
        #[arg(long)]
        out: String,
    },
    /// Loads a fetched real-batch fixture (`fixtures/inbox/txv1-dev-batch-<id>.json`) and writes it in the bincode
    /// wire format.
    FromFixture {
        #[arg(long)]
        fixture: String,
        #[arg(long)]
        out: String,
    },
}

#[derive(serde::Deserialize)]
struct FixtureJson {
    chain_id: u64,
    batch: u64,
    open_slot: u64,
    expected_count: u32,
    /// Hex-encoded (no 0x prefix) chunk bodies, idx order.
    chunk_bodies_hex: Vec<String>,
}

fn main() {
    let cli = Cli::parse();
    let (input, out) = match cli.cmd {
        Cmd::Synthetic {
            chain_id,
            batch,
            open_slot,
            blocks,
            txs_per_block,
            tx_len,
            out,
        } => {
            let block_list: Vec<rome_zk_channel::Block> = (0..blocks)
                .map(|n| rome_zk_channel::Block {
                    number: n + 1,
                    timestamp: open_slot + n,
                    gas_limit: 40_000_000,
                    txs: (0..txs_per_block)
                        .map(|i| {
                            alloy_primitives::Bytes::from(
                                rome_zk_bench_decode_lib::pseudo_random_tx(n, i as u64, tx_len),
                            )
                        })
                        .collect(),
                })
                .collect();
            let compressed = rome_zk_channel::encode_stream(&block_list);
            let frames = rome_zk_channel::cut_frames(
                chain_id,
                batch,
                &compressed,
                rome_zk_channel::DEFAULT_MAX_FRAME_BODY_LEN,
            );
            let chunk_bodies: Vec<Vec<u8>> = frames.iter().map(|f| f.to_bytes()).collect();
            eprintln!(
                "synthetic: {blocks} blocks x {txs_per_block} txs x {tx_len} B raw -> {} B compressed -> {} chunks",
                compressed.len(),
                chunk_bodies.len()
            );
            (
                BenchInput {
                    chain_id,
                    batch,
                    open_slot,
                    expected_count: chunk_bodies.len() as u32,
                    chunk_bodies,
                },
                out,
            )
        }
        Cmd::FromFixture { fixture, out } => {
            let raw = std::fs::read_to_string(&fixture).expect("read fixture json");
            let f: FixtureJson = serde_json::from_str(&raw).expect("parse fixture json");
            let chunk_bodies: Vec<Vec<u8>> = f
                .chunk_bodies_hex
                .iter()
                .map(|h| hex::decode(h).expect("hex-decode chunk body"))
                .collect();
            (
                BenchInput {
                    chain_id: f.chain_id,
                    batch: f.batch,
                    open_slot: f.open_slot,
                    expected_count: f.expected_count,
                    chunk_bodies,
                },
                out,
            )
        }
    };
    let payload =
        bincode::serde::encode_to_vec(&input, bincode::config::standard()).expect("bincode encode");
    // ziskos's own input wire format (src/lib.rs `read_input`/`read_slice_zerocopy`, both `zisk_guest`
    // and native): an 8-byte LE length prefix, then exactly that many payload bytes — `ziskos::io::read`
    // decodes only the payload slice, never the prefix. ziskemu additionally requires the whole file's
    // length to be a multiple of 8 ("input size must be a multiple of 8"); zero-pad the tail to the next
    // 8-byte boundary after the prefix+payload (the length prefix still names the true, unpadded payload
    // length, so `bincode::decode_from_slice`'s exact-length read is unaffected).
    let mut file_bytes = Vec::with_capacity(8 + payload.len() + 7);
    file_bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    file_bytes.extend_from_slice(&payload);
    while file_bytes.len() % 8 != 0 {
        file_bytes.push(0);
    }
    std::fs::write(&out, &file_bytes).expect("write input file");
    eprintln!(
        "wrote {} bytes to {out} (payload {} bytes)",
        file_bytes.len(),
        payload.len()
    );
}
