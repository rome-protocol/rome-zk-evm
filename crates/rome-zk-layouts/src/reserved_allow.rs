//! The reserved-chain-id allowlist marker account layout ("ZKRA", PDA `["reserved_allow", chain_id]` under the
//! settlement program). Its *existence*, not its contents, is the allowlist entry: `AllowReservedId` creates it
//! (registry-authority-only), `RevokeReservedId` closes it (registry-authority-only, rent back out), and a
//! reserved `InitChain` requires it to exist for the id being registered. `chain_id` inside is redundant with
//! the PDA's own seed — kept so a reader holding only the address (not the id it was derived from) can still
//! recover which id an allowlist account is for.
//!
//! ```text
//! magic 'ZKRA' u32 | chain_id u64
//! ```
//! All integers little-endian. Fixed size, no version byte (there is nothing about this shape that a
//! future version would need to change without also changing the magic).

pub const MAGIC: u32 = 0x5a4b_5241; // "ZKRA"

pub const OFF_MAGIC: usize = 0;
pub const OFF_CHAIN_ID: usize = 4;
/// Full fixed-size account length.
pub const LEN: usize = 12;

/// `["reserved_allow", chain_id]`.
#[inline]
pub fn seeds(chain_id: u64) -> [Vec<u8>; 2] {
    [b"reserved_allow".to_vec(), chain_id.to_le_bytes().to_vec()]
}

/// Derives the reserved-allow PDA under `program_id` (the settlement program) — the one place this
/// derivation is computed.
#[cfg(feature = "solana")]
#[inline]
pub fn pda(
    program_id: &solana_program::pubkey::Pubkey,
    chain_id: u64,
) -> (solana_program::pubkey::Pubkey, u8) {
    let s = seeds(chain_id);
    solana_program::pubkey::Pubkey::find_program_address(&[&s[0], &s[1]], program_id)
}

pub fn read_chain_id(d: &[u8]) -> Result<u64, crate::LayoutError> {
    if d.len() < LEN {
        return Err(crate::LayoutError::TooShort {
            need: LEN,
            got: d.len(),
        });
    }
    if u32::from_le_bytes(d[OFF_MAGIC..OFF_MAGIC + 4].try_into().unwrap()) != MAGIC {
        return Err(crate::LayoutError::BadMagic);
    }
    Ok(u64::from_le_bytes(
        d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].try_into().unwrap(),
    ))
}

pub fn write(d: &mut [u8], chain_id: u64) {
    d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
    d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].copy_from_slice(&chain_id.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_round_trips() {
        let mut d = vec![0u8; LEN];
        write(&mut d, 200101);
        assert_eq!(read_chain_id(&d).unwrap(), 200101);
    }

    #[test]
    fn read_rejects_bad_magic() {
        let mut d = vec![0u8; LEN];
        write(&mut d, 1);
        d[OFF_MAGIC] ^= 0xff;
        assert_eq!(read_chain_id(&d).unwrap_err(), crate::LayoutError::BadMagic);
    }

    #[test]
    fn read_rejects_too_short() {
        let d = vec![0u8; LEN - 1];
        assert!(matches!(
            read_chain_id(&d).unwrap_err(),
            crate::LayoutError::TooShort { .. }
        ));
    }

    #[test]
    fn seeds_golden_bytes_for_a_fixed_chain_id() {
        let s = seeds(0x0102_0304_0506_0708);
        assert_eq!(s[0], b"reserved_allow".to_vec());
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
