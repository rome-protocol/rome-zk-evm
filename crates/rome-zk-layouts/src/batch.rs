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
//! `OFF_FINALIZE_CURSOR`) is unchanged — only `HEADER_LEN` (and everything derived from it:
//! `bitmap_len`/`leaves_offset`/`account_len`) moves. It is not part of `acc`: the accumulator still
//! binds only the DA bytes (`chain_id ‖ batch ‖ open_slot ‖ expected_count ‖ root ‖ forced_root`) — the
//! clock reading is authoritative because the program wrote it, not because it is committed into the
//! Merkle/acc chain. There is no migration: a v1 account (`OFF_VERSION == 1`) is refused by every reader
//! (`BadVersion`) — Tiber's v1 batches are wiped by the chain reset, never
//! upgraded in place.

pub const MAGIC: u32 = 0x5a4b_4254; // "ZKBT"
pub const VERSION: u8 = 2;

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
/// End of the fixed header; the presence bitmap starts here. v1 was 202 (no `open_unix_ts`); every v1
/// account is refused (`BadVersion`), never read at the old length.
pub const HEADER_LEN: usize = 210;

/// `ceil(expected_count / 8)` bytes for the leaf-presence bitmap.
pub fn bitmap_len(expected_count: u32) -> usize {
    (expected_count as usize).div_ceil(8)
}

/// Byte offset where `leaf_hashes` begins (`HEADER_LEN + bitmap_len(expected_count)`).
pub fn leaves_offset(expected_count: u32) -> usize {
    HEADER_LEN + bitmap_len(expected_count)
}

/// Total account size for `expected_count` leaves.
pub fn account_len(expected_count: u32) -> usize {
    leaves_offset(expected_count) + 32 * expected_count as usize
}

/// `["batch", chain_id, batch]` — owned by the inbox program. `zk-settlement` derives the same address
/// (passing the inbox program id, read from its own registry account) to validate the inbox batch
/// account it reads at `PostRoot`/`PostRootProved` — same seeds, different `program_id`.
#[inline]
pub fn seeds(chain_id: u64, batch: u64) -> [Vec<u8>; 3] {
    [
        b"batch".to_vec(),
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
    chain_id: u64,
    batch: u64,
) -> (solana_program::pubkey::Pubkey, u8) {
    let s = seeds(chain_id, batch);
    solana_program::pubkey::Pubkey::find_program_address(&[&s[0], &s[1], &s[2]], program_id)
}

/// Field-for-field decode of a batch account's fixed header (not the bitmap or leaf hashes, which the
/// caller indexes directly via [`leaves_offset`]). Layout structs carry raw `[u8; 32]`; only `pda()` uses
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
}

/// Validates magic + version + minimum length and decodes the fixed header. Does not check the
/// account's owner — callers hold `program_id` and must check that themselves.
///
/// **Check order matters for v1 (there is no migration).** A v1 account is exactly 202 bytes — short
/// of v2's 210-byte `HEADER_LEN` — but magic and every field up to `finalize_cursor` are still valid v1
/// bytes; a length check ahead of the version check would report a v1 account as merely `TooShort`
/// rather than naming the real cause. So magic and version are checked as soon as there are enough bytes
/// to read them (`OFF_VERSION + 1`), and only a header that passes both is then checked against the full
/// v2 `HEADER_LEN` — a v1 account gets `BadVersion`, never `TooShort`.
pub fn read(d: &[u8]) -> Result<BatchFields, crate::LayoutError> {
    if d.len() < OFF_VERSION + 1 {
        return Err(crate::LayoutError::TooShort {
            need: HEADER_LEN,
            got: d.len(),
        });
    }
    if u32::from_le_bytes(d[OFF_MAGIC..OFF_MAGIC + 4].try_into().unwrap()) != MAGIC {
        return Err(crate::LayoutError::BadMagic);
    }
    if d[OFF_VERSION] != VERSION {
        return Err(crate::LayoutError::BadVersion);
    }
    if d.len() < HEADER_LEN {
        return Err(crate::LayoutError::TooShort {
            need: HEADER_LEN,
            got: d.len(),
        });
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
    })
}

/// Encodes a batch account's fixed header (the inverse of [`read`], for the header portion only — the
/// caller writes the presence bitmap and leaf hashes itself via [`leaves_offset`]). Returns exactly
/// [`HEADER_LEN`] bytes.
#[inline]
pub fn write_header(f: &BatchFields) -> [u8; HEADER_LEN] {
    let mut d = [0u8; HEADER_LEN];
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
        };
        let d = write_header(&f);
        #[rustfmt::skip]
        let expected: [u8; HEADER_LEN] = [
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
        assert_eq!(leaves_offset(9), HEADER_LEN + 2);
        assert_eq!(account_len(9), HEADER_LEN + 2 + 32 * 9);
    }

    fn build(chain_id: u64, batch: u64, expected_count: u32) -> Vec<u8> {
        let mut d = vec![0u8; account_len(expected_count)];
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
        let mut d = vec![0u8; HEADER_LEN - 1];
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
        let s = seeds(0x0102_0304_0506_0708, 9);
        assert_eq!(s[0], b"batch".to_vec());
        assert_eq!(s[1], vec![0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
        assert_eq!(s[2], vec![9, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[cfg(feature = "solana")]
    #[test]
    fn pda_is_deterministic_and_varies_with_batch_and_program() {
        let program = solana_program::pubkey::Pubkey::new_unique();
        let other = solana_program::pubkey::Pubkey::new_unique();
        let (a1, _) = pda(&program, 7, 1);
        let (a2, _) = pda(&program, 7, 1);
        assert_eq!(a1, a2);
        let (b, _) = pda(&program, 7, 2);
        assert_ne!(a1, b);
        let (c, _) = pda(&other, 7, 1);
        assert_ne!(a1, c);
    }
}
