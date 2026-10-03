//! The zk-inbox `batch_cursor` account layout ("ZKBC", PDA `["batch_cursor",
//! settlement_program, chain_id]`, one per (settlement program, chain), owned by the inbox program). Holds `next_batch`, the only batch id
//! `OpenBatch` may open next; `OpenBatch` requires `batch == next_batch` and increments it on success,
//! and nothing — not even `AbandonBatch` — ever decrements it. An id, once assigned, can never be
//! reused: this is what stops a stale chunk PDA left over from an abandoned attempt at some id from ever
//! being sealed into a *later* batch opened at that same id (closed at the core rather than by the
//! batcher's own resume scan alone).
//!
//! ```text
//! magic 'ZKBC' u32 | version u8 | chain_id u64 | next_batch u64
//! | (v2 only) deposit_next u64 | deposit_hash [32] | deposit_final u64
//! ```
//! All integers little-endian. Fixed size, no variable-length tail.
//!
//! **Version 2** appends the deposit queue's cursor after `next_batch`, so the account goes from 21 to
//! 69 bytes and every v1 offset stays where it is. `deposit_next` is the index of the first deposit
//! the next batch takes, `deposit_hash` the queue's hash-chain value before that deposit, and
//! `deposit_final` the end of the deposit range of the highest batch closed after its root went final.
//! [`read`] accepts both versions; [`write`] still writes v1 (the inbox keeps writing v1 until it
//! learns deposits) and [`write_v2`] writes v2.

pub const MAGIC: u32 = 0x5a4b_4243; // "ZKBC"
/// The version [`write`] produces.
pub const VERSION: u8 = 1;
/// The version with the deposit cursor appended; see the module doc.
pub const VERSION_V2: u8 = 2;

pub const OFF_MAGIC: usize = 0;
pub const OFF_VERSION: usize = 4;
pub const OFF_CHAIN_ID: usize = 5;
pub const OFF_NEXT_BATCH: usize = 13;
/// v2: index of the first deposit the next batch takes.
pub const OFF_DEPOSIT_NEXT: usize = 21;
/// v2: the queue's hash-chain value before deposit `deposit_next`.
pub const OFF_DEPOSIT_HASH: usize = 29;
/// v2: end of the deposit range of the highest batch closed after its root went final.
pub const OFF_DEPOSIT_FINAL: usize = 61;
/// Total v1 account size — fixed, no variable-length tail.
pub const LEN: usize = 21;
/// Total v2 account size.
pub const LEN_V2: usize = 69;

/// Account size for `version` (1 or 2); `BadVersion` for any other.
pub fn len_for_version(version: u8) -> Result<usize, crate::LayoutError> {
    match version {
        VERSION => Ok(LEN),
        VERSION_V2 => Ok(LEN_V2),
        _ => Err(crate::LayoutError::BadVersion),
    }
}

/// `["batch_cursor", settlement_program, chain_id]`. The settlement program is part of the key so a
/// chain's inbox accounts can only be created through its own settlement program.
#[inline]
pub fn seeds(settlement_program: &[u8; 32], chain_id: u64) -> [Vec<u8>; 3] {
    [
        b"batch_cursor".to_vec(),
        settlement_program.to_vec(),
        chain_id.to_le_bytes().to_vec(),
    ]
}

/// Derives the batch-cursor PDA under `program_id` (the inbox program) for the chain registered with
/// `settlement_program` — the one place this derivation is computed.
#[cfg(feature = "solana")]
#[inline]
pub fn pda(
    program_id: &solana_program::pubkey::Pubkey,
    settlement_program: &solana_program::pubkey::Pubkey,
    chain_id: u64,
) -> (solana_program::pubkey::Pubkey, u8) {
    let s = seeds(&settlement_program.to_bytes(), chain_id);
    solana_program::pubkey::Pubkey::find_program_address(&[&s[0], &s[1], &s[2]], program_id)
}

/// Field-for-field decode of a cursor account. Pubkeys are raw `[u8; 32]` elsewhere in this crate (only
/// `pda` above needs the real type). Does not check the account's owner or PDA seeds — callers hold
/// `program_id` and must check those themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorFields {
    pub chain_id: u64,
    pub next_batch: u64,
    /// `Some` for a v2 account, `None` for v1.
    pub deposit: Option<CursorDeposit>,
}

/// The v2 deposit cursor: the three fields appended after `next_batch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorDeposit {
    /// Index of the first deposit the next batch takes.
    pub next: u64,
    /// The queue's hash-chain value before deposit `next`.
    pub hash: [u8; 32],
    /// End of the deposit range of the highest batch closed after its root went final.
    pub final_: u64,
}

/// Validates magic + version + the length that version needs, and decodes the fields. A v1 account
/// decodes with `deposit: None`; a v2 account with `Some`. Any other version is `BadVersion`.
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
    let need = len_for_version(d[OFF_VERSION])?;
    if d.len() < need {
        return Err(crate::LayoutError::TooShort { need, got: d.len() });
    }
    let deposit = (d[OFF_VERSION] == VERSION_V2).then(|| CursorDeposit {
        next: u64::from_le_bytes(
            d[OFF_DEPOSIT_NEXT..OFF_DEPOSIT_NEXT + 8]
                .try_into()
                .unwrap(),
        ),
        hash: d[OFF_DEPOSIT_HASH..OFF_DEPOSIT_HASH + 32]
            .try_into()
            .unwrap(),
        final_: u64::from_le_bytes(
            d[OFF_DEPOSIT_FINAL..OFF_DEPOSIT_FINAL + 8]
                .try_into()
                .unwrap(),
        ),
    });
    Ok(CursorFields {
        chain_id: u64::from_le_bytes(d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].try_into().unwrap()),
        next_batch: u64::from_le_bytes(d[OFF_NEXT_BATCH..OFF_NEXT_BATCH + 8].try_into().unwrap()),
        deposit,
    })
}

/// Encodes a **v1** cursor account (the inverse of [`read`] for a v1 account). Returns exactly
/// [`LEN`] bytes. A v1 account cannot carry a deposit cursor, so `f.deposit` must be `None`; use
/// [`write_v2`] for one that has it.
#[inline]
pub fn write(f: &CursorFields) -> [u8; LEN] {
    debug_assert!(f.deposit.is_none(), "a v1 cursor has no deposit fields");
    let mut d = [0u8; LEN];
    d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
    d[OFF_VERSION] = VERSION;
    d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].copy_from_slice(&f.chain_id.to_le_bytes());
    d[OFF_NEXT_BATCH..OFF_NEXT_BATCH + 8].copy_from_slice(&f.next_batch.to_le_bytes());
    d
}

/// Encodes a **v2** cursor account. Returns exactly [`LEN_V2`] bytes, or `None` when `f.deposit` is
/// `None` (there is nothing to put in the appended fields).
#[inline]
pub fn write_v2(f: &CursorFields) -> Option<[u8; LEN_V2]> {
    let dep = f.deposit.as_ref()?;
    let mut d = [0u8; LEN_V2];
    d[..LEN].copy_from_slice(&write(&CursorFields {
        deposit: None,
        ..*f
    }));
    d[OFF_VERSION] = VERSION_V2;
    d[OFF_DEPOSIT_NEXT..OFF_DEPOSIT_NEXT + 8].copy_from_slice(&dep.next.to_le_bytes());
    d[OFF_DEPOSIT_HASH..OFF_DEPOSIT_HASH + 32].copy_from_slice(&dep.hash);
    d[OFF_DEPOSIT_FINAL..OFF_DEPOSIT_FINAL + 8].copy_from_slice(&dep.final_.to_le_bytes());
    Some(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(chain_id: u64, next_batch: u64) -> Vec<u8> {
        write(&CursorFields {
            chain_id,
            next_batch,
            deposit: None,
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
            deposit: None,
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

    /// Byte-golden test for v2: a **literal** 69-byte vector (no `OFF_*`), at the offsets 21, 29 and 61.
    #[test]
    fn write_v2_produces_a_known_byte_vector() {
        let f = CursorFields {
            chain_id: 7,
            next_batch: 42,
            deposit: Some(CursorDeposit {
                next: 3,
                hash: [0xab; 32],
                final_: 2,
            }),
        };
        let d = write_v2(&f).unwrap();
        #[rustfmt::skip]
        let expected: [u8; 69] = [
            // magic 'ZKBC' = 0x5a4b_4243 LE
            0x43, 0x42, 0x4b, 0x5a,
            // version = 2
            2,
            // chain_id = 7 LE u64
            7, 0, 0, 0, 0, 0, 0, 0,
            // next_batch = 42 LE u64 (ends at 21)
            42, 0, 0, 0, 0, 0, 0, 0,
            // deposit_next = 3 LE u64 (offset 21)
            3, 0, 0, 0, 0, 0, 0, 0,
            // deposit_hash: 32 bytes of 0xab (offset 29)
            0xab, 0xab, 0xab, 0xab, 0xab, 0xab, 0xab, 0xab,
            0xab, 0xab, 0xab, 0xab, 0xab, 0xab, 0xab, 0xab,
            0xab, 0xab, 0xab, 0xab, 0xab, 0xab, 0xab, 0xab,
            0xab, 0xab, 0xab, 0xab, 0xab, 0xab, 0xab, 0xab,
            // deposit_final = 2 LE u64 (offset 61)
            2, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert_eq!(d, expected);
        assert_eq!(read(&d).unwrap(), f);
    }

    /// The v1 literal from `write_produces_a_known_byte_vector` still decodes, with no deposit cursor.
    #[test]
    fn a_v1_cursor_reads_as_it_always_did() {
        #[rustfmt::skip]
        let v1: [u8; 21] = [
            0x43, 0x42, 0x4b, 0x5a, 1,
            7, 0, 0, 0, 0, 0, 0, 0,
            42, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert_eq!(
            read(&v1).unwrap(),
            CursorFields {
                chain_id: 7,
                next_batch: 42,
                deposit: None,
            }
        );
        // A v1 account that was allocated longer (for instance padded) is still v1.
        let mut longer = v1.to_vec();
        longer.extend_from_slice(&[0xff; 48]);
        assert_eq!(read(&longer).unwrap().deposit, None);
    }

    #[test]
    fn a_v2_cursor_shorter_than_69_bytes_is_too_short() {
        let mut d = write_v2(&CursorFields {
            chain_id: 1,
            next_batch: 1,
            deposit: Some(CursorDeposit {
                next: 0,
                hash: [1; 32],
                final_: 0,
            }),
        })
        .unwrap()
        .to_vec();
        d.truncate(LEN_V2 - 1);
        assert_eq!(
            read(&d).unwrap_err(),
            crate::LayoutError::TooShort {
                need: LEN_V2,
                got: LEN_V2 - 1
            }
        );
    }

    #[test]
    fn version_3_and_0_are_refused() {
        for v in [0u8, 3, 0xff] {
            let mut d = vec![0u8; LEN_V2];
            d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
            d[OFF_VERSION] = v;
            assert_eq!(read(&d).unwrap_err(), crate::LayoutError::BadVersion);
        }
        assert_eq!(len_for_version(1), Ok(21));
        assert_eq!(len_for_version(2), Ok(69));
        assert_eq!(len_for_version(3), Err(crate::LayoutError::BadVersion));
    }

    #[test]
    fn write_v2_needs_a_deposit_cursor() {
        let f = CursorFields {
            chain_id: 1,
            next_batch: 1,
            deposit: None,
        };
        assert!(write_v2(&f).is_none());
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
        let s = seeds(&[0x5Au8; 32], 0x0102_0304_0506_0708);
        assert_eq!(s[0], b"batch_cursor".to_vec());
        assert_eq!(s[1], vec![0x5Au8; 32]);
        assert_eq!(s[2], vec![0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
    }

    #[cfg(feature = "solana")]
    #[test]
    fn pda_is_deterministic_and_varies_with_chain_id() {
        let program = solana_program::pubkey::Pubkey::new_unique();
        let settlement = solana_program::pubkey::Pubkey::new_unique();
        let other_settlement = solana_program::pubkey::Pubkey::new_unique();
        let (a1, _) = pda(&program, &settlement, 7);
        let (a2, _) = pda(&program, &settlement, 7);
        assert_eq!(a1, a2);
        let (b, _) = pda(&program, &settlement, 8);
        assert_ne!(a1, b);
        let (c, _) = pda(&program, &other_settlement, 7);
        assert_ne!(a1, c);
    }
}
