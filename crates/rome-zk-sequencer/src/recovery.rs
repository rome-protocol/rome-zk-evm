//! Startup recovery: replay the ordered log into a fresh executor and derive where sealing resumes.
//!
//! The requirement: "on start, replay the log into the Executor (mock) and assert the rebuilt head equals the
//! head recorded in the last complete record; the binary refuses to start ... if the log is torn beyond
//! the last sealed block boundary and `--truncate-torn` is not set."

use std::path::{Path, PathBuf};

use alloy::primitives::Address;

use crate::executor::{BlockSealInputs, Executor};
use crate::log::{self, LogError as LogRecoveryError};
use crate::profile::ProfileIdentity;
use crate::sealer::ResumePoint;
use crate::signing::recover_header_signer;
use rome_zk_profile::{ProfileJsonError, StoredProfileJson};

/// Filename of the profile-identity file persisted beside the ordered log.
/// Re-exported from `rome-zk-profile`, which now owns the constant.
pub use rome_zk_profile::PROFILE_JSON_FILENAME;

#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error(transparent)]
    Log(#[from] LogRecoveryError),
    #[error(
        "log is torn at {segment:?} offset {offset} and --truncate-torn was not set — refusing to start"
    )]
    TornLogRefusingStart {
        segment: std::path::PathBuf,
        offset: u64,
    },
    #[error("executor error replaying sub-block: {0}")]
    Replay(String),
    /// Replay recomputed `field` from the executor's own outcome for this record and it does not match
    /// what the log says was sealed — the log is not trusted blind.
    #[error(
        "replay diverged at block {block} index {index}: {field} logged {logged} but recomputed {recomputed} \
         — refusing to start"
    )]
    ReplayDiverged {
        block: u64,
        index: u16,
        field: &'static str,
        logged: String,
        recomputed: String,
    },
    /// The record at `block`/`index` is signed by a key other than the sequencer this node is configured
    /// with — a log built (or tampered with) by a foreign key is refused, not silently replayed.
    #[error(
        "log record at block {block} index {index} is signed by a foreign key — refusing to start"
    )]
    ForeignSigner { block: u64, index: u16 },
    /// Tail-only replay: the executor reports `last_persisted_block() == Some(p)` —
    /// its own durable storage already reflects sequencer block `p` — but the log's own records never
    /// show block `p` fully closed (20 sub-block records). The database claims to be ahead of the
    /// log, which is supposed to be authoritative ("recomputable from log + Solana; not a
    /// source of truth") — this can only mean the log itself is missing records the executor's
    /// storage already committed, a genuine corruption/mismatch this node refuses to paper over by
    /// trusting the database's word for state the log cannot corroborate.
    #[error(
        "executor reports block {persisted_block} already persisted, but the log's own records \
         never show it fully closed (highest fully-closed block seen: {log_closed_through:?}) — \
         refusing to start"
    )]
    LogShorterThanPersistedHead {
        persisted_block: u64,
        log_closed_through: Option<u64>,
    },
    /// The log's own block/index shape does not fit the
    /// `sub_blocks_per_block` this replay was configured with — either a record's own `index` is at or
    /// past the configured bound, or a block-number change lands on a boundary the configured profile
    /// never produces (the previous record's `index + 1` does not equal `sub_blocks_per_block`). A log
    /// written under one profile (e.g. 20 sub-blocks/block) replayed under a different one (e.g. 40)
    /// must be refused with the configured value named, not silently produce a divergent head. Persisting
    /// the profile beside the log, so a mismatch is caught before any replay runs, is what `profile.json`
    /// does (`reconcile_profile_identity` / `ProfileJsonMismatch`). This check remains the fail-closed
    /// backstop for a log whose `profile.json` was bypassed.
    #[error(
        "log record at block {block} index {index} does not fit the configured profile \
         (sub_blocks_per_block = {sub_blocks_per_block}) — refusing to start"
    )]
    ProfileMismatch {
        block: u64,
        index: u16,
        sub_blocks_per_block: u16,
    },
    /// The log directory already has
    /// existing sub-block records but no `profile.json` beside it — this log predates the
    /// profile-identity check and this node cannot know what chain/profile it was actually written
    /// under. Refused rather than silently assuming today's config always described it; the one-time
    /// migration (`--write-profile-json`) performs that assumption explicitly, once, at operator
    /// request.
    #[error(
        "log at {log_dir:?} has existing records but no {PROFILE_JSON_FILENAME} beside it — refusing to \
         start; run once with --write-profile-json to record today's config as this log's identity"
    )]
    ProfileJsonMissing { log_dir: PathBuf },
    /// The log directory's `profile.json` exists and predates `blocks_per_batch` (it
    /// has every other field but not this one) — written under an earlier binary, before this field
    /// joined the persisted identity. Every other field already agrees with `configured`
    /// (a disagreement on any of *those* is still reported as `ProfileJsonMismatch`, named, ahead of
    /// this check) — refused unless `--write-profile-json` (the same one-time migration flag)
    /// is passed again, which rewrites the file with `blocks_per_batch` added from today's
    /// configured profile. Never silently defaulted or assumed equal.
    #[error(
        "log's {PROFILE_JSON_FILENAME} at {log_dir:?} predates blocks_per_batch — refusing \
         to start; run once more with --write-profile-json to add it from today's configured profile"
    )]
    ProfileJsonMissingBlocksPerBatch { log_dir: PathBuf },
    /// The sequencer's `profile.json` exists
    /// and predates `first_block` (it has `blocks_per_batch` and every other field, but not this one) —
    /// written by an earlier binary whose ordered log is numbered from block 0, not 1. Every other field
    /// already agrees with `configured` (a disagreement on any of *those* is still reported as that named
    /// `ProfileJsonMismatch`, ahead of this check) — refused unless `--write-profile-json` (the same
    /// one-time migration flag) is passed again. This is the cheapest place to
    /// catch an old, 0-based log: before `reconcile_profile_identity` even returns, let alone before
    /// `replay_into_executor` walks a single record — see [`RecoveryError::LogNumbering`] for the
    /// backstop that still fires on the replay path if this check is ever bypassed (e.g. a fresh log dir
    /// with no `profile.json` at all).
    #[error(
        "log's {PROFILE_JSON_FILENAME} at {log_dir:?} predates first_block — this log \
         was written under 0-based block numbering and cannot be replayed by this binary; refusing to \
         start. A 0-based log is incompatible, not migratable — bring up a fresh chain (fresh genesis, \
         fresh log, fresh profile.json), never `--write-profile-json` over this one"
    )]
    ProfileJsonMissingFirstBlock { log_dir: PathBuf },
    /// The ordered log's very first
    /// record is not `(block 1, index 0)` — this can only mean the log was written by an earlier
    /// sequencer under 0-based numbering (design block 0 sealed as the log's first record) rather than
    /// the current numbering-from-1 premise. Valid to check unconditionally against the
    /// log's first record, never a later one: the ordered log is never pruned,
    /// so the log's first record is always genuinely the chain's first-ever sealed block, not a
    /// survivor of some rotation that could legitimately start elsewhere. This is the backstop
    /// [`reconcile_profile_identity`]'s `first_block` check exists to make
    /// redundant in the common case — but this one runs even when `profile.json` was somehow bypassed
    /// (a fresh log dir wiped along with its profile.json, say), so the premise is unconstructable at
    /// every entry point, not just the fast one.
    #[error(
        "the ordered log's first record is (block {first_block}, index {first_index}), not (block 1, \
         index 0) — this log was written under 0-based numbering by a pre-C.4d sequencer and cannot be \
         replayed by this binary; refusing to start"
    )]
    LogNumbering { first_block: u64, first_index: u16 },
    /// The log's persisted `profile.json`
    /// disagrees with the configured profile (chain id, cadence, or gas shape — after every env
    /// override `Config::load` already applied) on `field`. A restart under a changed `[profile]`, or a
    /// config accidentally pointed at a different chain's log, is refused here — before any replay work
    /// touches the log — rather than discovered later as a divergent head or a `ProfileMismatch` deep
    /// into replay.
    #[error(
        "log's {PROFILE_JSON_FILENAME} disagrees with the configured profile: {field} stored={stored} \
         configured={configured} — refusing to start"
    )]
    ProfileJsonMismatch {
        field: &'static str,
        stored: String,
        configured: String,
    },
    #[error("{path:?}: {source}")]
    ProfileJsonIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing {path:?}: {source}")]
    ProfileJsonParse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// Reconcile the ordered log's persisted
/// chain identity with `configured` (today's profile, after every env override `Config::load` already
/// applied) — called before any replay work touches the log.
///
/// - A genuinely fresh log directory (no records yet — this node's very first start against it) gets
///   `profile.json` written from `configured` right here, so every later restart has something to check
///   against.
/// - An existing log (has records) with no `profile.json` predates this check: refused, unless
///   `allow_migration` (the binary's `--write-profile-json` flag) is set, in which case `configured` is
///   written now as a one-time migration.
/// - An existing `profile.json` is loaded and compared field-by-field against `configured`
///   ([`ProfileIdentity::first_mismatch`]); any disagreement refuses with the field, stored, and
///   configured values named.
/// - An existing `profile.json` that has every field EXCEPT `blocks_per_batch` (a file
///   written before that field joined the persisted identity) is refused
///   ([`RecoveryError::ProfileJsonMissingBlocksPerBatch`]) unless `allow_migration` is set, in which case
///   the file is rewritten from `configured` (adding `blocks_per_batch`) exactly as the no-file case does.
///   Every other field is still compared first — a genuine disagreement on, say, `chain_id` is reported as
///   that mismatch, never masked as "just an old-format file".
pub fn reconcile_profile_identity(
    log_dir: &Path,
    configured: ProfileIdentity,
    allow_migration: bool,
) -> Result<(), RecoveryError> {
    match rome_zk_profile::read_profile_json(log_dir) {
        Ok(StoredProfileJson::V0(legacy)) => {
            // An old-format profile.json, written before `blocks_per_batch` existed.
            // Compare every OTHER field against `configured` first (forcing `blocks_per_batch` to agree
            // so it can never itself trip `first_mismatch`) — a real disagreement elsewhere still
            // surfaces as that named field, not swallowed by the old-format branch.
            let stored_as_current = ProfileIdentity {
                chain_id: legacy.chain_id,
                sub_block_ms: legacy.sub_block_ms,
                sub_blocks_per_block: legacy.sub_blocks_per_block,
                sub_block_gas_limit: legacy.sub_block_gas_limit,
                block_gas_limit: legacy.block_gas_limit,
                blocks_per_batch: configured.blocks_per_batch,
                first_block: configured.first_block,
            };
            if let Some((field, stored_v, configured_v)) =
                stored_as_current.first_mismatch(&configured)
            {
                return Err(RecoveryError::ProfileJsonMismatch {
                    field,
                    stored: stored_v,
                    configured: configured_v,
                });
            }
            if !allow_migration {
                return Err(RecoveryError::ProfileJsonMissingBlocksPerBatch {
                    log_dir: log_dir.to_path_buf(),
                });
            }
            // This branch has no `has_existing_records` gate of its own — the
            // numbering-origin check is what makes the stamp unconstructable over a real 0-based log.
            log_numbering_origin(log_dir)?;
            write_profile_identity(log_dir, configured)
        }
        Ok(StoredProfileJson::V1(legacy)) => {
            // A profile.json written before `first_block` joined the persisted
            // identity (it has `blocks_per_batch` — the V0 classification above already ruled out the
            // older shape without it — but not this field). Compare every OTHER field first, exactly as the
            // blocks_per_batch branch does, so a genuine disagreement (say, chain_id) is still reported
            // as that named mismatch.
            let stored_as_current = ProfileIdentity {
                chain_id: legacy.chain_id,
                sub_block_ms: legacy.sub_block_ms,
                sub_blocks_per_block: legacy.sub_blocks_per_block,
                sub_block_gas_limit: legacy.sub_block_gas_limit,
                block_gas_limit: legacy.block_gas_limit,
                blocks_per_batch: legacy.blocks_per_batch,
                first_block: configured.first_block,
            };
            if let Some((field, stored_v, configured_v)) =
                stored_as_current.first_mismatch(&configured)
            {
                return Err(RecoveryError::ProfileJsonMismatch {
                    field,
                    stored: stored_v,
                    configured: configured_v,
                });
            }
            // Unlike `blocks_per_batch` (a harmless metadata gap over an otherwise
            // compatible log), a profile.json missing `first_block` beside a log that already has
            // sealed records means that log was genuinely written 0-based by an earlier binary — the
            // records themselves, not just this file, disagree with today's numbering. `--write-
            // profile-json` may only assert a fact that was simply never recorded about an empty log
            // (nothing sealed yet under this profile.json); it can never paper over real 0-based
            // history. See the operator's deploy notes ("chain RESET, not an in-place migration").
            let has_existing_records = log_dir_has_existing_records(log_dir).map_err(|source| {
                RecoveryError::ProfileJsonIo {
                    path: log_dir.to_path_buf(),
                    source,
                }
            })?;
            if has_existing_records || !allow_migration {
                return Err(RecoveryError::ProfileJsonMissingFirstBlock {
                    log_dir: log_dir.to_path_buf(),
                });
            }
            // `has_existing_records` above already rules out a log with any
            // records at all — this call is the same numbering-origin check every other write goes
            // through, kept here so the premise is enforced identically everywhere, not by this
            // branch's own (necessarily coarser) has-records gate alone.
            log_numbering_origin(log_dir)?;
            write_profile_identity(log_dir, configured)
        }
        Ok(StoredProfileJson::Current(stored)) => {
            if let Some((field, stored_v, configured_v)) = stored.first_mismatch(&configured) {
                return Err(RecoveryError::ProfileJsonMismatch {
                    field,
                    stored: stored_v,
                    configured: configured_v,
                });
            }
            Ok(())
        }
        Ok(StoredProfileJson::Missing) => {
            if log_dir_has_existing_records(log_dir).map_err(|source| {
                RecoveryError::ProfileJsonIo {
                    path: log_dir.to_path_buf(),
                    source,
                }
            })? && !allow_migration
            {
                return Err(RecoveryError::ProfileJsonMissing {
                    log_dir: log_dir.to_path_buf(),
                });
            }
            // The no-file branch, exactly like the V0 and V1 migration branches
            // above, must not stamp `first_block: 1` over a directory whose existing records are
            // genuinely 0-based.
            log_numbering_origin(log_dir)?;
            write_profile_identity(log_dir, configured)
        }
        Err(ProfileJsonError::Io { path, source }) => {
            Err(RecoveryError::ProfileJsonIo { path, source })
        }
        Err(ProfileJsonError::Parse { path, source }) => {
            Err(RecoveryError::ProfileJsonParse { path, source })
        }
    }
}

/// Anything in `log_dir` other than `profile.json` itself is an existing log segment — see
/// `LogWriter::open`/`log::replay`'s own `fs::read_dir` discovery, which this mirrors read-only.
fn log_dir_has_existing_records(log_dir: &Path) -> std::io::Result<bool> {
    if !log_dir.exists() {
        return Ok(false);
    }
    for entry in std::fs::read_dir(log_dir)? {
        if entry?.file_name() != std::ffi::OsStr::new(PROFILE_JSON_FILENAME) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The single check for the log's numbering origin, reused by every caller that must
/// refuse a 0-based log — [`reconcile_profile_identity`] before every `profile.json` write
/// over a directory with existing records (the V0, V1 and no-file branches alike), [`replay_into_executor`]
/// before any record is replayed, and `rome-zk-batcher::anchor::resolve_anchor` before an anchor is
/// resolved, independent of what `profile.json` says. Reads only the log's first record (via
/// [`log::LogReader`], not a full replay) — `Ok(())` on an empty or nonexistent log directory (nothing
/// sealed there yet), [`RecoveryError::LogNumbering`] naming the actual first record unless it is
/// exactly `(block 1, index 0)`. Valid unconditionally, not just "usually true": the ordered log is
/// never pruned, so whatever this log's first record actually is, is genuinely the
/// chain's first-ever sealed block — there is no legitimate scenario where a healthy log's first record
/// is anything but `(1, 0)`.
///
/// The check itself (open the log, read its first record, compare) lives in `rome_zk_log::log_numbering_origin`
/// — this crate's own logic, moved wholesale so the log crate and everyone who reads it (this crate
/// included) shares one implementation. This function is the thin wrapper every existing caller already
/// resolves to `rome_zk_sequencer::recovery::log_numbering_origin` for: it maps the log crate's own error
/// onto this crate's [`RecoveryError::LogNumbering`], so no caller (including `rome-zk-batcher::anchor`,
/// which pattern-matches this crate's concrete error type) needs to change.
pub fn log_numbering_origin(log_dir: &Path) -> Result<(), RecoveryError> {
    rome_zk_log::log_numbering_origin(log_dir).map_err(|e| match e {
        rome_zk_log::LogNumberingError::Io(io) => RecoveryError::Log(LogRecoveryError::Io(io)),
        rome_zk_log::LogNumberingError::WrongOrigin {
            first_block,
            first_index,
        } => RecoveryError::LogNumbering {
            first_block,
            first_index,
        },
    })
}

/// Thin wrapper over [`rome_zk_profile::write_profile_identity`] (the crate's pure file
/// layer) that maps its error type to this crate's own [`RecoveryError`].
fn write_profile_identity(log_dir: &Path, identity: ProfileIdentity) -> Result<(), RecoveryError> {
    rome_zk_profile::write_profile_identity(log_dir, &identity).map_err(|e| match e {
        ProfileJsonError::Io { path, source } => RecoveryError::ProfileJsonIo { path, source },
        ProfileJsonError::Parse { path, source } => {
            RecoveryError::ProfileJsonParse { path, source }
        }
    })
}

/// Replay every complete sub-block record in `log_dir` into `executor`, in order, closing a block every
/// 20th sub-block exactly as the live sealer would. Every record's signature must recover to
/// `expected_signer`, and every record's `tx_root`/`receipts_root`/`gas_used` are recomputed from the
/// executor's own outcome and compared to what was logged — replay verifies the log, it does not trust it.
/// Returns where sealing should resume.
///
/// Tail-only replay. `executor.last_persisted_block()` is read exactly once, before the
/// loop below starts, and never re-derived mid-replay (see that method's doc: it is a fact about what
/// was durable at *this call's* startup). Records for a sequencer block at or below that value are
/// **not** re-executed — no `open_block`/`execute_sub_block`/`seal_block` call reaches the executor for
/// them, only this function's own log-derived bookkeeping (the timestamp-monotonicity trackers, the
/// per-block hash/gas accumulators) runs, so the state those calls would need to agree with (an already
/// up-to-date `factory.latest()`) is never disturbed. The signer check still runs for every record
/// regardless (cheap, and the log's own integrity is worth checking either way); the
/// tx_root/receipts_root/gas_used cross-check against the executor's outcome is **not** performed for a
/// skipped record — there is nothing to recompute without executing, and the executor's own database
/// already reflects it deterministically. If the executor claims a persisted block the log's own records
/// never actually show fully closed, that is a genuine log/database divergence and this function refuses
/// to start ([`RecoveryError::LogShorterThanPersistedHead`]) rather than silently trusting the database's
/// word for state the log cannot corroborate.
pub async fn replay_into_executor<E: Executor>(
    log_dir: &Path,
    executor: &mut E,
    expected_signer: Address,
    truncate_torn: bool,
    // Replay must open each block with the identical `BlockEnv` the live
    // sealer opened it with — `gas_limit` is never logged per record (it is a chain-config constant, not
    // block-specific data), so it must be supplied here the same way the live sealer's own config
    // supplies it.
    block_gas_limit: u64,
    // This chain's own fee recipient (genesis `coinbase`) — same reasoning
    // as `block_gas_limit` above: never logged per record, supplied the same way the live sealer's own
    // config supplies it.
    fee_recipient: Address,
    // The chain's profile-declared sub-blocks-per-block — replay must close a block at
    // the identical boundary the live sealer used, never the compile-time default.
    sub_blocks_per_block: u16,
) -> Result<ResumePoint, RecoveryError> {
    // The log's very first
    // record must be (block 1, index 0) — the numbering-from-1 premise, checked here via
    // the same [`log_numbering_origin`] every other caller uses, as the backstop that catches a
    // 0-based log even when `reconcile_profile_identity`'s own `first_block` check was somehow
    // bypassed (this crate's binary always calls that first, but this function has no way to enforce
    // call order on every caller/test). Checked before the full replay below so a 0-based log is
    // refused without walking every record first.
    log_numbering_origin(log_dir)?;

    let mut records = Vec::new();
    let torn = log::replay(log_dir, truncate_torn, |r| records.push(r.clone()))?;
    if let Some(tail) = torn {
        return Err(RecoveryError::TornLogRefusingStart {
            segment: tail.segment,
            offset: tail.offset,
        });
    }

    // Mirrors exactly the live sealer's own accumulators: a crash
    // mid-block must not lose the current block's first timestamp, accumulated sub-block hashes, or gas
    // — and the two monotonicity trackers must also survive, since neither is itself logged
    // per record (`prev_block_timestamp_secs` in particular is never persisted anywhere; it must be
    // rederived by replaying with the exact same formula the live sealer used, or the recomputed block
    // outcome would silently diverge from the one the live sealer actually produced).
    let mut sub_block_hashes_in_block = Vec::new();
    let mut gas_in_block = 0u64;
    let mut first_ts_in_block = 0u64;
    let mut prev_sub_block_ts_us = 0u64;
    let mut prev_block_timestamp_secs = 0u64;

    // Captured once, before the loop — see this function's doc.
    let already_persisted = executor.last_persisted_block();
    // The highest sequencer block number the log's OWN records show fully closed so far — used only
    // to prove, at the end, that the log actually corroborates whatever `already_persisted` claims
    // (see [`RecoveryError::LogShorterThanPersistedHead`]).
    let mut log_closed_through: Option<u64> = None;
    // The previous record's own (block, index) — used to check that
    // a block-number change lands exactly on this profile's boundary (`prev_index + 1 ==
    // sub_blocks_per_block`), never re-derived from anything the log itself doesn't say.
    let mut prev_record: Option<(u64, u16)> = None;

    for record in &records {
        let block = record.header.block;
        let index = record.header.index;

        // A record's own index must fit inside the configured profile's block — checked before any other
        // work on this record, so an out-of-range index never reaches the block-sealing arithmetic below
        // (which would otherwise silently produce a divergent head).
        if index >= sub_blocks_per_block {
            return Err(RecoveryError::ProfileMismatch {
                block,
                index,
                sub_blocks_per_block,
            });
        }
        // Whenever the block number changes, the PREVIOUS
        // record must have been that block's own last sub-block under the configured profile — a log
        // written at a different `sub_blocks_per_block` (e.g. 20/block) rolls its block number over at
        // the wrong index for a replay configured with a different value (e.g. 40/block), and that
        // mismatch must be caught here, before replay treats the new record as a fresh block 0.
        if let Some((prev_block, prev_index)) = prev_record {
            if block != prev_block && prev_index + 1 != sub_blocks_per_block {
                return Err(RecoveryError::ProfileMismatch {
                    block: prev_block,
                    index: prev_index,
                    sub_blocks_per_block,
                });
            }
        }
        prev_record = Some((block, index));

        let recovered = recover_header_signer(record.header.signing_hash(), &record.signature)
            .map_err(|e| RecoveryError::Replay(format!("recover signer: {e}")))?;
        if recovered != expected_signer {
            return Err(RecoveryError::ForeignSigner { block, index });
        }

        // A record for a block the executor's own durable storage already reflects is
        // never re-executed — only this function's log-derived bookkeeping below runs for it.
        let skip = already_persisted.is_some_and(|p| block <= p);

        if index == 0 {
            first_ts_in_block = record.header.timestamp_us;
        }
        // Computed identically whether this
        // record is skipped or replayed, since it depends only on log-derived inputs — this is what
        // keeps the monotonicity trackers correct across the skip/replay boundary, so the first
        // REAL `open_block` call past the persisted prefix opens with the exact env the live sealer
        // used.
        let block_timestamp_secs = crate::sealer::resolve_block_timestamp_secs(
            first_ts_in_block / 1_000_000,
            prev_block_timestamp_secs,
        );

        if index == 0 && !skip {
            let env = crate::executor::BlockEnv {
                number: block,
                timestamp_secs: block_timestamp_secs,
                gas_limit: block_gas_limit,
                // This chain's own fee recipient (genesis `coinbase`).
                coinbase: fee_recipient,
                // A function of (chain_id, number) only — see the
                // same note in `sealer.rs`.
                prev_randao: crate::executor::prev_randao(record.header.chain_id, block),
                base_fee: None,
                // The deposits this block credited, as the live sealer logged them on its index-0 record: replay credits
                // the same list, so the block rebuilds with the same withdrawals_root and state.
                withdrawals: record.withdrawals.clone(),
            };
            executor
                .open_block(env)
                .await
                .map_err(|e| RecoveryError::Replay(e.to_string()))?;
        }

        if !skip {
            // Unbounded: this record already holds only the txs the live sealer actually reached
            // (`not_executed` txs are never logged for the
            // sub-block that couldn't reach them), so replay must not reapply a cutoff a second time.
            let outcome = executor
                .execute_sub_block(&record.txs, crate::executor::SubBlockLimits::unbounded())
                .await
                .map_err(|e| RecoveryError::Replay(e.to_string()))?;

            let recomputed_tx_root = crate::merkle::root(&outcome.included);
            if recomputed_tx_root != record.header.tx_root {
                return Err(RecoveryError::ReplayDiverged {
                    block,
                    index,
                    field: "tx_root",
                    logged: format!("{:#x}", record.header.tx_root),
                    recomputed: format!("{recomputed_tx_root:#x}"),
                });
            }
            if outcome.receipts_root != record.header.receipts_root {
                return Err(RecoveryError::ReplayDiverged {
                    block,
                    index,
                    field: "receipts_root",
                    logged: format!("{:#x}", record.header.receipts_root),
                    recomputed: format!("{:#x}", outcome.receipts_root),
                });
            }
            if outcome.gas_used != record.header.gas_used {
                return Err(RecoveryError::ReplayDiverged {
                    block,
                    index,
                    field: "gas_used",
                    logged: record.header.gas_used.to_string(),
                    recomputed: outcome.gas_used.to_string(),
                });
            }
            // The record is the block — every tx in
            // `record.txs` is supposed to be exactly `outcome.included`, so replaying it must never
            // produce a rejection. `tx_root`/`receipts_root`/`gas_used` alone cannot catch a rejected
            // tx riding along in the record (none of the three cover a rejected tx either way), so
            // this is a dedicated check: a logged tx that fails on re-execution is divergence, not a
            // skip (the strict policy), never silently accepted just because the other
            // three fields still happen to match.
            if !outcome.rejected.is_empty() {
                return Err(RecoveryError::ReplayDiverged {
                    block,
                    index,
                    field: "rejected",
                    logged: "0".to_string(),
                    recomputed: outcome.rejected.len().to_string(),
                });
            }
        }

        let header_hash = record.header.hash();
        sub_block_hashes_in_block.push(header_hash);
        gas_in_block += record.header.gas_used;
        prev_sub_block_ts_us = record.header.timestamp_us;

        if record.header.index + 1 == sub_blocks_per_block {
            if !skip {
                let inputs = BlockSealInputs {
                    block: record.header.block,
                    timestamp_secs: block_timestamp_secs,
                    sub_block_header_hashes: std::mem::take(&mut sub_block_hashes_in_block),
                    total_gas_used: gas_in_block,
                };
                executor
                    .seal_block(inputs)
                    .await
                    .map_err(|e| RecoveryError::Replay(e.to_string()))?;
            } else {
                sub_block_hashes_in_block.clear();
            }
            log_closed_through = Some(record.header.block);
            prev_block_timestamp_secs = block_timestamp_secs;
            gas_in_block = 0;
            first_ts_in_block = 0;
        }
    }

    // The log must corroborate whatever the executor's own storage claims is durable —
    // blocks are sealed in contiguous, increasing order (the live sealer only ever increments
    // `next_block` by 1), so the log closing any block at or above `p` proves it also closed block
    // `p` itself somewhere earlier in the sequence; if the log's own highest closed block never
    // reaches `p`, the database is ahead of a log that is supposed to be authoritative.
    if let Some(p) = already_persisted {
        if log_closed_through.is_none_or(|c| c < p) {
            return Err(RecoveryError::LogShorterThanPersistedHead {
                persisted_block: p,
                log_closed_through,
            });
        }
    }

    let resume = match records.last() {
        None => ResumePoint::default(),
        Some(r) if r.header.index + 1 == sub_blocks_per_block => crate::sealer::ResumePoint {
            next_block: r.header.block + 1,
            next_index: 0,
            prev_header_hash: r.header.hash(),
            first_timestamp_us_in_block: 0,
            sub_block_header_hashes: Vec::new(),
            gas_in_block: 0,
            prev_sub_block_ts_us,
            prev_block_timestamp_secs,
        },
        Some(r) => crate::sealer::ResumePoint {
            next_block: r.header.block,
            next_index: r.header.index + 1,
            prev_header_hash: r.header.hash(),
            first_timestamp_us_in_block: first_ts_in_block,
            sub_block_header_hashes: sub_block_hashes_in_block,
            gas_in_block,
            prev_sub_block_ts_us,
            prev_block_timestamp_secs,
        },
    };
    Ok(resume)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{BlockEnv, MockExecutor};
    use crate::preconf::ChannelSink;
    use crate::sealer::SealerState;
    use crate::testutil::signed_raw_tx;
    use alloy::signers::local::PrivateKeySigner;
    use tempfile::tempdir;

    /// The core invariant: run a sequence of sub-blocks against one executor writing to a log, note its
    /// head; independently replay that same log into a brand-new executor; the two heads must match
    /// exactly. This is what "crash → replay → identical head" means in practice — the log, not the
    /// in-memory executor, is what a restart trusts.
    #[tokio::test]
    async fn replay_reproduces_the_identical_head_as_the_live_run() {
        let dir = tempdir().unwrap();
        let signer = PrivateKeySigner::random();
        let sequencer_key = PrivateKeySigner::random();
        let sequencer_address = sequencer_key.address();

        let mut live = SealerState::new(
            MockExecutor::new(),
            log::LogWriter::open(dir.path(), 1_000).unwrap(),
            sequencer_key,
            ChannelSink::new(16),
            1,
            crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            crate::sealer::SUB_BLOCKS_PER_BLOCK,
            crate::sealer::ResumePoint::default(),
        )
        // This test is about replay/head reproduction, not idle behaviour — a nonzero
        // interval (the minimum legal one at this profile's 1s block time) keeps every sub-block below
        // sealing exactly as it did before this knob existed, with no other change to this test.
        .with_empty_block_interval_secs(1);

        // Run 45 sub-blocks (two full blocks + a partial third) with a couple of real txs threaded
        // through, then capture the live head.
        let mut nonce = 0u64;
        for i in 0..45u64 {
            let txs = if i % 3 == 0 {
                let tx = signed_raw_tx(&signer, 1, nonce);
                nonce += 1;
                vec![tx]
            } else {
                vec![]
            };
            live.seal_sub_block(
                txs,
                1_757_000_000_000_000 + i * 50_000,
                crate::executor::SubBlockLimits::unbounded(),
            )
            .await
            .unwrap();
        }
        let live_head = live.executor.head();

        let mut replayed_executor = MockExecutor::new();
        let resume = replay_into_executor(
            dir.path(),
            &mut replayed_executor,
            sequencer_address,
            false,
            crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            crate::sealer::SUB_BLOCKS_PER_BLOCK,
        )
        .await
        .unwrap();

        assert_eq!(
            replayed_executor.head(),
            live_head,
            "replay must reproduce the exact live head"
        );
        assert_eq!(
            resume.next_block, 3,
            "a fresh chain's first sealed block is 1: 45 sub-blocks = blocks 1 and 2 \
             full, 5 into block 3"
        );
        assert_eq!(resume.next_index, 5);
    }

    /// Extends the test above with a genuine idle gap — the log has
    /// no idle records at all (an idle tick writes nothing), so replay must reproduce the identical live
    /// head from a log with GAPS in wall-clock time between its sealed blocks, never depending on how
    /// many ticks fired in between.
    #[tokio::test]
    async fn replay_after_idle_reproduces_the_live_head() {
        let dir = tempdir().unwrap();
        let signer = PrivateKeySigner::random();
        let sequencer_key = PrivateKeySigner::random();
        let sequencer_address = sequencer_key.address();

        let mut live = SealerState::new(
            MockExecutor::new(),
            log::LogWriter::open(dir.path(), 1_000).unwrap(),
            sequencer_key,
            ChannelSink::new(16),
            1,
            crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            crate::sealer::SUB_BLOCKS_PER_BLOCK,
            crate::sealer::ResumePoint::default(),
        ); // empty_block_interval_secs stays 0 — never seal a block with no transactions.

        let base_ts = 1_757_000_000_000_000u64;
        // Block 1: one real tx opens it, the rest of its sub-blocks are empty (an open block always
        // completes, per contract).
        let tx1 = signed_raw_tx(&signer, 1, 0);
        live.seal_sub_block(
            vec![tx1],
            base_ts,
            crate::executor::SubBlockLimits::unbounded(),
        )
        .await
        .unwrap()
        .sealed();
        for i in 1..20u64 {
            live.seal_sub_block(
                vec![],
                base_ts + i * 50_000,
                crate::executor::SubBlockLimits::unbounded(),
            )
            .await
            .unwrap()
            .sealed();
        }

        // A real idle gap: 200 ticks (10s of wall-clock time) with no transactions — every one of these
        // must be `Tick::Idle` and write nothing to the log.
        for i in 0..200u64 {
            let now_us = base_ts + 20 * 50_000 + i * 50_000;
            let tick = live
                .seal_sub_block(vec![], now_us, crate::executor::SubBlockLimits::unbounded())
                .await
                .unwrap();
            assert!(
                tick.is_idle(),
                "tick {i} of the idle gap must write nothing"
            );
        }

        // Block 2 opens after the gap, at the real wall-clock time, and closes normally.
        let tx2 = signed_raw_tx(&signer, 1, 1);
        let after_gap_us = base_ts + 20 * 50_000 + 200 * 50_000;
        live.seal_sub_block(
            vec![tx2],
            after_gap_us,
            crate::executor::SubBlockLimits::unbounded(),
        )
        .await
        .unwrap()
        .sealed();
        for i in 1..20u64 {
            live.seal_sub_block(
                vec![],
                after_gap_us + i * 50_000,
                crate::executor::SubBlockLimits::unbounded(),
            )
            .await
            .unwrap()
            .sealed();
        }

        let live_head = live.executor.head();

        let mut replayed_executor = MockExecutor::new();
        let resume = replay_into_executor(
            dir.path(),
            &mut replayed_executor,
            sequencer_address,
            false,
            crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            crate::sealer::SUB_BLOCKS_PER_BLOCK,
        )
        .await
        .unwrap();

        assert_eq!(
            replayed_executor.head(),
            live_head,
            "replay must reproduce the identical live head even though the live run had a real \
             10s idle gap with no records in it at all"
        );
        assert_eq!(
            resume.next_block, 3,
            "exactly two blocks sealed, none for the idle gap"
        );
        assert_eq!(resume.next_index, 0);

        let mut records = Vec::new();
        let torn = crate::log::replay(dir.path(), false, |r| records.push(r.clone())).unwrap();
        assert!(torn.is_none());
        assert_eq!(
            records.len(),
            2 * crate::sealer::SUB_BLOCKS_PER_BLOCK as usize,
            "the idle gap must not have added any records to the log — exactly two full blocks"
        );
    }

    /// Records every `BlockSealInputs` the executor is asked to seal, so a test can compare block-level
    /// outputs across two independent runs.
    #[derive(Default)]
    struct RecordingExecutor {
        inner: MockExecutor,
        seal_inputs: Vec<BlockSealInputs>,
    }
    impl Executor for RecordingExecutor {
        async fn open_block(
            &mut self,
            env: BlockEnv,
        ) -> Result<(), crate::executor::ExecutorError> {
            self.inner.open_block(env).await
        }
        async fn execute_sub_block(
            &mut self,
            txs: &[alloy::primitives::Bytes],
            limits: crate::executor::SubBlockLimits,
        ) -> Result<crate::executor::SubBlockOutcome, crate::executor::ExecutorError> {
            self.inner.execute_sub_block(txs, limits).await
        }
        async fn seal_block(
            &mut self,
            inputs: BlockSealInputs,
        ) -> Result<crate::executor::BlockOutcome, crate::executor::ExecutorError> {
            self.seal_inputs.push(inputs.clone());
            self.inner.seal_block(inputs).await
        }
        fn head(&self) -> crate::executor::Head {
            self.inner.head()
        }
        fn nonce(&self, addr: alloy::primitives::Address) -> u64 {
            self.inner.nonce(addr)
        }
    }

    /// Resume mid-block must reproduce the uninterrupted run exactly, not
    /// just "start again from zeros". Runs 40 sub-blocks (two full blocks) straight through as the
    /// baseline; separately, crashes (drops, no clean shutdown) right after sealing block 1's index 7 (28
    /// sub-blocks in), replays, and continues to the same 40. Block 1's `BlockSealInputs` — its 20 header
    /// hashes, its gas, its EVM timestamp — must come out byte-identical between the two runs, and the
    /// first sub-block sealed after resume (block 1 index 8) must chain its `prev_hash` to the header hash
    /// the crashed run actually logged for index 7.
    #[tokio::test]
    async fn resume_mid_block_reproduces_the_uninterrupted_run_exactly() {
        let base_ts = 1_757_000_000_000_000u64;
        let sequencer_key = PrivateKeySigner::random();
        let sequencer_address = sequencer_key.address();

        // Baseline: 40 sub-blocks, one continuous run, no crash.
        let live_dir = tempdir().unwrap();
        let mut live = SealerState::new(
            RecordingExecutor::default(),
            log::LogWriter::open(live_dir.path(), 1_000).unwrap(),
            sequencer_key.clone(),
            ChannelSink::new(16),
            1,
            crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            crate::sealer::SUB_BLOCKS_PER_BLOCK,
            ResumePoint::default(),
        )
        .with_empty_block_interval_secs(1);
        for i in 0..40u64 {
            live.seal_sub_block(
                vec![],
                base_ts + i * 50_000,
                crate::executor::SubBlockLimits::unbounded(),
            )
            .await
            .unwrap();
        }
        assert_eq!(live.executor.seal_inputs.len(), 2, "two blocks must close");
        let block2_inputs_live = live.executor.seal_inputs[1].clone();

        // Crashed run: seal only through block 2 index 7 (28 sub-blocks total), then drop without any
        // clean shutdown — simulating a process crash mid-block. (A fresh chain's first sealed block
        // is design block 1, so 20 sub-blocks close block 1 and the remaining 8 are
        // block 2's indices 0..7.)
        let crash_dir = tempdir().unwrap();
        let mut logged_index7_hash = None;
        {
            let mut crashy = SealerState::new(
                RecordingExecutor::default(),
                log::LogWriter::open(crash_dir.path(), 1_000).unwrap(),
                sequencer_key.clone(),
                ChannelSink::new(16),
                1,
                crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
                Address::ZERO,
                crate::sealer::SUB_BLOCKS_PER_BLOCK,
                ResumePoint::default(),
            )
            .with_empty_block_interval_secs(1);
            for i in 0..28u64 {
                let r = crashy
                    .seal_sub_block(
                        vec![],
                        base_ts + i * 50_000,
                        crate::executor::SubBlockLimits::unbounded(),
                    )
                    .await
                    .unwrap()
                    .sealed();
                if r.header.block == 2 && r.header.index == 7 {
                    logged_index7_hash = Some(r.header_hash);
                }
            }
            // crashy drops here: no explicit close, no final flush beyond the per-record fsync already
            // done inside seal_sub_block.
        }
        let logged_index7_hash =
            logged_index7_hash.expect("index 7 of block 2 must have been sealed");

        // Replay recovers resume state from the log alone.
        let mut resumed_executor = RecordingExecutor::default();
        let resume = replay_into_executor(
            crash_dir.path(),
            &mut resumed_executor,
            sequencer_address,
            false,
            crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            crate::sealer::SUB_BLOCKS_PER_BLOCK,
        )
        .await
        .unwrap();
        assert_eq!(resume.next_block, 2, "must resume inside block 2");
        assert_eq!(resume.next_index, 8, "must resume right after index 7");

        // Continue the resumed sealer to the same overall 40 sub-blocks as the baseline.
        let mut resumed = SealerState::new(
            resumed_executor,
            log::LogWriter::open(crash_dir.path(), 1_000).unwrap(),
            sequencer_key,
            ChannelSink::new(16),
            1,
            crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            crate::sealer::SUB_BLOCKS_PER_BLOCK,
            resume,
        )
        .with_empty_block_interval_secs(1);
        let mut resumed_index8_prev_hash = None;
        for i in 28..40u64 {
            let r = resumed
                .seal_sub_block(
                    vec![],
                    base_ts + i * 50_000,
                    crate::executor::SubBlockLimits::unbounded(),
                )
                .await
                .unwrap()
                .sealed();
            if r.header.block == 2 && r.header.index == 8 {
                resumed_index8_prev_hash = Some(r.header.prev_hash);
            }
        }

        assert_eq!(
            resumed_index8_prev_hash,
            Some(logged_index7_hash),
            "the resumed header for index 8 must chain to the logged index-7 hash"
        );

        let block2_inputs_recovered = resumed
            .executor
            .seal_inputs
            .iter()
            .find(|inputs| inputs.block == 2)
            .cloned()
            .expect("block 2 must close after resuming");
        assert_eq!(
            block2_inputs_recovered, block2_inputs_live,
            "block 2's BlockSealInputs must be byte-identical between the uninterrupted run and the \
             crash-then-resume run"
        );
    }

    #[tokio::test]
    async fn refuses_to_start_on_a_torn_tail_without_truncate_flag() {
        let dir = tempdir().unwrap();
        let sequencer_key = PrivateKeySigner::random();
        let sequencer_address = sequencer_key.address();
        let mut live = SealerState::new(
            MockExecutor::new(),
            log::LogWriter::open(dir.path(), 1_000).unwrap(),
            sequencer_key,
            ChannelSink::new(16),
            1,
            crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            crate::sealer::SUB_BLOCKS_PER_BLOCK,
            crate::sealer::ResumePoint::default(),
        )
        .with_empty_block_interval_secs(1);
        live.seal_sub_block(
            vec![],
            1_757_000_000_000_000,
            crate::executor::SubBlockLimits::unbounded(),
        )
        .await
        .unwrap();
        drop(live);

        // Truncate the last record so it is torn.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        let path = entries[0].path();
        let len = std::fs::metadata(&path).unwrap().len();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(len - 2)
            .unwrap();

        let mut fresh = MockExecutor::new();
        let err = replay_into_executor(
            dir.path(),
            &mut fresh,
            sequencer_address,
            false,
            crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            crate::sealer::SUB_BLOCKS_PER_BLOCK,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RecoveryError::TornLogRefusingStart { .. }));
    }

    /// An executor whose `execute_sub_block` returns the real outcome from a real `MockExecutor`, but with
    /// `gas_used` bumped by 1 afterward — `tx_root` and `receipts_root` are untouched (computed from the
    /// real, unperturbed outcome), so this isolates a `gas_used`-only divergence between what replay
    /// recomputes and what the log says was sealed.
    #[derive(Default)]
    struct GasPerturbingExecutor {
        inner: MockExecutor,
    }
    impl Executor for GasPerturbingExecutor {
        async fn open_block(
            &mut self,
            env: BlockEnv,
        ) -> Result<(), crate::executor::ExecutorError> {
            self.inner.open_block(env).await
        }
        async fn execute_sub_block(
            &mut self,
            txs: &[alloy::primitives::Bytes],
            limits: crate::executor::SubBlockLimits,
        ) -> Result<crate::executor::SubBlockOutcome, crate::executor::ExecutorError> {
            let mut outcome = self.inner.execute_sub_block(txs, limits).await?;
            outcome.gas_used += 1;
            Ok(outcome)
        }
        async fn seal_block(
            &mut self,
            inputs: BlockSealInputs,
        ) -> Result<crate::executor::BlockOutcome, crate::executor::ExecutorError> {
            self.inner.seal_block(inputs).await
        }
        fn head(&self) -> crate::executor::Head {
            self.inner.head()
        }
        fn nonce(&self, addr: alloy::primitives::Address) -> u64 {
            self.inner.nonce(addr)
        }
    }

    /// Replay must **verify**, not trust — it recomputes `tx_root`,
    /// `receipts_root` and `gas_used` from the executor's own outcome for every replayed record and
    /// compares them to what the log says was sealed. A divergence (here: an executor whose replayed
    /// `gas_used` doesn't match what was actually logged) must fail closed, never silently accept the
    /// executor's word over the signed log.
    #[tokio::test]
    async fn replay_gas_used_divergence_is_refused_fail_closed() {
        let dir = tempdir().unwrap();
        let sequencer_key = PrivateKeySigner::random();
        let sequencer_address = sequencer_key.address();
        let mut live = SealerState::new(
            MockExecutor::new(),
            log::LogWriter::open(dir.path(), 1_000).unwrap(),
            sequencer_key,
            ChannelSink::new(16),
            1,
            crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            crate::sealer::SUB_BLOCKS_PER_BLOCK,
            ResumePoint::default(),
        )
        .with_empty_block_interval_secs(1);
        live.seal_sub_block(
            vec![],
            1_757_000_000_000_000,
            crate::executor::SubBlockLimits::unbounded(),
        )
        .await
        .unwrap();
        drop(live);

        let mut perturbing = GasPerturbingExecutor::default();
        let err = replay_into_executor(
            dir.path(),
            &mut perturbing,
            sequencer_address,
            false,
            crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            crate::sealer::SUB_BLOCKS_PER_BLOCK,
        )
        .await
        .unwrap_err();
        match err {
            RecoveryError::ReplayDiverged {
                field,
                block,
                index,
                ..
            } => {
                assert_eq!(field, "gas_used");
                assert_eq!(block, 1);
                assert_eq!(index, 0);
            }
            other => panic!("expected ReplayDiverged{{field: \"gas_used\", ..}}, got {other:?}"),
        }
    }

    /// Found while building rome-zk-batcher: a record
    /// whose `txs` field holds a tx the executor rejects on replay is a genuine divergence from "the
    /// record is the block" — it must be refused, not silently accepted just because
    /// `tx_root`/`receipts_root`/`gas_used` still happen to match (they can: `MockExecutor` only folds
    /// *included* hashes into those, so a rejected tx riding along in `txs` is invisible to all three).
    /// This hand-builds the record an older sealer would have written — `txs = [a, b, c]` with `b` a
    /// wrong-nonce tx — to test `replay_into_executor`'s own check in isolation, independent of what
    /// `sealer.rs` writes today.
    #[tokio::test]
    async fn replay_refuses_when_a_logged_tx_is_rejected_on_replay() {
        use crate::header::SubBlockHeader;
        use crate::signing::sign_header;
        use alloy::primitives::B256;

        let dir = tempdir().unwrap();
        let sequencer_key = PrivateKeySigner::random();
        let sequencer_address = sequencer_key.address();

        let sender_a = PrivateKeySigner::random();
        let sender_b = PrivateKeySigner::random();
        let sender_c = PrivateKeySigner::random();
        let tx_a = signed_raw_tx(&sender_a, 1, 0);
        let tx_b = signed_raw_tx(&sender_b, 1, 5); // wrong nonce -> rejected on replay
        let tx_c = signed_raw_tx(&sender_c, 1, 0);

        // What the sealer computes this record's header fields from today: the real included set
        // [a, c] alone — `tx_root`/`receipts_root`/`gas_used` never covered `b`, whichever sealer wrote
        // the record, since `MockExecutor` only folds included hashes into either.
        let mut probe = MockExecutor::new();
        probe
            .open_block(BlockEnv {
                // The ordered log starts at block 1 — genesis 0 is never sealed.
                number: 1,
                timestamp_secs: 1_757_000_000,
                gas_limit: crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
                coinbase: alloy::primitives::Address::ZERO,
                prev_randao: crate::executor::prev_randao(1, 1),
                base_fee: None,
                withdrawals: vec![],
            })
            .await
            .unwrap();
        let probe_outcome = probe
            .execute_sub_block(
                &[tx_a.clone(), tx_c.clone()],
                crate::executor::SubBlockLimits::unbounded(),
            )
            .await
            .unwrap();
        assert!(
            probe_outcome.rejected.is_empty(),
            "sanity: a and c both admit clean"
        );

        let header = SubBlockHeader {
            chain_id: 1,
            block: 1,
            index: 0,
            timestamp_us: 1_757_000_000_000_000,
            tx_root: crate::merkle::root(&probe_outcome.included),
            receipts_root: probe_outcome.receipts_root,
            gas_used: probe_outcome.gas_used,
            prev_hash: B256::ZERO,
            deposits_end: None,
        };
        let signature = sign_header(&sequencer_key, &header);

        // Hand-craft the on-disk record exactly as an older sealer would have written it: `txs`
        // holds the whole attempted superset `[a, b, c]`, not just the included `[a, c]` the header's
        // own fields already commit to.
        let mut writer = log::LogWriter::open(dir.path(), 1_000).unwrap();
        writer
            .append(&header, &signature, &[tx_a, tx_b, tx_c])
            .unwrap();
        drop(writer);

        let mut fresh = MockExecutor::new();
        let err = replay_into_executor(
            dir.path(),
            &mut fresh,
            sequencer_address,
            false,
            crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            crate::sealer::SUB_BLOCKS_PER_BLOCK,
        )
        .await
        .unwrap_err();
        match err {
            RecoveryError::ReplayDiverged {
                field,
                block,
                index,
                ..
            } => {
                assert_eq!(field, "rejected");
                assert_eq!(block, 1);
                assert_eq!(index, 0);
            }
            other => panic!("expected ReplayDiverged{{field: \"rejected\", ..}}, got {other:?}"),
        }
    }

    /// A log whose records are signed by a key other than the sequencer
    /// this node is configured with must be refused, not silently replayed as if it were trusted.
    #[tokio::test]
    async fn replay_refuses_a_log_signed_by_a_foreign_key() {
        let dir = tempdir().unwrap();
        let real_key = PrivateKeySigner::random();
        let mut live = SealerState::new(
            MockExecutor::new(),
            log::LogWriter::open(dir.path(), 1_000).unwrap(),
            real_key,
            ChannelSink::new(16),
            1,
            crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            crate::sealer::SUB_BLOCKS_PER_BLOCK,
            ResumePoint::default(),
        )
        .with_empty_block_interval_secs(1);
        live.seal_sub_block(
            vec![],
            1_757_000_000_000_000,
            crate::executor::SubBlockLimits::unbounded(),
        )
        .await
        .unwrap();
        drop(live);

        let foreign_address = PrivateKeySigner::random().address();
        let mut fresh = MockExecutor::new();
        let err = replay_into_executor(
            dir.path(),
            &mut fresh,
            foreign_address,
            false,
            crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            crate::sealer::SUB_BLOCKS_PER_BLOCK,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RecoveryError::ForeignSigner { .. }));
    }

    /// A log genuinely sealed at 20 sub-blocks/block (2 full blocks + a 5-sub-block partial third = 45
    /// records) replayed with a *different* configured profile (40/block) must be refused with
    /// `ProfileMismatch` naming the configured value — never silently produce a divergent head (before this
    /// check existed, a mock-executor run returned `Ok` with a divergent head).
    #[tokio::test]
    async fn replay_under_a_different_profile_than_the_log_was_written_at_is_refused_20_then_40() {
        let dir = tempdir().unwrap();
        let sequencer_key = PrivateKeySigner::random();
        let sequencer_address = sequencer_key.address();

        let mut live = SealerState::new(
            MockExecutor::new(),
            log::LogWriter::open(dir.path(), 1_000).unwrap(),
            sequencer_key,
            ChannelSink::new(16),
            1,
            crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            20, // written at 20 sub-blocks/block
            ResumePoint::default(),
        )
        .with_empty_block_interval_secs(1);
        for i in 0..45u64 {
            live.seal_sub_block(
                vec![],
                1_757_000_000_000_000 + i * 50_000,
                crate::executor::SubBlockLimits::unbounded(),
            )
            .await
            .unwrap();
        }
        drop(live);

        let mut fresh = MockExecutor::new();
        let err = replay_into_executor(
            dir.path(),
            &mut fresh,
            sequencer_address,
            false,
            crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            40, // replayed as if the profile said 40/block
        )
        .await
        .unwrap_err();
        match err {
            RecoveryError::ProfileMismatch {
                sub_blocks_per_block,
                ..
            } => assert_eq!(sub_blocks_per_block, 40),
            other => {
                panic!("expected ProfileMismatch{{sub_blocks_per_block: 40, ..}}, got {other:?}")
            }
        }
    }

    /// The reverse mismatch — a log genuinely sealed at 40 sub-blocks/block (2 full blocks + a 5-sub-block
    /// partial third = 85 records) replayed with a configured profile of 20/block must also be refused with
    /// `ProfileMismatch`, caught here by the direct `index >= sub_blocks_per_block` bound (index 20 of block
    /// 1 — a fresh chain's first sealed block — is already out of range for a 20-per-block profile), not by
    /// the block-boundary check.
    #[tokio::test]
    async fn replay_under_a_different_profile_than_the_log_was_written_at_is_refused_40_then_20() {
        let dir = tempdir().unwrap();
        let sequencer_key = PrivateKeySigner::random();
        let sequencer_address = sequencer_key.address();

        let mut live = SealerState::new(
            MockExecutor::new(),
            log::LogWriter::open(dir.path(), 1_000).unwrap(),
            sequencer_key,
            ChannelSink::new(16),
            1,
            crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            40, // written at 40 sub-blocks/block
            ResumePoint::default(),
        )
        .with_empty_block_interval_secs(1);
        for i in 0..85u64 {
            live.seal_sub_block(
                vec![],
                1_757_000_000_000_000 + i * 25_000,
                crate::executor::SubBlockLimits::unbounded(),
            )
            .await
            .unwrap();
        }
        drop(live);

        let mut fresh = MockExecutor::new();
        let err = replay_into_executor(
            dir.path(),
            &mut fresh,
            sequencer_address,
            false,
            crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            20, // replayed as if the profile said 20/block
        )
        .await
        .unwrap_err();
        match err {
            RecoveryError::ProfileMismatch {
                block,
                index,
                sub_blocks_per_block,
            } => {
                assert_eq!(sub_blocks_per_block, 20);
                assert_eq!(block, 1);
                assert_eq!(
                    index, 20,
                    "caught at the first out-of-range index, not a boundary check"
                );
            }
            other => panic!("expected ProfileMismatch{{block: 1, index: 20, ..}}, got {other:?}"),
        }
    }

    fn identity(chain_id: u64) -> ProfileIdentity {
        ProfileIdentity::new(chain_id, &crate::profile::Profile::default())
    }

    /// A genuinely fresh log directory (nothing written to it yet) gets
    /// `profile.json` written from the configured identity, so every later restart has something to
    /// check against.
    #[test]
    fn a_fresh_log_dir_gets_profile_json_written_from_the_configured_identity() {
        let dir = tempdir().unwrap();
        let configured = identity(200_101);
        reconcile_profile_identity(dir.path(), configured, false).unwrap();

        let path = dir.path().join(PROFILE_JSON_FILENAME);
        assert!(path.exists());
        let stored: ProfileIdentity =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(stored, configured);
    }

    /// An existing `profile.json` that agrees with the configured identity in every field is accepted
    /// (and left untouched).
    #[test]
    fn a_matching_profile_json_is_accepted() {
        let dir = tempdir().unwrap();
        let configured = identity(200_101);
        reconcile_profile_identity(dir.path(), configured, false).unwrap();
        // Second start, same config: must still succeed (not "already exists" refused).
        reconcile_profile_identity(dir.path(), configured, false).unwrap();
    }

    /// A restart whose configured profile
    /// disagrees with the log's stored identity on the gas shape (a changed `sub_block_gas_limit`
    /// implies a changed `block_gas_limit` too) is refused, naming the first field that differs — not
    /// silently re-executing the unpersisted tail under a different `BlockEnv.gas_limit` than the era the
    /// pre-confirmations were signed in. Disabling the `first_mismatch` comparison in
    /// `reconcile_profile_identity` turns this test red (it would return `Ok` instead).
    #[test]
    fn a_restart_under_a_changed_gas_limit_is_refused_naming_the_field() {
        let dir = tempdir().unwrap();
        let original = ProfileIdentity {
            sub_block_gas_limit: 5_000_000,
            block_gas_limit: 100_000_000,
            ..identity(200_101)
        };
        reconcile_profile_identity(dir.path(), original, false).unwrap();

        let changed = ProfileIdentity {
            sub_block_gas_limit: 2_000_000,
            block_gas_limit: 40_000_000,
            ..identity(200_101)
        };
        let err = reconcile_profile_identity(dir.path(), changed, false).unwrap_err();
        match err {
            RecoveryError::ProfileJsonMismatch {
                field,
                stored,
                configured,
            } => {
                assert_eq!(field, "sub_block_gas_limit");
                assert_eq!(stored, "5000000");
                assert_eq!(configured, "2000000");
            }
            other => panic!(
                "expected ProfileJsonMismatch{{field: \"sub_block_gas_limit\", ..}}, got {other:?}"
            ),
        }
    }

    /// A log written under chain 1 must be refused
    /// when replayed under a config declaring chain 2 — the chain_id check "rides the same comparison"
    /// as the profile.json identity check, rather than needing its own per-record mechanism.
    /// Disabling the `chain_id` branch inside `ProfileIdentity::first_mismatch` turns
    /// this test red (a chain_id-only mismatch would fall through every other equal field and return
    /// `None`, and `reconcile_profile_identity` would return `Ok`).
    #[test]
    fn a_chain_1_log_replayed_under_chain_2_is_refused() {
        let dir = tempdir().unwrap();
        reconcile_profile_identity(dir.path(), identity(1), false).unwrap();

        let err = reconcile_profile_identity(dir.path(), identity(2), false).unwrap_err();
        match err {
            RecoveryError::ProfileJsonMismatch {
                field,
                stored,
                configured,
            } => {
                assert_eq!(field, "chain_id");
                assert_eq!(stored, "1");
                assert_eq!(configured, "2");
            }
            other => {
                panic!("expected ProfileJsonMismatch{{field: \"chain_id\", ..}}, got {other:?}")
            }
        }
    }

    /// An existing log (has real sub-block records) but no `profile.json` predates this check — refused
    /// by default, naming the migration flag.
    #[tokio::test]
    async fn an_existing_log_without_profile_json_is_refused_without_the_migration_flag() {
        let dir = tempdir().unwrap();
        let sequencer_key = PrivateKeySigner::random();
        let mut live = SealerState::new(
            MockExecutor::new(),
            log::LogWriter::open(dir.path(), 1_000).unwrap(),
            sequencer_key,
            ChannelSink::new(16),
            1,
            crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            crate::sealer::SUB_BLOCKS_PER_BLOCK,
            ResumePoint::default(),
        )
        .with_empty_block_interval_secs(1);
        live.seal_sub_block(
            vec![],
            1_757_000_000_000_000,
            crate::executor::SubBlockLimits::unbounded(),
        )
        .await
        .unwrap();
        drop(live);

        let err = reconcile_profile_identity(dir.path(), identity(1), false).unwrap_err();
        assert!(matches!(err, RecoveryError::ProfileJsonMissing { .. }));
        assert!(!dir.path().join(PROFILE_JSON_FILENAME).exists());
    }

    /// `--write-profile-json` (`allow_migration = true`) performs the one-time migration: an existing
    /// log with no `profile.json` gets one written from the configured identity instead of being
    /// refused.
    #[tokio::test]
    async fn an_existing_log_without_profile_json_migrates_when_allowed() {
        let dir = tempdir().unwrap();
        let sequencer_key = PrivateKeySigner::random();
        let mut live = SealerState::new(
            MockExecutor::new(),
            log::LogWriter::open(dir.path(), 1_000).unwrap(),
            sequencer_key,
            ChannelSink::new(16),
            1,
            crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            crate::sealer::SUB_BLOCKS_PER_BLOCK,
            ResumePoint::default(),
        )
        .with_empty_block_interval_secs(1);
        live.seal_sub_block(
            vec![],
            1_757_000_000_000_000,
            crate::executor::SubBlockLimits::unbounded(),
        )
        .await
        .unwrap();
        drop(live);

        let configured = identity(1);
        reconcile_profile_identity(dir.path(), configured, true).unwrap();
        let stored: ProfileIdentity = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join(PROFILE_JSON_FILENAME)).unwrap(),
        )
        .unwrap();
        assert_eq!(stored, configured);
    }

    /// A restart whose configured `blocks_per_batch` disagrees with
    /// the stored identity is refused, naming `blocks_per_batch` specifically.
    #[test]
    fn a_restart_under_a_changed_blocks_per_batch_is_refused_naming_the_field() {
        let dir = tempdir().unwrap();
        let original = identity(200_101);
        assert_eq!(original.blocks_per_batch, 10);
        reconcile_profile_identity(dir.path(), original, false).unwrap();

        let changed = ProfileIdentity {
            blocks_per_batch: 5,
            ..original
        };
        let err = reconcile_profile_identity(dir.path(), changed, false).unwrap_err();
        match err {
            RecoveryError::ProfileJsonMismatch {
                field,
                stored,
                configured,
            } => {
                assert_eq!(field, "blocks_per_batch");
                assert_eq!(stored, "10");
                assert_eq!(configured, "5");
            }
            other => panic!(
                "expected ProfileJsonMismatch{{field: \"blocks_per_batch\", ..}}, got {other:?}"
            ),
        }
    }

    /// Writes a `profile.json` in the shape that predates `blocks_per_batch` (every field `ProfileIdentity` had
    /// before `blocks_per_batch` joined it) — used to prove the old-format migration path without
    /// depending on any earlier binary's actual output.
    fn write_legacy_profile_json(dir: &Path, identity: ProfileIdentity) {
        let legacy = serde_json::json!({
            "chain_id": identity.chain_id,
            "sub_block_ms": identity.sub_block_ms,
            "sub_blocks_per_block": identity.sub_blocks_per_block,
            "sub_block_gas_limit": identity.sub_block_gas_limit,
            "block_gas_limit": identity.block_gas_limit,
        });
        std::fs::write(
            dir.join(PROFILE_JSON_FILENAME),
            serde_json::to_string_pretty(&legacy).unwrap(),
        )
        .unwrap();
    }

    /// An old-format `profile.json` — every field
    /// `ProfileIdentity` had before `blocks_per_batch` joined it, and nothing else — is refused without
    /// `--write-profile-json`, naming the migration flag, not silently defaulted or accepted as a match.
    #[test]
    fn an_old_format_profile_json_without_blocks_per_batch_is_refused_without_the_flag() {
        let dir = tempdir().unwrap();
        let configured = identity(200_101);
        write_legacy_profile_json(dir.path(), configured);

        let err = reconcile_profile_identity(dir.path(), configured, false).unwrap_err();
        assert!(
            matches!(err, RecoveryError::ProfileJsonMissingBlocksPerBatch { .. }),
            "{err:?}"
        );
        // Refused, so the file on disk is untouched — still the old format, not silently upgraded.
        let raw = std::fs::read_to_string(dir.path().join(PROFILE_JSON_FILENAME)).unwrap();
        assert!(!raw.contains("blocks_per_batch"));
    }

    /// `--write-profile-json` upgrades an old-format
    /// `profile.json` by rewriting it from the configured identity, adding `blocks_per_batch`.
    #[test]
    fn an_old_format_profile_json_migrates_when_allowed() {
        let dir = tempdir().unwrap();
        let configured = identity(200_101);
        write_legacy_profile_json(dir.path(), configured);

        reconcile_profile_identity(dir.path(), configured, true).unwrap();
        let stored: ProfileIdentity = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join(PROFILE_JSON_FILENAME)).unwrap(),
        )
        .unwrap();
        assert_eq!(stored, configured);
        assert_eq!(stored.blocks_per_batch, configured.blocks_per_batch);
    }

    /// An old-format file whose OTHER fields already disagree with `configured` is still reported as
    /// that named mismatch — never swallowed into "just needs the migration flag".
    #[test]
    fn an_old_format_profile_json_with_a_real_mismatch_is_refused_naming_that_field_not_the_migration(
    ) {
        let dir = tempdir().unwrap();
        let original = identity(1);
        write_legacy_profile_json(dir.path(), original);

        let err = reconcile_profile_identity(dir.path(), identity(2), true).unwrap_err();
        match err {
            RecoveryError::ProfileJsonMismatch { field, .. } => assert_eq!(field, "chain_id"),
            other => {
                panic!("expected ProfileJsonMismatch{{field: \"chain_id\", ..}}, got {other:?}")
            }
        }
    }

    /// The same real-mismatch guard as the test above, but with
    /// `allow_migration = false` — the normal operator start. The previous test alone cannot see a V0
    /// branch whose two checks were swapped (missing-field refusal moved above `first_mismatch`) because
    /// it passes `allow_migration = true`, where the missing-field refusal never fires in either order.
    /// With the flag off, a swapped V0 branch would report "needs --write-profile-json"
    /// (`ProfileJsonMissingBlocksPerBatch`) for an old-format file with a wrong `chain_id`, instead of the
    /// `chain_id` mismatch the migration path must still report. Moving the
    /// `!allow_migration` check above `first_mismatch` in the V0 branch turns this test red.
    #[test]
    fn an_old_format_profile_json_with_a_real_mismatch_is_refused_naming_that_field_even_without_the_flag(
    ) {
        let dir = tempdir().unwrap();
        let original = identity(1);
        write_legacy_profile_json(dir.path(), original);

        let err = reconcile_profile_identity(dir.path(), identity(2), false).unwrap_err();
        match err {
            RecoveryError::ProfileJsonMismatch { field, .. } => assert_eq!(field, "chain_id"),
            other => {
                panic!("expected ProfileJsonMismatch{{field: \"chain_id\", ..}}, got {other:?}")
            }
        }
    }

    /// The V0 migration branch (old-format `profile.json`, no `has_existing_records` gate of its
    /// own) must not stamp `first_block: 1` over a directory whose log was genuinely sealed 0-based by
    /// the real sealer — `--write-profile-json` may only assert a fact never recorded about an EMPTY
    /// log, never paper over real 0-based history. Removing
    /// `log_numbering_origin`'s call from this branch (or making the function always return `Ok`) turns
    /// this test green when it must be red.
    #[tokio::test]
    async fn v0_profile_json_over_a_real_0_based_log_with_the_migration_flag_is_refused() {
        let dir = tempdir().unwrap();
        let configured = identity(200_101);
        write_legacy_profile_json(dir.path(), configured);

        // A real 0-based log: an earlier sealer's own fresh-start point.
        let sequencer_key = PrivateKeySigner::random();
        let mut live = SealerState::new(
            MockExecutor::new(),
            log::LogWriter::open(dir.path(), 1_000).unwrap(),
            sequencer_key,
            ChannelSink::new(16),
            configured.chain_id,
            crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            crate::sealer::SUB_BLOCKS_PER_BLOCK,
            crate::sealer::ResumePoint {
                next_block: 0,
                ..crate::sealer::ResumePoint::default()
            },
        )
        .with_empty_block_interval_secs(1);
        live.seal_sub_block(
            vec![],
            1_757_000_000_000_000,
            crate::executor::SubBlockLimits::unbounded(),
        )
        .await
        .unwrap();
        drop(live);

        let err = reconcile_profile_identity(dir.path(), configured, true).unwrap_err();
        assert!(
            matches!(
                err,
                RecoveryError::LogNumbering {
                    first_block: 0,
                    first_index: 0,
                }
            ),
            "the migration flag must not be honoured over a real 0-based log via the V0 branch: {err:?}"
        );
        // Refused, so the file on disk is untouched — still the old format, never stamped first_block.
        let raw = std::fs::read_to_string(dir.path().join(PROFILE_JSON_FILENAME)).unwrap();
        assert!(!raw.contains("first_block"));
    }

    /// The no-file branch (no `profile.json` at all) must equally refuse to stamp `first_block: 1`
    /// over a directory whose log was genuinely sealed 0-based. Making
    /// `log_numbering_origin` always return `Ok` turns this test green when it must be red.
    #[tokio::test]
    async fn no_profile_json_over_a_real_0_based_log_with_the_migration_flag_is_refused() {
        let dir = tempdir().unwrap();
        let configured = identity(200_101);
        // No profile.json written at all — the no-file branch.

        let sequencer_key = PrivateKeySigner::random();
        let mut live = SealerState::new(
            MockExecutor::new(),
            log::LogWriter::open(dir.path(), 1_000).unwrap(),
            sequencer_key,
            ChannelSink::new(16),
            configured.chain_id,
            crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            crate::sealer::SUB_BLOCKS_PER_BLOCK,
            crate::sealer::ResumePoint {
                next_block: 0,
                ..crate::sealer::ResumePoint::default()
            },
        )
        .with_empty_block_interval_secs(1);
        live.seal_sub_block(
            vec![],
            1_757_000_000_000_000,
            crate::executor::SubBlockLimits::unbounded(),
        )
        .await
        .unwrap();
        drop(live);

        let err = reconcile_profile_identity(dir.path(), configured, true).unwrap_err();
        assert!(
            matches!(
                err,
                RecoveryError::LogNumbering {
                    first_block: 0,
                    first_index: 0,
                }
            ),
            "the migration flag must not be honoured over a real 0-based log via the no-file branch: {err:?}"
        );
        assert!(
            !dir.path().join(PROFILE_JSON_FILENAME).exists(),
            "refused, so no profile.json must be written at all"
        );
    }

    /// Control for the two tests above: a genuinely fresh, 1-based log (the real sealer's own default
    /// start point) reconciles and replays cleanly with the migration flag set — the numbering-origin
    /// check refuses a 0-based log, not every log.
    #[tokio::test]
    async fn a_fresh_1_based_log_reconciles_and_replays_ok_with_the_migration_flag() {
        let dir = tempdir().unwrap();
        let configured = identity(200_101);
        write_legacy_profile_json(dir.path(), configured);

        let sequencer_key = PrivateKeySigner::random();
        let sequencer_address = sequencer_key.address();
        let mut live = SealerState::new(
            MockExecutor::new(),
            log::LogWriter::open(dir.path(), 1_000).unwrap(),
            sequencer_key,
            ChannelSink::new(16),
            configured.chain_id,
            crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            crate::sealer::SUB_BLOCKS_PER_BLOCK,
            crate::sealer::ResumePoint::default(),
        )
        .with_empty_block_interval_secs(1);
        for i in 0..(crate::sealer::SUB_BLOCKS_PER_BLOCK as u64) {
            live.seal_sub_block(
                vec![],
                1_757_000_000_000_000 + i * 50_000,
                crate::executor::SubBlockLimits::unbounded(),
            )
            .await
            .unwrap();
        }
        drop(live);

        reconcile_profile_identity(dir.path(), configured, true).unwrap();
        let mut executor = MockExecutor::new();
        let resume = replay_into_executor(
            dir.path(),
            &mut executor,
            sequencer_address,
            false,
            configured.block_gas_limit,
            Address::ZERO,
            configured.sub_blocks_per_block,
        )
        .await
        .unwrap();
        assert_eq!(resume.next_block, 2);
    }

    /// Writes a `profile.json` in the shape an earlier binary produced (every field through `blocks_per_batch`,
    /// nothing after) — used to prove the `first_block` migration path without depending on any earlier
    /// binary's actual output.
    fn write_pre_first_block_profile_json(dir: &Path, identity: ProfileIdentity) {
        let legacy = serde_json::json!({
            "chain_id": identity.chain_id,
            "sub_block_ms": identity.sub_block_ms,
            "sub_blocks_per_block": identity.sub_blocks_per_block,
            "sub_block_gas_limit": identity.sub_block_gas_limit,
            "block_gas_limit": identity.block_gas_limit,
            "blocks_per_batch": identity.blocks_per_batch,
        });
        std::fs::write(
            dir.join(PROFILE_JSON_FILENAME),
            serde_json::to_string_pretty(&legacy).unwrap(),
        )
        .unwrap();
    }

    /// A `profile.json` predating `first_block`, beside an EMPTY log dir
    /// (nothing sealed yet under it), is refused without `--write-profile-json` — naming the field, not
    /// silently accepted as a match.
    #[test]
    fn a_profile_json_without_first_block_is_refused_without_the_flag() {
        let dir = tempdir().unwrap();
        let configured = identity(200_101);
        write_pre_first_block_profile_json(dir.path(), configured);

        let err = reconcile_profile_identity(dir.path(), configured, false).unwrap_err();
        assert!(
            matches!(err, RecoveryError::ProfileJsonMissingFirstBlock { .. }),
            "{err:?}"
        );
        let raw = std::fs::read_to_string(dir.path().join(PROFILE_JSON_FILENAME)).unwrap();
        assert!(!raw.contains("first_block"));
    }

    /// The V1 analogue of the test above: a V1-shaped `profile.json` (has
    /// `blocks_per_batch`, predates `first_block`) whose OTHER fields already disagree with `configured`
    /// must still be refused as that named mismatch, not as "predates first_block" — with
    /// `allow_migration = false`, the normal operator start. Guards the same ordering property as the V0
    /// test above for the V1 branch: `first_mismatch` must run before the `has_existing_records ||
    /// !allow_migration` refusal, never after.
    #[test]
    fn a_profile_json_without_first_block_with_a_real_mismatch_is_refused_naming_that_field_not_missing_first_block(
    ) {
        let dir = tempdir().unwrap();
        write_pre_first_block_profile_json(dir.path(), identity(1));

        let err = reconcile_profile_identity(dir.path(), identity(2), false).unwrap_err();
        match err {
            RecoveryError::ProfileJsonMismatch { field, .. } => assert_eq!(field, "chain_id"),
            other => {
                panic!("expected ProfileJsonMismatch{{field: \"chain_id\", ..}}, got {other:?}")
            }
        }
    }

    /// `--write-profile-json` may add `first_block` to a profile.json whose log
    /// dir has nothing sealed under it yet — there is no 0-based history to falsely paper over.
    #[test]
    fn a_profile_json_without_first_block_migrates_when_allowed_and_the_log_is_empty() {
        let dir = tempdir().unwrap();
        let configured = identity(200_101);
        write_pre_first_block_profile_json(dir.path(), configured);

        reconcile_profile_identity(dir.path(), configured, true).unwrap();
        let stored: ProfileIdentity = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join(PROFILE_JSON_FILENAME)).unwrap(),
        )
        .unwrap();
        assert_eq!(stored, configured);
        assert_eq!(stored.first_block, crate::profile::FIRST_BLOCK);
    }

    /// The actual safety property: a `profile.json` missing
    /// `first_block` beside a log dir that ALREADY has real sealed records is refused even WITH
    /// `--write-profile-json` — the flag may only assert a fact that was never recorded about an empty
    /// log, never paper over a genuinely 0-based log's real history (the operator's deploy notes: "chain
    /// RESET, not an in-place migration"). Removing the `has_existing_records`
    /// check (leaving only `!allow_migration`) turns this test red. This test
    /// additionally asserts the file itself — `profile.json` on disk must still lack `first_block` after
    /// the refusal, proving the V1 branch's `has_existing_records`/`allow_migration` gate runs strictly
    /// *before* any write, never after. Temporarily moving the
    /// `write_profile_identity` call in `reconcile_profile_identity`'s V1 branch ahead of its
    /// `has_existing_records || !allow_migration` gate turns this exact assertion red (the refusal
    /// itself still fires — the write already happened by the time the caller sees the error — so only
    /// the on-disk assertion below catches it).
    #[tokio::test]
    async fn a_profile_json_without_first_block_over_a_real_0_based_log_is_never_migratable() {
        let dir = tempdir().unwrap();
        let configured = identity(200_101);
        write_pre_first_block_profile_json(dir.path(), configured);

        // A real 0-based log: an earlier sealer's own fresh-start point.
        let sequencer_key = PrivateKeySigner::random();
        let mut live = SealerState::new(
            MockExecutor::new(),
            log::LogWriter::open(dir.path(), 1_000).unwrap(),
            sequencer_key,
            ChannelSink::new(16),
            configured.chain_id,
            crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            crate::sealer::SUB_BLOCKS_PER_BLOCK,
            crate::sealer::ResumePoint {
                next_block: 0,
                ..crate::sealer::ResumePoint::default()
            },
        )
        .with_empty_block_interval_secs(1);
        live.seal_sub_block(
            vec![],
            1_757_000_000_000_000,
            crate::executor::SubBlockLimits::unbounded(),
        )
        .await
        .unwrap();
        drop(live);

        let err = reconcile_profile_identity(dir.path(), configured, true).unwrap_err();
        assert!(
            matches!(err, RecoveryError::ProfileJsonMissingFirstBlock { .. }),
            "the migration flag must not be honoured over a log with real 0-based records: {err:?}"
        );
        // The file on disk must still lack `first_block` — the refusal above is
        // not enough on its own to prove the gate ran before any write.
        let raw = std::fs::read_to_string(dir.path().join(PROFILE_JSON_FILENAME)).unwrap();
        assert!(
            !raw.contains("first_block"),
            "profile.json must be untouched by a refused migration, but was: {raw}"
        );
    }

    /// A log sealed from
    /// `ResumePoint { next_block: 0, .. }` (an earlier sequencer's own fresh start) is refused by name
    /// — `replay_into_executor` must never silently replay a 0-based log and keep sealing forward from
    /// wherever it left off. Removing this function's first-record check turns this
    /// test red (replay would instead succeed, resuming at whatever the 0-based log's last record says).
    #[tokio::test]
    async fn a_log_sealed_from_next_block_zero_is_refused_by_name() {
        let dir = tempdir().unwrap();
        let sequencer_key = PrivateKeySigner::random();
        let sequencer_address = sequencer_key.address();

        let mut live = SealerState::new(
            MockExecutor::new(),
            log::LogWriter::open(dir.path(), 1_000).unwrap(),
            sequencer_key,
            ChannelSink::new(16),
            1,
            crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            crate::sealer::SUB_BLOCKS_PER_BLOCK,
            crate::sealer::ResumePoint {
                next_block: 0,
                ..crate::sealer::ResumePoint::default()
            },
        )
        .with_empty_block_interval_secs(1);
        live.seal_sub_block(
            vec![],
            1_757_000_000_000_000,
            crate::executor::SubBlockLimits::unbounded(),
        )
        .await
        .unwrap();
        drop(live);

        let mut executor = MockExecutor::new();
        let err = replay_into_executor(
            dir.path(),
            &mut executor,
            sequencer_address,
            false,
            crate::sealer::DEFAULT_BLOCK_GAS_LIMIT,
            Address::ZERO,
            crate::sealer::SUB_BLOCKS_PER_BLOCK,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                err,
                RecoveryError::LogNumbering {
                    first_block: 0,
                    first_index: 0,
                }
            ),
            "{err:?}"
        );
    }
}
