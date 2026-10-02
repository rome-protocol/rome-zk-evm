//! The zk-inbox `batch_cursor` account layout ("ZKBC", PDA `["batch_cursor",
//! chain_id]`, one per chain, owned by the inbox program). Holds `next_batch`, the only batch id
//! `OpenBatch` may open next; `OpenBatch` requires `batch == next_batch` and increments it on success,
//! and nothing — not even `AbandonBatch` — ever decrements it. An id, once assigned, can never be
//! reused: this is what stops a stale chunk PDA left over from an abandoned attempt at some id from ever
//! being sealed into a *later* batch opened at that same id (closed at the core rather than by the
//! batcher's own resume scan alone).
//!
//! ```text
//! magic 'ZKBC' u32 | version u8 | chain_id u64 | next_batch u64
//! ```
//! All integers little-endian. Fixed size, no variable-length tail.

pub const MAGIC: u32 = 0x5a4b_4243; // "ZKBC"
pub const VERSION: u8 = 1;

pub const OFF_MAGIC: usize = 0;
pub const OFF_VERSION: usize = 4;
pub const OFF_CHAIN_ID: usize = 5;
pub const OFF_NEXT_BATCH: usize = 13;
/// Total account size — fixed, no variable-length tail.
pub const LEN: usize = 21;

/// `["batch_cursor", chain_id]`.
#[inline]
pub fn seeds(chain_id: u64) -> [Vec<u8>; 2] {
    [b"batch_cursor".to_vec(), chain_id.to_le_bytes().to_vec()]
}

/// Derives the batch-cursor PDA under `program_id` (the inbox program) — the one place this derivation
/// is computed.
#[cfg(feature = "solana")]
#[inline]
pub fn pda(
    program_id: &solana_program::pubkey::Pubkey,
    chain_id: u64,
) -> (solana_program::pubkey::Pubkey, u8) {
    let s = seeds(chain_id);
    solana_program::pubkey::Pubkey::find_program_address(&[&s[0], &s[1]], program_id)
}

/// Field-for-field decode of a cursor account. Pubkeys are raw `[u8; 32]` elsewhere in this crate (only
/// `pda` above needs the real type). Does not check the account's owner or PDA seeds — callers hold
/// `program_id` and must check those themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorFields {
    pub chain_id: u64,
    pub next_batch: u64,
}

/// Validates magic + version + minimum length and decodes both fields.
pub fn read(d: &[u8]) -> Result<CursorFields, crate::LayoutError> {
    if d.len() < LEN {
        return Err(crate::LayoutError::TooShort {
            need: LEN,
            got: d.len(),
        });
    }
    if u32::from_le_bytes(d[OFF_MAGIC..OFF_MAGIC + 4].try_into().unwrap()) != MAGIC {
        return Err(crate::LayoutError::BadMagic);
    }
    if d[OFF_VERSION] != VERSION {
        return Err(crate::LayoutError::BadVersion);
    }
    Ok(CursorFields {
        chain_id: u64::from_le_bytes(d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].try_into().unwrap()),
        next_batch: u64::from_le_bytes(d[OFF_NEXT_BATCH..OFF_NEXT_BATCH + 8].try_into().unwrap()),
    })
}

/// Encodes a cursor account (the inverse of [`read`]). Returns exactly [`LEN`] bytes.
#[inline]
pub fn write(f: &CursorFields) -> [u8; LEN] {
    let mut d = [0u8; LEN];
    d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
    d[OFF_VERSION] = VERSION;
    d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].copy_from_slice(&f.chain_id.to_le_bytes());
    d[OFF_NEXT_BATCH..OFF_NEXT_BATCH + 8].copy_from_slice(&f.next_batch.to_le_bytes());
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(chain_id: u64, next_batch: u64) -> Vec<u8> {
        write(&CursorFields {
            chain_id,
            next_batch,
        })
        .to_vec()
    }

    /// Byte-golden test (contract): a **literal** 21-byte vector (every byte position
    /// written as a number, never through `OFF_*`/`MAGIC`) decodes to the exact fields `write` encoded.
    #[test]
    fn write_produces_a_known_byte_vector() {
        let f = CursorFields {
            chain_id: 7,
            next_batch: 42,
        };
        let d = write(&f);
        #[rustfmt::skip]
        let expected: [u8; LEN] = [
            // magic 'ZKBC' = 0x5a4b_4243 LE
            0x43, 0x42, 0x4b, 0x5a,
            // version
            1,
            // chain_id = 7 LE u64
            7, 0, 0, 0, 0, 0, 0, 0,
            // next_batch = 42 LE u64
            42, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert_eq!(d, expected);
        assert_eq!(read(&d).unwrap(), f);
    }

    #[test]
    fn read_round_trips_a_hand_built_account() {
        let d = build(7, 42);
        let f = read(&d).unwrap();
        assert_eq!(f.chain_id, 7);
        assert_eq!(f.next_batch, 42);
    }

    #[test]
    fn read_rejects_bad_magic() {
        let mut d = build(1, 0);
        d[OFF_MAGIC] ^= 0xff;
        assert_eq!(read(&d).unwrap_err(), crate::LayoutError::BadMagic);
    }

    #[test]
    fn read_rejects_bad_version() {
        let mut d = build(1, 0);
        d[OFF_VERSION] = 0xff;
        assert_eq!(read(&d).unwrap_err(), crate::LayoutError::BadVersion);
    }

    #[test]
    fn read_rejects_too_short() {
        let d = vec![0u8; LEN - 1];
        assert!(matches!(
            read(&d).unwrap_err(),
            crate::LayoutError::TooShort { .. }
        ));
    }

    #[test]
    fn seeds_golden_bytes_for_a_fixed_chain_id() {
        let s = seeds(0x0102_0304_0506_0708);
        assert_eq!(s[0], b"batch_cursor".to_vec());
        assert_eq!(s[1], vec![0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
    }

    #[cfg(feature = "solana")]
    #[test]
    fn pda_is_deterministic_and_varies_with_chain_id() {
        let program = solana_program::pubkey::Pubkey::new_unique();
        let (a1, _) = pda(&program, 7);
        let (a2, _) = pda(&program, 7);
        assert_eq!(a1, a2);
        let (b, _) = pda(&program, 8);
        assert_ne!(a1, b);
    }
}
