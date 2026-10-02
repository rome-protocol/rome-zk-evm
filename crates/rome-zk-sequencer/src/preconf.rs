//! `SubBlockSink` — the batcher hand-off seam — and the preconfirmation feed event shape.
//!
//! The sequencer's job ends at "signed, fsynced sub-block"; the batcher consumes that stream to build
//! the DA channel. `SubBlockSink` is the seam between them: the only implementation here is an
//! in-memory broadcast channel (also what feeds `rome_subscribe`); the batcher gets its own subscription
//! without this crate knowing anything about chunks, frames, or Solana.
//!
//! **This channel is a best-effort feed, never the source of truth.** A
//! `broadcast` subscriber that falls behind the channel's capacity silently misses sub-blocks (standard
//! `broadcast` lagged behavior — see [`SubBlockSink`]'s doc), and nothing here retries or backpressures
//! the sealer to protect a slow subscriber. The batcher must build its ordered tx stream by
//! reading the **ordered log** (`crate::log`) — the fsynced, gap-free, replayable record every
//! pre-confirmation is actually backed by — and may use this channel only as a low-latency hint to know
//! when new log records are ready to read, never as the data itself.

use alloy::primitives::{Signature, TxHash};
use tokio::sync::broadcast;

use crate::header::SubBlockHeader;

/// One sealed sub-block, as handed to every sink subscriber (the WS preconf feed and, later, the
/// batcher). Carries everything a subscriber needs without re-reading the log.
#[derive(Debug, Clone)]
pub struct SealedSubBlock {
    pub header: SubBlockHeader,
    pub header_hash: alloy::primitives::B256,
    pub signature: Signature,
    /// Tx hashes in inclusion order — position in this vec is each tx's `position` in the
    /// pre-confirmation.
    pub included: Vec<TxHash>,
}

/// Hand-off point for sealed sub-blocks. A `SubBlockSink` implementation must never block or fail the
/// seal — dropping a slow subscriber is always preferable to stalling the sealer (see the crate doc:
/// "never answer a user before the fsync", not "never seal because a subscriber is slow").
pub trait SubBlockSink: Send + Sync {
    fn publish(&self, sub_block: SealedSubBlock);
}

/// A `tokio::sync::broadcast`-backed sink: every subscriber gets every sealed sub-block; a subscriber
/// that falls behind the channel's capacity silently misses old ones (broadcast's standard lagged
/// behavior) rather than backpressuring the sealer. Cloning shares the same underlying channel (cloning
/// a `broadcast::Sender` is how the RPC layer gets its own subscribe handle onto the sealer's sink).
#[derive(Clone)]
pub struct ChannelSink {
    tx: broadcast::Sender<SealedSubBlock>,
}

impl ChannelSink {
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SealedSubBlock> {
        self.tx.subscribe()
    }
}

impl SubBlockSink for ChannelSink {
    fn publish(&self, sub_block: SealedSubBlock) {
        // No subscribers is not an error — the feed and the batcher are both optional consumers.
        let _ = self.tx.send(sub_block);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::B256;

    fn fixture() -> SealedSubBlock {
        let header = SubBlockHeader {
            chain_id: 1,
            block: 1,
            index: 0,
            timestamp_us: 0,
            tx_root: B256::ZERO,
            receipts_root: B256::ZERO,
            gas_used: 0,
            prev_hash: B256::ZERO,
        };
        SealedSubBlock {
            header_hash: header.hash(),
            header,
            signature: Signature::test_signature(),
            included: vec![],
        }
    }

    #[tokio::test]
    async fn subscribers_receive_published_sub_blocks() {
        let sink = ChannelSink::new(16);
        let mut rx1 = sink.subscribe();
        let mut rx2 = sink.subscribe();
        sink.publish(fixture());
        let a = rx1.recv().await.unwrap();
        let b = rx2.recv().await.unwrap();
        assert_eq!(a.header_hash, b.header_hash);
    }
}
