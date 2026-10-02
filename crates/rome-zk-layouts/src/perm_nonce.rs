//! The per-authority permissionless-nonce account layout ("ZKPN", PDA `["perm_nonce", authority]` under the
//! settlement program). Created on an authority's first permissionless `InitChain`, incremented on every one
//! after — the sequential counter [`crate::chainid::permissionless_chain_id`] folds into the derived chain id,
//! so ids cannot be ground (an authority cannot choose which id a given call produces) or front-run (nothing
//! but this account's own current value determines the *next* nonce, and it only ever advances by one, in
//! order).
//!
//! ```text
//! magic 'ZKPN' u32 | version u8 | authority [32] | nonce u64
//! ```
//! All integers little-endian. `nonce` is the *next* value `InitChain` will consume (0 before this
//! account exists).

pub const MAGIC: u32 = 0x5a4b_504e; // "ZKPN"
pub const VERSION: u8 = 1;

pub const OFF_MAGIC: usize = 0;
pub const OFF_VERSION: usize = 4;
pub const OFF_AUTHORITY: usize = 5;
pub const OFF_NONCE: usize = 37;
/// Full fixed-size account length.
pub const LEN: usize = 45;

/// `["perm_nonce", authority]`. Takes a real `Pubkey` (unlike every other module's `seeds()`, which takes
/// only scalars) because the authority itself is a pubkey seed component — gated the same as `pda()`.
#[cfg(feature = "solana")]
#[inline]
pub fn seeds(authority: &solana_program::pubkey::Pubkey) -> [Vec<u8>; 2] {
    [b"perm_nonce".to_vec(), authority.to_bytes().to_vec()]
}

/// Derives the perm-nonce PDA under `program_id` (the settlement program) — the one place this
/// derivation is computed.
#[cfg(feature = "solana")]
#[inline]
pub fn pda(
    program_id: &solana_program::pubkey::Pubkey,
    authority: &solana_program::pubkey::Pubkey,
) -> (solana_program::pubkey::Pubkey, u8) {
    let s = seeds(authority);
    solana_program::pubkey::Pubkey::find_program_address(&[&s[0], &s[1]], program_id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NonceFields {
    pub authority: [u8; 32],
    pub nonce: u64,
}

pub fn read(d: &[u8]) -> Result<NonceFields, crate::LayoutError> {
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
    Ok(NonceFields {
        authority: d[OFF_AUTHORITY..OFF_AUTHORITY + 32].try_into().unwrap(),
        nonce: u64::from_le_bytes(d[OFF_NONCE..OFF_NONCE + 8].try_into().unwrap()),
    })
}

pub fn write(d: &mut [u8], f: &NonceFields) {
    d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
    d[OFF_VERSION] = VERSION;
    d[OFF_AUTHORITY..OFF_AUTHORITY + 32].copy_from_slice(&f.authority);
    d[OFF_NONCE..OFF_NONCE + 8].copy_from_slice(&f.nonce.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_round_trips() {
        let f = NonceFields {
            authority: [5u8; 32],
            nonce: 3,
        };
        let mut d = vec![0u8; LEN];
        write(&mut d, &f);
        assert_eq!(read(&d).unwrap(), f);
    }

    #[test]
    fn read_rejects_bad_magic() {
        let mut d = vec![0u8; LEN];
        write(
            &mut d,
            &NonceFields {
                authority: [0u8; 32],
                nonce: 0,
            },
        );
        d[OFF_MAGIC] ^= 0xff;
        assert_eq!(read(&d).unwrap_err(), crate::LayoutError::BadMagic);
    }

    #[test]
    fn read_rejects_too_short() {
        let d = vec![0u8; LEN - 1];
        assert!(matches!(
            read(&d).unwrap_err(),
            crate::LayoutError::TooShort { .. }
        ));
    }

    #[cfg(feature = "solana")]
    #[test]
    fn seeds_golden_bytes_for_a_fixed_authority() {
        let authority = solana_program::pubkey::Pubkey::new_from_array([7u8; 32]);
        let s = seeds(&authority);
        assert_eq!(s[0], b"perm_nonce".to_vec());
        assert_eq!(s[1], vec![7u8; 32]);
    }

    #[cfg(feature = "solana")]
    #[test]
    fn pda_is_deterministic_and_varies_with_authority() {
        let program = solana_program::pubkey::Pubkey::new_unique();
        let a = solana_program::pubkey::Pubkey::new_unique();
        let b = solana_program::pubkey::Pubkey::new_unique();
        let (p1, _) = pda(&program, &a);
        let (p2, _) = pda(&program, &a);
        assert_eq!(p1, p2);
        let (p3, _) = pda(&program, &b);
        assert_ne!(p1, p3);
    }
}
