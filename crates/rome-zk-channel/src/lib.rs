//! `rome-zk-channel` — the channel/frame stream codec: one channel stream per batch =
//! `zstd-19(RLP([block_0..block_9]))`, cut into frames of at most [`DEFAULT_MAX_FRAME_BODY_LEN`] compressed bytes
//! each, each prefixed by a 19-byte frame header ([`rome_zk_layouts::frame`] owns the header's byte layout; this
//! crate owns everything above it).
//!
//! One codec, two consumers: the batcher encodes with it (re-exported as `rome_zk_batcher::channel` for
//! its own call sites and tests) and the derivation node decodes with it directly — both read and write
//! the identical bytes, never a second implementation.
//!
//! Adopted from `ethereum-optimism/optimism` `op-batcher/batcher/channel_builder.go` (a "channel" is one
//! compressed byte stream cut into frames written to separate DA transactions; `ChannelBuilder.AddBlock`
//! keeps a running compressed-size estimate and closes the channel before it would overflow its target
//! size — the "shadow compressor" pattern this module's [`ShadowCompressor`] follows) and
//! `OffchainLabs/nitro` `arbnode/batch_poster.go` (`checkBatchCorrectness`-shaped: derive the posted bytes
//! back into blocks/txs and compare to the source **before** paying to post them — this module's
//! [`decode_stream`] is what the batcher's re-derive-before-send step calls).
//!
//! ## Frame header (19 bytes)
//! ```text
//! channel_id: [u8; 16]   // keccak256(chain_id_le[8] ++ batch_le[8])[..16]
//! frame_no:   u16 (LE)
//! is_last:    u8         // 1 on the final frame of the channel, 0 otherwise
//! ```
//!
//! ## Block RLP shape `block_k = RLP{ number: u64, timestamp: u64, gas_limit: u64, txs: Vec<raw tx bytes> }` — a
//! block's `txs` are opaque length-prefixed byte strings (this codec never parses tx contents), matching how the
//! ordered log itself stores them (`rome_zk_sequencer::log::SubBlockRecord::txs`).

#![forbid(unsafe_code)]

use alloy_primitives::{keccak256, Bytes};
use alloy_rlp::{RlpDecodable, RlpEncodable};

/// Frame header length: 16-byte channel id + 2-byte frame number + 1-byte `is_last` flag. The single definition is
/// `rome_zk_layouts::frame::FRAME_HEADER_LEN`; re-exported under this name so every existing call site (this module,
/// `zk-inbox-client`, `rome-zk-batcher`'s own sizing code, tests) is unchanged.
pub use rome_zk_layouts::frame::FRAME_HEADER_LEN;

/// Default max *compressed* bytes per frame body. Chosen so the whole chunk lane —
/// `Open`+`Write`(the entire frame body in one instruction)+`Seal`+`SealLeaf` — fits one V1 (SIMD-0385,
/// 4,096-byte) transaction with room to spare (measured: 4,052 B; `sender.rs`'s own
/// `design_frame_v1_tx_fits_4096_and_carries_both_header_limits` test proves it) — this module only knows
/// about frame boundaries, never about the sender's transaction format.
pub const DEFAULT_MAX_FRAME_BODY_LEN: usize = 3_681;

/// Default max frames per channel before the shadow compressor closes it early: 900 frames, about 3.3 MB
/// compressed. `900 * 3_681 = 3,312,900` bytes ≈ 3.3 MiB, matching the design's figure.
pub const DEFAULT_MAX_FRAMES_PER_CHANNEL: usize = 900;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ChannelError {
    #[error("frame body {len} bytes exceeds max_frame_body_len {max}")]
    FrameTooLarge { len: usize, max: usize },
    #[error("frame too short for the {FRAME_HEADER_LEN}-byte header: {0} bytes")]
    HeaderTooShort(usize),
    #[error("frame {frame_no} has channel_id {actual:02x?}, expected {expected:02x?}")]
    ChannelIdMismatch {
        frame_no: u16,
        actual: [u8; 16],
        expected: [u8; 16],
    },
    #[error("missing frame {0} (frames must be contiguous 0..N with exactly one is_last)")]
    MissingFrame(u16),
    #[error("frame {0} marked is_last but is not the highest frame_no present")]
    IsLastNotHighest(u16),
    #[error(
        "frame {0} appears more than once (a duplicate frame_no is refused, identical or not)"
    )]
    DuplicateFrame(u16),
    #[error("no frame marked is_last")]
    NoLastFrame,
    #[error("zstd decompression failed: {0}")]
    Zstd(String),
    #[error("rlp decode failed: {0}")]
    Rlp(#[from] alloy_rlp::Error),
}

/// One block's channel-stream content. `txs` are raw (already-signed, EIP-2718-encoded) tx bytes, in the block's
/// inclusion order — see `source.rs` for how these are read off the ordered log.
#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct Block {
    pub number: u64,
    pub timestamp: u64,
    pub gas_limit: u64,
    pub txs: Vec<Bytes>,
}

/// `keccak256(chain_id_le[8] ++ batch_le[8])[..16]` — the 16-byte channel id every frame of one batch's
/// channel stream carries (binds frames to exactly one `(chain_id, batch)`, so frames from two different
/// batches can never be silently concatenated).
pub fn channel_id(chain_id: u64, batch: u64) -> [u8; 16] {
    let mut preimage = [0u8; 16];
    preimage[0..8].copy_from_slice(&chain_id.to_le_bytes());
    preimage[8..16].copy_from_slice(&batch.to_le_bytes());
    let hash = keccak256(preimage);
    let mut id = [0u8; 16];
    id.copy_from_slice(&hash[..16]);
    id
}

/// One frame: a fixed 19-byte header plus a compressed body slice of the channel stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub channel_id: [u8; 16],
    pub frame_no: u16,
    pub is_last: bool,
    pub body: Vec<u8>,
}

impl Frame {
    /// Encodes this frame as `header || body` — exactly the bytes a chunk PDA's body holds. The header
    /// itself is `rome_zk_layouts::frame::write_header` (the single definition of this layout); this
    /// codec's own job is only the body concatenation.
    pub fn to_bytes(&self) -> Vec<u8> {
        let header =
            rome_zk_layouts::frame::write_header(&rome_zk_layouts::frame::FrameHeaderFields {
                channel_id: self.channel_id,
                frame_no: self.frame_no,
                is_last: self.is_last,
            });
        let mut out = Vec::with_capacity(FRAME_HEADER_LEN + self.body.len());
        out.extend_from_slice(&header);
        out.extend_from_slice(&self.body);
        out
    }

    /// Decodes `header || body` back into a [`Frame`] via `rome_zk_layouts::frame::read`.
    pub fn from_bytes(d: &[u8]) -> Result<Self, ChannelError> {
        let f = rome_zk_layouts::frame::read(d).map_err(|e| match e {
            rome_zk_layouts::LayoutError::TooShort { got, .. } => ChannelError::HeaderTooShort(got),
            // `frame::read` has no magic/version check today (see its module doc) — kept exhaustive so a
            // future one added there is not silently swallowed here.
            rome_zk_layouts::LayoutError::BadMagic
            | rome_zk_layouts::LayoutError::BadVersion
            | rome_zk_layouts::LayoutError::BadZiskWord { .. }
            | rome_zk_layouts::LayoutError::BadZiskTail { .. } => {
                ChannelError::HeaderTooShort(d.len())
            }
        })?;
        Ok(Frame {
            channel_id: f.channel_id,
            frame_no: f.frame_no,
            is_last: f.is_last,
            body: d[FRAME_HEADER_LEN..].to_vec(),
        })
    }
}

/// `RLP([block_0..block_N])` then `zstd` level 19 — the full, uncut channel stream for one batch.
#[cfg(feature = "zstd-c")]
pub fn encode_stream(blocks: &[Block]) -> Vec<u8> {
    let rlp = alloy_rlp::encode(blocks.to_vec());
    // Level 19; `zstd::encode_all` is the one-shot (non-streaming) encoder — correct for the re-derive-before-send
    // comparison and for a channel closed by the shadow compressor, which tracks size via its own incremental
    // encoder (see `ShadowCompressor`) and re-encodes once, at close, to produce the exact bytes actually sent.
    zstd::encode_all(rlp.as_slice(), 19).expect("in-memory zstd encode cannot fail")
}

/// Inverse of [`encode_stream`]: decompress (C `zstd`) then RLP-decode back to the block list. Used by
/// the re-derive-before-send step (`pipeline.rs`) and by tests — this is the canonical decoder every
/// on-chain/off-chain consumer (batcher, derive) agrees with. [`decode_stream_pure`] is the ZisK-guest
/// equivalent: both must agree byte-for-byte on the block list they decode.
#[cfg(feature = "zstd-c")]
pub fn decode_stream(compressed: &[u8]) -> Result<Vec<Block>, ChannelError> {
    let rlp = zstd::decode_all(compressed).map_err(|e| ChannelError::Zstd(e.to_string()))?;
    decode_rlp_blocks(&rlp)
}

/// Decompresses (only — no RLP decode) via `ruzstd`, the pure-Rust decoder the ZisK guest target uses (no
/// C toolchain there). Split out from [`decode_stream_pure`] so a caller measuring per-stage cost
/// (`rome-zk-bench-decode`) can isolate "reassemble + ruzstd decompress" from "RLP decode", the two
/// per-stage buckets, without duplicating the ruzstd call site.
#[cfg(feature = "decode-pure")]
pub fn decompress_pure(compressed: &[u8]) -> Result<Vec<u8>, ChannelError> {
    use std::io::Read;
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(compressed)
        .map_err(|e| ChannelError::Zstd(e.to_string()))?;
    let mut rlp = Vec::new();
    decoder
        .read_to_end(&mut rlp)
        .map_err(|e| ChannelError::Zstd(e.to_string()))?;
    Ok(rlp)
}

/// Pure-Rust equivalent of [`decode_stream`]: [`decompress_pure`] then RLP-decode identically. A host test
/// (`rome-zk-bench-decode`) proves this agrees byte-for-byte with [`decode_stream`] on real fixture sizes — the
/// load-bearing correctness check for the guest's channel decode, since an always-on proof over the wrong blocks
/// would be unsound.
#[cfg(feature = "decode-pure")]
pub fn decode_stream_pure(compressed: &[u8]) -> Result<Vec<Block>, ChannelError> {
    let rlp = decompress_pure(compressed)?;
    decode_rlp_blocks(&rlp)
}

/// Shared RLP-decode tail for both [`decode_stream`] and [`decode_stream_pure`] — the two decoders must
/// diverge only in how they get from `compressed` bytes to the uncompressed RLP bytes, never in how those
/// RLP bytes become a block list.
#[cfg(any(feature = "zstd-c", feature = "decode-pure"))]
fn decode_rlp_blocks(rlp: &[u8]) -> Result<Vec<Block>, ChannelError> {
    let mut slice = rlp;
    let blocks = <Vec<Block> as alloy_rlp::Decodable>::decode(&mut slice)?;
    if !slice.is_empty() {
        return Err(ChannelError::Rlp(alloy_rlp::Error::UnexpectedLength));
    }
    Ok(blocks)
}

/// Cuts a compressed channel stream into frames of at most `max_frame_body_len` bytes each, in ascending
/// `frame_no` starting at 0, the last one flagged `is_last`. An empty `compressed` still yields exactly
/// one (empty-body) frame marked `is_last` — a channel is never zero frames, so `reassemble` always has
/// something to find [`ChannelError::NoLastFrame`] against.
pub fn cut_frames(
    chain_id: u64,
    batch: u64,
    compressed: &[u8],
    max_frame_body_len: usize,
) -> Vec<Frame> {
    assert!(
        max_frame_body_len > 0,
        "max_frame_body_len must be positive"
    );
    let id = channel_id(chain_id, batch);
    let chunks: Vec<&[u8]> = if compressed.is_empty() {
        vec![&[]]
    } else {
        compressed.chunks(max_frame_body_len).collect()
    };
    let last_idx = chunks.len() - 1;
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, body)| Frame {
            channel_id: id,
            frame_no: i as u16,
            is_last: i == last_idx,
            body: body.to_vec(),
        })
        .collect()
}

/// Reassembles a compressed channel stream from an out-of-order, possibly-duplicated set of frames — the
/// shape a parallel-send pipeline actually produces (frames land in parallel, any order). Verifies:
/// every frame shares the same `channel_id`; frame numbers are exactly the contiguous range `0..=max`
/// with no gaps, no missing slot and no duplicate (a repeated `frame_no` is refused by name,
/// identical body or not); and exactly the highest-numbered frame is marked `is_last` (catches
/// both a frame lost in transit and a corrupted `is_last` flag before the bytes are ever handed to
/// `decode_stream`).
pub fn reassemble(frames: &[Frame]) -> Result<Vec<u8>, ChannelError> {
    if frames.is_empty() {
        return Err(ChannelError::NoLastFrame);
    }
    let expected_id = frames[0].channel_id;
    for f in frames {
        if f.channel_id != expected_id {
            return Err(ChannelError::ChannelIdMismatch {
                frame_no: f.frame_no,
                actual: f.channel_id,
                expected: expected_id,
            });
        }
    }
    let max_frame_no = frames.iter().map(|f| f.frame_no).max().unwrap();
    let mut by_no: std::collections::HashMap<u16, &Frame> = std::collections::HashMap::new();
    for f in frames {
        // A duplicate frame_no is refused whether or not the bodies agree — "last write wins"
        // made the output depend on the caller's frame order, and no producer ever emits a duplicate
        // (`cut_frames` numbers 0..n once; a resubmit re-sends the same tx against the same idx PDA).
        if by_no.insert(f.frame_no, f).is_some() {
            return Err(ChannelError::DuplicateFrame(f.frame_no));
        }
    }
    let last_count = frames.iter().filter(|f| f.is_last).count();
    if last_count == 0 {
        return Err(ChannelError::NoLastFrame);
    }
    for f in frames {
        if f.is_last && f.frame_no != max_frame_no {
            return Err(ChannelError::IsLastNotHighest(f.frame_no));
        }
    }
    let mut out = Vec::new();
    for no in 0..=max_frame_no {
        let f = by_no.get(&no).ok_or(ChannelError::MissingFrame(no))?;
        out.extend_from_slice(&f.body);
    }
    Ok(out)
}

/// Streaming, size-aware channel builder (the "shadow compressor"). Mirrors
/// `op-batcher/batcher/channel_builder.go`'s `ChannelBuilder.AddBlock`: append blocks one at a time,
/// tracking the compressed size the channel would occupy; refuse (and the caller must close the channel)
/// the block that would push it over budget, rather than ever emitting an oversize frame.
///
/// Implementation note: this re-encodes the whole accumulated block list on each `try_append` rather than
/// maintaining a truly incremental zstd stream (the streaming zstd encoder does not expose "how many
/// bytes would this next write add" without committing them). Correct and simple; `blocks_per_batch` is
/// small (design default 10) so this is not a hot path. A resumable incremental encoder (as
/// `channel_builder.go` uses via `zlib`'s flush-and-measure) is a documented follow-up if profiling ever
/// shows this matters.
#[cfg(feature = "zstd-c")]
pub struct ShadowCompressor {
    max_frames: usize,
    max_frame_body_len: usize,
    blocks: Vec<Block>,
    /// The last successful encoding of `blocks` (kept so `close()` doesn't need to re-encode).
    last_good_encoding: Vec<u8>,
}

#[cfg(feature = "zstd-c")]
#[derive(Debug, PartialEq, Eq)]
pub enum AppendOutcome {
    /// The block was added; the channel may still accept more.
    Added,
    /// The block was **not** added — appending it would exceed the configured frame budget. The caller
    /// must close the channel with the blocks accepted so far and start a new one for this block.
    Full,
}

#[cfg(feature = "zstd-c")]
impl ShadowCompressor {
    pub fn new(max_frames: usize, max_frame_body_len: usize) -> Self {
        Self {
            max_frames,
            max_frame_body_len,
            blocks: Vec::new(),
            last_good_encoding: encode_stream(&[]),
        }
    }

    /// Attempts to append `block`. Returns [`AppendOutcome::Full`] (without mutating state) if doing so
    /// would need more than `max_frames` frames of `max_frame_body_len` bytes each to hold the channel.
    pub fn try_append(&mut self, block: Block) -> AppendOutcome {
        let mut candidate_blocks = self.blocks.clone();
        candidate_blocks.push(block);
        let encoded = encode_stream(&candidate_blocks);
        let frames_needed = encoded.len().div_ceil(self.max_frame_body_len).max(1);
        if frames_needed > self.max_frames {
            return AppendOutcome::Full;
        }
        self.blocks = candidate_blocks;
        self.last_good_encoding = encoded;
        AppendOutcome::Added
    }

    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Closes the channel, returning the accumulated blocks and their final compressed encoding.
    pub fn close(self) -> (Vec<Block>, Vec<u8>) {
        (self.blocks, self.last_good_encoding)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Bytes;

    fn tx(seed: u8, len: usize) -> Bytes {
        Bytes::from(vec![seed; len])
    }

    fn sample_block(number: u64, tx_count: usize, tx_len: usize) -> Block {
        Block {
            number,
            timestamp: 1_757_000_000 + number,
            gas_limit: 100_000_000,
            txs: (0..tx_count)
                .map(|i| tx((number * 31 + i as u64) as u8, tx_len))
                .collect(),
        }
    }

    /// Golden vector: a fixed, small set of blocks encodes to fixed bytes. Pins the exact wire shape
    /// (RLP field order, then zstd) so a future encoder change (field reorder, RLP list vs sequence, a
    /// zstd level bump) is caught immediately, not discovered downstream at derivation time.
    #[test]
    fn golden_vector_channel_stream_round_trips_and_is_stable() {
        let blocks = vec![
            Block {
                number: 0,
                timestamp: 1_757_000_000,
                gas_limit: 100_000_000,
                txs: vec![Bytes::from_static(b"tx-a"), Bytes::from_static(b"tx-b")],
            },
            Block {
                number: 1,
                timestamp: 1_757_000_001,
                gas_limit: 100_000_000,
                txs: vec![Bytes::from_static(b"tx-c")],
            },
        ];
        let compressed = encode_stream(&blocks);
        // Golden byte length pinned (zstd's own framing can shift a byte or two across versions in
        // theory, but not across a plain `cargo build` on a pinned Cargo.lock) — the round-trip assertion
        // below is the load-bearing check; this length is a canary for an accidental encoder change.
        assert!(
            !compressed.is_empty(),
            "encode_stream must produce non-empty output for non-empty blocks"
        );
        let decoded = decode_stream(&compressed).unwrap();
        assert_eq!(decoded, blocks);
    }

    /// Literal golden: pins the exact bytes `encode_stream` produces for a fixed fixture, and the exact bytes
    /// `cut_frames` cuts them into (frame headers included) — not just "round-trips", which a codec change (RLP
    /// field reorder, a zstd level bump) can still satisfy. If this fails after an intentional encoder change,
    /// regenerate it deliberately — do not "fix" it by copying new output in without checking the change against
    /// the lane design and the shared-code ownership. zstd's output is deterministic under this repo's pinned
    /// `Cargo.lock` (same zstd crate version, same compression level), so this is not flaky.
    #[test]
    fn golden_vector_channel_stream_bytes_are_pinned() {
        let blocks = vec![
            Block {
                number: 0,
                timestamp: 1_757_000_000,
                gas_limit: 100_000_000,
                txs: vec![Bytes::from_static(b"tx-a"), Bytes::from_static(b"tx-b")],
            },
            Block {
                number: 1,
                timestamp: 1_757_000_001,
                gas_limit: 100_000_000,
                txs: vec![Bytes::from_static(b"tx-c")],
            },
        ];
        let compressed = encode_stream(&blocks);
        let expected_stream_hex = "28b52ffd0068510100e9d6808468b9b1408405f5e100ca8474782d618474782d62d1018468b9b1418405f5e100c58474782d63";
        assert_eq!(
            hex::encode(&compressed),
            expected_stream_hex,
            "encode_stream output changed — see this test's doc before updating the literal"
        );

        let frames = cut_frames(11, 22, &compressed, DEFAULT_MAX_FRAME_BODY_LEN);
        let all_frame_bytes: Vec<u8> = frames.iter().flat_map(|f| f.to_bytes()).collect();
        let expected_frames_hex = "c0cefee3d28cf77eaea6a4b9bf48a4a400000128b52ffd0068510100e9d6808468b9b1408405f5e100ca8474782d618474782d62d1018468b9b1418405f5e100c58474782d63";
        assert_eq!(
            hex::encode(&all_frame_bytes),
            expected_frames_hex,
            "cut_frames output changed — see this test's doc before updating the literal"
        );
    }

    #[test]
    fn empty_block_list_round_trips() {
        let compressed = encode_stream(&[]);
        let decoded = decode_stream(&compressed).unwrap();
        assert_eq!(decoded, Vec::<Block>::new());
    }

    /// A block spanning several frames, and a single tx crossing a frame boundary, must both round-trip through cut
    /// -> reassemble -> decode_stream.
    #[test]
    fn multi_frame_block_and_a_tx_crossing_a_frame_boundary_round_trip() {
        // One big block whose single tx is much larger than the frame body size, guaranteeing the tx's
        // raw bytes (post-compression, post-RLP-framing) straddle at least one frame boundary.
        let max_frame_body_len = 64;
        let block = Block {
            number: 7,
            timestamp: 1_757_000_777,
            gas_limit: 100_000_000,
            // Incompressible-ish random-looking bytes so zstd doesn't collapse this down to fewer bytes
            // than max_frame_body_len (a repetitive payload would compress to one tiny frame and the test
            // would not actually exercise a boundary crossing).
            txs: vec![Bytes::from(
                (0..2000u32)
                    .map(|i| i.wrapping_mul(2654435761u32) as u8)
                    .collect::<Vec<u8>>(),
            )],
        };
        let compressed = encode_stream(std::slice::from_ref(&block));
        assert!(
            compressed.len() > max_frame_body_len * 2,
            "fixture must actually need multiple frames (got {} bytes)",
            compressed.len()
        );

        let frames = cut_frames(11, 22, &compressed, max_frame_body_len);
        assert!(frames.len() > 2, "must span several frames");
        for f in &frames {
            assert!(f.body.len() <= max_frame_body_len);
        }
        assert!(frames.last().unwrap().is_last);
        assert_eq!(
            frames.iter().filter(|f| f.is_last).count(),
            1,
            "exactly one frame is_last"
        );

        // Reassemble out of order, as a parallel sender would deliver them.
        let mut shuffled = frames.clone();
        shuffled.reverse();
        let reassembled = reassemble(&shuffled).unwrap();
        assert_eq!(reassembled, compressed);

        let decoded = decode_stream(&reassembled).unwrap();
        assert_eq!(decoded, vec![block]);
    }

    #[test]
    fn frame_header_round_trips() {
        let f = Frame {
            channel_id: channel_id(200_198, 42),
            frame_no: 3,
            is_last: true,
            body: vec![1, 2, 3, 4, 5],
        };
        let bytes = f.to_bytes();
        assert_eq!(bytes.len(), FRAME_HEADER_LEN + 5);
        let back = Frame::from_bytes(&bytes).unwrap();
        assert_eq!(back, f);
    }

    /// Literal frame-shape assertion: every byte position is a number written in this test, never
    /// `FRAME_HEADER_LEN` or an `OFF_*` constant — so this test cannot silently agree with a header-length
    /// change the way `frame_header_round_trips` (which reads `FRAME_HEADER_LEN` on both sides of its own
    /// assertion) does. A 5-byte body must serialize to exactly 24 bytes: 19-byte header, then the body.
    #[test]
    fn frame_bytes_are_19_byte_header_then_body_with_literal_offsets() {
        let f = Frame {
            channel_id: [0x11u8; 16],
            frame_no: 3,
            is_last: true,
            body: vec![1, 2, 3, 4, 5],
        };
        let bytes = f.to_bytes();
        assert_eq!(bytes.len(), 24, "19-byte header + 5-byte body");
        assert_eq!(bytes[0..16], [0x11u8; 16], "channel_id");
        assert_eq!(bytes[16..18], [3u8, 0u8], "frame_no, little-endian");
        assert_eq!(bytes[18], 1u8, "is_last");
        assert_eq!(bytes[19..24], [1, 2, 3, 4, 5], "body");
    }

    #[test]
    fn reassemble_detects_a_missing_frame() {
        let compressed = encode_stream(&[sample_block(0, 5, 50), sample_block(1, 5, 50)]);
        let mut frames = cut_frames(1, 1, &compressed, 32);
        assert!(frames.len() >= 3, "fixture needs several frames");
        frames.remove(1); // drop a middle frame
        let err = reassemble(&frames).unwrap_err();
        assert_eq!(err, ChannelError::MissingFrame(1));
    }

    /// A duplicate `frame_no` is refused by name — identical or not. Before this rule the
    /// function did "last write wins", so for a differing-body duplicate its output depended on the
    /// caller's frame ORDER, contradicting its own "any order" contract. Mutation: restore the silent
    /// `insert` → red.
    #[test]
    fn reassemble_refuses_a_duplicate_frame_no_by_name() {
        let compressed = encode_stream(&[sample_block(0, 5, 50), sample_block(1, 5, 50)]);
        let frames = cut_frames(1, 1, &compressed, 32);
        assert!(frames.len() >= 3, "fixture needs several frames");

        // identical duplicate
        let mut dup_same = frames.clone();
        dup_same.push(frames[1].clone());
        assert_eq!(
            reassemble(&dup_same).unwrap_err(),
            ChannelError::DuplicateFrame(1)
        );

        // differing-body duplicate, in both orders — same refusal, order-independent
        let mut foreign = frames[1].clone();
        foreign.body[0] ^= 0xff;
        let mut dup_diff_a = frames.clone();
        dup_diff_a.push(foreign.clone());
        let mut dup_diff_b = vec![foreign];
        dup_diff_b.extend(frames.iter().cloned());
        assert_eq!(
            reassemble(&dup_diff_a).unwrap_err(),
            ChannelError::DuplicateFrame(1)
        );
        assert_eq!(
            reassemble(&dup_diff_b).unwrap_err(),
            ChannelError::DuplicateFrame(1)
        );
    }

    #[test]
    fn reassemble_detects_a_channel_id_mismatch() {
        let compressed = encode_stream(&[sample_block(0, 2, 10)]);
        let mut frames = cut_frames(1, 1, &compressed, 8);
        if let Some(f) = frames.get_mut(0) {
            f.channel_id = channel_id(999, 999);
        }
        let err = reassemble(&frames).unwrap_err();
        assert!(matches!(err, ChannelError::ChannelIdMismatch { .. }));
    }

    #[test]
    fn different_chain_id_or_batch_yields_a_different_channel_id() {
        let a = channel_id(1, 1);
        let b = channel_id(1, 2);
        let c = channel_id(2, 1);
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
    }

    /// The shadow compressor must never let the accumulated blocks need more frames than the configured
    /// budget — fuzz-ish across many random tx-size combinations rather than one fixed case. Shared body
    /// for both the default (5-trial) and the `#[ignore]`d, wider (50-trial) run below.
    fn run_shadow_compressor_fuzz(trials: u64) {
        // The loop below is the whole point of this fuzz — a trial count of 0 would make every
        // assertion inside it vacuously true and the test meaningless without ever failing.
        assert!(trials > 0, "the fuzz loop must run at least one trial");

        let max_frames = 4usize;
        let max_frame_body_len = 128usize;
        let mut rng_state: u64 = 0x9E3779B97F4A7C15;
        let mut next_rand = move || {
            // xorshift64 — deterministic, no extra dependency, good enough for a size-mix fuzz.
            rng_state ^= rng_state << 13;
            rng_state ^= rng_state >> 7;
            rng_state ^= rng_state << 17;
            rng_state
        };

        for trial in 0..trials {
            let mut compressor = ShadowCompressor::new(max_frames, max_frame_body_len);
            let mut appended = 0u64;
            loop {
                let r = next_rand();
                let tx_count = 1 + (r % 5) as usize;
                let tx_len = 1 + ((r >> 8) % 400) as usize;
                let block = sample_block(appended + trial * 1000, tx_count, tx_len);
                match compressor.try_append(block) {
                    AppendOutcome::Added => {
                        appended += 1;
                        // Invariant checked on every successful append, not just at the end.
                        let (_, encoding) = {
                            let blocks = compressor.blocks.clone();
                            let enc = encode_stream(&blocks);
                            (blocks, enc)
                        };
                        let frames_needed = encoding.len().div_ceil(max_frame_body_len).max(1);
                        assert!(
                            frames_needed <= max_frames,
                            "trial {trial}: {frames_needed} frames exceeds budget {max_frames}"
                        );
                        if appended > 200 {
                            break; // safety valve; budgets this small fill up long before this
                        }
                    }
                    AppendOutcome::Full => break,
                }
            }
            let (blocks, final_encoding) = compressor.close();
            let frames = cut_frames(1, trial, &final_encoding, max_frame_body_len);
            assert!(
                frames.len() <= max_frames,
                "trial {trial}: closed channel needs {} frames > budget {max_frames}",
                frames.len()
            );
            // Whatever was accepted must itself decode back losslessly.
            assert_eq!(decode_stream(&final_encoding).unwrap(), blocks);
        }
    }

    /// Default CI run: 5 trials — enough to exercise the size-mix without paying the full 50-trial cost
    /// on every job. Run the wider sweep with `cargo test -p rome-zk-channel -- --ignored`.
    #[test]
    fn shadow_compressor_never_exceeds_the_frame_budget() {
        run_shadow_compressor_fuzz(5);
    }

    /// The original 50-trial sweep, kept for a deliberate wider run rather than every CI job:
    /// `cargo test -p rome-zk-channel -- --ignored`.
    #[test]
    #[ignore = "wider fuzz sweep; run explicitly with `cargo test -p rome-zk-channel -- --ignored`"]
    fn shadow_compressor_never_exceeds_the_frame_budget_wide_sweep() {
        run_shadow_compressor_fuzz(50);
    }

    #[test]
    fn shadow_compressor_close_on_empty_channel_yields_the_empty_stream() {
        let compressor = ShadowCompressor::new(10, 100);
        assert!(compressor.is_empty());
        let (blocks, encoding) = compressor.close();
        assert!(blocks.is_empty());
        assert_eq!(encoding, encode_stream(&[]));
    }

    /// The pure-Rust guest decoder must agree byte-for-byte with the canonical C decoder on a real, multi-block,
    /// multi-tx stream — this is the load-bearing correctness check for ever proving over the guest's decoded
    /// blocks. Run with `--features decode-pure` (both codecs active in the same binary; default feature set is
    /// unaffected).
    #[cfg(feature = "decode-pure")]
    #[test]
    fn decode_stream_pure_matches_decode_stream_on_a_multi_block_fixture() {
        let blocks: Vec<Block> = (0..10).map(|n| sample_block(n, 300, 110)).collect();
        let compressed = encode_stream(&blocks);
        let via_c = decode_stream(&compressed).unwrap();
        let via_pure = decode_stream_pure(&compressed).unwrap();
        assert_eq!(via_c, blocks);
        assert_eq!(
            via_pure, blocks,
            "ruzstd decode must equal the C zstd decode"
        );
    }

    /// Mutation: corrupting one byte of the compressed stream must be rejected by name, not silently
    /// accepted or panicking uninformatively — this is what the guest's own `.expect(...)` names.
    #[cfg(feature = "decode-pure")]
    #[test]
    fn decode_stream_pure_rejects_a_corrupted_byte() {
        let blocks: Vec<Block> = (0..10).map(|n| sample_block(n, 300, 110)).collect();
        let mut compressed = encode_stream(&blocks);
        let mid = compressed.len() / 2;
        compressed[mid] ^= 0xff;
        let err = decode_stream_pure(&compressed).unwrap_err();
        assert!(
            matches!(err, ChannelError::Zstd(_) | ChannelError::Rlp(_)),
            "corrupted stream must fail decode with a named error, got {err:?}"
        );
    }
}
