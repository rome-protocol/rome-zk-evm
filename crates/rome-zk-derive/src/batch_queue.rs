//! `BatchQueue`: decompresses a reassembled channel stream back into the batch's ordered `Block` list (via
//! `rome_zk_channel::decode_stream` — the same codec the batcher encodes with, never reimplemented here) and
//! enforces the strict validity gate: "every tx decodes, chain id ok, signature form ok" — a chunk/channel/tx
//! that fails any of these is [`PipelineError::Critical`], never a skip.
//!
//! **Batch ids are DA containers, not block arithmetic.** The previous rule (a batch's blocks must fall in
//! the exact range `batch * BLOCKS_PER_BATCH .. batch * BLOCKS_PER_BATCH + BLOCKS_PER_BATCH`) assumed a
//! 1:1, fixed-size mapping from batch id to block range that the live batcher does not honour (it posts
//! every log block under whatever id the cursor is at, and abandoned ids are skipped —
//! `crate::traversal`). The invariants this module checks now are the ones the design actually
//! guarantees:
//!
//! - a batch holds **at most `blocks_per_batch`** consecutive blocks (a config/profile value, default
//!   10 — the cap, not an exact size);
//! - the blocks **within** one batch are strictly consecutive (`number, number+1, number+2, ...`);
//! - the batch's **first** block continues immediately from the **previous** batch's last block
//!   (`expected_first_block`, `None` only for the very first batch this pipeline ever derives) — this is
//!   a cross-batch check the caller ([`crate::pipeline::DerivePipeline`]) drives by threading its own
//!   `last_design_block + 1` through every call.
//!
//! A batch account carries no block-range field of its own (`zk_inbox_client::BatchAccount`) — the only
//! way to learn a batch's block numbers is to decode it, which is exactly what this function does; there
//! is no cheaper shortcut to check continuity ahead of a full decode.

use alloy_consensus::transaction::SignerRecoverable;
use alloy_consensus::{Transaction, TxEnvelope};
use alloy_eips::eip2718::Decodable2718;
use rome_zk_channel as channel;
use rome_zk_channel::Block;

use crate::PipelineError;

/// The one-sided timestamp drift bound: `block.timestamp <= anchor_unix_ts +
/// max_drift_secs`, where `anchor_unix_ts` is a committed Solana clock reading — the batch account's own
/// `open_unix_ts` (header v2), read by `crate::traversal::SolanaTraversal` into
/// `BatchRef::open_unix_ts` and threaded through `crate::pipeline::DerivePipeline::derive_one_batch`.
/// **One-sided, not a range check:** the bound was first two-sided (`anchor - max_drift <= timestamp <=
/// anchor + max_drift`) and was made one-sided because that would make any honest batch whose DA posting
/// lagged sealing by more than `max_drift` permanently Critical — the attack this bound stops is a sequencer
/// *future-dating* a block, and honest posting latency must never trip it; the lower bound (a block's
/// timestamp advancing monotonically from its parent) is the sequencer's own invariant, not this function's
/// concern. [`DriftBound::unbounded`] is a no-op (every timestamp passes) — the default for a pipeline built
/// without `crate::pipeline::DerivePipeline::with_drift_bound`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriftBound {
    pub max_drift_secs: u64,
}

impl DriftBound {
    pub fn unbounded() -> Self {
        Self {
            max_drift_secs: u64::MAX,
        }
    }
}

/// The timestamp drift bound, checked against every block a batch decodes to,
/// against the SAME `anchor_unix_ts` for all of them (the anchor is a per-batch value,
/// committed once when the batch account opens — not per-block). [`PipelineError::Critical`] on a
/// violation: a sequencer whose declared timestamp runs materially ahead of the anchor is either a clock
/// fault or a deliberately future-dated block, either way a strict-validity failure, not a
/// retry. A late anchor (the batcher posted this batch's DA well after the blocks were sealed) only
/// raises the bound — it can never turn an honest, on-time block Critical.
pub fn enforce_drift_bound(
    blocks: &[Block],
    anchor_unix_ts: u64,
    drift: DriftBound,
) -> Result<(), PipelineError> {
    let bound = anchor_unix_ts.saturating_add(drift.max_drift_secs);
    for block in blocks {
        if block.timestamp > bound {
            return Err(PipelineError::Critical(format!(
                "block {}: timestamp {} exceeds the drift bound anchor_unix_ts({anchor_unix_ts}) + max_drift({}) = {bound}",
                block.number, block.timestamp, drift.max_drift_secs
            )));
        }
    }
    Ok(())
}

/// Decodes the channel stream and validates every tx in every block (the decode gate), plus
/// the block-numbering invariants that replace the old fixed-arithmetic rule (see module
/// doc). `expected_first_block` is `None` only for the very first batch this pipeline instance derives
/// (nothing to continue from yet); every subsequent call passes `Some(last_design_block + 1)`.
/// Documented non-bug (measured): a
/// corrupted ECDSA signature still recovers *some* signer (ECDSA has no MAC) and is therefore not a
/// decode failure here — it is caught later, by [`crate::engine::EngineController`]'s block-vs-frame
/// tx-set check, when it fails at execution (no funds/nonce for the recovered address) instead.
pub fn decode_batch(
    compressed: &[u8],
    chain_id: u64,
    batch: u64,
    blocks_per_batch: u64,
    expected_first_block: Option<u64>,
) -> Result<Vec<Block>, PipelineError> {
    let blocks = channel::decode_stream(compressed)
        .map_err(|e| PipelineError::Critical(format!("batch {batch}: channel decode: {e}")))?;

    if blocks.is_empty() {
        return Err(PipelineError::Critical(format!(
            "batch {batch}: decoded to zero blocks"
        )));
    }
    if blocks.len() as u64 > blocks_per_batch {
        return Err(PipelineError::Critical(format!(
            "batch {batch}: holds {} blocks, exceeds blocks_per_batch cap {blocks_per_batch}",
            blocks.len()
        )));
    }
    if let Some(expected_first) = expected_first_block {
        if blocks[0].number != expected_first {
            return Err(PipelineError::Critical(format!(
                "batch {batch}: first block is {}, expected {expected_first} (continuity: must start \
                 immediately after the previous batch's last block)",
                blocks[0].number
            )));
        }
    }
    for w in blocks.windows(2) {
        if w[1].number != w[0].number + 1 {
            return Err(PipelineError::Critical(format!(
                "batch {batch}: blocks not consecutive: {} followed by {} (strict order within a batch)",
                w[0].number, w[1].number
            )));
        }
    }

    for block in &blocks {
        for (tx_i, raw) in block.txs.iter().enumerate() {
            let env = TxEnvelope::decode_2718(&mut raw.as_ref()).map_err(|e| {
                PipelineError::Critical(format!(
                    "batch {batch} block {}: tx {tx_i}: decode: {e}",
                    block.number
                ))
            })?;
            if env.chain_id() != Some(chain_id) {
                return Err(PipelineError::Critical(format!(
                    "batch {batch} block {}: tx {tx_i}: chain id {:?} != {chain_id}",
                    block.number,
                    env.chain_id()
                )));
            }
            // Signature *form* only ("signature form ok?") — recovers a signer to prove
            // the signature parses as a valid (r, s, v) point, not that it belongs to who it claims.
            env.recover_signer().map_err(|e| {
                PipelineError::Critical(format!(
                    "batch {batch} block {}: tx {tx_i}: signature form: {e}",
                    block.number
                ))
            })?;
        }
    }
    Ok(blocks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_BLOCKS_PER_BATCH;
    use alloy_consensus::{SignableTransaction, TxEip1559};
    use alloy_primitives::{Address, Bytes, TxKind, U256};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;

    const CHAIN_ID: u64 = 200_101;
    const CAP: u64 = DEFAULT_BLOCKS_PER_BATCH;

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

    fn block_at(batch: u64, offset: u64, txs: Vec<Bytes>) -> Block {
        Block {
            number: batch * DEFAULT_BLOCKS_PER_BATCH + offset,
            timestamp: 1_757_000_000 + batch * DEFAULT_BLOCKS_PER_BATCH + offset,
            gas_limit: 100_000_000,
            txs,
            deposits_end: None,
        }
    }

    #[test]
    fn decodes_a_valid_batch_of_real_signed_txs() {
        let signer = PrivateKeySigner::random();
        let blocks = vec![
            block_at(3, 0, vec![signed_tx(&signer, 0)]),
            block_at(3, 1, vec![signed_tx(&signer, 1), signed_tx(&signer, 2)]),
        ];
        let compressed = channel::encode_stream(&blocks);
        let decoded = decode_batch(&compressed, CHAIN_ID, 3, CAP, Some(blocks[0].number)).unwrap();
        assert_eq!(decoded, blocks);
    }

    #[test]
    fn rejects_a_tx_for_the_wrong_chain_id() {
        let signer = PrivateKeySigner::random();
        // Sign for a different chain id than we validate against below.
        let tx = TxEip1559 {
            chain_id: CHAIN_ID + 1,
            nonce: 0,
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
        let raw = Bytes::from(alloy_eips::eip2718::Encodable2718::encoded_2718(
            &TxEnvelope::from(tx.into_signed(signature)),
        ));
        let blocks = vec![block_at(0, 0, vec![raw])];
        let compressed = channel::encode_stream(&blocks);
        let err = decode_batch(&compressed, CHAIN_ID, 0, CAP, None).unwrap_err();
        assert!(matches!(err, PipelineError::Critical(_)));
    }

    #[test]
    fn rejects_a_tx_that_does_not_decode() {
        let blocks = vec![block_at(
            0,
            0,
            vec![Bytes::from_static(&[0xff, 0x00, 0x01])],
        )];
        let compressed = channel::encode_stream(&blocks);
        let err = decode_batch(&compressed, CHAIN_ID, 0, CAP, None).unwrap_err();
        assert!(matches!(err, PipelineError::Critical(_)));
    }

    #[test]
    fn a_flipped_signature_byte_still_decodes_here_not_rejected_until_execution() {
        let signer = PrivateKeySigner::random();
        let mut raw = signed_tx(&signer, 0).to_vec();
        let n = raw.len();
        raw[n - 10] ^= 1; // corrupt a signature byte; still recovers *some* signer
        let blocks = vec![block_at(0, 0, vec![Bytes::from(raw)])];
        let compressed = channel::encode_stream(&blocks);
        assert!(decode_batch(&compressed, CHAIN_ID, 0, CAP, None).is_ok());
    }

    #[test]
    fn a_corrupted_channel_stream_is_critical_not_a_panic() {
        let err = decode_batch(&[0xff, 0x00, 0x01, 0x02], CHAIN_ID, 0, CAP, None).unwrap_err();
        assert!(matches!(err, PipelineError::Critical(_)));
    }

    /// Positive case: a 3-block batch under the `blocks_per_batch` (10) cap, continuing
    /// immediately from the previous batch's last block, passes.
    #[test]
    fn a_three_block_batch_under_the_cap_with_correct_continuity_passes() {
        let blocks = vec![
            block_at(1, 0, vec![]),
            block_at(1, 1, vec![]),
            block_at(1, 2, vec![]),
        ];
        let compressed = channel::encode_stream(&blocks);
        let expected_first = blocks[0].number;
        let decoded = decode_batch(&compressed, CHAIN_ID, 1, CAP, Some(expected_first)).unwrap();
        assert_eq!(decoded, blocks);
    }

    /// Negative case: the next batch must continue at `last_block + 1` — a gap (or a
    /// backward jump) is Critical, never silently accepted or skipped.
    #[test]
    fn a_batch_not_continuing_from_the_expected_first_block_is_critical() {
        let blocks = vec![block_at(2, 0, vec![]), block_at(2, 1, vec![])];
        let compressed = channel::encode_stream(&blocks);
        // Previous batch's last block was `blocks[0].number` (i.e. one less than this batch's actual
        // first block minus one) — force a mismatch by asking for a first block this batch doesn't have.
        let wrong_expected_first = blocks[0].number + 5;
        let err =
            decode_batch(&compressed, CHAIN_ID, 2, CAP, Some(wrong_expected_first)).unwrap_err();
        assert!(matches!(err, PipelineError::Critical(_)));
    }

    /// The very first batch this pipeline ever derives has nothing to continue from —
    /// `expected_first_block: None` must skip the continuity check entirely (only the internal
    /// consecutive-numbering and cap checks apply).
    #[test]
    fn the_first_batch_ever_derived_skips_the_continuity_check() {
        let blocks = vec![block_at(0, 0, vec![])];
        let compressed = channel::encode_stream(&blocks);
        assert!(decode_batch(&compressed, CHAIN_ID, 0, CAP, None).is_ok());
    }

    /// A batch holding more than `blocks_per_batch` consecutive blocks is Critical — the cap
    /// is enforced even though there is no longer a fixed per-batch size.
    #[test]
    fn a_batch_exceeding_the_blocks_per_batch_cap_is_critical() {
        let cap = 3u64;
        let blocks: Vec<Block> = (0..=cap).map(|i| block_at(0, i, vec![])).collect(); // cap+1 blocks
        let compressed = channel::encode_stream(&blocks);
        let err = decode_batch(&compressed, CHAIN_ID, 0, cap, None).unwrap_err();
        assert!(matches!(err, PipelineError::Critical(_)));
    }

    /// The old fixed `batch * BLOCKS_PER_BATCH + i` rule is gone — a batch's blocks may start
    /// at any number, as long as they are internally consecutive and (if a previous batch exists)
    /// continue from it. This numbering would have been rejected under the old rule; it must pass now.
    #[test]
    fn a_batch_whose_blocks_do_not_start_at_batch_times_blocks_per_batch_is_accepted() {
        let mut block = block_at(2, 0, vec![]);
        block.number = 5; // batch id 2, but blocks numbered from 5, not 2*BLOCKS_PER_BATCH
        block.timestamp = 1_757_000_005;
        let compressed = channel::encode_stream(&[block.clone()]);
        let decoded = decode_batch(&compressed, CHAIN_ID, 2, CAP, None).unwrap();
        assert_eq!(decoded, vec![block]);
    }

    /// Internal consecutiveness is still enforced within one batch, independent of the cross-batch
    /// continuity check (which only looks at the first block).
    #[test]
    fn non_consecutive_blocks_within_one_batch_are_critical() {
        let mut second = block_at(0, 1, vec![]);
        second.number += 1; // skip a number: 0, 2 instead of 0, 1
        let blocks = vec![block_at(0, 0, vec![]), second];
        let compressed = channel::encode_stream(&blocks);
        let err = decode_batch(&compressed, CHAIN_ID, 0, CAP, None).unwrap_err();
        assert!(matches!(err, PipelineError::Critical(_)));
    }

    /// A batch whose every block's timestamp is within `anchor_unix_ts + max_drift`
    /// passes.
    #[test]
    fn a_timestamp_within_the_drift_bound_passes() {
        let blocks = vec![block_at(0, 0, vec![])]; // timestamp = 1_757_000_000 (see `block_at`)
        let drift = DriftBound { max_drift_secs: 10 };
        assert!(enforce_drift_bound(&blocks, 1_757_000_000 - 5, drift).is_ok());
    }

    /// A block's timestamp running ahead of `anchor_unix_ts + max_drift` is
    /// Critical, not silently accepted — the one-sided bound's whole purpose (future-dating).
    #[test]
    fn a_timestamp_exceeding_the_drift_bound_is_critical() {
        let blocks = vec![block_at(0, 0, vec![])]; // timestamp = 1_757_000_000
        let drift = DriftBound { max_drift_secs: 10 };
        let err = enforce_drift_bound(&blocks, 1_757_000_000 - 1_000, drift).unwrap_err();
        assert!(matches!(err, PipelineError::Critical(_)), "got {err:?}");
    }

    /// Why the bound is one-sided: an anchor posted well AFTER the
    /// block's own timestamp (a batcher backlog / posting latency) never rejects — only a timestamp
    /// running AHEAD of the anchor does. A two-sided bound (the withdrawn earlier form) would have
    /// rejected exactly this honest, merely-late batch.
    #[test]
    fn a_late_anchor_from_batcher_backlog_never_rejects() {
        let blocks = vec![block_at(0, 0, vec![])]; // timestamp = 1_757_000_000
        let drift = DriftBound { max_drift_secs: 60 };
        assert!(enforce_drift_bound(&blocks, 1_757_000_000 + 500, drift).is_ok());
    }

    /// [`DriftBound::unbounded`] is a genuine no-op — an absurdly early anchor and a tight
    /// `max_drift_secs` would otherwise reject every real timestamp; `unbounded()` must never do that
    /// (existing callers that have not opted into a real anchor must see unchanged behavior).
    #[test]
    fn unbounded_drift_bound_never_rejects() {
        let blocks = vec![block_at(0, 0, vec![])];
        assert!(enforce_drift_bound(&blocks, 12_345_678, DriftBound::unbounded()).is_ok());
    }
}
