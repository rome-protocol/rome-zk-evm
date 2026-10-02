//! The poster hand-off seam. Once a batch is finalized on chain and its `acc` verified against the
//! client-side reference, `pipeline.rs` hands the result to a `PostRootSink` — the poster is the real
//! consumer; only a channel-backed implementation exists so far (no poster exists yet to wire it into).

use alloy_primitives::B256;

/// Everything the poster needs to build and submit `PostRoot` against `zk-settlement`, without
/// re-reading the inbox account itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizedBatch {
    pub chain_id: u64,
    pub batch: u64,
    /// The batch account's verified `acc` commitment.
    pub acc: [u8; 32],
    pub first_block: u64,
    pub last_block: u64,
    /// State roots for every block in the batch, in order — from `source.rs`'s
    /// `SourcedBlock::sub_block_header_hashes` derived per-block state, or (for now) the batch's own
    /// content only; a real state root comes from the executor, which is out of scope here — see
    /// `pipeline.rs`'s doc for exactly what is filled in.
    pub state_roots: Vec<B256>,
}

/// Hand-off point for a finalized batch. Implementations must never block the pipeline for long — a slow
/// or unavailable poster should not stall batch production (mirrors
/// `rome_zk_sequencer::preconf::SubBlockSink`'s "never block or fail the seal" rule at the sequencer
/// layer).
pub trait PostRootSink: Send + Sync {
    fn publish(&self, batch: FinalizedBatch);
}

/// A `tokio::sync::mpsc`-backed sink: the poster (when it exists) takes the receiver end.
pub struct ChannelPostRootSink {
    tx: tokio::sync::mpsc::UnboundedSender<FinalizedBatch>,
}

impl ChannelPostRootSink {
    pub fn new() -> (Self, tokio::sync::mpsc::UnboundedReceiver<FinalizedBatch>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Self { tx }, rx)
    }
}

impl PostRootSink for ChannelPostRootSink {
    fn publish(&self, batch: FinalizedBatch) {
        // No receiver (poster not running yet) is not an error — mirrors `ChannelSink::publish`.
        let _ = self.tx.send(batch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_batch_is_received() {
        let (sink, mut rx) = ChannelPostRootSink::new();
        let batch = FinalizedBatch {
            chain_id: 1,
            batch: 2,
            acc: [7u8; 32],
            first_block: 100,
            last_block: 109,
            state_roots: vec![B256::ZERO],
        };
        sink.publish(batch.clone());
        let got = rx.try_recv().unwrap();
        assert_eq!(got, batch);
    }

    #[test]
    fn publish_with_no_receiver_does_not_panic() {
        let (sink, rx) = ChannelPostRootSink::new();
        drop(rx);
        sink.publish(FinalizedBatch {
            chain_id: 1,
            batch: 1,
            acc: [0u8; 32],
            first_block: 0,
            last_block: 9,
            state_roots: vec![],
        });
    }
}
