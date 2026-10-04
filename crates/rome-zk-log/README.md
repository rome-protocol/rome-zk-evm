# rome-zk-log

The sub-block header and the sequencer's append-only ordered log: record format, segments, torn-tail
recovery, and the tail-follow reader. The sequencer writes it; the batcher, the derivation node and the
indexer read it.

## Why this crate exists

Before it did, the record format (`crates/rome-zk-sequencer/src/log.rs`) and the header it wraps
(`src/header.rs`) lived inside the sequencer crate, and every other reader — the batcher, most visibly —
depended on the whole sequencer crate (with its `reth` feature turned off) just to reach `LogReader` and
`SubBlockHeader`. Splitting it out gives the record format one home, owned by neither the writer's nor a
reader's larger crate, with the sequencer re-exporting it so no existing call site changed.

## What it owns

- **`SubBlockHeader`** (moved from the sequencer's `header.rs`) — canonical RLP encoding, content hash, and
  the domain-separated signing hash (`keccak256(SIGNING_DOMAIN || hash())`) the sequencer's key actually
  signs, never the bare header hash.
- **The record format** — `total_len:u32(LE) ‖ payload ‖ crc32:u32(LE)`, where `payload = RLP(header) ‖
  signature:[u8;65] ‖ tx_count:u32(LE) ‖ (tx_len:u32(LE) tx_bytes)*`. A record's `txs` are exactly the
  sub-block's *included* transactions — a rejected or not-yet-reached transaction never reaches this
  format at all (see `LogWriter::append`'s module doc for what that means for a reader). The first record
  of a block that credits deposits also carries the block's withdrawals after its transactions,
  `withdrawal_count:u32(LE) ‖ (index:u64(LE) recipient:[u8;20] amount_gwei:u64(LE))*`, and its header
  carries an optional ninth item, `deposits_end` (the deposit queue's index after the block), which the
  header hash and the signature cover. A record without deposits has neither and is byte for byte the
  format above.
- **`LogWriter`** — appends one record per sub-block, `fsync`ing the segment file (and, on a new segment,
  the containing directory) before returning; a caller must not acknowledge a sub-block until this
  returns `Ok`.
- **`replay`** — walks every complete record across a log directory's segments in order, reporting a torn
  tail (necessarily the last bytes of the newest segment; anywhere else is `LogError::CorruptMidLog`) and
  optionally truncating it.
- **`LogReader`** — a read-only, resumable, tail-following cursor: opens positioned at `(from_block,
  from_index)`, yields records one at a time, and picks up newly-rolled segments on `refresh_segments`
  without ever re-reading a segment it has already exhausted.
- **`log_numbering_origin`** — the single check that the log's first record is `(block 1, index 0)`:
  `Ok(())` on an empty or nonexistent log directory, `LogNumberingError::WrongOrigin` naming the actual
  first record otherwise. The ordered log starts at block 1 — genesis 0 is never sealed — and is never
  pruned, so this is a fact about the whole log, not a heuristic.

## Torn tail vs corruption

**The torn-write model.** A torn tail is a byte-order prefix of exactly one frame —
whatever bytes of the frame the writer had handed the kernel before it died, in order, with nothing past
them. Zero-fill and out-of-order sectors are out of model: this crate does not attempt to recognize or
repair them. When the model holds (the case every test here exercises), a record can fail to read back
for two different reasons, and only one of them is ever legitimate:

- **Torn (retry later).** The newest segment on disk is still being appended to, so its very last bytes
  can be a partial `write_all` that has not finished landing — a short body/CRC, or a length prefix
  itself only partly flushed (1 to 3 of its 4 bytes present — too few to decode at all, so this can never
  be read as a value, in range or out of it). `LogReader::read_next` reports this as `Ok(None)` — the
  same answer as a clean end-of-segment — because a live tail-follower simply has not caught up yet, and
  `replay --truncate-torn` is what repairs it on the writer's own next restart.
- **Corrupt (hard error).** The same shapes found anywhere else are never legitimate: a segment that is
  no longer the newest on disk has stopped growing forever, so nothing found wrong in it can resolve
  itself on a later read; and a length prefix decoding out of range with more bytes still sitting past it
  proves a writer kept appending after it — under the model above, an honest torn write can never leave
  real data behind it. `LogReader::read_next` reports both as a hard `io::Error` (`InvalidData`, "corrupt
  record in {path} at offset {offset}") rather than the tail-follow `Ok(None)`, so a consumer never waits
  forever at a record that was never going to complete. `replay`'s own classification
  (`LogError::CorruptMidLog` outside the newest segment) already drew this same "is this the newest
  segment's tail" line and is unchanged; the reader now mirrors it.

**When the model is violated.** `LogReader` is allowed to be stricter than `replay` at the newest
segment's own tail — a shape `replay` would call a truncatable torn tail (say, several zero-filled bytes
survived a crash and now sit past what looks like a bogus length prefix) can still surface at the reader
as a hard error, because the model above says that shape should never occur from an honest writer at all.
Nothing is ever posted from a record the CRC did not accept. The repair when this happens is the same
whether the reader or `replay` catches it first: the batcher (or any other reader) fails loud, and the
sequencer's own `--truncate-torn` restart is what repairs the log — Tiber's compose deliberately runs
without that flag, so a torn tail there means refuse-to-start until an operator adds it, which is also
what keeps every non-newest segment free of torn tails in the first place.

**Bounded stall, not a gap.** A record whose declared length exceeds what currently remains in the file,
followed later by a genuinely valid record, cannot be told apart at the newest segment's tail from an
honest in-progress append — both look like "not enough bytes yet." The reader waits (`Ok(None)`) until
either the file grows past `offset + 4 + declared_len` (bounded: `declared_len <= MAX_FRAME_LEN`, so this
resolves once the writer catches up) or the next `replay --truncate-torn` truncates from that offset,
which is a corruption scenario (it deletes the valid record sitting after it), not the honest one this
crate is designed around.

## What it deliberately does not own

Sealing cadence, admission, execution, and the sequencer's own migration/reconciliation state machine
(`--write-profile-json`, `RecoveryError::ProfileJsonMismatch`) stay sequencer-side, built on top of this
crate's pure record format. `rome-zk-sequencer::recovery::log_numbering_origin` is a thin wrapper over this
crate's function of the same name, mapping its error onto the sequencer's own `RecoveryError::LogNumbering`
so every one of that check's call sites (before a `profile.json` write, before replay, in the batcher's
anchor resolution, and the stored-`first_block` value check) sees the same error type it always has.

## Consumers

- **`rome-zk-sequencer`** re-exports this crate's whole public surface under its historical
  `rome_zk_sequencer::log`/`rome_zk_sequencer::header` module paths, so no existing call site changed. It
  is the only writer (`LogWriter`); `sealer.rs`, `recovery.rs` and `signing.rs` are its readers/signers.
- **`rome-zk-batcher`** reads through the sequencer's re-export (`source.rs` opens a `LogReader` at the
  chain's resume point; `anchor.rs` calls `log_numbering_origin` via `rome_zk_sequencer::recovery` — its
  tests use `replay`) — still a genuine dependency on `rome-zk-sequencer`, independent of this move (see
  that crate's README for why).
- **`rome-zk-derive`** and the indexer are downstream readers of the same record format, one level
  removed: they read the batcher's decoded channel stream (`rome-zk-channel`), which was itself built from
  records this crate's `LogReader` yielded.
