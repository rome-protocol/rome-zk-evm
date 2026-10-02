//! Permissionless chain-id derivation.
//!
//! A permissionless `chain_id` (>= 2^32) must equal `2^32 + (keccak(authority ‖ program_nonce) mod
//! (2^53 - 2^32))`, where `program_nonce` is a sequential per-authority counter the settlement program
//! keeps (`crate::perm_nonce`). Hash-agnostic like [`crate::acc`] and `rome_zk_merkle`'s reduction: the
//! on-chain program passes the syscall-backed keccak, off-chain callers a software keccak — both must
//! produce the same id for the same `(authority, nonce)` pair, or a client-computed id would never match
//! what the program independently recomputes and checks.
//!
//! The reduction treats the 32-byte digest as a big-endian unsigned integer and folds it byte-by-byte
//! (`acc = acc * 256 + byte, mod m`) — this is the one true modulus of a 256-bit number by `m`, not a
//! truncation of the digest's low bits, so every bit of the hash affects the result.

use crate::HashV;

/// `2^32`. Also the smallest permissionless chain id — reserved ids are always `< PERMISSIONLESS_BASE`.
pub const PERMISSIONLESS_BASE: u64 = 1u64 << 32;
/// `2^53 - 2^32` — the modulus. `2^53` is the largest integer a float can represent
/// exactly, kept as a safety margin for any off-chain JS/JSON consumer of a chain id.
pub const PERMISSIONLESS_MODULUS: u64 = (1u64 << 53) - (1u64 << 32);

/// `2^32 + (keccak(authority ‖ nonce_le) mod PERMISSIONLESS_MODULUS)`.
pub fn permissionless_chain_id(h: &impl HashV, authority: &[u8; 32], nonce: u64) -> u64 {
    let digest = h.hashv(&[authority, &nonce.to_le_bytes()]);
    let m = PERMISSIONLESS_MODULUS as u128;
    let mut acc: u128 = 0;
    for byte in digest {
        acc = (acc * 256 + byte as u128) % m;
    }
    PERMISSIONLESS_BASE + acc as u64
}

/// A chain id `< 2^32` is reserved; `>= 2^32` is permissionless.
pub fn is_reserved(chain_id: u64) -> bool {
    chain_id < PERMISSIONLESS_BASE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h() -> impl Fn(&[&[u8]]) -> [u8; 32] {
        rome_zk_merkle::keccak256 as fn(&[&[u8]]) -> [u8; 32]
    }

    #[test]
    fn is_reserved_splits_at_2_32() {
        assert!(is_reserved(0));
        assert!(is_reserved(PERMISSIONLESS_BASE - 1));
        assert!(!is_reserved(PERMISSIONLESS_BASE));
        assert!(!is_reserved(u64::MAX));
    }

    #[test]
    fn derived_id_is_always_in_the_permissionless_range() {
        for nonce in 0..50u64 {
            let id = permissionless_chain_id(&h(), &[7u8; 32], nonce);
            assert!(!is_reserved(id));
            assert!(id < PERMISSIONLESS_BASE + PERMISSIONLESS_MODULUS);
        }
    }

    #[test]
    fn derivation_is_deterministic() {
        let a = permissionless_chain_id(&h(), &[3u8; 32], 5);
        let b = permissionless_chain_id(&h(), &[3u8; 32], 5);
        assert_eq!(a, b);
    }

    #[test]
    fn different_authority_or_nonce_changes_the_id() {
        let base = permissionless_chain_id(&h(), &[1u8; 32], 0);
        assert_ne!(base, permissionless_chain_id(&h(), &[2u8; 32], 0));
        assert_ne!(base, permissionless_chain_id(&h(), &[1u8; 32], 1));
    }

    /// Golden test (contract): pins the derivation for a fixed `(authority, nonce)` against a value
    /// computed once and hard-coded here, so a future change to field order, endianness, or the
    /// reduction algorithm is caught by this test rather than silently changing every derived id.
    #[test]
    fn golden_id_for_fixed_authority_and_nonce() {
        let authority = [0x42u8; 32];
        let id = permissionless_chain_id(&h(), &authority, 0);
        assert_eq!(id, 7_066_855_974_458_915u64);
    }
}
