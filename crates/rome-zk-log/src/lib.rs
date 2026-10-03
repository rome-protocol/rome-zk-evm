//! `rome-zk-log` — the sub-block header and the sequencer's append-only ordered log: the source of truth
//! replayed on restart. The sequencer writes it (re-exported as `rome_zk_sequencer::log` /
//! `rome_zk_sequencer::header` for its own call sites); the batcher, the derivation node and the indexer
//! read it.
//!
//! A signed pre-confirmation is only returned to a sender **after** its sub-block is fsynced here — this file, not
//! the mock (or later, reth's) in-memory state, is what a restart replays.
//!
//! **The record is the block.** A record's `txs` field is exactly the sub-block's *included* transactions, in
//! execution order — the same set whose hashes form the header's `tx_root`. Rejected transactions (bad nonce,
//! insufficient funds, ...) are never persisted here, and a not-yet-reached transaction is carried forward to a
//! later sub-block's own record instead — this module never writes a marker distinguishing the two, because neither
//! ever reaches the log at all. Consequence: a reader (the batcher, a derivation node, the challenger) can treat a
//! record's `txs` as the literal executed block, with nothing further to reconcile (the sequencer's own
//! `sealer::included_raw_txs` is the boundary that builds this list; `recovery::replay_into_executor` is what
//! verifies replay never sees a rejection from a record it trusts).
//!
//! ## Record format
//!
//! One **frame** per sub-block, written with a single `write_all` and `File::sync_data()` before the
//! sequencer acks the sub-block to any sender:
//!
//! ```text
//! frame := total_len:u32(LE)  payload  crc32:u32(LE)
//! payload := header:RLP(SubBlockHeader)  signature:[u8;65]  tx_count:u32(LE)  (tx_len:u32(LE) tx_bytes)*
//!            [ withdrawal_count:u32(LE)  (index:u64(LE) recipient:[u8;20] amount_gwei:u64(LE))* ]
//! ```
//!
//! The bracketed tail is present only on the index-0 record of a block that credits deposits: the block's EIP-4895
//! withdrawals, in order, after its transactions. A record without deposits has no tail, so it is byte for byte the
//! record this format always wrote (the golden test below pins that). A tail is never written for zero withdrawals,
//! and a reader refuses one: there is exactly one encoding of "no withdrawals". The tail goes with the header's
//! `deposits_end` (the header says the queue index after the block, the tail says which deposits make it up): either
//! both are present or neither, and `deposits_end` is one past the last withdrawal's index. Both the writer and the
//! reader enforce that. Each withdrawal is rebuilt on read through `rome_zk_executor_api::deposit_withdrawal`, the one
//! constructor of a deposit's withdrawal, so a logged withdrawal can only ever be a deposit credit.
//!
//! `total_len` covers everything after itself (`payload` + the trailing `crc32`), so a reader knows
//! exactly how many bytes to pull off the file before validating. `header` is decoded with
//! `alloy_rlp::Decodable` directly off the payload slice — RLP is self-length-prefixed, so no extra
//! length field is needed for it. `crc32` (`crc32fast`) covers `payload` only, guarding against a
//! bit-flip that happens to leave `total_len` looking consistent.
//!
//! **Torn-write detection**: on replay, a record is torn if fewer than `total_len` bytes remain in the
//! file, or if the trailing `crc32` does not match. Because every record is fsynced individually before
//! the next one starts, a torn record can only be the very last bytes of the very last segment file — a
//! torn record found anywhere else is `LogError::CorruptMidLog` regardless of `--truncate-torn`.
//!
//! ## Segments
//!
//! One log directory holds `segment-<start_block:012>.log` files, each covering
//! `[start_block, start_block + blocks_per_segment)`. Truncating a torn tail only ever touches the
//! newest (highest-numbered) segment file.

#![forbid(unsafe_code)]

use alloy::primitives::{Address, Bytes, Signature};
use alloy_eips::eip4895::Withdrawal;
use rome_zk_executor_api::deposit_withdrawal;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

mod header;
pub use header::{SubBlockHeader, SIGNING_DOMAIN};

const SEGMENT_PREFIX: &str = "segment-";
const SEGMENT_SUFFIX: &str = ".log";
const SEGMENT_WIDTH: usize = 12;

/// The largest `total_len` replay will ever act on. A `total_len` above
/// this is treated as torn **before** anything is allocated or read for it — a corrupt (or adversarial)
/// length field must never drive an allocation sized by attacker-controlled bytes off disk. No real
/// sub-block gets remotely close to this (the design's whole sub-block, txs included, is bounded far
/// smaller); 32 MiB is a generous ceiling with headroom, not a tuned limit.
pub const MAX_FRAME_LEN: u32 = 32 * 1024 * 1024;

fn segment_path(dir: &Path, start_block: u64) -> PathBuf {
    dir.join(format!(
        "{SEGMENT_PREFIX}{start_block:0width$}{SEGMENT_SUFFIX}",
        width = SEGMENT_WIDTH
    ))
}

fn parse_segment_start(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    let stem = name
        .strip_prefix(SEGMENT_PREFIX)?
        .strip_suffix(SEGMENT_SUFFIX)?;
    stem.parse().ok()
}

/// One fully-formed sub-block record read back from the log.
#[derive(Debug, Clone)]
pub struct SubBlockRecord {
    pub header: SubBlockHeader,
    pub signature: Signature,
    pub txs: Vec<Bytes>,
    /// The block's EIP-4895 withdrawals (the deposits it credits), credited after its transactions. Only ever
    /// non-empty on an index-0 record; empty for every block without deposits. Recovery replays them.
    pub withdrawals: Vec<Withdrawal>,
}

/// The rule that ties a record's withdrawals to its header, shared by the writer and the reader: withdrawals only on
/// an index-0 record; the header's `deposits_end` is present exactly when there are withdrawals, and is one past the
/// last withdrawal's index.
fn check_withdrawals(header: &SubBlockHeader, withdrawals: &[Withdrawal]) -> Result<(), String> {
    match (withdrawals.last(), header.deposits_end) {
        (None, None) => Ok(()),
        (None, Some(end)) => Err(format!(
            "header carries deposits_end {end} but the record has no withdrawals"
        )),
        (Some(_), None) => {
            Err("record has withdrawals but the header carries no deposits_end".into())
        }
        (Some(last), Some(end)) => {
            if header.index != 0 {
                return Err(format!(
                    "withdrawals on sub-block index {} (only index 0 carries a block's withdrawals)",
                    header.index
                ));
            }
            if last.index.checked_add(1) != Some(end) {
                return Err(format!(
                    "deposits_end {end} is not one past the last withdrawal's index {}",
                    last.index
                ));
            }
            Ok(())
        }
    }
}

fn encode_payload(
    header: &SubBlockHeader,
    signature: &Signature,
    txs: &[Bytes],
    withdrawals: &[Withdrawal],
) -> Vec<u8> {
    let mut payload = header.encode_canonical();
    payload.extend_from_slice(&signature.as_bytes());
    payload.extend_from_slice(&(txs.len() as u32).to_le_bytes());
    for tx in txs {
        payload.extend_from_slice(&(tx.len() as u32).to_le_bytes());
        payload.extend_from_slice(tx);
    }
    if !withdrawals.is_empty() {
        payload.extend_from_slice(&(withdrawals.len() as u32).to_le_bytes());
        for w in withdrawals {
            payload.extend_from_slice(&w.index.to_le_bytes());
            payload.extend_from_slice(w.address.as_slice());
            payload.extend_from_slice(&w.amount.to_le_bytes());
        }
    }
    payload
}

/// Bytes one withdrawal takes in a record's tail: `index u64 | recipient [20] | amount_gwei u64`.
const WITHDRAWAL_LEN: usize = 8 + 20 + 8;

fn decode_payload(mut payload: &[u8]) -> io::Result<SubBlockRecord> {
    let header: SubBlockHeader = alloy_rlp::Decodable::decode(&mut payload)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("header rlp: {e}")))?;
    if payload.len() < 65 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "payload too short for signature",
        ));
    }
    let mut sig_bytes = [0u8; 65];
    sig_bytes.copy_from_slice(&payload[..65]);
    let signature = Signature::from_raw_array(&sig_bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("signature: {e}")))?;
    payload = &payload[65..];

    if payload.len() < 4 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "payload too short for tx_count",
        ));
    }
    let tx_count = u32::from_le_bytes(payload[..4].try_into().unwrap());
    payload = &payload[4..];

    let mut txs = Vec::with_capacity(tx_count as usize);
    for _ in 0..tx_count {
        if payload.len() < 4 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "payload too short for tx_len",
            ));
        }
        let tx_len = u32::from_le_bytes(payload[..4].try_into().unwrap()) as usize;
        payload = &payload[4..];
        if payload.len() < tx_len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "payload too short for tx body",
            ));
        }
        txs.push(Bytes::copy_from_slice(&payload[..tx_len]));
        payload = &payload[tx_len..];
    }
    let mut withdrawals = Vec::new();
    if !payload.is_empty() {
        if payload.len() < 4 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "payload too short for withdrawal_count",
            ));
        }
        let count = u32::from_le_bytes(payload[..4].try_into().unwrap()) as usize;
        payload = &payload[4..];
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "withdrawal_count 0 (a block without withdrawals has no tail)",
            ));
        }
        // Checked against the bytes actually present before anything is allocated for it.
        if payload.len() != count.saturating_mul(WITHDRAWAL_LEN) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "withdrawal tail length does not match withdrawal_count",
            ));
        }
        withdrawals.reserve(count);
        for chunk in payload.chunks_exact(WITHDRAWAL_LEN) {
            let index = u64::from_le_bytes(chunk[..8].try_into().unwrap());
            let recipient = Address::from_slice(&chunk[8..28]);
            let amount = u64::from_le_bytes(chunk[28..36].try_into().unwrap());
            withdrawals.push(deposit_withdrawal(index, recipient, amount));
        }
        payload = &[];
    }
    if !payload.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing bytes after last tx",
        ));
    }
    check_withdrawals(&header, &withdrawals)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(SubBlockRecord {
        header,
        signature,
        txs,
        withdrawals,
    })
}

/// Appends sub-block records to the segmented log, fsyncing each one before returning.
pub struct LogWriter {
    dir: PathBuf,
    blocks_per_segment: u64,
    current_segment_start: u64,
    file: File,
}

/// fsync the directory itself so the new segment's directory entry is durable: after creating a new segment file, the
/// file's *data* is only as durable as its own fsync, but the file's *existence* — its directory entry —
/// is a separate write to the containing directory's inode and needs its own fsync, or a crash right
/// after creation can leave the file missing (or present with a torn/zero-length name entry) even though
/// `File::create` returned `Ok`. POSIX practice for "durably create a file": fsync the file, then fsync
/// the directory that names it.
fn fsync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

impl LogWriter {
    /// Open (creating if needed) the log directory and resume appending to its newest segment, or start
    /// segment 0 if the directory is empty.
    pub fn open(dir: impl AsRef<Path>, blocks_per_segment: u64) -> io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        let mut starts: Vec<u64> = fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .filter_map(|e| parse_segment_start(&e.path()))
            .collect();
        starts.sort_unstable();
        let current_segment_start = starts.last().copied().unwrap_or(0);
        let path = segment_path(&dir, current_segment_start);
        let is_new_segment = !path.exists();
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        if is_new_segment {
            fsync_dir(&dir)?;
        }
        Ok(Self {
            dir,
            blocks_per_segment,
            current_segment_start,
            file,
        })
    }

    fn roll_if_needed(&mut self, block: u64) -> io::Result<()> {
        if block >= self.current_segment_start + self.blocks_per_segment {
            let new_start = (block / self.blocks_per_segment) * self.blocks_per_segment;
            let path = segment_path(&self.dir, new_start);
            self.file = OpenOptions::new().create(true).append(true).open(&path)?;
            fsync_dir(&self.dir)?;
            self.current_segment_start = new_start;
        }
        Ok(())
    }

    /// Append one sub-block record and fsync (`File::sync_data`) before returning. The caller must not
    /// ack any pre-confirmation for this sub-block until this returns `Ok`.
    pub fn append(
        &mut self,
        header: &SubBlockHeader,
        signature: &Signature,
        txs: &[Bytes],
    ) -> io::Result<()> {
        self.append_with_withdrawals(header, signature, txs, &[])
    }

    /// [`Self::append`] for the index-0 record of a block that credits deposits: `withdrawals` are the block's
    /// EIP-4895 withdrawals, written after its transactions. With an empty list this writes exactly the bytes
    /// [`Self::append`] writes. Refuses (`InvalidInput`, nothing written) a list that does not agree with the
    /// header's `deposits_end`, or one on a record that is not index 0 (see the module doc).
    pub fn append_with_withdrawals(
        &mut self,
        header: &SubBlockHeader,
        signature: &Signature,
        txs: &[Bytes],
        withdrawals: &[Withdrawal],
    ) -> io::Result<()> {
        check_withdrawals(header, withdrawals)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        self.roll_if_needed(header.block)?;
        let payload = encode_payload(header, signature, txs, withdrawals);
        let crc = crc32fast::hash(&payload);
        let total_len = (payload.len() + 4) as u32;

        let mut frame = Vec::with_capacity(4 + payload.len() + 4);
        frame.extend_from_slice(&total_len.to_le_bytes());
        frame.extend_from_slice(&payload);
        frame.extend_from_slice(&crc.to_le_bytes());

        self.file.write_all(&frame)?;
        self.file.sync_data()?;
        Ok(())
    }
}

/// Where replay stopped and why.
#[derive(Debug)]
pub struct TornTail {
    pub segment: PathBuf,
    /// Byte offset within `segment` where the torn record begins.
    pub offset: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("log I/O error: {0}")]
    Io(#[from] io::Error),
    /// A torn record was found somewhere other than the tail of the newest segment — every record
    /// before it was fsynced individually, so this indicates corruption, not an interrupted append, and
    /// `--truncate-torn` deliberately does not paper over it.
    #[error(
        "log corrupt in {segment:?} at offset {offset}, not at the tail — refusing to truncate"
    )]
    CorruptMidLog { segment: PathBuf, offset: u64 },
}

/// Replays every complete record across all segments, in order, calling `on_record` for each. Stops at
/// the first torn record (necessarily the tail of the newest segment, or it is `CorruptMidLog`).
///
/// When `truncate_torn` is `true` and the torn record is the tail of the newest segment, the segment
/// file is truncated at the torn record's start offset and replay reports success with `torn: None`.
/// When `false`, a torn tail is reported as `torn: Some(..)` and the caller (the binary) decides whether
/// that is a refusal.
pub fn replay(
    dir: impl AsRef<Path>,
    truncate_torn: bool,
    mut on_record: impl FnMut(&SubBlockRecord),
) -> Result<Option<TornTail>, LogError> {
    let dir = dir.as_ref();
    if !dir.exists() {
        return Ok(None);
    }
    let mut segments: Vec<(u64, PathBuf)> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| parse_segment_start(&e.path()).map(|s| (s, e.path())))
        .collect();
    segments.sort_unstable_by_key(|(s, _)| *s);

    for (i, (_, path)) in segments.iter().enumerate() {
        let is_last_segment = i + 1 == segments.len();
        let mut file = File::open(path)?;
        let mut offset: u64 = 0;
        loop {
            // Bulk `replay` and the positional `LogReader` share this one function's one CRC comparison — see
            // [`read_one_record`]'s doc for what each of its outcomes means.
            match read_one_record(&mut file, offset)? {
                ReadOutcome::Record(record, new_offset) => {
                    on_record(&record);
                    offset = new_offset;
                }
                ReadOutcome::NoMoreData => break, // clean end of segment
                ReadOutcome::Incomplete | ReadOutcome::LengthOutOfRange | ReadOutcome::Corrupt => {
                    return handle_torn(path, offset, is_last_segment, truncate_torn);
                }
            }
        }
    }
    Ok(None)
}

/// Read-only cursor over the ordered log, resuming from a given `(block, index)` and able to follow the tail as the
/// sealer keeps appending (the batcher's `source.rs` is this reader's only consumer). Deliberately separate from
/// [`replay`]'s callback shape: the batcher wants to pull records one at a time across repeated polls (a live
/// tail-follow), not receive a single callback burst over the whole log in one call.
///
/// Non-breaking: this is purely additive read access over the same on-disk format `replay` already
/// parses — no change to [`LogWriter`], the frame layout, or the torn-tail rules above.
pub struct LogReader {
    dir: PathBuf,
    /// Segment start blocks not yet opened, ascending order (discovered at construction and on each
    /// [`Self::refresh_segments`] call, so a segment created after the reader started is picked up).
    pending_segments: Vec<u64>,
    /// The segment file currently being read, if any: its start block (so `refresh_segments` never
    /// re-queues the segment already open — re-queuing it would reopen it from offset 0 on the next
    /// rollover and re-yield every record already consumed) and the byte offset consumed so far.
    current: Option<(u64, File, u64)>,
    /// Every segment start already opened and fully moved past (exhausted before rolling to the next
    /// one), so `refresh_segments` never re-queues those either.
    exhausted: std::collections::HashSet<u64>,
    /// One record already pulled off the log by `open()`'s skip-to-position loop but not yet before the
    /// resume point — handed back by the very next `next()` call before any further reads.
    rewound: Option<SubBlockRecord>,
    /// Test-only interleaving hook for the roll race: fires at most once, immediately after `read_one_record`
    /// reports `Incomplete`/`LengthOutOfRange` and before `is_current_the_newest_segment` is checked — the exact
    /// window in which a real roll can complete a record's fsync between this reader's two observations. Lets a
    /// test simulate the writer's actions happening in that window without a second thread.
    #[cfg(test)]
    before_newest_check: Option<Box<dyn FnMut()>>,
}

impl LogReader {
    /// Opens a reader positioned to start yielding the record at `(from_block, from_index)` — i.e. it
    /// skips every record strictly before that position via [`Self::next`], and skips it entirely if
    /// `from_block`/`from_index` is beyond every record currently on disk (a subsequent [`Self::next`]
    /// call — possibly after [`Self::refresh_segments`] once the sealer appends more — returns it).
    pub fn open(dir: impl AsRef<Path>, from_block: u64, from_index: u16) -> io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        let mut reader = Self {
            dir,
            pending_segments: Vec::new(),
            current: None,
            exhausted: std::collections::HashSet::new(),
            rewound: None,
            #[cfg(test)]
            before_newest_check: None,
        };
        reader.refresh_segments()?;
        // Skip every record strictly before (from_block, from_index).
        while let Some(record) = reader.read_next()? {
            if (record.header.block, record.header.index) >= (from_block, from_index) {
                // Rewind by re-seeking: simplest correct way to "un-consume" the one record we peeked
                // past is to reopen from the position recorded before this call — but `next()` already
                // advanced `offset`. Instead, stash it: see `rewound` below.
                reader.rewound = Some(record);
                break;
            }
        }
        Ok(reader)
    }

    /// Re-scans `dir` for segment files not yet queued — call this to pick up a segment the sealer rolled
    /// into after this reader was constructed (a live tail-follow). Segments already open, already queued,
    /// or already fully consumed are untouched.
    pub fn refresh_segments(&mut self) -> io::Result<()> {
        if !self.dir.exists() {
            return Ok(());
        }
        let already_have: std::collections::HashSet<u64> = self
            .pending_segments
            .iter()
            .copied()
            .chain(self.current.as_ref().map(|(start, _, _)| *start))
            .chain(self.exhausted.iter().copied())
            .collect();
        let mut fresh: Vec<u64> = fs::read_dir(&self.dir)?
            .filter_map(|e| e.ok())
            .filter_map(|e| parse_segment_start(&e.path()))
            .filter(|s| !already_have.contains(s))
            .collect();
        fresh.sort_unstable();
        self.pending_segments.extend(fresh);
        self.pending_segments.sort_unstable();
        self.pending_segments.dedup();
        Ok(())
    }

    /// Opens the next queued segment (ascending order) as `self.current`, marking whatever segment was
    /// open before (if any) as permanently exhausted — correct only when called because a *newer*
    /// segment is already known to exist (a real roll happened), never merely because the current segment
    /// is caught up to its own EOF (that segment can still grow further; see [`Self::next`]'s Eof arm,
    /// which does not call this unless `pending_segments` is non-empty). Returns `false` when there is no
    /// next segment queued.
    fn advance_segment(&mut self) -> io::Result<bool> {
        if self.pending_segments.is_empty() {
            return Ok(false);
        }
        if let Some((start, _, _)) = self.current.take() {
            self.exhausted.insert(start);
        }
        let start = self.pending_segments.remove(0);
        let file = File::open(segment_path(&self.dir, start))?;
        self.current = Some((start, file, 0));
        Ok(true)
    }

    /// True if, after picking up any segment rolled in since the last refresh, `self.current` is still the newest
    /// segment on disk — no other segment is queued or already exhausted with a higher start than this one. Only at
    /// this position can a torn record legitimately be an in-progress append rather than corruption — an older,
    /// rolled-past segment can never grow again, so nothing found wrong there was ever going to resolve itself on a
    /// later read.
    fn is_current_the_newest_segment(&mut self) -> io::Result<bool> {
        self.refresh_segments()?;
        Ok(self.pending_segments.is_empty())
    }

    /// Returns the next complete record, or `None` if the reader has caught up to the current end of the
    /// log (call [`Self::refresh_segments`] and retry to follow the tail after the sealer appends more —
    /// or simply call this again once a new segment has rolled in, since a torn/incomplete final record
    /// at the tail of the newest segment is also reported as `None`, not an error: the writer has not
    /// finished fsyncing it yet, which is expected during a live tail-follow, not corruption).
    ///
    /// Catching up to EOF on the currently-open segment does **not** advance away from it while it might
    /// still be the newest (actively-appended) segment: it keeps `self.current` open at the same offset so
    /// the next call re-reads from there and picks up bytes fsynced since. Only once a *newer* segment is
    /// discovered (via [`Self::refresh_segments`], reflected in `pending_segments`) does hitting EOF here
    /// roll the reader onto it.
    ///
    /// A torn record (`Incomplete` or `LengthOutOfRange`) is "wait and retry" — the same live-tail-follow treatment
    /// as `NoMoreData` — only at the tail of the newest segment on disk (see
    /// [`Self::is_current_the_newest_segment`]); found anywhere else (an older, already rolled-past segment, or the
    /// corrupt-length-with-bytes-following shape [`read_one_record`] already promotes straight to `Corrupt`) it is
    /// a hard `InvalidData` error, exactly like a CRC/decode failure — a segment that can never grow again, or a
    /// length field with real data sitting past it, can never explain itself away on a later read the way a
    /// genuinely in-progress append can.
    ///
    /// **Roll race.** The read that produced the torn outcome and the
    /// `is_current_the_newest_segment` check right after it are two separate observations, not one
    /// atomic step: a real roll can complete the record's `fsync` and open the next segment strictly
    /// between them. So a non-newest verdict here re-reads the same offset once more before erroring — a
    /// non-newest segment can never grow, so an identical torn outcome on the second read is conclusive,
    /// while a completed `Record` means exactly this race happened and the record is yielded normally.
    pub fn read_next(&mut self) -> io::Result<Option<SubBlockRecord>> {
        if let Some(record) = self.rewound.take() {
            return Ok(Some(record));
        }
        loop {
            if self.current.is_none() {
                if !self.advance_segment()? {
                    return Ok(None);
                }
                continue;
            }
            let (segment_start, read_offset) = {
                let (start, _, offset) = self.current.as_ref().unwrap();
                (*start, *offset)
            };
            let outcome = {
                let (_, file, offset) = self.current.as_mut().unwrap();
                read_one_record(file, *offset)?
            };
            match outcome {
                ReadOutcome::Record(record, new_offset) => {
                    let (_, _, offset) = self.current.as_mut().unwrap();
                    *offset = new_offset;
                    return Ok(Some(*record));
                }
                ReadOutcome::NoMoreData => {
                    if !self.advance_segment()? {
                        return Ok(None);
                    }
                }
                ReadOutcome::Incomplete | ReadOutcome::LengthOutOfRange => {
                    #[cfg(test)]
                    if let Some(mut hook) = self.before_newest_check.take() {
                        hook();
                    }
                    if self.is_current_the_newest_segment()? {
                        return Ok(None);
                    }
                    // Roll race: the read above and this newest-segment check are not atomic -- a genuine roll can
                    // complete a record's fsync in between them. A non-newest segment can never grow, so re-reading
                    // once more is conclusive: an identical torn outcome proves corruption, while a completed
                    // `Record` means the roll raced this reader, and the record is real.
                    let reread = {
                        let (_, file, _) = self.current.as_mut().unwrap();
                        read_one_record(file, read_offset)?
                    };
                    match reread {
                        ReadOutcome::Record(record, new_offset) => {
                            let (_, _, offset) = self.current.as_mut().unwrap();
                            *offset = new_offset;
                            return Ok(Some(*record));
                        }
                        _ => {
                            let path = segment_path(&self.dir, segment_start);
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!("corrupt record in {path:?} at offset {read_offset}"),
                            ));
                        }
                    }
                }
                ReadOutcome::Corrupt => {
                    let path = segment_path(&self.dir, segment_start);
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("corrupt record in {path:?} at offset {read_offset}"),
                    ));
                }
            }
        }
    }
}

/// Outcome of reading one length-prefixed, CRC-checked frame at a known file offset — shared by [`replay`] (bulk,
/// sequential) and [`LogReader`] (positional, tail-following; one CRC check, exercised by both consumers' own
/// tests) via [`read_one_record`].
enum ReadOutcome {
    /// A complete, valid record; the `u64` is the offset the next record (if any) begins at.
    Record(Box<SubBlockRecord>, u64),
    /// Not one byte was present at this offset — an exact, clean boundary (the readable data simply ends here),
    /// never itself a sign of corruption. Distinct from `Incomplete`: even a 1..=3-byte partial length prefix is
    /// `Incomplete`, not this (see the crate README's "Torn tail vs corruption").
    NoMoreData,
    /// Fewer bytes are present than this record needs, anywhere from a partly-landed length prefix (1..=3 of its 4
    /// bytes on disk, none of the payload) to a length prefix that decoded fine but whose body-and-CRC bytes were
    /// not fully present. `read_exact` only fails these ways when the file genuinely has nothing more past this
    /// point. Under the torn-write model this crate assumes (see the README's "Torn tail vs corruption": a torn
    /// tail is a byte-order prefix of one frame, so a partial write never leaves valid bytes sitting past it) there
    /// is no way to observe this outcome with additional, valid bytes still sitting later in the file.
    Incomplete,
    /// The 4-byte length prefix decoded to a value outside `4..=MAX_FRAME_LEN`, **and** nothing else follows those
    /// 4 bytes in the file. Under the torn-write model (a torn tail is a byte-order prefix of one frame) this is
    /// what a crash mid-`write_all` of the length prefix itself looks like: the writer's real `total_len` is always
    /// in range, so an out-of-range value can only come from a length field that itself never finished landing on
    /// disk, and the model says nothing valid can sit past an incomplete write. A violation of that model (e.g.
    /// zero-fill after a crash) is out of scope — see `Corrupt`'s doc and the README section for what happens then.
    LengthOutOfRange,
    /// The full `total_len` bytes were present but validation failed (CRC mismatch, the payload does
    /// not decode), **or** the length prefix was out of range while more bytes still follow it in the
    /// file. The writer's single `write_all` covers the whole frame including its trailing CRC, so once
    /// every byte the length prefix promises is on disk, a mismatch here cannot be an in-progress write;
    /// bytes surviving past a bogus length prefix are the same tell under the torn-write model (a
    /// torn write can never leave anything after it) — corruption, not a race, in every
    /// segment. If that model itself is violated, this reader is stricter than `replay` at the newest
    /// segment's tail (accepted): the batcher fails loud and the sequencer's
    /// `--truncate-torn` restart is the repair, never a record posted from bytes the CRC did not accept.
    Corrupt,
}

/// Reads one record starting at `offset` in `file`, without disturbing bytes before `offset`. Never
/// allocates based on an unchecked length (same `MAX_FRAME_LEN` guard both callers rely on) — see
/// [`ReadOutcome`]'s doc for what each outcome means to its two callers.
fn read_one_record(file: &mut File, offset: u64) -> io::Result<ReadOutcome> {
    use std::io::Seek;
    file.seek(io::SeekFrom::Start(offset))?;
    let mut len_buf = [0u8; 4];
    match file.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
            // 1..=3 bytes of a length prefix landing here is a torn write (a short read on the prefix itself), not
            // a clean end of segment — only an exact `offset` boundary (nothing at all past it) is the latter.
            let file_len = file.metadata()?.len();
            return Ok(if file_len > offset {
                ReadOutcome::Incomplete
            } else {
                ReadOutcome::NoMoreData
            });
        }
        Err(e) => return Err(e),
    }
    let raw_total_len = u32::from_le_bytes(len_buf);
    if !(4..=MAX_FRAME_LEN).contains(&raw_total_len) {
        // A bogus length prefix is only ever a legitimate in-progress append if the file truly ends right here —
        // anything left over past these 4 bytes proves a writer kept going after them, which a real torn write
        // cannot do.
        let file_len = file.metadata()?.len();
        return Ok(if file_len == offset + 4 {
            ReadOutcome::LengthOutOfRange
        } else {
            ReadOutcome::Corrupt
        });
    }
    let total_len = raw_total_len as usize;
    let mut rest = vec![0u8; total_len];
    if file.read_exact(&mut rest).is_err() {
        return Ok(ReadOutcome::Incomplete);
    }
    let (payload, crc_bytes) = rest.split_at(total_len - 4);
    let expected_crc = u32::from_le_bytes(crc_bytes.try_into().unwrap());
    if crc32fast::hash(payload) != expected_crc {
        return Ok(ReadOutcome::Corrupt);
    }
    match decode_payload(payload) {
        Ok(record) => Ok(ReadOutcome::Record(
            Box::new(record),
            offset + 4 + total_len as u64,
        )),
        Err(_) => Ok(ReadOutcome::Corrupt),
    }
}

fn handle_torn(
    path: &Path,
    offset: u64,
    is_last_segment: bool,
    truncate_torn: bool,
) -> Result<Option<TornTail>, LogError> {
    if !is_last_segment {
        return Err(LogError::CorruptMidLog {
            segment: path.to_path_buf(),
            offset,
        });
    }
    if truncate_torn {
        let file = OpenOptions::new().write(true).open(path)?;
        file.set_len(offset)?;
        Ok(None)
    } else {
        Ok(Some(TornTail {
            segment: path.to_path_buf(),
            offset,
        }))
    }
}

/// The ordered log starts at block 1, index 0 — genesis 0 is never sealed. This is the single check every caller
/// that cares about a log's numbering origin runs against: before writing `profile.json` over a directory that
/// already holds records, before replaying a log into the executor, and before the batcher resolves its posting
/// anchor. `Ok(())` on an empty or nonexistent log directory (nothing sealed yet, so there is no origin to check).
/// A first record at any other `(block, index)` is [`LogNumberingError::WrongOrigin`], never silently accepted or
/// coerced.
#[derive(Debug, thiserror::Error)]
pub enum LogNumberingError {
    #[error("log I/O error: {0}")]
    Io(#[from] io::Error),
    #[error(
        "ordered log must start at (block 1, index 0) — found (block {first_block}, index {first_index})"
    )]
    WrongOrigin { first_block: u64, first_index: u16 },
}

pub fn log_numbering_origin(log_dir: &Path) -> Result<(), LogNumberingError> {
    let mut reader = LogReader::open(log_dir, 0, 0)?;
    if let Some(first) = reader.read_next()? {
        if (first.header.block, first.header.index) != (1, 0) {
            return Err(LogNumberingError::WrongOrigin {
                first_block: first.header.block,
                first_index: first.header.index,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::consensus::{SignableTransaction, TxEip1559, TxEnvelope};
    use alloy::primitives::{TxKind, B256, U256};
    use alloy::signers::local::PrivateKeySigner;
    use alloy::signers::SignerSync;
    use alloy_eips::eip2718::Encodable2718;
    use tempfile::tempdir;

    /// Test-only fixture helper, mirroring the sequencer's own `signing::sign_header` one-liner: signs a
    /// header's domain-separated signing hash (see [`SubBlockHeader::signing_hash`]) — never the bare
    /// header hash.
    fn sign_header(signer: &PrivateKeySigner, header: &SubBlockHeader) -> Signature {
        signer
            .sign_hash_sync(&header.signing_hash())
            .expect("PrivateKeySigner::sign_hash_sync over a prehash never fails")
    }

    /// Test-only fixture helper: an arbitrary, real EIP-1559 signed tx as opaque raw bytes — this crate's
    /// own record format never parses tx contents (see the module doc), so any validly-encoded tx serves
    /// as a fixture.
    fn signed_raw_tx(signer: &PrivateKeySigner, chain_id: u64, nonce: u64) -> Bytes {
        let tx = TxEip1559 {
            chain_id,
            nonce,
            gas_limit: 21_000,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: TxKind::Call(Default::default()),
            value: U256::ZERO,
            access_list: Default::default(),
            input: Bytes::new(),
        };
        let sig_hash = tx.signature_hash();
        let signature = signer.sign_hash_sync(&sig_hash).expect("sign fixture tx");
        Bytes::from(TxEnvelope::from(tx.into_signed(signature)).encoded_2718())
    }

    fn fixture_record(block: u64, index: u16, sender: &PrivateKeySigner) -> SubBlockRecord {
        let header = SubBlockHeader {
            chain_id: 1,
            block,
            index,
            timestamp_us: 1_757_000_000_000_000 + index as u64 * 50_000,
            tx_root: B256::repeat_byte(index as u8 + 1),
            receipts_root: B256::repeat_byte(index as u8 + 2),
            gas_used: 21_000,
            prev_hash: B256::ZERO,
            deposits_end: None,
        };
        let sequencer_key = PrivateKeySigner::random();
        let signature = sign_header(&sequencer_key, &header);
        let tx = signed_raw_tx(sender, 1, index as u64);
        SubBlockRecord {
            header,
            signature,
            txs: vec![tx],
            withdrawals: vec![],
        }
    }

    #[test]
    fn append_fsync_replay_round_trip() {
        let dir = tempdir().unwrap();
        let sender = PrivateKeySigner::random();
        let mut writer = LogWriter::open(dir.path(), 1_000).unwrap();
        let records: Vec<_> = (0..5).map(|i| fixture_record(0, i, &sender)).collect();
        for r in &records {
            writer.append(&r.header, &r.signature, &r.txs).unwrap();
        }

        let mut replayed = Vec::new();
        let torn = replay(dir.path(), false, |r| replayed.push(r.clone())).unwrap();
        assert!(torn.is_none());
        assert_eq!(replayed.len(), 5);
        for (original, back) in records.iter().zip(replayed.iter()) {
            assert_eq!(original.header, back.header);
            assert_eq!(original.signature, back.signature);
            assert_eq!(original.txs, back.txs);
        }
    }

    #[test]
    fn segments_roll_at_the_configured_block_boundary() {
        let dir = tempdir().unwrap();
        let sender = PrivateKeySigner::random();
        let mut writer = LogWriter::open(dir.path(), 2).unwrap(); // 2 blocks per segment
        for block in 0..5u64 {
            let r = fixture_record(block, 0, &sender);
            writer.append(&r.header, &r.signature, &r.txs).unwrap();
        }
        let mut names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "segment-000000000000.log",
                "segment-000000000002.log",
                "segment-000000000004.log"
            ]
        );
    }

    /// After a segment roll, the log directory itself is fsynced (not
    /// just the new segment file) so the new file's directory entry survives a crash right after the
    /// roll. A unit test cannot simulate an actual power-cut, but it can exercise the exact realistic
    /// sequence — write across a roll, drop, reopen from scratch — and confirm every record across both
    /// segments still replays correctly, with the directory fsync in the path and not erroring.
    #[test]
    fn segment_roll_then_reopen_replays_every_record() {
        let dir = tempdir().unwrap();
        let sender = PrivateKeySigner::random();

        {
            let mut writer = LogWriter::open(dir.path(), 2).unwrap(); // 2 blocks per segment
            for block in 0..3u64 {
                // crosses one roll: segment 0 (blocks 0-1), segment 2 (block 2)
                let r = fixture_record(block, 0, &sender);
                writer.append(&r.header, &r.signature, &r.txs).unwrap();
            }
        } // writer dropped — nothing left open

        // Reopen fresh, as the binary does on restart, and append one more record into the segment the
        // reopened writer resumes.
        let mut reopened = LogWriter::open(dir.path(), 2).unwrap();
        let r3 = fixture_record(2, 1, &sender);
        reopened.append(&r3.header, &r3.signature, &r3.txs).unwrap();

        let mut replayed = Vec::new();
        let torn = replay(dir.path(), false, |r| replayed.push(r.clone())).unwrap();
        assert!(torn.is_none());
        assert_eq!(replayed.len(), 4, "3 records before reopen + 1 after");
    }

    /// A `total_len` above `MAX_FRAME_LEN` must be reported torn
    /// **without ever allocating a buffer of that size** — a corrupt or adversarial length must not be
    /// able to make replay attempt a multi-gigabyte allocation. Bounded by wall-clock time: if this
    /// allocated before checking, it would take far longer than a few milliseconds (or abort the process
    /// under memory pressure) instead of returning immediately.
    #[test]
    fn oversized_frame_length_is_torn_without_allocation() {
        let dir = tempdir().unwrap();
        let path = segment_path(dir.path(), 0);
        // A bogus total_len of u32::MAX, with no payload bytes following it at all.
        fs::write(&path, u32::MAX.to_le_bytes()).unwrap();

        let start = std::time::Instant::now();
        let torn = replay(dir.path(), false, |_| {}).unwrap();
        let elapsed = start.elapsed();

        assert!(
            torn.is_some(),
            "an oversized total_len must be reported torn"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "must reject the oversized length before allocating anything (took {elapsed:?})"
        );
    }

    /// A stronger version of the above: the size cap must reject an oversized frame **even when the
    /// frame is otherwise perfectly well-formed** — valid CRC, valid decode, every byte actually present
    /// on disk. The log layer never parses tx *contents* (it only length-prefixes opaque bytes), so a
    /// single oversized "tx" blob is enough to build a real record that would decode successfully if the
    /// cap did not exist. This is what actually distinguishes "checked before reading" from "checked via
    /// running out of bytes": the wall-clock test above would pass even from a build with no cap at all,
    /// on a platform where a large zeroed `Vec` is cheap to allocate — this one would not, because without
    /// the cap the well-formed oversized record replays successfully instead of being torn.
    #[test]
    fn frame_over_max_len_is_torn_even_when_otherwise_well_formed() {
        let dir = tempdir().unwrap();
        let sequencer_key = PrivateKeySigner::random();
        let header = SubBlockHeader {
            chain_id: 1,
            block: 1,
            index: 0,
            timestamp_us: 1_757_000_000_000_000,
            tx_root: B256::ZERO,
            receipts_root: B256::ZERO,
            gas_used: 21_000,
            prev_hash: B256::ZERO,
            deposits_end: None,
        };
        let signature = sign_header(&sequencer_key, &header);
        // The log layer treats a tx as an opaque length-prefixed byte blob — it never parses tx contents
        // — so this huge blob makes an otherwise perfectly valid record that just happens to be oversized.
        let huge_tx = Bytes::from(vec![0u8; MAX_FRAME_LEN as usize + 1_000_000]);
        let payload = encode_payload(&header, &signature, std::slice::from_ref(&huge_tx), &[]);
        assert!(
            (payload.len() as u32).checked_add(4).unwrap() > MAX_FRAME_LEN,
            "fixture must actually exceed the cap"
        );

        let crc = crc32fast::hash(&payload);
        let total_len = (payload.len() + 4) as u32;
        let mut frame = Vec::with_capacity(4 + payload.len() + 4);
        frame.extend_from_slice(&total_len.to_le_bytes());
        frame.extend_from_slice(&payload);
        frame.extend_from_slice(&crc.to_le_bytes());
        fs::write(segment_path(dir.path(), 0), &frame).unwrap();

        let mut replayed = Vec::new();
        let torn = replay(dir.path(), false, |r| replayed.push(r.clone())).unwrap();

        assert!(
            torn.is_some(),
            "a well-formed record over MAX_FRAME_LEN must still be rejected as torn, purely for size"
        );
        assert!(replayed.is_empty());
    }

    #[test]
    fn torn_tail_refused_without_truncate_flag() {
        let dir = tempdir().unwrap();
        let sender = PrivateKeySigner::random();
        let mut writer = LogWriter::open(dir.path(), 1_000).unwrap();
        for i in 0..3u16 {
            let r = fixture_record(0, i, &sender);
            writer.append(&r.header, &r.signature, &r.txs).unwrap();
        }
        drop(writer);

        // Simulate a crash mid-append: truncate the segment file so its last record is incomplete.
        let path = segment_path(dir.path(), 0);
        let full_len = fs::metadata(&path).unwrap().len();
        let f = OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(full_len - 3).unwrap(); // chop the last 3 bytes off the final record

        let mut replayed = Vec::new();
        let torn = replay(dir.path(), false, |r| replayed.push(r.clone())).unwrap();
        assert_eq!(
            replayed.len(),
            2,
            "the two complete records before the torn tail must still replay"
        );
        assert!(
            torn.is_some(),
            "a torn tail without --truncate-torn must be reported, not silently accepted"
        );
    }

    #[test]
    fn torn_tail_recovered_with_truncate_flag() {
        let dir = tempdir().unwrap();
        let sender = PrivateKeySigner::random();
        let mut writer = LogWriter::open(dir.path(), 1_000).unwrap();
        for i in 0..3u16 {
            let r = fixture_record(0, i, &sender);
            writer.append(&r.header, &r.signature, &r.txs).unwrap();
        }
        drop(writer);
        let path = segment_path(dir.path(), 0);
        let full_len = fs::metadata(&path).unwrap().len();
        let f = OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(full_len - 3).unwrap();

        let mut replayed = Vec::new();
        let torn = replay(dir.path(), true, |r| replayed.push(r.clone())).unwrap();
        assert!(
            torn.is_none(),
            "with --truncate-torn, replay must succeed after truncating"
        );
        assert_eq!(replayed.len(), 2);

        // The file on disk is now physically truncated: a second replay is clean with no torn report,
        // and appending more records afterward must produce a readable log again.
        let mut writer = LogWriter::open(dir.path(), 1_000).unwrap();
        let r = fixture_record(0, 9, &sender);
        writer.append(&r.header, &r.signature, &r.txs).unwrap();
        let mut replayed2 = Vec::new();
        let torn2 = replay(dir.path(), false, |r| replayed2.push(r.clone())).unwrap();
        assert!(torn2.is_none());
        assert_eq!(replayed2.len(), 3);
    }

    /// [`LogReader::open`] at `(0, 0)` must yield every record, in order — the plain "read the whole log"
    /// case a fresh batcher start uses.
    #[test]
    fn log_reader_from_genesis_yields_every_record_in_order() {
        let dir = tempdir().unwrap();
        let sender = PrivateKeySigner::random();
        let mut writer = LogWriter::open(dir.path(), 1_000).unwrap();
        let records: Vec<_> = (0..5).map(|i| fixture_record(0, i, &sender)).collect();
        for r in &records {
            writer.append(&r.header, &r.signature, &r.txs).unwrap();
        }

        let mut reader = LogReader::open(dir.path(), 0, 0).unwrap();
        let mut read_back = Vec::new();
        while let Some(r) = reader.read_next().unwrap() {
            read_back.push(r);
        }
        assert_eq!(read_back.len(), 5);
        for (original, back) in records.iter().zip(read_back.iter()) {
            assert_eq!(original.header, back.header);
            assert_eq!(original.txs, back.txs);
        }
    }

    /// Resuming from a mid-log `(block, index)` must skip every record strictly before it and yield the
    /// rest — the shape the batcher's stateless resume uses after a restart.
    #[test]
    fn log_reader_resumes_from_a_mid_log_position() {
        let dir = tempdir().unwrap();
        let sender = PrivateKeySigner::random();
        let mut writer = LogWriter::open(dir.path(), 1_000).unwrap();
        for block in 0..3u64 {
            for index in 0..4u16 {
                let r = fixture_record(block, index, &sender);
                writer.append(&r.header, &r.signature, &r.txs).unwrap();
            }
        }

        // Resume at block 1, index 2 — skip block 0 entirely and (1,0),(1,1).
        let mut reader = LogReader::open(dir.path(), 1, 2).unwrap();
        let mut read_back = Vec::new();
        while let Some(r) = reader.read_next().unwrap() {
            read_back.push((r.header.block, r.header.index));
        }
        let expected: Vec<(u64, u16)> = vec![(1, 2), (1, 3), (2, 0), (2, 1), (2, 2), (2, 3)];
        assert_eq!(read_back, expected);
    }

    /// A reader that has caught up to the log's current end returns `None` (not an error); after the
    /// writer appends more (into a newly-rolled segment) and the reader calls `refresh_segments`, `next`
    /// picks the new records up — the live tail-follow path a running batcher uses between polls.
    #[test]
    fn log_reader_follows_the_tail_across_a_segment_roll() {
        let dir = tempdir().unwrap();
        let sender = PrivateKeySigner::random();
        let mut writer = LogWriter::open(dir.path(), 2).unwrap(); // 2 blocks per segment
        for block in 0..2u64 {
            let r = fixture_record(block, 0, &sender);
            writer.append(&r.header, &r.signature, &r.txs).unwrap();
        }

        let mut reader = LogReader::open(dir.path(), 0, 0).unwrap();
        assert_eq!(reader.read_next().unwrap().unwrap().header.block, 0);
        assert_eq!(reader.read_next().unwrap().unwrap().header.block, 1);
        assert!(
            reader.read_next().unwrap().is_none(),
            "caught up to the tail must yield None, not an error"
        );

        // Writer rolls into a new segment and appends more.
        for block in 2..4u64 {
            let r = fixture_record(block, 0, &sender);
            writer.append(&r.header, &r.signature, &r.txs).unwrap();
        }
        assert!(
            reader.read_next().unwrap().is_none(),
            "without refresh_segments, the reader must not see the new segment yet"
        );
        reader.refresh_segments().unwrap();
        assert_eq!(reader.read_next().unwrap().unwrap().header.block, 2);
        assert_eq!(reader.read_next().unwrap().unwrap().header.block, 3);
        assert!(reader.read_next().unwrap().is_none());
    }

    /// A resume position past the end of the log (nothing there yet) must not error and must not skip a
    /// record that later becomes the actual next one to write — `open` returns cleanly and a later
    /// `refresh_segments` + `next` picks up the awaited record once appended.
    #[test]
    fn log_reader_resume_position_beyond_current_end_waits_for_it() {
        let dir = tempdir().unwrap();
        let sender = PrivateKeySigner::random();
        let mut writer = LogWriter::open(dir.path(), 1_000).unwrap();
        let r0 = fixture_record(0, 0, &sender);
        writer.append(&r0.header, &r0.signature, &r0.txs).unwrap();

        // Resume position is (0, 1) — the next record to be written, not yet on disk.
        let mut reader = LogReader::open(dir.path(), 0, 1).unwrap();
        assert!(reader.read_next().unwrap().is_none());

        let r1 = fixture_record(0, 1, &sender);
        writer.append(&r1.header, &r1.signature, &r1.txs).unwrap();
        reader.refresh_segments().unwrap();
        let got = reader.read_next().unwrap().unwrap();
        assert_eq!((got.header.block, got.header.index), (0, 1));
    }

    /// Literal golden: pins the exact on-disk bytes (`total_len:u32(LE) ‖ payload ‖ crc32:u32(LE)`) for one fixed
    /// `SubBlockRecord` — not just "writer and reader agree", which a writer/reader change that moves in lockstep
    /// can still satisfy. If this fails after an intentional format change, regenerate it deliberately — do not
    /// "fix" it by copying new output in without checking the change against this module's doc and the lane design.
    #[test]
    fn record_bytes_are_pinned() {
        // Fixed (not `random()`) keys: the golden must reproduce byte-for-byte on every run, and ECDSA
        // signing here is deterministic (RFC 6979) for a fixed key + fixed digest — only the key source
        // needs pinning, not the signature algorithm itself.
        let sequencer_key = PrivateKeySigner::from_slice(&[0x11u8; 32]).unwrap();
        let sender = PrivateKeySigner::from_slice(&[0x22u8; 32]).unwrap();
        let header = SubBlockHeader {
            chain_id: 1,
            block: 0,
            index: 0,
            timestamp_us: 1_757_000_000_000_000,
            tx_root: B256::repeat_byte(1),
            receipts_root: B256::repeat_byte(2),
            gas_used: 21_000,
            prev_hash: B256::ZERO,
            deposits_end: None,
        };
        let signature = sign_header(&sequencer_key, &header);
        let tx = signed_raw_tx(&sender, 1, 0);

        let dir = tempdir().unwrap();
        let mut writer = LogWriter::open(dir.path(), 1_000).unwrap();
        writer.append(&header, &signature, &[tx]).unwrap();
        drop(writer);

        let bytes = fs::read(segment_path(dir.path(), 0)).unwrap();
        let expected_hex = "2d010000f87101808087063dfb70ded000a00101010101010101010101010101010101010101010101010101010101010101a00202020202020202020202020202020202020202020202020202020202020202825208a00000000000000000000000000000000000000000000000000000000000000000cf3b9d31974f4c9f67f132fed38e5400889379125611978d74ff84df76ca9b04701f33147f9fe7e91f8f4db1bf4feabd6153251034aa5e4f56afcf3dd363def11c010000006d00000002f86a0180843b9aca00843b9aca008252089400000000000000000000000000000000000000008080c001a01f27802e0008a691503da42b393f4f01b9d0dab706242b8a387002e63c334f1ea032bceadd0736ef7630103789ea7cf04043a0a2b5e70ad89050567fe1cabdd7f19a736280";
        assert_eq!(
            hex::encode(&bytes),
            expected_hex,
            "on-disk record bytes changed — see this test's doc before updating the literal"
        );
    }

    #[test]
    fn bit_flip_without_length_change_is_caught_by_crc() {
        let dir = tempdir().unwrap();
        let sender = PrivateKeySigner::random();
        let mut writer = LogWriter::open(dir.path(), 1_000).unwrap();
        let r = fixture_record(0, 0, &sender);
        writer.append(&r.header, &r.signature, &r.txs).unwrap();
        drop(writer);

        let path = segment_path(dir.path(), 0);
        let mut bytes = fs::read(&path).unwrap();
        // Flip a bit well inside the payload (past the 4-byte length prefix) without changing length.
        let flip_at = 10;
        bytes[flip_at] ^= 0x01;
        let mut f = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        f.write_all(&bytes).unwrap();

        let mut replayed = Vec::new();
        let torn = replay(dir.path(), false, |r| replayed.push(r.clone())).unwrap();
        assert!(replayed.is_empty());
        assert!(
            torn.is_some(),
            "a same-length bit flip must be caught by the CRC, not silently replayed"
        );
    }

    /// `LogReader` — the batcher's only production read path (via
    /// `rome_zk_sequencer::log::LogReader` re-export) — must refuse a bit-flipped record rather than
    /// silently stopping as if it had merely caught up to the tail. Two records are written and the
    /// *second* one is flipped: `open(dir, 0, 0)` stops at the first record already `>= (0, 0)` (the
    /// first record itself), so the good first record is yielded normally by one `read_next()` before
    /// the corrupted second record is reached by the next.
    #[test]
    fn log_reader_refuses_a_bit_flipped_record() {
        let dir = tempdir().unwrap();
        let sender = PrivateKeySigner::random();
        let mut writer = LogWriter::open(dir.path(), 1_000).unwrap();
        let r0 = fixture_record(0, 0, &sender);
        let r1 = fixture_record(0, 1, &sender);
        writer.append(&r0.header, &r0.signature, &r0.txs).unwrap();
        let first_record_len = fs::metadata(segment_path(dir.path(), 0)).unwrap().len();
        writer.append(&r1.header, &r1.signature, &r1.txs).unwrap();
        drop(writer);

        let path = segment_path(dir.path(), 0);
        let mut bytes = fs::read(&path).unwrap();
        // Flip a bit inside the second record's payload (past its own 4-byte length prefix), leaving
        // `total_len` untouched.
        let flip_at = first_record_len as usize + 10;
        bytes[flip_at] ^= 0x01;
        let mut f = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        f.write_all(&bytes).unwrap();

        let mut reader = LogReader::open(dir.path(), 0, 0).unwrap();
        let first = reader
            .read_next()
            .unwrap()
            .expect("the first, uncorrupted record must still read back fine");
        assert_eq!(first.header.index, 0);
        let err = reader
            .read_next()
            .expect_err("a bit-flipped record must be a hard error, not a silent None");
        assert_eq!(
            err.kind(),
            io::ErrorKind::InvalidData,
            "must be reported as InvalidData: {err:?}"
        );
    }

    /// Encodes one record's on-disk bytes (`total_len ‖ payload ‖ crc32`) without going through
    /// [`LogWriter`] — the corruption tests below need full control over what sits *around* a record on
    /// disk, not just the record itself.
    fn frame_bytes(header: &SubBlockHeader, signature: &Signature, txs: &[Bytes]) -> Vec<u8> {
        let payload = encode_payload(header, signature, txs, &[]);
        let crc = crc32fast::hash(&payload);
        let total_len = (payload.len() + 4) as u32;
        let mut frame = Vec::with_capacity(4 + payload.len() + 4);
        frame.extend_from_slice(&total_len.to_le_bytes());
        frame.extend_from_slice(&payload);
        frame.extend_from_slice(&crc.to_le_bytes());
        frame
    }

    /// A record's length prefix reads as a value outside `4..=MAX_FRAME_LEN`, but a complete, valid record's bytes
    /// sit right after it in the very same (only, hence newest) segment. A genuine crash mid-`write_all` could
    /// never leave anything *after* a torn length field — the writer never got that far — so bytes surviving past
    /// it prove this is corruption, not an in-progress append, whatever segment it is found in.
    #[test]
    fn out_of_range_length_prefix_mid_segment_with_data_after_is_corrupt() {
        let dir = tempdir().unwrap();
        let sender = PrivateKeySigner::random();
        let r0 = fixture_record(0, 0, &sender);

        let mut bytes = frame_bytes(&r0.header, &r0.signature, &r0.txs);
        // A bogus length prefix, then a second, otherwise perfectly valid record right after it.
        bytes.extend_from_slice(&(MAX_FRAME_LEN + 1).to_le_bytes());
        let r1 = fixture_record(0, 1, &sender);
        bytes.extend_from_slice(&frame_bytes(&r1.header, &r1.signature, &r1.txs));
        fs::write(segment_path(dir.path(), 0), &bytes).unwrap();

        let mut reader = LogReader::open(dir.path(), 0, 0).unwrap();
        let first = reader
            .read_next()
            .unwrap()
            .expect("the first record before the corrupt prefix still reads back fine");
        assert_eq!(first.header.index, 0);
        let err = reader.read_next().expect_err(
            "an out-of-range length prefix with real data following it must be a hard error, not Ok(None)",
        );
        assert_eq!(
            err.kind(),
            io::ErrorKind::InvalidData,
            "must be reported as InvalidData: {err:?}"
        );
    }

    /// The same out-of-range length prefix, this time genuinely at the *end* of its own segment file (nothing
    /// follows it there) — but that segment is no longer the newest one on disk. An older, rolled-past segment can
    /// never grow again, so this can never resolve itself on a later read the way a live tail-follow would; it must
    /// be refused, not silently skipped past via `advance_segment`.
    #[test]
    fn out_of_range_length_prefix_in_an_older_segment_is_corrupt_even_at_its_own_tail() {
        let dir = tempdir().unwrap();
        let sender = PrivateKeySigner::random();

        // Segment 0 (older): one valid record, then a bogus length prefix as the very last bytes in the
        // file — on its own, indistinguishable from an honest torn write at *a* tail.
        let r0 = fixture_record(0, 0, &sender);
        let mut seg0_bytes = frame_bytes(&r0.header, &r0.signature, &r0.txs);
        seg0_bytes.extend_from_slice(&(MAX_FRAME_LEN + 1).to_le_bytes());
        fs::write(segment_path(dir.path(), 0), &seg0_bytes).unwrap();

        // Segment 2 (newer): exists on disk, so segment 0 is provably not the newest segment any more.
        let r2 = fixture_record(2, 0, &sender);
        let seg2_bytes = frame_bytes(&r2.header, &r2.signature, &r2.txs);
        fs::write(segment_path(dir.path(), 2), &seg2_bytes).unwrap();

        let mut reader = LogReader::open(dir.path(), 0, 0).unwrap();
        let first = reader
            .read_next()
            .unwrap()
            .expect("segment 0's own valid record still reads back fine");
        assert_eq!(first.header.block, 0);
        let err = reader.read_next().expect_err(
            "an out-of-range length prefix in a non-newest segment must be a hard error, not a silent advance_segment",
        );
        assert_eq!(
            err.kind(),
            io::ErrorKind::InvalidData,
            "must be reported as InvalidData: {err:?}"
        );
    }

    /// Control: a short trailing record at the tail of the newest (here: only) segment on disk must still be "retry
    /// later" (`Ok(None)`), unchanged — this is the exact shape a live sequencer's in-progress append leaves
    /// behind, and `replay --truncate-torn` is what repairs it; the reader must not turn this into a hard error.
    #[test]
    fn short_trailing_record_at_the_newest_segments_tail_still_waits_for_it() {
        let dir = tempdir().unwrap();
        let sender = PrivateKeySigner::random();
        let mut writer = LogWriter::open(dir.path(), 1_000).unwrap();
        for i in 0..2u16 {
            let r = fixture_record(0, i, &sender);
            writer.append(&r.header, &r.signature, &r.txs).unwrap();
        }
        drop(writer);

        // Simulate a crash mid-append on the only (hence newest) segment: chop the last 3 bytes off the
        // final record, the same shape as `torn_tail_refused_without_truncate_flag`.
        let path = segment_path(dir.path(), 0);
        let full_len = fs::metadata(&path).unwrap().len();
        let f = OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(full_len - 3).unwrap();

        let mut reader = LogReader::open(dir.path(), 0, 0).unwrap();
        let first = reader
            .read_next()
            .unwrap()
            .expect("the first, complete record still reads back fine");
        assert_eq!(first.header.index, 0);
        assert!(
            reader.read_next().unwrap().is_none(),
            "a torn record at the newest segment's own tail must still be retry-later, not an error"
        );
    }

    /// Roll race: `read_next`'s two observations of a non-newest segment — "the read at this offset came back
    /// Incomplete" and "no newer segment is queued" — are not atomic. If a real roll completes the record's fsync
    /// and creates the next segment strictly between those two observations, the record is complete on disk by the
    /// time the second observation runs, yet the old code reported a hard `InvalidData` for it anyway.
    /// `before_newest_check` (test-only) fires in exactly that window, simulating the writer racing to completion;
    /// `read_next` must still finish with `Ok(Some(record))`.
    #[test]
    fn read_next_survives_a_roll_racing_the_torn_read() {
        let dir = tempdir().unwrap();
        let sender = PrivateKeySigner::random();
        let mut writer = LogWriter::open(dir.path(), 1_000).unwrap();
        let r0 = fixture_record(0, 0, &sender);
        writer.append(&r0.header, &r0.signature, &r0.txs).unwrap();

        // Manually append a torn second frame straight to the segment file (bypassing `LogWriter`, whose
        // own `append` always leaves a frame either fully fsynced or entirely absent as far as a reader
        // observes it) -- this reproduces the "first read sees Incomplete" state that motivated this fix.
        let r1 = fixture_record(0, 1, &sender);
        let full_frame = frame_bytes(&r1.header, &r1.signature, &r1.txs);
        let torn_len = full_frame.len() - 3;
        {
            let mut f = OpenOptions::new()
                .append(true)
                .open(segment_path(dir.path(), 0))
                .unwrap();
            f.write_all(&full_frame[..torn_len]).unwrap();
        }

        let mut reader = LogReader::open(dir.path(), 0, 0).unwrap();
        assert_eq!(reader.read_next().unwrap().unwrap().header.index, 0);

        // Arm the hook: fires after this next `read_next()` call's read sees `Incomplete` but before
        // `is_current_the_newest_segment` is checked -- simulating the writer completing + fsyncing the
        // frame and rolling into segment 1_000 in that exact window.
        let dir_path = dir.path().to_path_buf();
        let seg0_path = segment_path(&dir_path, 0);
        let remaining = full_frame[torn_len..].to_vec();
        reader.before_newest_check = Some(Box::new(move || {
            let mut f = OpenOptions::new().append(true).open(&seg0_path).unwrap();
            f.write_all(&remaining).unwrap();
            f.sync_data().unwrap();
            fs::write(segment_path(&dir_path, 1_000), []).unwrap();
        }));

        let record = reader
            .read_next()
            .expect("a roll racing the torn read must not be a hard error")
            .expect("the record completed mid-race must still be yielded, not treated as absent");
        assert_eq!((record.header.block, record.header.index), (0, 1));
    }

    /// 1..=3 stray bytes of a length prefix landing at the newest segment's tail are a torn write (a short read on
    /// the length prefix itself), not a clean end-of-segment. `LogReader` must treat it exactly like any other torn
    /// tail: `Ok(None)` (retry later), never an error.
    #[test]
    fn stray_length_prefix_bytes_at_the_newest_tail_are_retry_later_not_an_error() {
        for stray in 1..=3u64 {
            let dir = tempdir().unwrap();
            let sender = PrivateKeySigner::random();
            let mut writer = LogWriter::open(dir.path(), 1_000).unwrap();
            for i in 0..2u16 {
                let r = fixture_record(0, i, &sender);
                writer.append(&r.header, &r.signature, &r.txs).unwrap();
            }
            drop(writer);
            let path = segment_path(dir.path(), 0);
            // Append `stray` bytes of a would-be third record's length prefix -- never the full 4, which
            // would be a different (LengthOutOfRange/Corrupt) shape covered by other tests.
            {
                let mut f = OpenOptions::new().append(true).open(&path).unwrap();
                f.write_all(&5u32.to_le_bytes()[..stray as usize]).unwrap();
            }

            let mut reader = LogReader::open(dir.path(), 0, 0).unwrap();
            assert_eq!(reader.read_next().unwrap().unwrap().header.index, 0);
            assert_eq!(reader.read_next().unwrap().unwrap().header.index, 1);
            assert!(
                reader.read_next().unwrap().is_none(),
                "stray={stray}: {stray} stray length-prefix bytes at the newest tail must be Ok(None), not an error"
            );
        }
    }

    /// 1..=3 stray bytes, at the tail of the log's only (hence newest) segment, must be reported by
    /// `replay(truncate_torn=true)` as a torn tail and truncated away — the file must shrink back to exactly the
    /// last good record's length, not be left with the stray bytes still on disk. Before the fix, `NoMoreData` made
    /// `replay` treat the segment as clean, so nothing was truncated and the sequencer would later append after the
    /// stray bytes; the test requires the torn tail to be reported and removed. A `LogWriter` that resumes on the
    /// repaired log then appends cleanly.
    #[test]
    fn stray_length_prefix_bytes_at_the_newest_tail_are_reported_and_truncated_by_replay() {
        for stray in 1..=3u64 {
            let dir = tempdir().unwrap();
            let sender = PrivateKeySigner::random();
            let mut writer = LogWriter::open(dir.path(), 1_000).unwrap();
            for i in 0..2u16 {
                let r = fixture_record(0, i, &sender);
                writer.append(&r.header, &r.signature, &r.txs).unwrap();
            }
            drop(writer);
            let path = segment_path(dir.path(), 0);
            let good_len = fs::metadata(&path).unwrap().len();
            {
                let mut f = OpenOptions::new().append(true).open(&path).unwrap();
                f.write_all(&5u32.to_le_bytes()[..stray as usize]).unwrap();
            }
            assert_eq!(fs::metadata(&path).unwrap().len(), good_len + stray);

            // replay(false) must at least report the torn tail (never silently "clean").
            let mut replayed = Vec::new();
            let torn = replay(dir.path(), false, |r| replayed.push(r.clone())).unwrap();
            assert!(
                torn.is_some(),
                "stray={stray}: {stray} stray length-prefix bytes must be reported as a torn tail, not silently clean"
            );
            assert_eq!(replayed.len(), 2);

            // replay(true) truncates it away: the file shrinks back to exactly the last good record.
            let mut replayed2 = Vec::new();
            let torn2 = replay(dir.path(), true, |r| replayed2.push(r.clone())).unwrap();
            assert!(
                torn2.is_none(),
                "stray={stray}: truncate_torn must succeed after truncating the stray bytes away"
            );
            assert_eq!(replayed2.len(), 2);
            assert_eq!(
                fs::metadata(&path).unwrap().len(),
                good_len,
                "stray={stray}: file must be truncated back to good_len={good_len}, not left with stray bytes"
            );

            // A LogWriter resuming on the repaired log appends cleanly.
            let mut resumed = LogWriter::open(dir.path(), 1_000).unwrap();
            let r2 = fixture_record(0, 9, &sender);
            resumed.append(&r2.header, &r2.signature, &r2.txs).unwrap();
            let mut replayed3 = Vec::new();
            let torn3 = replay(dir.path(), false, |r| replayed3.push(r.clone())).unwrap();
            assert!(torn3.is_none());
            assert_eq!(replayed3.len(), 3);
        }
    }

    /// 1..=3 stray bytes, found in a non-newest segment (an older, rolled-past segment can never grow again), must
    /// be a hard error — exactly the same "is this the newest segment's tail" line `LogReader` already draws for a
    /// full bogus length prefix.
    #[test]
    fn stray_length_prefix_bytes_in_a_non_newest_segment_is_corrupt() {
        for stray in 1..=3u64 {
            let dir = tempdir().unwrap();
            let sender = PrivateKeySigner::random();

            let r0 = fixture_record(0, 0, &sender);
            let mut seg0_bytes = frame_bytes(&r0.header, &r0.signature, &r0.txs);
            seg0_bytes.extend_from_slice(&5u32.to_le_bytes()[..stray as usize]);
            fs::write(segment_path(dir.path(), 0), &seg0_bytes).unwrap();

            let r2 = fixture_record(2, 0, &sender);
            let seg2_bytes = frame_bytes(&r2.header, &r2.signature, &r2.txs);
            fs::write(segment_path(dir.path(), 2), &seg2_bytes).unwrap();

            let mut reader = LogReader::open(dir.path(), 0, 0).unwrap();
            let first = reader
                .read_next()
                .unwrap()
                .expect("segment 0's own valid record still reads back fine");
            assert_eq!(first.header.block, 0);
            let err = reader.read_next().expect_err(&format!(
                "stray={stray}: stray length-prefix bytes in a non-newest segment must be a hard error, not Ok(None)"
            ));
            assert_eq!(
                err.kind(),
                io::ErrorKind::InvalidData,
                "stray={stray}: must be reported as InvalidData: {err:?}"
            );
        }
    }

    // ---- blocks that carry withdrawals (deposits) --------------------------------------------------------------------

    /// A fixed sequencer key, so the signature (RFC 6979, deterministic) and so the whole frame are reproducible.
    fn fixed_signer() -> PrivateKeySigner {
        PrivateKeySigner::from_bytes(&B256::repeat_byte(0x42)).expect("fixed key")
    }

    fn index0_header(deposits_end: Option<u64>) -> SubBlockHeader {
        SubBlockHeader {
            chain_id: 200_101,
            block: 9,
            index: 0,
            timestamp_us: 1_757_000_000_000_000,
            tx_root: B256::repeat_byte(0x11),
            receipts_root: B256::repeat_byte(0x22),
            gas_used: 21_000,
            prev_hash: B256::repeat_byte(0x33),
            deposits_end,
        }
    }

    fn three_withdrawals() -> Vec<Withdrawal> {
        (5u64..8)
            .map(|i| deposit_withdrawal(i, Address::repeat_byte(0xA0 + i as u8), 1_000 * (i + 1)))
            .collect()
    }

    /// The record format as it was before deposits, assembled here by hand from the module doc and nothing else:
    /// `header RLP ‖ signature ‖ tx_count ‖ (tx_len ‖ tx)*`.
    fn old_format_payload(
        header: &SubBlockHeader,
        signature: &Signature,
        txs: &[Bytes],
    ) -> Vec<u8> {
        let mut out = header.encode_canonical();
        out.extend_from_slice(&signature.as_bytes());
        out.extend_from_slice(&(txs.len() as u32).to_le_bytes());
        for tx in txs {
            out.extend_from_slice(&(tx.len() as u32).to_le_bytes());
            out.extend_from_slice(tx);
        }
        out
    }

    /// A block without deposits is byte for byte what the log always wrote: the frame on disk equals the old format
    /// assembled by hand, whichever append the writer was called through; the header's own golden bytes are pinned in
    /// `header.rs`.
    #[test]
    fn a_record_without_deposits_is_byte_identical_to_the_old_format() {
        let signer = fixed_signer();
        let header = index0_header(None);
        let signature = sign_header(&signer, &header);
        let txs = vec![Bytes::from_static(&[1, 2, 3]), Bytes::from_static(&[9; 40])];

        let payload = old_format_payload(&header, &signature, &txs);
        let mut expected = Vec::new();
        expected.extend_from_slice(&((payload.len() + 4) as u32).to_le_bytes());
        expected.extend_from_slice(&payload);
        expected.extend_from_slice(&crc32fast::hash(&payload).to_le_bytes());

        let via_append = tempdir().unwrap();
        LogWriter::open(via_append.path(), 1_000)
            .unwrap()
            .append(&header, &signature, &txs)
            .unwrap();
        let via_withdrawals = tempdir().unwrap();
        LogWriter::open(via_withdrawals.path(), 1_000)
            .unwrap()
            .append_with_withdrawals(&header, &signature, &txs, &[])
            .unwrap();
        for dir in [via_append.path(), via_withdrawals.path()] {
            let on_disk = fs::read(segment_path(dir, 0)).unwrap();
            assert_eq!(on_disk, expected);
        }
        // The header's own bytes are the eight-item list, counted by hand: chain id 4 + block 1 + index 1 +
        // timestamp 8 + tx_root 33 + receipts_root 33 + gas_used 3 + prev_hash 33 = 116 = 0x74 bytes of items.
        assert_eq!(payload[0], 0xf8);
        assert_eq!(payload[1], 0x74);

        // And the old frame decodes to a record with no withdrawals.
        let mut seen = Vec::new();
        replay(via_append.path(), false, |r| seen.push(r.clone())).unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].header, header);
        assert_eq!(seen[0].txs, txs);
        assert!(seen[0].withdrawals.is_empty());
    }

    /// An index-0 record with withdrawals: the tail is the old payload plus `count ‖ (index ‖ recipient ‖ amount)*`,
    /// it reads back (through `replay` and `LogReader`) with the same withdrawals, and the header's hash and the
    /// signature over it commit to `deposits_end`.
    #[test]
    fn a_record_with_withdrawals_round_trips_and_the_header_commits_to_the_range() {
        let signer = fixed_signer();
        let withdrawals = three_withdrawals();
        let header = index0_header(Some(8));
        let signature = sign_header(&signer, &header);
        let txs = vec![Bytes::from_static(&[7, 7, 7])];

        let dir = tempdir().unwrap();
        LogWriter::open(dir.path(), 1_000)
            .unwrap()
            .append_with_withdrawals(&header, &signature, &txs, &withdrawals)
            .unwrap();

        let mut expected_payload = old_format_payload(&header, &signature, &txs);
        expected_payload.extend_from_slice(&3u32.to_le_bytes());
        for (i, a) in [(5u64, 0xA5u8), (6, 0xA6), (7, 0xA7)] {
            expected_payload.extend_from_slice(&i.to_le_bytes());
            expected_payload.extend_from_slice(&[a; 20]);
            expected_payload.extend_from_slice(&(1_000 * (i + 1)).to_le_bytes());
        }
        let on_disk = fs::read(segment_path(dir.path(), 0)).unwrap();
        assert_eq!(&on_disk[4..on_disk.len() - 4], expected_payload.as_slice());

        let mut seen = Vec::new();
        replay(dir.path(), false, |r| seen.push(r.clone())).unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].withdrawals, withdrawals);
        assert_eq!(seen[0].header, header);
        assert_eq!(seen[0].txs, txs);
        // Every logged withdrawal is a deposit credit: validator index 0.
        assert!(seen[0].withdrawals.iter().all(|w| w.validator_index == 0));

        let mut reader = LogReader::open(dir.path(), 0, 0).unwrap();
        let first = reader.read_next().unwrap().unwrap();
        assert_eq!(first.withdrawals, withdrawals);
        assert!(reader.read_next().unwrap().is_none());

        // The signed pre-confirmation commits to the range: the same header without it has a different signing hash.
        assert_ne!(header.signing_hash(), index0_header(None).signing_hash());
    }

    /// The writer refuses a record whose withdrawals and `deposits_end` disagree, and writes nothing for it.
    #[test]
    fn the_writer_refuses_withdrawals_that_disagree_with_the_header() {
        let signer = fixed_signer();
        let withdrawals = three_withdrawals(); // indices 5, 6, 7: the range ends at 8
        let cases: Vec<(&str, SubBlockHeader, Vec<Withdrawal>)> = vec![
            (
                "withdrawals, no deposits_end",
                index0_header(None),
                withdrawals.clone(),
            ),
            (
                "deposits_end, no withdrawals",
                index0_header(Some(8)),
                vec![],
            ),
            ("wrong end", index0_header(Some(9)), withdrawals.clone()),
            (
                "not index 0",
                SubBlockHeader {
                    index: 1,
                    ..index0_header(Some(8))
                },
                withdrawals.clone(),
            ),
        ];
        for (name, header, list) in cases {
            let dir = tempdir().unwrap();
            let mut writer = LogWriter::open(dir.path(), 1_000).unwrap();
            let signature = sign_header(&signer, &header);
            let err = writer
                .append_with_withdrawals(&header, &signature, &[], &list)
                .expect_err(name);
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{name}: {err:?}");
            let len = fs::metadata(segment_path(dir.path(), 0)).unwrap().len();
            assert_eq!(len, 0, "{name}: nothing may be written");
        }
    }

    /// The reader refuses every malformed tail: a zero count (a second encoding of "none"), a short or long tail, a
    /// tail on a header without `deposits_end`, and a `deposits_end` that is not one past the last withdrawal.
    #[test]
    fn the_reader_refuses_a_malformed_withdrawal_tail() {
        let signer = fixed_signer();
        let header = index0_header(Some(8));
        let signature = sign_header(&signer, &header);
        let base = old_format_payload(&header, &signature, &[]);
        let one = |index: u64| {
            let mut w = index.to_le_bytes().to_vec();
            w.extend_from_slice(&[0xA5; 20]);
            w.extend_from_slice(&1_000u64.to_le_bytes());
            w
        };
        let tail = |count: u32, body: &[u8]| {
            let mut p = base.clone();
            p.extend_from_slice(&count.to_le_bytes());
            p.extend_from_slice(body);
            p
        };

        // Well formed: one withdrawal at index 7, range end 8.
        let ok = decode_payload(&tail(1, &one(7))).expect("well-formed tail decodes");
        assert_eq!(ok.withdrawals.len(), 1);
        assert_eq!(
            ok.withdrawals[0],
            deposit_withdrawal(7, Address::repeat_byte(0xA5), 1_000)
        );

        assert!(decode_payload(&tail(0, &[])).is_err(), "zero count");
        assert!(
            decode_payload(&tail(1, &one(7)[..35])).is_err(),
            "short tail"
        );
        let mut long = one(7);
        long.push(0);
        assert!(decode_payload(&tail(1, &long)).is_err(), "long tail");
        assert!(
            decode_payload(&tail(2, &one(7))).is_err(),
            "count above the bytes present"
        );
        assert!(
            decode_payload(&tail(1, &one(3))).is_err(),
            "deposits_end not one past the last index"
        );
        assert!(
            decode_payload(&tail(u32::MAX, &one(7))).is_err(),
            "huge count"
        );
        let mut stray = base.clone();
        stray.push(1);
        assert!(
            decode_payload(&stray).is_err(),
            "a stray byte is not a tail"
        );

        // A tail under a header with no deposits_end.
        let bare = index0_header(None);
        let bare_sig = sign_header(&signer, &bare);
        let mut p = old_format_payload(&bare, &bare_sig, &[]);
        p.extend_from_slice(&1u32.to_le_bytes());
        p.extend_from_slice(&one(7));
        assert!(decode_payload(&p).is_err(), "tail without deposits_end");
        // deposits_end with no tail.
        assert!(
            decode_payload(&base).is_err(),
            "deposits_end without a tail"
        );
    }
}
