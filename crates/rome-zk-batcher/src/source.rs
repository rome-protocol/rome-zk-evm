//! Reads the sequencer's ordered log and groups its records into [`channel::Block`]s at the chain's own
//! `sub_blocks_per_block` (default 20; a chain's profile may declare a different value — the
//! sequencer's own `profile.json` is the single source of the block shape;
//! [`BlockSource::open`] takes it as an explicit argument rather than assuming the default —
//! `crate::config::read_profile_identity` is what the binary reads it from).
//!
//! ## Provenance — the record is the block
//! Since the `log-included-only` change, the sealer persists
//! exactly `outcome.included` — the txs that executed and are covered by the sub-block's `tx_root` — never
//! the attempted prefix. A tx rejected at execution is not in the log and therefore not in DA; a tx the
//! executor did not reach is carried to the next sub-block. This module can therefore publish each record's
//! tx list as the executed block with no per-tx marker, and derivation's strict policy holds: a logged
//! tx that fails on re-execution is divergence, not a skip. (An earlier version of this crate read the
//! attempted superset and said so here; that caveat is closed.)

use alloy_primitives::B256;
use rome_zk_log::{LogReader, SubBlockRecord};

use crate::channel::Block;

/// The design default (block = 20 sub-blocks, `rome_zk_profile::DEFAULT_SUB_BLOCKS_PER_BLOCK`) — kept here as the
/// value every caller that hasn't been handed a differing chain profile passes explicitly (the profile, not this
/// crate, is the single source of the block shape — [`BlockSource::open`] takes its own `sub_blocks_per_block` rather
/// than importing this constant directly).
pub const DEFAULT_SUB_BLOCKS_PER_BLOCK: u16 = rome_zk_profile::DEFAULT_SUB_BLOCKS_PER_BLOCK;

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("log I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "block {block}: expected sub-block index {expected}, got {got} — the log is not a contiguous \
         0..{sub_blocks_per_block} run for this block"
    )]
    NonContiguousIndex {
        block: u64,
        expected: u16,
        got: u16,
        sub_blocks_per_block: u16,
    },
    #[error("block {block}: sub-block at index {index} has header.block {header_block}, expected {block}")]
    BlockMismatch {
        block: u64,
        index: u16,
        header_block: u64,
    },
    #[error("expected chain_id {expected}, sub-block header has chain_id {got}")]
    ChainIdMismatch { expected: u64, got: u64 },
    /// The log genuinely holds more sub-blocks for
    /// `block` than this source is configured with — the record at `index` (`>= sub_blocks_per_block`)
    /// continues the *same* block number the just-completed group already closed at
    /// `sub_blocks_per_block` records. Raised by peeking one record ahead **before** the completed group
    /// is ever handed back to the caller (see [`BlockSource::next_block`]'s module doc), so a
    /// mis-configured `sub_blocks_per_block` is refused before any block — complete or not — is posted,
    /// rather than silently returning a truncated "complete" block (the earlier bug: a
    /// 40-sub-block log read at `sub_blocks_per_block=20` used to return a 20-tx block first, and only
    /// the *next* record raised `NonContiguousIndex`).
    #[error(
        "block {block}: the log holds a sub-block at index {index}, but this source is configured for \
         only {sub_blocks_per_block} sub-blocks per block — the log's real shape disagrees with the \
         configured profile (or the sequencer's persisted profile.json), refusing rather than emitting a \
         truncated block"
    )]
    ProfileMismatch {
        block: u64,
        index: u16,
        sub_blocks_per_block: u16,
    },
}

/// One fully-grouped block plus its sub-block header hashes (exposed so a later change can publish them
/// — a challenger/indexer consumer, not this module).
#[derive(Debug, Clone)]
pub struct SourcedBlock {
    pub block: Block,
    /// The 20 sub-block header hashes that make up this block, in index order.
    pub sub_block_header_hashes: Vec<B256>,
}

/// Reads the ordered log (read-only) and groups every [`Self`]-configured `sub_blocks_per_block`
/// consecutive sub-block records into one [`Block`] — the batcher's only consumer of `rome_zk_log`.
pub struct BlockSource {
    reader: LogReader,
    chain_id: u64,
    block_gas_limit: u64,
    /// The chain's own `sub_blocks_per_block` (from this crate's
    /// `Config`, mirroring `rome_zk_profile::Profile::sub_blocks_per_block`) — the value that decides
    /// where this source's own block boundary falls, checked against the log's actual shape by
    /// [`Self::next_block`] rather than assumed to be the design's own default.
    sub_blocks_per_block: u16,
    /// Mirrors `sealer.rs`: the EVM block timestamp is
    /// `max(first_sub_block_secs, prev_block_timestamp_secs + 1)`, monotonic across blocks. The batcher
    /// must replicate this exact rule to recompute the same block timestamp the sealer produced — see the
    /// module doc's note on why a private helper in `sealer.rs` can't simply be called from here (crate
    /// boundary); the formula itself is copied verbatim (one line, `max(a, b+1)`) and cited at point of
    /// use, no other write-path logic is duplicated. Seeded to 0 at genesis, matching the sequencer's own
    /// `sealer::ResumePoint::default()`.
    prev_block_timestamp_secs: u64,
    /// Sub-block records accumulated toward the block currently being grouped, carried across
    /// `next_block` calls that return `None` because the log doesn't have the rest yet (live
    /// tail-follow) — without this, a partial group would be silently dropped and its already-consumed
    /// records lost, since `LogReader` never rewinds past what it has already yielded.
    pending: Vec<SubBlockRecord>,
    /// Peek-before-emit: one record read ahead of `pending` and held here rather than
    /// handed to the log reader's own position — [`Self::read_one`] always drains this before calling
    /// `self.reader.read_next()`, so nothing already read is ever lost. Populated only by the
    /// peek [`Self::next_block`] performs right after a group completes (to look for a sub-block that
    /// would continue the *same* block past `sub_blocks_per_block` — [`SourceError::ProfileMismatch`]);
    /// when the peeked record legitimately starts the *next* block, it is stashed here so the following
    /// `next_block` call consumes it as that group's first record instead of re-reading it from the log.
    lookahead: Option<SubBlockRecord>,
}

/// `sealer::resolve_block_timestamp_secs`'s formula, copied verbatim:
/// `max(first_sub_block_secs, prev_block_timestamp_secs + 1)`. Not importable across the crate boundary
/// (it is `pub(crate)` in `rome_zk_sequencer`) — this is the read-only replication the module doc explains.
fn resolve_block_timestamp_secs(first_sub_block_secs: u64, prev_block_timestamp_secs: u64) -> u64 {
    first_sub_block_secs.max(prev_block_timestamp_secs + 1)
}

impl BlockSource {
    /// Opens a reader positioned at `(from_block, 0)` — blocks are only ever grouped whole, so resuming
    /// mid-block makes no sense; a caller resuming after a restart passes the first block not yet fully
    /// posted and `prev_block_timestamp_secs` recovered from its own resume state (design: the batcher is
    /// stateless on-disk, but the timestamp monotonicity floor must still come from *somewhere legitimate*
    /// — the on-chain inbox's last posted block's timestamp, or 0 at genesis; see `resume.rs`).
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        log_dir: impl AsRef<std::path::Path>,
        chain_id: u64,
        block_gas_limit: u64,
        sub_blocks_per_block: u16,
        from_block: u64,
        prev_block_timestamp_secs: u64,
    ) -> std::io::Result<Self> {
        Ok(Self {
            reader: LogReader::open(log_dir, from_block, 0)?,
            chain_id,
            block_gas_limit,
            sub_blocks_per_block,
            prev_block_timestamp_secs,
            pending: Vec::with_capacity(sub_blocks_per_block as usize),
            lookahead: None,
        })
    }

    /// Re-scans the log directory for newly-rolled segments (live tail-follow) — see
    /// [`LogReader::refresh_segments`].
    pub fn refresh(&mut self) -> std::io::Result<()> {
        self.reader.refresh_segments()
    }

    /// Reads the next record, draining [`Self::lookahead`] first if the peek-before-emit stashed one —
    /// never re-reads a record the log reader already yielded.
    fn read_one(&mut self) -> std::io::Result<Option<SubBlockRecord>> {
        if let Some(r) = self.lookahead.take() {
            return Ok(Some(r));
        }
        self.reader.read_next()
    }

    /// Pulls the next complete block (its `SUB_BLOCKS_PER_BLOCK` sub-block records all present in the
    /// log), or `None` if the log does not yet have a full block waiting — call [`Self::refresh`] and
    /// retry to follow the tail as the sealer keeps appending.
    ///
    /// **Peek-before-emit:** once a group reaches `sub_blocks_per_block` records, this
    /// function peeks exactly one record further (without losing it — see [`Self::read_one`]/
    /// [`Self::lookahead`]) before returning the completed group. If that peeked record continues the
    /// *same* block number, the log genuinely holds more sub-blocks for this block than configured —
    /// [`SourceError::ProfileMismatch`] is returned instead of the completed-but-truncated block. If the
    /// peek finds nothing yet (tail-follow) or a record starting the next block, the completed group is
    /// returned normally (the peeked record, if any, is stashed in `lookahead` for the following call).
    pub fn next_block(&mut self) -> Result<Option<SourcedBlock>, SourceError> {
        loop {
            // `self.pending` carries a partial group across calls (see its field doc) — `read_one`
            // reports `None` without disturbing its own position when the log doesn't have the next
            // record yet, so it is always safe to just return and retry later after `refresh`.
            let Some(record) = self.read_one()? else {
                return Ok(None);
            };
            if record.header.chain_id != self.chain_id {
                return Err(SourceError::ChainIdMismatch {
                    expected: self.chain_id,
                    got: record.header.chain_id,
                });
            }
            let expected_index = self.pending.len() as u16;
            if record.header.index != expected_index {
                return Err(SourceError::NonContiguousIndex {
                    block: record.header.block,
                    expected: expected_index,
                    got: record.header.index,
                    sub_blocks_per_block: self.sub_blocks_per_block,
                });
            }
            if let Some(first) = self.pending.first() {
                if record.header.block != first.header.block {
                    return Err(SourceError::BlockMismatch {
                        block: first.header.block,
                        index: record.header.index,
                        header_block: record.header.block,
                    });
                }
            }
            self.pending.push(record);
            if self.pending.len() == self.sub_blocks_per_block as usize {
                break;
            }
        }

        // Peek one record ahead before this group is ever handed back — see this function's
        // own doc.
        let this_block = self.pending[0].header.block;
        if let Some(next) = self.read_one()? {
            if next.header.block == this_block {
                return Err(SourceError::ProfileMismatch {
                    block: this_block,
                    index: next.header.index,
                    sub_blocks_per_block: self.sub_blocks_per_block,
                });
            }
            // Legitimately the next block's own first record — hand it back to the next call rather than
            // dropping it (it has already been consumed off the log reader's own position).
            self.lookahead = Some(next);
        }

        let group = std::mem::take(&mut self.pending);
        let number = group[0].header.block;
        let first_sub_block_secs = group[0].header.timestamp_us / 1_000_000;
        let timestamp =
            resolve_block_timestamp_secs(first_sub_block_secs, self.prev_block_timestamp_secs);
        self.prev_block_timestamp_secs = timestamp;

        let sub_block_header_hashes = group.iter().map(|r| r.header.hash()).collect();
        // See the module doc: the log holds exactly the included set (`outcome.included`)
        // — this is that same executed, DA-covered set, not an attempted superset.
        let txs = group.into_iter().flat_map(|r| r.txs).collect();

        Ok(Some(SourcedBlock {
            block: Block {
                number,
                timestamp,
                gas_limit: self.block_gas_limit,
                txs,
                deposits_end: None,
            },
            sub_block_header_hashes,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::B256;
    use alloy::signers::local::PrivateKeySigner;
    use rome_zk_sequencer::header::SubBlockHeader;
    use rome_zk_sequencer::log::LogWriter;
    use rome_zk_sequencer::signing::sign_header;
    use rome_zk_sequencer::testutil::signed_raw_tx;
    use std::fs;
    use tempfile::tempdir;

    const CHAIN_ID: u64 = 200_198;

    fn write_block(
        writer: &mut LogWriter,
        block: u64,
        base_ts_us: u64,
        sender: &PrivateKeySigner,
        prev_hash_start: B256,
    ) -> B256 {
        write_block_with_cadence(
            writer,
            block,
            base_ts_us,
            sender,
            prev_hash_start,
            DEFAULT_SUB_BLOCKS_PER_BLOCK,
            50_000,
        )
    }

    /// Same as [`write_block`] but at an arbitrary `sub_blocks_per_block`/`step_us` cadence — used to
    /// prove [`BlockSource`] groups by its OWN configured `sub_blocks_per_block`, not the design
    /// default.
    fn write_block_with_cadence(
        writer: &mut LogWriter,
        block: u64,
        base_ts_us: u64,
        sender: &PrivateKeySigner,
        prev_hash_start: B256,
        sub_blocks_per_block: u16,
        step_us: u64,
    ) -> B256 {
        let mut prev_hash = prev_hash_start;
        for index in 0..sub_blocks_per_block {
            let header = SubBlockHeader {
                chain_id: CHAIN_ID,
                block,
                index,
                timestamp_us: base_ts_us + index as u64 * step_us,
                tx_root: B256::repeat_byte(index as u8),
                receipts_root: B256::repeat_byte(index as u8 + 1),
                gas_used: 21_000,
                prev_hash,
            };
            let signature = sign_header(&PrivateKeySigner::random(), &header);
            let tx = signed_raw_tx(sender, CHAIN_ID, block * 1_000 + index as u64);
            writer.append(&header, &signature, &[tx]).unwrap();
            prev_hash = header.hash();
        }
        prev_hash
    }

    /// The source must group exactly `SUB_BLOCKS_PER_BLOCK` consecutive sub-blocks into one `Block`,
    /// matching the sealer's own block boundary.
    #[test]
    fn groups_twenty_sub_blocks_into_one_block() {
        let dir = tempdir().unwrap();
        let sender = PrivateKeySigner::random();
        let mut writer = LogWriter::open(dir.path(), 10_000).unwrap();
        write_block(&mut writer, 1, 1_757_000_000_000_000, &sender, B256::ZERO);

        let mut source = BlockSource::open(
            dir.path(),
            CHAIN_ID,
            100_000_000,
            DEFAULT_SUB_BLOCKS_PER_BLOCK,
            1,
            0,
        )
        .unwrap();
        let sourced = source.next_block().unwrap().expect("one full block");
        assert_eq!(sourced.block.number, 1);
        assert_eq!(
            sourced.block.txs.len(),
            DEFAULT_SUB_BLOCKS_PER_BLOCK as usize
        );
        assert_eq!(sourced.block.gas_limit, 100_000_000);
        assert_eq!(
            sourced.sub_block_header_hashes.len(),
            DEFAULT_SUB_BLOCKS_PER_BLOCK as usize
        );
        // Block timestamp = index-0 sub-block's second (first block, so the monotonic floor of 0+1
        // doesn't bind: 1_757_000_000 > 1).
        assert_eq!(sourced.block.timestamp, 1_757_000_000);

        // Nothing more available yet.
        assert!(source.next_block().unwrap().is_none());
    }

    /// A second block's timestamp is `max(its own first second, prev + 1)` — exercised with two blocks
    /// sealed inside the same wall-clock second so the monotonic floor actually binds.
    #[test]
    fn block_timestamps_are_monotonic_across_blocks() {
        let dir = tempdir().unwrap();
        let sender = PrivateKeySigner::random();
        let mut writer = LogWriter::open(dir.path(), 10_000).unwrap();
        let base = 1_757_000_000_000_000u64;
        let h0 = write_block(&mut writer, 1, base, &sender, B256::ZERO);
        // Second block starts in the very same wall-clock second as the first.
        write_block(&mut writer, 2, base + 100_000, &sender, h0);

        let mut source = BlockSource::open(
            dir.path(),
            CHAIN_ID,
            100_000_000,
            DEFAULT_SUB_BLOCKS_PER_BLOCK,
            1,
            0,
        )
        .unwrap();
        let b0 = source.next_block().unwrap().unwrap();
        let b1 = source.next_block().unwrap().unwrap();
        assert_eq!(b0.block.timestamp, 1_757_000_000);
        assert_eq!(
            b1.block.timestamp, 1_757_000_001,
            "same-second second block must still get a strictly greater timestamp"
        );
    }

    /// Resuming a `BlockSource` mid-history must be seeded with the correct
    /// `prev_block_timestamp_secs` floor (design: recovered from the on-chain inbox / resume state), or
    /// it would recompute a timestamp lower than what the sealer actually produced.
    #[test]
    fn resuming_from_a_later_block_needs_the_correct_prev_timestamp_seed() {
        let dir = tempdir().unwrap();
        let sender = PrivateKeySigner::random();
        let mut writer = LogWriter::open(dir.path(), 10_000).unwrap();
        let base = 1_757_000_000_000_000u64;
        let h0 = write_block(&mut writer, 1, base, &sender, B256::ZERO);
        write_block(&mut writer, 2, base + 100_000, &sender, h0);

        // Resume at block 2 seeded with the real prior block's timestamp (1_757_000_000), as a correct
        // resume path would recover it.
        let mut source = BlockSource::open(
            dir.path(),
            CHAIN_ID,
            100_000_000,
            DEFAULT_SUB_BLOCKS_PER_BLOCK,
            2,
            1_757_000_000,
        )
        .unwrap();
        let b1 = source.next_block().unwrap().unwrap();
        assert_eq!(b1.block.timestamp, 1_757_000_001);
    }

    /// A log with a non-contiguous index (a gap, or an out-of-order sub-block) must error, not silently
    /// group whatever happens to be next — see `SourceError::NonContiguousIndex`.
    #[test]
    fn non_contiguous_index_is_an_error_not_a_guess() {
        let dir = tempdir().unwrap();
        let sender = PrivateKeySigner::random();
        let mut writer = LogWriter::open(dir.path(), 10_000).unwrap();
        // Write only index 0 and then index 2 (skip 1) for block 0.
        for index in [0u16, 2u16] {
            let header = SubBlockHeader {
                chain_id: CHAIN_ID,
                block: 1,
                index,
                timestamp_us: 1_757_000_000_000_000 + index as u64 * 50_000,
                tx_root: B256::ZERO,
                receipts_root: B256::ZERO,
                gas_used: 21_000,
                prev_hash: B256::ZERO,
            };
            let signature = sign_header(&PrivateKeySigner::random(), &header);
            let tx = signed_raw_tx(&sender, CHAIN_ID, index as u64);
            writer.append(&header, &signature, &[tx]).unwrap();
        }
        let mut source = BlockSource::open(
            dir.path(),
            CHAIN_ID,
            100_000_000,
            DEFAULT_SUB_BLOCKS_PER_BLOCK,
            1,
            0,
        )
        .unwrap();
        let err = source.next_block().unwrap_err();
        assert!(matches!(err, SourceError::NonContiguousIndex { .. }));
    }

    /// A sub-block record whose header carries a different `chain_id` than configured must error rather
    /// than being silently folded into this chain's block (cross-chain log confusion would otherwise
    /// silently corrupt the DA content).
    #[test]
    fn mismatched_chain_id_is_an_error() {
        let dir = tempdir().unwrap();
        let sender = PrivateKeySigner::random();
        let mut writer = LogWriter::open(dir.path(), 10_000).unwrap();
        let header = SubBlockHeader {
            chain_id: 999_999,
            block: 1,
            index: 0,
            timestamp_us: 1_757_000_000_000_000,
            tx_root: B256::ZERO,
            receipts_root: B256::ZERO,
            gas_used: 21_000,
            prev_hash: B256::ZERO,
        };
        let signature = sign_header(&PrivateKeySigner::random(), &header);
        let tx = signed_raw_tx(&sender, 999_999, 0);
        writer.append(&header, &signature, &[tx]).unwrap();

        let mut source = BlockSource::open(
            dir.path(),
            CHAIN_ID,
            100_000_000,
            DEFAULT_SUB_BLOCKS_PER_BLOCK,
            1,
            0,
        )
        .unwrap();
        let err = source.next_block().unwrap_err();
        assert!(matches!(err, SourceError::ChainIdMismatch { .. }));
    }

    /// Live tail-follow: a block not yet fully sealed returns `None`; once the sealer finishes it and the
    /// source refreshes, the block becomes available.
    #[test]
    fn follows_the_tail_for_a_still_sealing_block() {
        let dir = tempdir().unwrap();
        let sender = PrivateKeySigner::random();
        let mut writer = LogWriter::open(dir.path(), 10_000).unwrap();
        // Write only the first 10 of 20 sub-blocks.
        for index in 0..10u16 {
            let header = SubBlockHeader {
                chain_id: CHAIN_ID,
                block: 1,
                index,
                timestamp_us: 1_757_000_000_000_000 + index as u64 * 50_000,
                tx_root: B256::ZERO,
                receipts_root: B256::ZERO,
                gas_used: 21_000,
                prev_hash: B256::ZERO,
            };
            let signature = sign_header(&PrivateKeySigner::random(), &header);
            let tx = signed_raw_tx(&sender, CHAIN_ID, index as u64);
            writer.append(&header, &signature, &[tx]).unwrap();
        }

        let mut source = BlockSource::open(
            dir.path(),
            CHAIN_ID,
            100_000_000,
            DEFAULT_SUB_BLOCKS_PER_BLOCK,
            1,
            0,
        )
        .unwrap();
        assert!(source.next_block().unwrap().is_none());

        for index in 10..20u16 {
            let header = SubBlockHeader {
                chain_id: CHAIN_ID,
                block: 1,
                index,
                timestamp_us: 1_757_000_000_000_000 + index as u64 * 50_000,
                tx_root: B256::ZERO,
                receipts_root: B256::ZERO,
                gas_used: 21_000,
                prev_hash: B256::ZERO,
            };
            let signature = sign_header(&PrivateKeySigner::random(), &header);
            let tx = signed_raw_tx(&sender, CHAIN_ID, 100 + index as u64);
            writer.append(&header, &signature, &[tx]).unwrap();
        }
        source.refresh().unwrap();
        let sourced = source.next_block().unwrap().expect("now complete");
        assert_eq!(sourced.block.txs.len(), 20);
    }

    /// A log genuinely sealed at a 25 ms x 40-per-block profile
    /// (still a 1 s block, `crate::profile::Profile`'s own whole-seconds invariant) must batch cleanly
    /// through `BlockSource` when it is configured with that SAME `sub_blocks_per_block` — before this was
    /// fixed, `BlockSource` always grouped by the hardcoded design default (20) and this profile would
    /// have failed with `SourceError::NonContiguousIndex` at record 21 (index 20 where 0 was expected).
    #[test]
    fn twenty_five_ms_times_forty_profile_batches_without_non_contiguous_index() {
        let dir = tempdir().unwrap();
        let sender = PrivateKeySigner::random();
        let mut writer = LogWriter::open(dir.path(), 10_000).unwrap();
        write_block_with_cadence(
            &mut writer,
            1,
            1_757_000_000_000_000,
            &sender,
            B256::ZERO,
            40,
            25_000,
        );

        let mut source = BlockSource::open(dir.path(), CHAIN_ID, 100_000_000, 40, 1, 0).unwrap();
        let sourced = source
            .next_block()
            .expect("a 40/block log configured at sub_blocks_per_block=40 must not error")
            .expect("one full block");
        assert_eq!(sourced.block.number, 1);
        assert_eq!(sourced.block.txs.len(), 40);
        assert_eq!(sourced.sub_block_header_hashes.len(), 40);
    }

    /// Peek-before-emit: a log genuinely written at 40
    /// sub-blocks/block, opened at the WRONG `sub_blocks_per_block=20`, must error before any `Some(_)`
    /// block is ever returned — never hand back a truncated 20-tx "complete" block first. This is the
    /// exact repro of the *old* behaviour: "the 40-record block
    /// came back as a 20-tx 'complete' block, not as an error".
    #[test]
    fn a_forty_per_block_log_opened_at_twenty_errors_before_any_block_is_returned() {
        let dir = tempdir().unwrap();
        let sender = PrivateKeySigner::random();
        let mut writer = LogWriter::open(dir.path(), 10_000).unwrap();
        write_block_with_cadence(
            &mut writer,
            1,
            1_757_000_000_000_000,
            &sender,
            B256::ZERO,
            40,
            25_000,
        );

        let mut source = BlockSource::open(dir.path(), CHAIN_ID, 100_000_000, 20, 1, 0).unwrap();
        let err = source
            .next_block()
            .expect_err("must refuse, not return a truncated 20-tx block");
        assert!(
            matches!(
                err,
                SourceError::ProfileMismatch {
                    block: 1,
                    index: 20,
                    sub_blocks_per_block: 20,
                }
            ),
            "{err:?}"
        );
    }

    /// The peeked record that legitimately starts the *next* block is not lost — it becomes that next
    /// group's own first record on the following `next_block` call (proves `lookahead` round-trips
    /// correctly rather than silently dropping a record once read off the log).
    #[test]
    fn the_peeked_record_starting_the_next_block_is_not_lost() {
        let dir = tempdir().unwrap();
        let sender = PrivateKeySigner::random();
        let mut writer = LogWriter::open(dir.path(), 10_000).unwrap();
        let base = 1_757_000_000_000_000u64;
        let h0 = write_block(&mut writer, 1, base, &sender, B256::ZERO);
        write_block(&mut writer, 2, base + 21_000_000, &sender, h0);

        let mut source = BlockSource::open(
            dir.path(),
            CHAIN_ID,
            100_000_000,
            DEFAULT_SUB_BLOCKS_PER_BLOCK,
            1,
            0,
        )
        .unwrap();
        let b0 = source.next_block().unwrap().expect("block 1");
        assert_eq!(b0.block.number, 1);
        let b1 = source
            .next_block()
            .unwrap()
            .expect("block 2 (via lookahead)");
        assert_eq!(b1.block.number, 2);
        assert_eq!(b1.block.txs.len(), DEFAULT_SUB_BLOCKS_PER_BLOCK as usize);
        // Nothing left.
        assert!(source.next_block().unwrap().is_none());
    }

    /// The batcher is what would post a corrupted record, so it must refuse one by
    /// name rather than silently emitting a truncated (or wrong) block. `BlockSource` reads the log
    /// only through `LogReader` (`rome_zk_log::LogReader` — see the module doc), so this is
    /// the same corruption `rome-zk-log`'s own `log_reader_refuses_a_bit_flipped_record` proves at the
    /// reader level, exercised here through the consumer that would actually post it: `SourceError`'s
    /// existing `#[from] std::io::Error` variant carries `LogReader::read_next`'s new `InvalidData`
    /// error straight through `next_block`'s `?`. Two sub-blocks are written (`sub_blocks_per_block =
    /// 2`) and the *second* is flipped: `BlockSource::open`'s `LogReader` stops its skip-to-position
    /// loop at the first record already at `(from_block, 0)` (the first sub-block itself), so opening
    /// succeeds and the good first sub-block is folded into `pending` before the corrupted second one
    /// is reached by `next_block`'s own loop.
    #[test]
    fn block_source_refuses_a_corrupted_record_by_name() {
        let dir = tempdir().unwrap();
        let sender = PrivateKeySigner::random();
        let path = dir.path().join("segment-000000000000.log");
        let mut writer = LogWriter::open(dir.path(), 10_000).unwrap();
        for index in 0u16..2 {
            let header = SubBlockHeader {
                chain_id: CHAIN_ID,
                block: 1,
                index,
                timestamp_us: 1_757_000_000_000_000 + index as u64 * 50_000,
                tx_root: B256::ZERO,
                receipts_root: B256::ZERO,
                gas_used: 21_000,
                prev_hash: B256::ZERO,
            };
            let signature = sign_header(&PrivateKeySigner::random(), &header);
            let tx = signed_raw_tx(&sender, CHAIN_ID, index as u64);
            writer.append(&header, &signature, &[tx]).unwrap();
        }
        drop(writer);

        // Same corruption shape as rome-zk-log's own reader-level test: flip a bit inside the second
        // record's payload (past its own 4-byte length prefix), leaving `total_len` untouched. The
        // first record's own length is re-derived by writing it alone into a scratch log, rather than
        // hardcoding a byte offset that would drift with the record format.
        let first_record_len = {
            let scratch = tempdir().unwrap();
            let mut w = LogWriter::open(scratch.path(), 10_000).unwrap();
            let header0 = SubBlockHeader {
                chain_id: CHAIN_ID,
                block: 1,
                index: 0,
                timestamp_us: 1_757_000_000_000_000,
                tx_root: B256::ZERO,
                receipts_root: B256::ZERO,
                gas_used: 21_000,
                prev_hash: B256::ZERO,
            };
            // Re-signing here (a fresh random sequencer key) does not reproduce the *exact* first-record
            // bytes written above (the signature differs) — only its byte *length*, which is all this
            // needs: ECDSA (r, s, v) signatures are a fixed 65 bytes regardless of key or message, so the
            // record length is identical either way.
            let sig0 = sign_header(&PrivateKeySigner::random(), &header0);
            let tx0 = signed_raw_tx(&sender, CHAIN_ID, 0);
            w.append(&header0, &sig0, &[tx0]).unwrap();
            drop(w);
            fs::metadata(scratch.path().join("segment-000000000000.log"))
                .unwrap()
                .len()
        };

        let mut bytes = fs::read(&path).unwrap();
        // Flip a byte inside the second record's raw-transaction bytes (an RLP byte string, so any
        // value still decodes): the only guard that can reject it is the record CRC, which is what
        // this test binds. `+ 10` from the record start would land on the RLP `block` field, where
        // 0x01 -> 0x00 is refused by the RLP decoder before the CRC is ever compared.
        let flip_at = bytes.len() - 6;
        assert!(
            flip_at > first_record_len as usize + 4,
            "flip must land inside the second record"
        );
        bytes[flip_at] ^= 0x01;
        fs::write(&path, &bytes).unwrap();

        let mut source = BlockSource::open(dir.path(), CHAIN_ID, 100_000_000, 2, 1, 0).unwrap();
        let err = source
            .next_block()
            .expect_err("a corrupted record must be refused by name, not silently truncated");
        assert!(
            matches!(err, SourceError::Io(_)),
            "must surface as the named SourceError::Io variant: {err:?}"
        );
    }

    /// A mid-segment out-of-range length
    /// prefix — real data sitting right after it, never something a genuine torn write could leave
    /// behind — must reach this crate's own `next_block()` as the same named `SourceError::Io` a
    /// bit-flipped record already refuses with, not as a silent `Ok(None)` that would stall `--follow`
    /// forever waiting at a record that will never complete. Two valid sub-blocks are written to form a
    /// full block (`sub_blocks_per_block = 2`) and a third, otherwise-valid record is written after them
    /// via the real writer (never hand-encoded); the bogus length prefix is then spliced in between the
    /// second and third records' own real bytes, so nothing about the frame *shapes* themselves is
    /// fabricated — only the extra 4 corrupt bytes are. `next_block`'s own peek reaches the
    /// splice on the very call that completes the first (good) group, so that one call itself returns
    /// the error.
    #[test]
    fn block_source_refuses_an_out_of_range_length_prefix_mid_segment() {
        let dir = tempdir().unwrap();
        let sender = PrivateKeySigner::random();
        let path = dir.path().join("segment-000000000000.log");

        let mut writer = LogWriter::open(dir.path(), 10_000).unwrap();
        for index in 0u16..2 {
            let header = SubBlockHeader {
                chain_id: CHAIN_ID,
                block: 1,
                index,
                timestamp_us: 1_757_000_000_000_000 + index as u64 * 50_000,
                tx_root: B256::ZERO,
                receipts_root: B256::ZERO,
                gas_used: 21_000,
                prev_hash: B256::ZERO,
            };
            let signature = sign_header(&PrivateKeySigner::random(), &header);
            let tx = signed_raw_tx(&sender, CHAIN_ID, index as u64);
            writer.append(&header, &signature, &[tx]).unwrap();
        }
        let boundary = fs::metadata(&path).unwrap().len();

        // A third, real record written by the same writer — never hand-encoded — so only the spliced-in
        // length prefix below is fabricated, not the frame shape around it.
        let header2 = SubBlockHeader {
            chain_id: CHAIN_ID,
            block: 2,
            index: 0,
            timestamp_us: 1_757_000_100_000_000,
            tx_root: B256::ZERO,
            receipts_root: B256::ZERO,
            gas_used: 21_000,
            prev_hash: B256::ZERO,
        };
        let signature2 = sign_header(&PrivateKeySigner::random(), &header2);
        let tx2 = signed_raw_tx(&sender, CHAIN_ID, 2);
        writer.append(&header2, &signature2, &[tx2]).unwrap();
        drop(writer);

        let mut bytes = fs::read(&path).unwrap();
        let boundary = boundary as usize;
        let mut spliced = bytes[..boundary].to_vec();
        spliced.extend_from_slice(&(rome_zk_sequencer::log::MAX_FRAME_LEN + 1).to_le_bytes());
        spliced.extend_from_slice(&bytes[boundary..]);
        bytes = spliced;
        fs::write(&path, &bytes).unwrap();

        let mut source = BlockSource::open(dir.path(), CHAIN_ID, 100_000_000, 2, 1, 0).unwrap();
        let err = source.next_block().expect_err(
            "an out-of-range length prefix with real data following it must be refused by name, not Ok(None)",
        );
        assert!(
            matches!(err, SourceError::Io(_)),
            "must surface as the named SourceError::Io variant: {err:?}"
        );
    }
}
