//! `ChannelBank` ("bounded, oldest-first prune"): buffers frames per `channel_id` until a
//! channel is complete, then hands its reassembled compressed bytes to [`crate::batch_queue`]. Mirrors
//! kona's own channel bank (`kona-derive`'s `ChannelProvider`/`ChannelBank` stage): several channels can
//! be open at once (a parallel-send batcher lands frames for a batch in any order, but a slow
//! Solana confirmation can also interleave two different batches' chunks in wall-clock time), bounded so
//! a channel that will never complete cannot hold memory forever.

use std::collections::{HashMap, VecDeque};

use rome_zk_channel as channel;
use rome_zk_channel::Frame;

use crate::PipelineError;

pub struct ChannelBank {
    max_channels: usize,
    /// Insertion order, oldest first — the eviction order.
    order: VecDeque<[u8; 16]>,
    frames: HashMap<[u8; 16], Vec<Frame>>,
}

impl ChannelBank {
    pub fn new(max_channels: usize) -> Self {
        assert!(max_channels > 0, "max_channels must be positive");
        Self {
            max_channels,
            order: VecDeque::new(),
            frames: HashMap::new(),
        }
    }

    /// Ingests one frame, opening its channel if this is the first frame seen for it. Evicts the
    /// single oldest channel first if this would open a `max_channels + 1`-th channel — never evicts to
    /// make room for a frame of a channel already open.
    pub fn ingest(&mut self, frame: Frame) {
        if !self.frames.contains_key(&frame.channel_id) {
            if self.order.len() >= self.max_channels {
                if let Some(oldest) = self.order.pop_front() {
                    self.frames.remove(&oldest);
                }
            }
            self.order.push_back(frame.channel_id);
            self.frames.insert(frame.channel_id, Vec::new());
        }
        // `frames.entry` was already established above (fresh or pre-existing) — the channel may have
        // been the one just evicted above only if `max_channels == 0`, which the constructor forbids.
        let buffered = self.frames.entry(frame.channel_id).or_default();
        // Ingest is idempotent for an identical re-delivery (the traversal re-reads a chunk
        // account across polls); a same-numbered frame with DIFFERENT bytes is kept so `reassemble`
        // refuses the channel by name (`DuplicateFrame`) at `take_complete` — never silently dropped.
        if buffered.iter().any(|f| f == &frame) {
            return;
        }
        buffered.push(frame);
    }

    /// The exact completeness condition `channel::reassemble` itself checks (contiguous `0..=max`, and
    /// the highest frame_no carries `is_last`) — checked here first so an incomplete channel reads as
    /// "not ready yet" (the caller keeps polling) rather than surfacing `reassemble`'s `MissingFrame`
    /// error, which is not what an incomplete-so-far channel means.
    pub fn is_complete(&self, channel_id: [u8; 16]) -> bool {
        let Some(frames) = self.frames.get(&channel_id) else {
            return false;
        };
        if frames.is_empty() {
            return false;
        }
        let max_no = frames.iter().map(|f| f.frame_no).max().unwrap();
        frames.iter().any(|f| f.is_last && f.frame_no == max_no)
            && (0..=max_no).all(|no| frames.iter().any(|f| f.frame_no == no))
    }

    /// Reassembles and removes a complete channel. `Ok(None)` if it does not exist or is not complete
    /// yet (a Temporary condition upstream, not this stage's concern); `Err(Critical)` only if
    /// `is_complete` said yes but `channel::reassemble` itself still rejects it (a channel-id mismatch
    /// among the buffered frames — `is_complete` does not check that, `reassemble` does).
    pub fn take_complete(
        &mut self,
        channel_id: [u8; 16],
    ) -> Result<Option<Vec<u8>>, PipelineError> {
        if !self.is_complete(channel_id) {
            return Ok(None);
        }
        let frames = self
            .frames
            .remove(&channel_id)
            .expect("just checked present");
        self.order.retain(|id| *id != channel_id);
        let bytes = channel::reassemble(&frames).map_err(|e| {
            PipelineError::Critical(format!("channel {channel_id:02x?}: reassemble: {e}"))
        })?;
        Ok(Some(bytes))
    }

    pub fn open_channel_count(&self) -> usize {
        self.order.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Bytes;

    fn block(number: u64, tx_len: u32) -> channel::Block {
        channel::Block {
            number,
            timestamp: 1_757_000_000 + number,
            gas_limit: 100_000_000,
            // Incompressible-ish bytes (channel.rs's own fixtures use the same trick) so zstd cannot
            // collapse this down to fewer bytes than the small `max_frame_body_len` these tests use —
            // a repetitive payload would fit in one frame and never exercise multi-frame reassembly.
            txs: vec![Bytes::from(
                (0..tx_len)
                    .map(|i| i.wrapping_mul(2_654_435_761) as u8)
                    .collect::<Vec<u8>>(),
            )],
            deposits_end: None,
        }
    }

    #[test]
    fn a_single_frame_channel_is_complete_immediately_and_reassembles() {
        let compressed = channel::encode_stream(&[block(0, 10)]);
        let frames = channel::cut_frames(1, 1, &compressed, 4096);
        assert_eq!(frames.len(), 1);
        let mut bank = ChannelBank::new(4);
        bank.ingest(frames[0].clone());
        assert!(bank.is_complete(frames[0].channel_id));
        let out = bank.take_complete(frames[0].channel_id).unwrap().unwrap();
        assert_eq!(out, compressed);
        assert_eq!(bank.open_channel_count(), 0, "completed channel is removed");
    }

    #[test]
    fn a_multi_frame_channel_is_incomplete_until_every_frame_arrives_in_any_order() {
        let compressed = channel::encode_stream(&[block(0, 2000)]);
        let frames = channel::cut_frames(1, 1, &compressed, 64);
        assert!(frames.len() > 2, "fixture must need several frames");
        let mut bank = ChannelBank::new(4);
        let channel_id = frames[0].channel_id;

        // Ingest every frame but the last, out of order.
        for f in frames[..frames.len() - 1].iter().rev() {
            bank.ingest(f.clone());
            assert!(!bank.is_complete(channel_id), "must not be complete yet");
        }
        // Now ingest the missing (is_last) frame.
        bank.ingest(frames.last().unwrap().clone());
        assert!(bank.is_complete(channel_id));
        let out = bank.take_complete(channel_id).unwrap().unwrap();
        assert_eq!(out, compressed);
    }

    /// An identical re-delivered frame (the traversal re-reads a chunk across polls) is
    /// ingested once — `reassemble` itself refuses any duplicate, so idempotence lives here.
    #[test]
    fn an_identical_redelivered_frame_is_ingested_once() {
        let compressed = channel::encode_stream(&[block(0, 10)]);
        let frames = channel::cut_frames(1, 1, &compressed, 4096);
        let mut bank = ChannelBank::new(4);
        bank.ingest(frames[0].clone());
        bank.ingest(frames[0].clone()); // identical re-delivery
        assert_eq!(bank.frames[&frames[0].channel_id].len(), 1);
        assert!(bank.is_complete(frames[0].channel_id));
        let out = bank.take_complete(frames[0].channel_id).unwrap().unwrap();
        assert_eq!(out, compressed);
    }

    /// A same-numbered frame with different bytes is NOT deduplicated — the channel is
    /// refused by name when taken (`DuplicateFrame`), never resolved by "last write wins".
    #[test]
    fn a_differing_duplicate_frame_is_refused_by_name_when_taken() {
        let compressed = channel::encode_stream(&[block(0, 10)]);
        let frames = channel::cut_frames(1, 1, &compressed, 4096);
        let mut bank = ChannelBank::new(4);
        bank.ingest(frames[0].clone());
        let mut foreign = frames[0].clone();
        foreign.body[0] ^= 0xff;
        bank.ingest(foreign);
        assert!(bank.is_complete(frames[0].channel_id));
        let err = bank.take_complete(frames[0].channel_id).unwrap_err();
        assert!(err.to_string().contains("appears more than once"), "{err}");
    }

    #[test]
    fn take_complete_on_an_unknown_or_incomplete_channel_is_none_not_an_error() {
        let mut bank = ChannelBank::new(4);
        assert_eq!(bank.take_complete([9u8; 16]).unwrap(), None);

        let compressed = channel::encode_stream(&[block(0, 2000)]);
        let frames = channel::cut_frames(1, 1, &compressed, 64);
        assert!(frames.len() > 1);
        bank.ingest(frames[0].clone());
        assert_eq!(bank.take_complete(frames[0].channel_id).unwrap(), None);
    }

    /// "Bounded, oldest-first prune" — a channel opened before `max_channels` other,
    /// still-incomplete channels is dropped (never completes) once a new one arrives past the bound.
    #[test]
    fn the_oldest_incomplete_channel_is_evicted_once_the_bank_is_full() {
        let mut bank = ChannelBank::new(2);
        let mk = |batch: u64| {
            let compressed = channel::encode_stream(&[block(batch, 2000)]);
            channel::cut_frames(1, batch, &compressed, 64)
        };
        let a = mk(1);
        let b = mk(2);
        let c = mk(3);
        // Open channel a (incomplete: only its first frame), then b (incomplete), filling the bank of
        // size 2.
        bank.ingest(a[0].clone());
        bank.ingest(b[0].clone());
        assert_eq!(bank.open_channel_count(), 2);
        // Opening channel c evicts the oldest (a) — a can now never complete even if the rest of its
        // frames arrive.
        bank.ingest(c[0].clone());
        assert_eq!(bank.open_channel_count(), 2);
        for f in &a[1..] {
            bank.ingest(f.clone());
        }
        assert!(
            !bank.is_complete(a[0].channel_id),
            "channel a was evicted and must never become complete again"
        );
    }
}
