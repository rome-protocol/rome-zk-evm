//! Forward heal of the Headers static file segment.
//!
//! Reth's own `ProviderFactory::check_consistency()` (called by [`crate::executor::RethExecutor::new`])
//! self-heals a torn Headers segment BACKWARD: its first step, `check_file_consistency`, runs
//! `NippyJarChecker::ensure_consistency`, which — when the offsets/data files hold more rows than the
//! `.conf` file's committed row count — TRUNCATES the offsets file then the data file down to that
//! committed count, permanently discarding whatever extra row was already durable on disk
//! (`crates/storage/nippy-jar/src/consistency.rs` ~74-86: "Happened during an appending job ...").
//! Reth's commit order (static data+offsets `sync_all` → `.conf` written to `.tmp`, fsynced, renamed and
//! the directory fsynced → RocksDB → MDBX `tx.commit()`, one thread — `crates/storage/nippy-jar/src/writer.rs`
//! ~349-378, `fs-util` `atomic_write_file`, `crates/storage/provider/src/providers/database/provider.rs`
//! ~4030-4055) means a `.conf` can only lag a COMMITTED MDBX if the rename itself was lost — which a kill
//! does not produce. The shape a kill does produce is one durable row that MDBX never
//! committed; this module refuses that row by name (its hash is absent from MDBX `HeaderNumbers`) and
//! reth's truncation repairs it, the ordered log replaying the block. The forward heal is therefore
//! defence in depth for the one shape where the row IS bound (the torn-conf tests build it by hand):
//! read and verify the row before `check_consistency` can discard it, then re-commit it forward.
//!
//! Sources read at the pinned tag (v2.5.2, checkout `~/.cargo/git/checkouts/reth-e231042ee7db3fb7/5a6940e`):
//! - `crates/storage/nippy-jar/src/lib.rs` — `NippyJar::load` (reads only the `.conf`), `rows()`,
//!   `columns()`, `open_data_reader()` → `DataReader::{offset, offsets_count, data}` (~201-441).
//! - `crates/storage/nippy-jar/src/consistency.rs` — `NippyJarChecker::ensure_consistency` (~52-152):
//!   the destructive truncate-on-open heal this module must read past.
//! - `crates/storage/nippy-jar/src/cursor.rs` — `NippyJarCursor::read_value` (~147-199): the
//!   column-offset arithmetic mirrored by [`read_column`] below, without its `row < jar.rows` bound
//!   (reading exactly one row past that bound is the entire point here).
//! - `crates/storage/nippy-jar/src/compression/mod.rs` — `Compression::decompress` (~10-15).
//! - `crates/static-file/types/src/segment.rs` — Headers segment = 3 columns: header, total
//!   difficulty, hash (`columns()` ~113-120).
//! - `crates/storage/provider/src/providers/static_file/manager.rs` — `get_highest_static_file_block`,
//!   `find_fixed_range`, `directory()` (`check_segment_consistency` ~1507-1524 builds the identical
//!   path), `StaticFileWriter::latest_writer` (~2295-2341), `ProviderFactory::check_consistency`'s own
//!   step order (`check_consistency`, database/mod.rs ~517-548: `check_file_consistency` runs FIRST —
//!   the reason this module's read must happen before that call, never after).
//! - `crates/storage/provider/src/providers/static_file/writer.rs` — `StaticFileProviderRW::{
//!   next_block_number ~847, append_header ~1131, commit ~603}`.
//! - `crates/storage/db-api/src/tables/mod.rs` — `HeaderNumbers { Key = BlockHash, Value =
//!   BlockNumber }` (~324-327); `crates/storage/provider/src/providers/database/provider.rs`
//!   `insert_block_mdbx_only` (~884-935) writes `HeaderNumbers` but no `CanonicalHeaders` under this
//!   reth (grep confirms 0 hits) — so `HeaderNumbers` is the binding MDBX fact this heal cross-checks.
//! - `reth-codecs` 0.6.0 (crates.io, the version reth v2.5.2's own Cargo.lock pins) —
//!   `src/alloy/header.rs`: `impl Compact for alloy_consensus::Header`; `src/lib.rs`: `impl Compact
//!   for B256` (a fixed-size type — not itself compacted, though NippyJar's own LZ4 layer still
//!   compresses the column bytes on disk).

use alloy_consensus::Header;
use alloy_primitives::B256;
use reth_codecs::Compact;
use reth_db_api::tables;
use reth_db_api::transaction::DbTx;
use reth_nippy_jar::compression::Compression;
use reth_nippy_jar::{DataReader, NippyJar};
use reth_provider::providers::StaticFileWriter;
use reth_provider::{ProviderFactory, StaticFileProviderFactory};
use reth_static_file_types::{SegmentHeader, StaticFileSegment};
use rome_zk_executor_api::ExecutorError;

use crate::chain::RethTypes;

/// Headers segment column layout (segment.rs `columns()`): header, total difficulty, hash.
const HEADERS_COLUMNS: usize = 3;
const HEADER_COLUMN: usize = 0;
const HASH_COLUMN: usize = 2;

fn backend(msg: String) -> ExecutorError {
    ExecutorError::Backend(msg)
}

/// A single Headers row found durable on disk one block beyond the segment's committed `.conf`,
/// verified against each of the four checks (header number, header hash, parent hash, `HeaderNumbers`
/// entry) — ready for [`commit_forward_heal`].
#[derive(Debug, PartialEq)]
pub struct VerifiedForwardHeal {
    pub header: Header,
    pub hash: B256,
    /// `header.number`, re-asserted against `StaticFileProviderRW::next_block_number()` by
    /// [`commit_forward_heal`] before it appends anything.
    pub block_number: u64,
}

/// Named reason a candidate extra row was NOT healed forward (the four checks) —
/// `Display`s into the `warn!` line [`crate::executor::RethExecutor::new`] logs before falling
/// through to reth's own (backward) heal.
#[derive(Debug, PartialEq, Eq)]
pub enum ForwardHealRefusal {
    HeaderNumberMismatch {
        expected: u64,
        found: u64,
    },
    HeaderHashMismatch {
        computed: B256,
        stored: B256,
    },
    ParentHashMismatch {
        expected: B256,
        found: B256,
    },
    HeaderNumbersMismatch {
        hash: B256,
        expected: u64,
        found: Option<u64>,
    },
}

impl std::fmt::Display for ForwardHealRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HeaderNumberMismatch { expected, found } => write!(
                f,
                "extra row's header.number {found} does not match the expected {expected}"
            ),
            Self::HeaderHashMismatch { computed, stored } => write!(
                f,
                "extra row's keccak256(rlp(header)) {computed} does not match its stored hash \
                 column {stored}"
            ),
            Self::ParentHashMismatch { expected, found } => write!(
                f,
                "extra row's header.parent_hash {found} does not match the config's last row's \
                 own hash {expected}"
            ),
            Self::HeaderNumbersMismatch {
                hash,
                expected,
                found,
            } => write!(
                f,
                "MDBX HeaderNumbers[{hash}] = {found:?}, expected Some({expected}) — MDBX never \
                 committed this hash, so the row is not bindingly durable"
            ),
        }
    }
}

/// What [`detect_and_verify_forward_heal`] found.
#[derive(Debug, PartialEq)]
pub enum ForwardHealOutcome {
    /// The offsets file holds exactly as many rows as the `.conf` already claims — nothing to heal.
    Consistent,
    /// The offsets file holds `extra` rows beyond the config and `extra != 1` — outside the single-
    /// block shape this module heals (more than one extra row is not healed forward). A
    /// negative `extra` (the offsets file holds FEWER rows than the config, reth's own "pruning
    /// interruption" shape) is reported the same way — also not this module's shape to heal.
    UnexpectedExtraRowCount { extra: i64 },
    /// Exactly one extra row was found but failed a verification check.
    Refused(ForwardHealRefusal),
    /// Exactly one extra row was found and every check passed. Boxed: `VerifiedForwardHeal` carries
    /// a full `Header` and this is otherwise a small, frequently-matched-on enum (clippy
    /// `large_enum_variant`).
    Verified(Box<VerifiedForwardHeal>),
}

/// Reads one column of one row directly off the Headers segment's data+offsets files, WITHOUT
/// `NippyJarCursor`'s `row < jar.rows` bound (cursor.rs `read_value`) — the entire point here is
/// reading exactly one row past that bound, before anything truncates it away.
fn read_column(
    jar: &NippyJar<SegmentHeader>,
    reader: &DataReader,
    row: usize,
    column: usize,
) -> Result<Vec<u8>, ExecutorError> {
    let offset_pos = row * HEADERS_COLUMNS + column;
    let value_offset = reader
        .offset(offset_pos)
        .map_err(|e| backend(e.to_string()))? as usize;
    let next_value_offset = reader
        .offset(offset_pos + 1)
        .map_err(|e| backend(e.to_string()))? as usize;
    let raw = reader.data(value_offset..next_value_offset);
    match jar.compressor() {
        Some(compressor) => compressor
            .decompress(raw)
            .map_err(|e| backend(e.to_string())),
        None => Ok(raw.to_vec()),
    }
}

/// First step: inspect the Headers segment for exactly one durable-but-uncommitted row and
/// verify it, BEFORE `factory.check_consistency()` runs (that call's first step,
/// `check_file_consistency`, is what would otherwise truncate this same row away — see this
/// module's own doc). Read-only: never mutates the datadir.
pub fn detect_and_verify_forward_heal(
    factory: &ProviderFactory<RethTypes>,
) -> Result<ForwardHealOutcome, ExecutorError> {
    let static_file_provider = factory.static_file_provider();
    let Some(config_highest_block) =
        static_file_provider.get_highest_static_file_block(StaticFileSegment::Headers)
    else {
        // No Headers static file committed at all yet (should not happen post-genesis) — nothing
        // durable could have torn ahead of a config that does not exist.
        return Ok(ForwardHealOutcome::Consistent);
    };

    let range =
        static_file_provider.find_fixed_range(StaticFileSegment::Headers, config_highest_block);
    let file_path = static_file_provider
        .directory()
        .join(StaticFileSegment::Headers.filename(&range));
    let jar = NippyJar::<SegmentHeader>::load(&file_path).map_err(|e| backend(e.to_string()))?;
    if jar.columns() != HEADERS_COLUMNS {
        return Err(backend(format!(
            "Headers static file segment has {} columns, expected {HEADERS_COLUMNS} (header, total \
             difficulty, hash) — refusing to reason about its rows",
            jar.columns()
        )));
    }
    let config_rows = jar.rows();
    let reader = jar.open_data_reader().map_err(|e| backend(e.to_string()))?;
    let offsets_count = reader.offsets_count().map_err(|e| backend(e.to_string()))?;
    let actual_rows_on_disk = (offsets_count.saturating_sub(1) / HEADERS_COLUMNS) as i64;
    let extra = actual_rows_on_disk - config_rows as i64;

    if extra == 0 {
        return Ok(ForwardHealOutcome::Consistent);
    }
    if extra != 1 {
        return Ok(ForwardHealOutcome::UnexpectedExtraRowCount { extra });
    }

    let header_bytes = read_column(&jar, &reader, config_rows, HEADER_COLUMN)?;
    let hash_bytes = read_column(&jar, &reader, config_rows, HASH_COLUMN)?;
    let (header, _) = Header::from_compact(&header_bytes, header_bytes.len());
    let (row_hash, _) = B256::from_compact(&hash_bytes, hash_bytes.len());

    let expected_number = config_highest_block + 1;
    if header.number != expected_number {
        return Ok(ForwardHealOutcome::Refused(
            ForwardHealRefusal::HeaderNumberMismatch {
                expected: expected_number,
                found: header.number,
            },
        ));
    }

    let computed_hash = header.hash_slow();
    if computed_hash != row_hash {
        return Ok(ForwardHealOutcome::Refused(
            ForwardHealRefusal::HeaderHashMismatch {
                computed: computed_hash,
                stored: row_hash,
            },
        ));
    }

    // config_rows >= 1 here (get_highest_static_file_block returned Some above), so config_rows - 1
    // never underflows: row config_rows - 1 is the config's own last committed row.
    let parent_bytes = read_column(&jar, &reader, config_rows - 1, HASH_COLUMN)?;
    let (config_last_hash, _) = B256::from_compact(&parent_bytes, parent_bytes.len());
    if header.parent_hash != config_last_hash {
        return Ok(ForwardHealOutcome::Refused(
            ForwardHealRefusal::ParentHashMismatch {
                expected: config_last_hash,
                found: header.parent_hash,
            },
        ));
    }

    let provider_ro = factory.provider().map_err(|e| backend(e.to_string()))?;
    let mdbx_number = provider_ro
        .tx_ref()
        .get::<tables::HeaderNumbers>(row_hash)
        .map_err(|e| backend(e.to_string()))?;
    drop(provider_ro);
    if mdbx_number != Some(expected_number) {
        return Ok(ForwardHealOutcome::Refused(
            ForwardHealRefusal::HeaderNumbersMismatch {
                hash: row_hash,
                expected: expected_number,
                found: mdbx_number,
            },
        ));
    }

    Ok(ForwardHealOutcome::Verified(Box::new(
        VerifiedForwardHeal {
            header,
            hash: row_hash,
            block_number: expected_number,
        },
    )))
}

/// Second step: re-commit `heal`'s row onto the Headers segment — whose `.conf` `factory.
/// check_consistency()` (already called by this point) truncated back down to `heal.block_number -
/// 1` via its own `NippyJarChecker::ensure_consistency` — and require the datadir be fully
/// consistent afterward. Never called except right after `detect_and_verify_forward_heal` returned
/// `Verified` and `factory.check_consistency()` reported the matching static-file unwind target.
pub fn commit_forward_heal(
    factory: &ProviderFactory<RethTypes>,
    heal: &VerifiedForwardHeal,
) -> Result<(), ExecutorError> {
    let static_file_provider = factory.static_file_provider();
    {
        let mut writer = static_file_provider
            .latest_writer(StaticFileSegment::Headers)
            .map_err(|e| backend(e.to_string()))?;
        let next = writer.next_block_number();
        if next != heal.block_number {
            return Err(backend(format!(
                "forward heal expected the Headers static file's next block to be {}, found {} \
                 — refusing to append",
                heal.block_number, next
            )));
        }
        writer
            .append_header(&heal.header, &heal.hash)
            .map_err(|e| backend(e.to_string()))?;
        writer.commit().map_err(|e| backend(e.to_string()))?;
        // `writer` (a `StaticFileProviderRWRefMut`) must be dropped before `check_consistency` runs
        // again below — manager.rs's own `check_consistency` doc: "WARNING: No static file writer
        // should be held before calling this function, otherwise it will deadlock."
    }

    let (rocksdb_unwind, static_file_unwind) = factory
        .check_consistency()
        .map_err(|e| backend(e.to_string()))?;
    if rocksdb_unwind.is_some() || static_file_unwind.is_some() {
        return Err(backend(format!(
            "forward heal to block {} left the datadir inconsistent \
             (rocksdb_unwind={rocksdb_unwind:?}, static_file_unwind={static_file_unwind:?}) — \
             refusing to trust it",
            heal.block_number
        )));
    }
    Ok(())
}
