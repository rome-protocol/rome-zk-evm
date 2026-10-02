//! The zk-inbox chunk account layout ("ZKIB", PDA `["inbox", chain_id, batch, idx]` under
//! the inbox program). One PDA per chunk of the compressed channel stream a batch carries as DA.
//!
//! ```text
//! magic 'ZKIB' u32 | authority [32] | chain_id u64 | batch u64 | idx u32 | len u32 | sealed u8 | pad [3]
//! ```
//! All integers little-endian. `pad` (bytes 61..64) is reserved and always zero on write; `read` does
//! not inspect it. The body (one `rome_zk_layouts::frame`-headed channel frame) follows
//! immediately after byte 64.

pub const MAGIC: u32 = 0x5a4b_4942; // "ZKIB"

pub const OFF_MAGIC: usize = 0;
pub const OFF_AUTHORITY: usize = 4;
pub const OFF_CHAIN_ID: usize = 36;
pub const OFF_BATCH: usize = 44;
pub const OFF_IDX: usize = 52;
pub const OFF_LEN: usize = 56;
pub const OFF_SEALED: usize = 60;
/// End of the fixed header; the body starts here.
pub const HEADER_LEN: usize = 64;

/// Field-for-field decode of a chunk account's fixed header. `authority` is a raw `[u8; 32]` — a caller
/// that wants a typed `Pubkey` wraps it itself (`zk-inbox-client::decode_chunk_header` does this).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkHeaderFields {
    pub authority: [u8; 32],
    pub chain_id: u64,
    pub batch: u64,
    pub idx: u32,
    pub len: u32,
    pub sealed: bool,
}

/// Validates magic + minimum length and decodes every header field. Does not check the account's owner
/// — callers hold `program_id` and must check that themselves.
#[inline]
pub fn read(d: &[u8]) -> Result<ChunkHeaderFields, crate::LayoutError> {
    if d.len() < HEADER_LEN {
        return Err(crate::LayoutError::TooShort {
            need: HEADER_LEN,
            got: d.len(),
        });
    }
    if u32::from_le_bytes(d[OFF_MAGIC..OFF_MAGIC + 4].try_into().unwrap()) != MAGIC {
        return Err(crate::LayoutError::BadMagic);
    }
    Ok(ChunkHeaderFields {
        authority: d[OFF_AUTHORITY..OFF_AUTHORITY + 32].try_into().unwrap(),
        chain_id: u64::from_le_bytes(d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].try_into().unwrap()),
        batch: u64::from_le_bytes(d[OFF_BATCH..OFF_BATCH + 8].try_into().unwrap()),
        idx: u32::from_le_bytes(d[OFF_IDX..OFF_IDX + 4].try_into().unwrap()),
        len: u32::from_le_bytes(d[OFF_LEN..OFF_LEN + 4].try_into().unwrap()),
        sealed: d[OFF_SEALED] != 0,
    })
}

/// Encodes a chunk account's fixed header (the inverse of [`read`], modulo the reserved `pad` bytes,
/// which are always written zero). Returns exactly [`HEADER_LEN`] bytes — the caller appends the body.
#[inline]
pub fn write_header(f: &ChunkHeaderFields) -> [u8; HEADER_LEN] {
    let mut d = [0u8; HEADER_LEN];
    d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
    d[OFF_AUTHORITY..OFF_AUTHORITY + 32].copy_from_slice(&f.authority);
    d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].copy_from_slice(&f.chain_id.to_le_bytes());
    d[OFF_BATCH..OFF_BATCH + 8].copy_from_slice(&f.batch.to_le_bytes());
    d[OFF_IDX..OFF_IDX + 4].copy_from_slice(&f.idx.to_le_bytes());
    d[OFF_LEN..OFF_LEN + 4].copy_from_slice(&f.len.to_le_bytes());
    d[OFF_SEALED] = f.sealed as u8;
    d
}

/// `["inbox", chain_id, batch, idx]`.
#[inline]
pub fn seeds(chain_id: u64, batch: u64, idx: u32) -> [Vec<u8>; 4] {
    [
        b"inbox".to_vec(),
        chain_id.to_le_bytes().to_vec(),
        batch.to_le_bytes().to_vec(),
        idx.to_le_bytes().to_vec(),
    ]
}

/// Derives the chunk PDA under `program_id` (the inbox program) — the one place this derivation is
/// computed.
#[cfg(feature = "solana")]
#[inline]
pub fn pda(
    program_id: &solana_program::pubkey::Pubkey,
    chain_id: u64,
    batch: u64,
    idx: u32,
) -> (solana_program::pubkey::Pubkey, u8) {
    let s = seeds(chain_id, batch, idx);
    solana_program::pubkey::Pubkey::find_program_address(&[&s[0], &s[1], &s[2], &s[3]], program_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_golden_bytes_for_fixed_inputs() {
        let s = seeds(0x0102_0304_0506_0708, 9, 3);
        assert_eq!(s[0], b"inbox".to_vec());
        assert_eq!(s[1], vec![0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
        assert_eq!(s[2], vec![9, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(s[3], vec![3, 0, 0, 0]);
    }

    #[cfg(feature = "solana")]
    #[test]
    fn pda_is_deterministic_and_varies_with_idx() {
        let program = solana_program::pubkey::Pubkey::new_unique();
        let (a1, _) = pda(&program, 7, 1, 0);
        let (a2, _) = pda(&program, 7, 1, 0);
        assert_eq!(a1, a2);
        let (b, _) = pda(&program, 7, 1, 1);
        assert_ne!(a1, b);
    }

    /// Byte-golden test (contract): a **literal** 64-byte vector (every byte position written as a
    /// number, never through `OFF_*`/`MAGIC`) decodes to the exact fields it encodes — pins the field
    /// order and every offset independently of the constants `read` itself uses, so a future change to
    /// an `OFF_*` value is caught here even though `read` and `write_header` would still agree with each
    /// other.
    #[test]
    fn read_decodes_a_known_byte_vector() {
        #[rustfmt::skip]
        let d: [u8; 64] = [
            // magic 'ZKIB' = 0x5a4b_4942 LE
            &[0x42, 0x49, 0x4b, 0x5a][..],
            // authority: 32 bytes of 0x42
            &[0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42,
              0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42,
              0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42,
              0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42],
            // chain_id = 7 LE u64
            &[7, 0, 0, 0, 0, 0, 0, 0],
            // batch = 3 LE u64
            &[3, 0, 0, 0, 0, 0, 0, 0],
            // idx = 2 LE u32
            &[2, 0, 0, 0],
            // len = 100 LE u32
            &[100, 0, 0, 0],
            // sealed = 1, pad[3] = 0
            &[1, 0, 0, 0],
        ]
        .concat()
        .try_into()
        .unwrap();
        let f = read(&d).unwrap();
        assert_eq!(
            f,
            ChunkHeaderFields {
                authority: [0x42u8; 32],
                chain_id: 7,
                batch: 3,
                idx: 2,
                len: 100,
                sealed: true,
            }
        );
    }

    #[test]
    fn write_header_then_read_round_trips() {
        let f = ChunkHeaderFields {
            authority: [9u8; 32],
            chain_id: 11,
            batch: 5,
            idx: 4,
            len: 50,
            sealed: false,
        };
        let d = write_header(&f);
        assert_eq!(d.len(), HEADER_LEN);
        assert_eq!(read(&d).unwrap(), f);
    }

    #[test]
    fn read_rejects_bad_magic() {
        let mut d = [0u8; HEADER_LEN];
        d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
        d[OFF_MAGIC] ^= 0xff;
        assert_eq!(read(&d).unwrap_err(), crate::LayoutError::BadMagic);
    }

    #[test]
    fn read_rejects_too_short() {
        let d = vec![0u8; HEADER_LEN - 1];
        assert!(matches!(
            read(&d).unwrap_err(),
            crate::LayoutError::TooShort { .. }
        ));
    }
}
