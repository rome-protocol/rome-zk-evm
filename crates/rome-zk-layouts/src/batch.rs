//! The zk-inbox batch account layout ("ZKBT"). Byte-exact, not borsh — the on-chain
//! program mutates it in place (realloc'd to `account_len(expected_count)`), so a fixed layout with
//! documented offsets is what both the program and off-chain readers rely on.
//!
//! ```text
//! magic 'ZKBT' u32 | version u8 | chain_id u64 | batch u64 | open_slot u64 | expected_count u32
//! | leaves_present u32 | finalized u8 | settlement_program [32] | authority [32] | root [32]
//! | forced_root [32] | acc [32] | finalize_cursor u32 | open_unix_ts i64
//! | leaf_present_bitmap [ceil(expected_count/8)] | leaf_hashes [32 × expected_count]
//! ```
//! All integers little-endian. `leaf_present_bitmap` is the source of truth for "is leaf `idx`
//! present" — never a zero-hash sentinel (a real leaf hashing to zero is astronomically unlikely, but
//! that must not be relied on).
//!
//! **Header version 2.** `open_unix_ts` is the committed
//! `Clock::unix_timestamp` reading `OpenBatch` takes at the same time as `open_slot` (one `Clock::get()`
//! call for both fields) — the anchor `rome-zk-derive`'s one-sided drift bound
//! (`block.timestamp <= open_unix_ts + max_drift_secs`) checks every block against. It is appended after
//! `finalize_cursor`, **not** inserted mid-header, so every existing offset (`OFF_MAGIC` through
//! `OFF_FINALIZE_CURSOR`) is unchanged — only `HEADER_LEN_V2` (and everything derived from it:
//! `bitmap_len`/`leaves_offset_for`/`account_len_for`) moves. It is not part of `acc`: the accumulator still
//! binds only the DA bytes (`chain_id ‖ batch ‖ open_slot ‖ expected_count ‖ root ‖ forced_root`) — the
//! clock reading is authoritative because the program wrote it, not because it is committed into the
//! Merkle/acc chain. There is no migration: a v1 account (`OFF_VERSION == 1`) is refused by every reader
//! (`BadVersion`) — Tiber's v1 batches are wiped by the chain reset, never
//! upgraded in place.
//!
//! **Header version 3** appends the batch's deposit range after `open_unix_ts`, the same way v2
//! appended that field:
//!
//! ```text
//! | (v3 only) deposit_from u64 | deposit_to u64 | deposit_hash_from [32] | deposit_hash_to [32]
//! ```
//!
//! The header goes from 210 to 290 bytes and every v2 offset stays where it is. [`read`] accepts v2 and
//! v3. [`write_header`] still writes v2 (the inbox keeps writing v2 until it learns deposits) and
//! [`write_header_v3`] writes v3. Because the header length depends on the account's version, the
//! version-taking [`header_len`], [`leaves_offset_for`] and [`account_len_for`] are the only way to get a
//! length or an offset, so a reader has to say which version it is reading; [`leaf_offsets`] does that
//! from a raw account's own version byte.

pub const MAGIC: u32 = 0x5a4b_4254; // "ZKBT"
/// The version [`write_header`] produces.
pub const VERSION: u8 = 2;
/// The version with the deposit range appended; see the module doc.
pub const VERSION_V3: u8 = 3;

pub const OFF_MAGIC: usize = 0;
pub const OFF_VERSION: usize = 4;
pub const OFF_CHAIN_ID: usize = 5;
pub const OFF_BATCH: usize = 13;
pub const OFF_OPEN_SLOT: usize = 21;
pub const OFF_EXPECTED_COUNT: usize = 29;
pub const OFF_LEAVES_PRESENT: usize = 33;
pub const OFF_FINALIZED: usize = 37;
pub const OFF_SETTLEMENT_PROGRAM: usize = 38;
pub const OFF_AUTHORITY: usize = 70;
pub const OFF_ROOT: usize = 102;
pub const OFF_FORCED_ROOT: usize = 134;
pub const OFF_ACC: usize = 166;
pub const OFF_FINALIZE_CURSOR: usize = 198;
/// v2: the committed Solana clock reading `OpenBatch` takes alongside `open_slot` —
/// see this module's doc for why it lives here (appended, not part of `acc`).
pub const OFF_OPEN_UNIX_TS: usize = 202;
/// v3: first deposit index of the batch's range.
pub const OFF_DEPOSIT_FROM: usize = 210;
/// v3: end (exclusive) of the batch's deposit range.
pub const OFF_DEPOSIT_TO: usize = 218;
/// v3: the queue's hash-chain value before deposit `deposit_from`.
pub const OFF_DEPOSIT_HASH_FROM: usize = 226;
/// v3: the queue's hash-chain value before deposit `deposit_to`.
pub const OFF_DEPOSIT_HASH_TO: usize = 258;
/// End of the fixed v2 header; the presence bitmap starts here. v1 was 202 (no `open_unix_ts`); every v1
/// account is refused (`BadVersion`), never read at the old length. A v3 header is [`HEADER_LEN_V3`].
pub const HEADER_LEN_V2: usize = 210;
/// End of the fixed v3 header (v2's 210 plus the 80-byte deposit range).
pub const HEADER_LEN_V3: usize = 290;

/// Fixed header length for the account's `version` byte (2 or 3); `BadVersion` for any other.
pub fn header_len(version: u8) -> Result<usize, crate::LayoutError> {
    match version {
        VERSION => Ok(HEADER_LEN_V2),
        VERSION_V3 => Ok(HEADER_LEN_V3),
        _ => Err(crate::LayoutError::BadVersion),
    }
}

/// `ceil(expected_count / 8)` bytes for the leaf-presence bitmap.
pub fn bitmap_len(expected_count: u32) -> usize {
    (expected_count as usize).div_ceil(8)
}

/// Byte offset where `leaf_hashes` begins for an account of `version` (2 or 3):
/// `header_len(version) + bitmap_len(expected_count)`.
pub fn leaves_offset_for(version: u8, expected_count: u32) -> Result<usize, crate::LayoutError> {
    Ok(header_len(version)? + bitmap_len(expected_count))
}

/// Total account size for `expected_count` leaves, for an account of `version` (2 or 3).
pub fn account_len_for(version: u8, expected_count: u32) -> Result<usize, crate::LayoutError> {
    Ok(leaves_offset_for(version, expected_count)? + 32 * expected_count as usize)
}

/// Where the presence bitmap and the leaf hashes start in a raw batch account, `(bitmap_offset,
/// leaves_offset)`, taken from the account's own version byte. For a reader that indexes the raw bytes
/// directly instead of going through [`read`]: it cannot take the offsets from a constant, because v2 and
/// v3 headers differ in length. `BadVersion` for any other version byte, `TooShort` when `d` is too short
/// to hold the version byte.
pub fn leaf_offsets(d: &[u8], expected_count: u32) -> Result<(usize, usize), crate::LayoutError> {
    let version = *d.get(OFF_VERSION).ok_or(crate::LayoutError::TooShort {
        need: OFF_VERSION + 1,
        got: d.len(),
    })?;
    Ok((
        header_len(version)?,
        leaves_offset_for(version, expected_count)?,
    ))
}

/// `["batch", settlement_program, chain_id, batch]` — owned by the inbox program. The settlement program is
/// part of the key so a chain's batch accounts can only be created through its own settlement program.
/// `zk-settlement` derives the same address (passing the inbox program id, read from its own registry
/// account, and its own program id as `settlement_program`) to validate the inbox batch account it reads at
/// `PostRoot`/`PostRootProved` — same seeds, different `program_id`.
#[inline]
pub fn seeds(settlement_program: &[u8; 32], chain_id: u64, batch: u64) -> [Vec<u8>; 4] {
    [
        b"batch".to_vec(),
        settlement_program.to_vec(),
        chain_id.to_le_bytes().to_vec(),
        batch.to_le_bytes().to_vec(),
    ]
}

/// Derives the batch PDA under `program_id` — the one place this derivation is computed. `program_id` is the inbox
/// program when the inbox itself derives its own account, and also the inbox program when `zk-settlement` derives it to
/// check an account it only reads (the settlement program never owns this account).
#[cfg(feature = "solana")]
#[inline]
pub fn pda(
    program_id: &solana_program::pubkey::Pubkey,
    settlement_program: &solana_program::pubkey::Pubkey,
    chain_id: u64,
    batch: u64,
) -> (solana_program::pubkey::Pubkey, u8) {
    let s = seeds(&settlement_program.to_bytes(), chain_id, batch);
    solana_program::pubkey::Pubkey::find_program_address(&[&s[0], &s[1], &s[2], &s[3]], program_id)
}

/// Field-for-field decode of a batch account's fixed header (not the bitmap or leaf hashes, which the
/// caller indexes directly via [`leaves_offset_for`]). Layout structs carry raw `[u8; 32]`; only `pda()` uses
/// `solana_program::Pubkey`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchFields {
    pub chain_id: u64,
    pub batch: u64,
    pub open_slot: u64,
    pub expected_count: u32,
    pub leaves_present: u32,
    pub finalized: bool,
    pub settlement_program: [u8; 32],
    pub authority: [u8; 32],
    pub root: [u8; 32],
    pub forced_root: [u8; 32],
    pub acc: [u8; 32],
    pub finalize_cursor: u32,
    /// v2: the committed `Clock::unix_timestamp` `OpenBatch` wrote alongside
    /// `open_slot`. Not part of `acc`.
    pub open_unix_ts: i64,
    /// `Some` for a v3 header, `None` for v2.
    pub deposit: Option<BatchDeposit>,
}

/// The v3 deposit range: the four fields appended after `open_unix_ts`. The range is `[from, to)`;
/// `hash_from` and `hash_to` are the queue's hash-chain values before deposit `from` and before
/// deposit `to`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchDeposit {
    pub from: u64,
    pub to: u64,
    pub hash_from: [u8; 32],
    pub hash_to: [u8; 32],
}

/// Validates magic + version + minimum length and decodes the fixed header. Does not check the
/// account's owner — callers hold `program_id` and must check that themselves.
///
/// **Check order matters for v1 (there is no migration).** A v1 account is exactly 202 bytes — short
/// of v2's 210-byte `HEADER_LEN_V2` — but magic and every field up to `finalize_cursor` are still valid v1
/// bytes; a length check ahead of the version check would report a v1 account as merely `TooShort`
/// rather than naming the real cause. So magic and version are checked as soon as there are enough bytes
/// to read them (`OFF_VERSION + 1`), and only a header that passes both is then checked against the full
/// header length of the version it names — a v1 account gets `BadVersion`, never `TooShort`.
///
/// Accepts v2 (`deposit: None`, 210-byte header) and v3 (`deposit: Some`, 290-byte header); any other
/// version is `BadVersion`.
pub fn read(d: &[u8]) -> Result<BatchFields, crate::LayoutError> {
    if d.len() < OFF_VERSION + 1 {
        return Err(crate::LayoutError::TooShort {
            need: HEADER_LEN_V2,
            got: d.len(),
        });
    }
    if u32::from_le_bytes(d[OFF_MAGIC..OFF_MAGIC + 4].try_into().unwrap()) != MAGIC {
        return Err(crate::LayoutError::BadMagic);
    }
    let need = header_len(d[OFF_VERSION])?;
    if d.len() < need {
        return Err(crate::LayoutError::TooShort { need, got: d.len() });
    }
    let u32_at = |o: usize| u32::from_le_bytes(d[o..o + 4].try_into().unwrap());
    let u64_at = |o: usize| u64::from_le_bytes(d[o..o + 8].try_into().unwrap());
    let i64_at = |o: usize| i64::from_le_bytes(d[o..o + 8].try_into().unwrap());
    let b32_at = |o: usize| -> [u8; 32] { d[o..o + 32].try_into().unwrap() };
    Ok(BatchFields {
        chain_id: u64_at(OFF_CHAIN_ID),
        batch: u64_at(OFF_BATCH),
        open_slot: u64_at(OFF_OPEN_SLOT),
        expected_count: u32_at(OFF_EXPECTED_COUNT),
        leaves_present: u32_at(OFF_LEAVES_PRESENT),
        finalized: d[OFF_FINALIZED] != 0,
        settlement_program: b32_at(OFF_SETTLEMENT_PROGRAM),
        authority: b32_at(OFF_AUTHORITY),
        root: b32_at(OFF_ROOT),
        forced_root: b32_at(OFF_FORCED_ROOT),
        acc: b32_at(OFF_ACC),
        finalize_cursor: u32_at(OFF_FINALIZE_CURSOR),
        open_unix_ts: i64_at(OFF_OPEN_UNIX_TS),
        deposit: (d[OFF_VERSION] == VERSION_V3).then(|| BatchDeposit {
            from: u64_at(OFF_DEPOSIT_FROM),
            to: u64_at(OFF_DEPOSIT_TO),
            hash_from: b32_at(OFF_DEPOSIT_HASH_FROM),
            hash_to: b32_at(OFF_DEPOSIT_HASH_TO),
        }),
    })
}

/// Encodes a **v2** batch account's fixed header (the inverse of [`read`] for a v2 header, for the
/// header portion only — the caller writes the presence bitmap and leaf hashes itself via
/// [`leaves_offset_for`]). Returns exactly [`HEADER_LEN_V2`] bytes. A v2 header cannot carry a deposit
/// range, so `f.deposit` must be `None`; use [`write_header_v3`] for one that has it.
#[inline]
pub fn write_header(f: &BatchFields) -> [u8; HEADER_LEN_V2] {
    debug_assert!(f.deposit.is_none(), "a v2 header has no deposit range");
    let mut d = [0u8; HEADER_LEN_V2];
    d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
    d[OFF_VERSION] = VERSION;
    d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].copy_from_slice(&f.chain_id.to_le_bytes());
    d[OFF_BATCH..OFF_BATCH + 8].copy_from_slice(&f.batch.to_le_bytes());
    d[OFF_OPEN_SLOT..OFF_OPEN_SLOT + 8].copy_from_slice(&f.open_slot.to_le_bytes());
    d[OFF_EXPECTED_COUNT..OFF_EXPECTED_COUNT + 4].copy_from_slice(&f.expected_count.to_le_bytes());
    d[OFF_LEAVES_PRESENT..OFF_LEAVES_PRESENT + 4].copy_from_slice(&f.leaves_present.to_le_bytes());
    d[OFF_FINALIZED] = f.finalized as u8;
    d[OFF_SETTLEMENT_PROGRAM..OFF_SETTLEMENT_PROGRAM + 32].copy_from_slice(&f.settlement_program);
    d[OFF_AUTHORITY..OFF_AUTHORITY + 32].copy_from_slice(&f.authority);
    d[OFF_ROOT..OFF_ROOT + 32].copy_from_slice(&f.root);
    d[OFF_FORCED_ROOT..OFF_FORCED_ROOT + 32].copy_from_slice(&f.forced_root);
    d[OFF_ACC..OFF_ACC + 32].copy_from_slice(&f.acc);
    d[OFF_FINALIZE_CURSOR..OFF_FINALIZE_CURSOR + 4]
        .copy_from_slice(&f.finalize_cursor.to_le_bytes());
    d[OFF_OPEN_UNIX_TS..OFF_OPEN_UNIX_TS + 8].copy_from_slice(&f.open_unix_ts.to_le_bytes());
    d
}

/// Encodes a **v3** batch account's fixed header: [`write_header`]'s bytes with the version byte 3 and
/// the deposit range appended. Returns exactly [`HEADER_LEN_V3`] bytes, or `None` when `f.deposit` is
/// `None` (there is nothing to put in the appended fields).
#[inline]
pub fn write_header_v3(f: &BatchFields) -> Option<[u8; HEADER_LEN_V3]> {
    let dep = f.deposit.as_ref()?;
    let mut d = [0u8; HEADER_LEN_V3];
    d[..HEADER_LEN_V2].copy_from_slice(&write_header(&BatchFields {
        deposit: None,
        ..f.clone()
    }));
    d[OFF_VERSION] = VERSION_V3;
    d[OFF_DEPOSIT_FROM..OFF_DEPOSIT_FROM + 8].copy_from_slice(&dep.from.to_le_bytes());
    d[OFF_DEPOSIT_TO..OFF_DEPOSIT_TO + 8].copy_from_slice(&dep.to.to_le_bytes());
    d[OFF_DEPOSIT_HASH_FROM..OFF_DEPOSIT_HASH_FROM + 32].copy_from_slice(&dep.hash_from);
    d[OFF_DEPOSIT_HASH_TO..OFF_DEPOSIT_HASH_TO + 32].copy_from_slice(&dep.hash_to);
    Some(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte-golden test (contract), **deliberately regenerated for header v2**:
    /// a **literal** 210-byte vector (every byte position written as a number, never
    /// through `OFF_*`) decodes to the exact fields `write_header` encoded — pins the field order and
    /// every offset independently of the constants both functions share. The v1 202-byte literal this
    /// replaced is gone on purpose (no migration) — `a_v1_shaped_account_is_refused` below
    /// is the golden for the byte shape this test no longer accepts.
    #[test]
    fn write_header_produces_a_known_byte_vector() {
        let f = BatchFields {
            chain_id: 7,
            batch: 3,
            open_slot: 100,
            expected_count: 5,
            leaves_present: 2,
            finalized: true,
            settlement_program: [0x11u8; 32],
            authority: [0x22u8; 32],
            root: [0x33u8; 32],
            forced_root: [0x44u8; 32],
            acc: [0x55u8; 32],
            finalize_cursor: 9,
            open_unix_ts: 1_700_000_000,
            deposit: None,
        };
        let d = write_header(&f);
        #[rustfmt::skip]
        let expected: [u8; HEADER_LEN_V2] = [
            // magic 'ZKBT' = 0x5a4b_4254 LE
            0x54, 0x42, 0x4b, 0x5a,
            // version = 2
            2,
            // chain_id = 7 LE u64
            7, 0, 0, 0, 0, 0, 0, 0,
            // batch = 3 LE u64
            3, 0, 0, 0, 0, 0, 0, 0,
            // open_slot = 100 LE u64
            100, 0, 0, 0, 0, 0, 0, 0,
            // expected_count = 5 LE u32
            5, 0, 0, 0,
            // leaves_present = 2 LE u32
            2, 0, 0, 0,
            // finalized
            1,
            // settlement_program: 32 bytes of 0x11
            0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
            0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
            0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
            0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
            // authority: 32 bytes of 0x22
            0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22,
            0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22,
            0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22,
            0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22,
            // root: 32 bytes of 0x33
            0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33,
            0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33,
            0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33,
            0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33,
            // forced_root: 32 bytes of 0x44
            0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
            0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
            0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
            0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
            // acc: 32 bytes of 0x55
            0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55,
            0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55,
            0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55,
            0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55,
            // finalize_cursor = 9 LE u32
            9, 0, 0, 0,
            // open_unix_ts = 1_700_000_000 LE i64 (v2)
            0x00, 0xf1, 0x53, 0x65, 0x00, 0x00, 0x00, 0x00,
        ];
        assert_eq!(d, expected);
        assert_eq!(read(&d).unwrap(), f);
    }

    fn v3_fields() -> BatchFields {
        BatchFields {
            chain_id: 7,
            batch: 3,
            open_slot: 100,
            expected_count: 5,
            leaves_present: 2,
            finalized: true,
            settlement_program: [0x11u8; 32],
            authority: [0x22u8; 32],
            root: [0x33u8; 32],
            forced_root: [0x44u8; 32],
            acc: [0x55u8; 32],
            finalize_cursor: 9,
            open_unix_ts: 1_700_000_000,
            deposit: Some(BatchDeposit {
                from: 4,
                to: 6,
                hash_from: [0x66u8; 32],
                hash_to: [0x77u8; 32],
            }),
        }
    }

    /// Byte-golden test for v3: a **literal** 290-byte vector (no `OFF_*`). Bytes 0..210 are the v2
    /// literal above with the version byte changed to 3; the deposit range sits at 210, 218, 226, 258.
    #[test]
    fn write_header_v3_produces_a_known_byte_vector() {
        let f = v3_fields();
        let d = write_header_v3(&f).unwrap();
        #[rustfmt::skip]
        let expected: [u8; 290] = [
            // magic 'ZKBT' = 0x5a4b_4254 LE
            0x54, 0x42, 0x4b, 0x5a,
            // version = 3
            3,
            // chain_id = 7, batch = 3, open_slot = 100 (LE u64 each)
            7, 0, 0, 0, 0, 0, 0, 0,
            3, 0, 0, 0, 0, 0, 0, 0,
            100, 0, 0, 0, 0, 0, 0, 0,
            // expected_count = 5, leaves_present = 2 (LE u32 each)
            5, 0, 0, 0,
            2, 0, 0, 0,
            // finalized
            1,
            // settlement_program (offset 38): 32 bytes of 0x11
            0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
            0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
            0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
            0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
            // authority (70): 0x22
            0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22,
            0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22,
            0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22,
            0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22,
            // root (102): 0x33
            0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33,
            0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33,
            0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33,
            0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33,
            // forced_root (134): 0x44
            0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
            0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
            0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
            0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
            // acc (166): 0x55
            0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55,
            0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55,
            0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55,
            0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55,
            // finalize_cursor = 9 (198)
            9, 0, 0, 0,
            // open_unix_ts = 1_700_000_000 (202)
            0x00, 0xf1, 0x53, 0x65, 0x00, 0x00, 0x00, 0x00,
            // deposit_from = 4 (210)
            4, 0, 0, 0, 0, 0, 0, 0,
            // deposit_to = 6 (218)
            6, 0, 0, 0, 0, 0, 0, 0,
            // deposit_hash_from (226): 0x66
            0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
            0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
            0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
            0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
            // deposit_hash_to (258): 0x77
            0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77,
            0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77,
            0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77,
            0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77,
        ];
        assert_eq!(d, expected);
        assert_eq!(read(&d).unwrap(), f);
    }

    /// A v2 header reads exactly as before: version 2, 210 bytes, no deposit range, and the same
    /// fields as the v3 literal's first 210 bytes.
    #[test]
    fn a_v2_header_reads_with_no_deposit_range() {
        let v3 = v3_fields();
        let v2 = BatchFields {
            deposit: None,
            ..v3.clone()
        };
        let d = write_header(&v2);
        assert_eq!(d.len(), 210);
        assert_eq!(d[OFF_VERSION], 2);
        assert_eq!(read(&d).unwrap(), v2);
        // v3 and v2 differ only in the version byte and the appended range.
        let d3 = write_header_v3(&v3).unwrap();
        assert_eq!(d3[..OFF_VERSION], d[..OFF_VERSION]);
        assert_eq!(d3[OFF_VERSION + 1..HEADER_LEN_V2], d[OFF_VERSION + 1..]);
        // A v2 account whose allocation is longer than the header (bitmap, leaves) is still v2.
        let mut full = d.to_vec();
        full.resize(account_len_for(2, 5).unwrap(), 0xee);
        assert_eq!(read(&full).unwrap().deposit, None);
    }

    #[test]
    fn a_v3_header_shorter_than_290_bytes_is_too_short() {
        let d = write_header_v3(&v3_fields()).unwrap();
        assert_eq!(
            read(&d[..HEADER_LEN_V3 - 1]).unwrap_err(),
            crate::LayoutError::TooShort {
                need: HEADER_LEN_V3,
                got: HEADER_LEN_V3 - 1
            }
        );
        // 210 bytes with the v3 version byte is not enough either.
        let mut short = write_header(&BatchFields {
            deposit: None,
            ..v3_fields()
        });
        short[OFF_VERSION] = 3;
        assert!(matches!(
            read(&short).unwrap_err(),
            crate::LayoutError::TooShort { need: 290, .. }
        ));
    }

    #[test]
    fn versions_other_than_2_and_3_are_refused() {
        for v in [0u8, 1, 4, 0xff] {
            let mut d = write_header_v3(&v3_fields()).unwrap();
            d[OFF_VERSION] = v;
            assert_eq!(read(&d).unwrap_err(), crate::LayoutError::BadVersion, "{v}");
            assert_eq!(header_len(v), Err(crate::LayoutError::BadVersion));
            assert_eq!(account_len_for(v, 1), Err(crate::LayoutError::BadVersion));
        }
    }

    #[test]
    fn write_header_v3_needs_a_deposit_range() {
        let f = BatchFields {
            deposit: None,
            ..v3_fields()
        };
        assert!(write_header_v3(&f).is_none());
    }

    #[test]
    fn version_taking_lengths_follow_the_formula_and_v3_adds_80() {
        assert_eq!(header_len(2), Ok(210));
        assert_eq!(header_len(3), Ok(290));
        for n in [0u32, 1, 8, 9, 900] {
            let v2_leaves = HEADER_LEN_V2 + bitmap_len(n);
            assert_eq!(leaves_offset_for(2, n), Ok(v2_leaves));
            assert_eq!(account_len_for(2, n), Ok(v2_leaves + 32 * n as usize));
            assert_eq!(leaves_offset_for(3, n), Ok(v2_leaves + 80));
            assert_eq!(account_len_for(3, n), Ok(v2_leaves + 80 + 32 * n as usize));
        }
    }

    /// 900 leaves in a v3 account: 290 + 113 (bitmap) + 28,800 (leaf hashes) = 29,203 bytes, inside
    /// the open allocation plus two grows (30,720 bytes).
    #[test]
    fn a_v3_account_for_900_leaves_is_29_203_bytes() {
        assert_eq!(account_len_for(3, 900), Ok(29_203));
        assert_eq!(leaves_offset_for(3, 900), Ok(403));
        // Open plus two grows at 10,240 bytes each.
        assert!(account_len_for(3, 900).unwrap() <= 3 * 10_240);
    }

    /// A v1-shaped account (version byte 1, the old 202-byte length, no
    /// `open_unix_ts`) is refused by `read` — `BadVersion`, never silently accepted at the old length or
    /// zero-padded. No migration: Tiber's v1 batches are wiped by the chain reset.
    #[test]
    fn a_v1_shaped_account_is_refused() {
        let mut d = vec![0u8; 202];
        d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
        d[OFF_VERSION] = 1;
        d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].copy_from_slice(&7u64.to_le_bytes());
        assert_eq!(read(&d).unwrap_err(), crate::LayoutError::BadVersion);
    }

    #[test]
    fn bitmap_and_leaves_offset_and_account_len_match_the_documented_formula() {
        assert_eq!(bitmap_len(0), 0);
        assert_eq!(bitmap_len(1), 1);
        assert_eq!(bitmap_len(8), 1);
        assert_eq!(bitmap_len(9), 2);
        assert_eq!(leaves_offset_for(2, 9), Ok(HEADER_LEN_V2 + 2));
        assert_eq!(account_len_for(2, 9), Ok(HEADER_LEN_V2 + 2 + 32 * 9));
    }

    #[test]
    fn leaf_offsets_follow_the_accounts_own_version_byte() {
        let mut d = vec![0u8; account_len_for(VERSION_V3, 9).unwrap()];
        d[OFF_VERSION] = VERSION;
        assert_eq!(leaf_offsets(&d, 9), Ok((210, 212)));
        d[OFF_VERSION] = VERSION_V3;
        assert_eq!(leaf_offsets(&d, 9), Ok((290, 292)));
        d[OFF_VERSION] = 1;
        assert_eq!(leaf_offsets(&d, 9), Err(crate::LayoutError::BadVersion));
        assert!(matches!(
            leaf_offsets(&d[..OFF_VERSION], 9),
            Err(crate::LayoutError::TooShort { .. })
        ));
    }

    fn build(chain_id: u64, batch: u64, expected_count: u32) -> Vec<u8> {
        let mut d = vec![0u8; account_len_for(VERSION, expected_count).unwrap()];
        d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
        d[OFF_VERSION] = VERSION;
        d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].copy_from_slice(&chain_id.to_le_bytes());
        d[OFF_BATCH..OFF_BATCH + 8].copy_from_slice(&batch.to_le_bytes());
        d[OFF_EXPECTED_COUNT..OFF_EXPECTED_COUNT + 4]
            .copy_from_slice(&expected_count.to_le_bytes());
        d
    }

    #[test]
    fn read_round_trips_a_hand_built_header() {
        let d = build(7, 3, 5);
        let f = read(&d).unwrap();
        assert_eq!(f.chain_id, 7);
        assert_eq!(f.batch, 3);
        assert_eq!(f.expected_count, 5);
        assert!(!f.finalized);
    }

    #[test]
    fn read_rejects_bad_magic() {
        let mut d = build(1, 1, 1);
        d[OFF_MAGIC] ^= 0xff;
        assert_eq!(read(&d).unwrap_err(), crate::LayoutError::BadMagic);
    }

    #[test]
    fn read_rejects_too_short() {
        // Correct magic and version so this test isolates the length check itself (checked last, after
        // magic/version) — a buffer this short with garbage magic/version would hit those checks first.
        let mut d = vec![0u8; HEADER_LEN_V2 - 1];
        d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
        d[OFF_VERSION] = VERSION;
        assert!(matches!(
            read(&d).unwrap_err(),
            crate::LayoutError::TooShort { .. }
        ));
    }

    /// A buffer too short to even hold magic + version is `TooShort`, not a panic.
    #[test]
    fn read_rejects_a_buffer_shorter_than_magic_and_version() {
        let d = vec![0u8; 3];
        assert!(matches!(
            read(&d).unwrap_err(),
            crate::LayoutError::TooShort { .. }
        ));
    }

    #[test]
    fn seeds_golden_bytes_for_fixed_inputs() {
        let s = seeds(&[0x5Au8; 32], 0x0102_0304_0506_0708, 9);
        assert_eq!(s[0], b"batch".to_vec());
        assert_eq!(s[1], vec![0x5Au8; 32]);
        assert_eq!(s[2], vec![0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
        assert_eq!(s[3], vec![9, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[cfg(feature = "solana")]
    #[test]
    fn pda_is_deterministic_and_varies_with_batch_and_program() {
        let program = solana_program::pubkey::Pubkey::new_unique();
        let other = solana_program::pubkey::Pubkey::new_unique();
        let settlement = solana_program::pubkey::Pubkey::new_unique();
        let other_settlement = solana_program::pubkey::Pubkey::new_unique();
        let (a1, _) = pda(&program, &settlement, 7, 1);
        let (a2, _) = pda(&program, &settlement, 7, 1);
        assert_eq!(a1, a2);
        let (b, _) = pda(&program, &settlement, 7, 2);
        assert_ne!(a1, b);
        let (c, _) = pda(&other, &settlement, 7, 1);
        assert_ne!(a1, c);
        // The settlement program is part of the key: another program's chain id maps elsewhere.
        let (d, _) = pda(&program, &other_settlement, 7, 1);
        assert_ne!(a1, d);
    }
}
