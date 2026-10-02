//! `FrameQueue`: parses each chunk's raw bytes into a [`rome_zk_channel::Frame`]
//! via the batcher's own decoder (`Frame::from_bytes`) — never a reimplementation, so this is
//! byte-for-byte the same parse the batcher's own round-trip tests exercise (the decoder
//! must match it byte for byte).
//!
//! A decoded frame's own `channel_id`/`frame_no` fields were never checked
//! against where this crate actually read the chunk from — a chunk holding a frame stamped for a
//! *different* channel (foreign `channel_id`) or out of position (`frame_no != idx`) parsed cleanly and
//! was silently ingested into [`crate::channel_bank::ChannelBank`] as if it belonged. Both are checked
//! here, at parse time, before any frame reaches the bank — mismatch is [`PipelineError::Critical`].

use rome_zk_channel as channel;
use rome_zk_channel::Frame;

use crate::PipelineError;

/// Parses every chunk body into a [`Frame`], in the same order they were read (strict
/// validity: a chunk whose bytes do not even parse as a frame is a decode failure, hence
/// [`PipelineError::Critical`] — never silently dropped), and checks each frame's `channel_id` matches
/// this exact `(chain_id, batch)`'s expected channel and its `frame_no` matches the chunk index it was
/// read from (idx `i` in `chunk_bodies` — [`crate::inbox::InboxRetrieval::chunks`] always reads chunks
/// `0..expected_count` in order, so position IS the chunk's own idx).
pub fn parse_frames(
    chunk_bodies: Vec<Vec<u8>>,
    chain_id: u64,
    batch: u64,
) -> Result<Vec<Frame>, PipelineError> {
    let expected_channel_id = channel::channel_id(chain_id, batch);
    chunk_bodies
        .into_iter()
        .enumerate()
        .map(|(idx, bytes)| {
            let frame = Frame::from_bytes(&bytes)
                .map_err(|e| PipelineError::Critical(format!("chunk {idx}: frame decode: {e}")))?;
            if frame.channel_id != expected_channel_id {
                return Err(PipelineError::Critical(format!(
                    "chunk {idx}: frame channel_id {:02x?} != expected {:02x?} for (chain_id={chain_id}, batch={batch})",
                    frame.channel_id, expected_channel_id
                )));
            }
            if frame.frame_no as usize != idx {
                return Err(PipelineError::Critical(format!(
                    "chunk {idx}: frame frame_no {} != chunk idx {idx}",
                    frame.frame_no
                )));
            }
            Ok(frame)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rome_zk_channel::Block;

    const CHAIN_ID: u64 = 200_101;
    const BATCH: u64 = 0;

    #[test]
    fn parses_frames_cut_by_the_batcher_byte_for_byte() {
        let blocks = vec![Block {
            number: 1,
            timestamp: 1_757_000_000,
            gas_limit: 100_000_000,
            txs: vec![alloy_primitives::Bytes::from_static(b"tx")],
        }];
        let compressed = channel::encode_stream(&blocks);
        let frames = channel::cut_frames(CHAIN_ID, BATCH, &compressed, 3_681);
        let chunk_bodies: Vec<Vec<u8>> = frames.iter().map(Frame::to_bytes).collect();

        let parsed = parse_frames(chunk_bodies, CHAIN_ID, BATCH).unwrap();
        assert_eq!(parsed, frames);
    }

    /// A frame stamped for a different channel than this `(chain_id, batch)`
    /// expects must be rejected, not silently ingested. `parse_frames` used to ignore `channel_id`
    /// entirely; the test requires `Critical`.
    #[test]
    fn a_frame_with_a_foreign_channel_id_is_critical() {
        let blocks = vec![Block {
            number: 1,
            timestamp: 1_757_000_000,
            gas_limit: 100_000_000,
            txs: vec![alloy_primitives::Bytes::from_static(b"tx")],
        }];
        let compressed = channel::encode_stream(&blocks);
        // Cut for a DIFFERENT batch id than we validate against below — same chain_id, wrong channel.
        let frames = channel::cut_frames(CHAIN_ID, BATCH + 1, &compressed, 3_681);
        let chunk_bodies: Vec<Vec<u8>> = frames.iter().map(Frame::to_bytes).collect();

        let err = parse_frames(chunk_bodies, CHAIN_ID, BATCH).unwrap_err();
        assert!(matches!(err, PipelineError::Critical(_)));
    }

    /// A frame's own `frame_no` must match the chunk position (idx) it was
    /// actually read from — a chunk holding another chunk's frame body (e.g. two chunks swapped) is
    /// rejected rather than silently reordered.
    #[test]
    fn a_frame_whose_frame_no_does_not_match_its_chunk_idx_is_critical() {
        let blocks = vec![Block {
            number: 1,
            timestamp: 1_757_000_000,
            gas_limit: 100_000_000,
            txs: vec![alloy_primitives::Bytes::from(
                (0..2000u32).map(|i| i as u8).collect::<Vec<u8>>(),
            )],
        }];
        let compressed = channel::encode_stream(&blocks);
        let frames = channel::cut_frames(CHAIN_ID, BATCH, &compressed, 64);
        assert!(frames.len() > 1, "fixture must need multiple frames");
        // Swap the first two chunk bodies — each still parses as a valid Frame, but frame_no != idx.
        let mut chunk_bodies: Vec<Vec<u8>> = frames.iter().map(Frame::to_bytes).collect();
        chunk_bodies.swap(0, 1);

        let err = parse_frames(chunk_bodies, CHAIN_ID, BATCH).unwrap_err();
        assert!(matches!(err, PipelineError::Critical(_)));
    }

    #[test]
    fn a_chunk_too_short_for_the_frame_header_is_critical() {
        let err = parse_frames(vec![vec![0u8; 5]], CHAIN_ID, BATCH).unwrap_err();
        assert!(matches!(err, PipelineError::Critical(_)));
    }
}
