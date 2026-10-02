//! Shared binary Merkle-tree reduction, hash-agnostic, plus this workspace's **one** `keccak256`: every other
//! copy (a hand-rolled `software_keccak` test helper duplicated in `rome-zk-layouts`, `zk-inbox-client`, and a
//! settlement-program test) is deleted; every caller imports [`keccak256`] instead.
//!
//! [`keccak256`] dispatches on `target_os`: the syscall-backed `solana_program::keccak::hashv` on-chain
//! (cheap in CU — no software keccak on BPF), a pure-Rust `sha3::Keccak256` off-chain. Both must produce
//! identical bytes for identical input, which is what the golden tests below and
//! `programs/zk-inbox/tests/accumulator.rs`'s on-chain/off-chain equivalence checks pin.
//!
//! The Merkle reduction itself keeps **no opinion on which keccak implementation computes a hash** — it
//! takes the hash function as a caller-supplied closure shaped exactly like [`keccak256`]
//! (`Fn(&[&[u8]]) -> [u8; 32]`, matching `solana_program::keccak::hashv`'s own signature) via the
//! [`HashV`] trait, so a caller can pass [`keccak256`] itself, or (as the golden tests below do to keep
//! this crate's own tests independent of its production `keccak256`) any other function of the same
//! shape.
//!
//! Construction (must match `crates/rome-zk-sequencer/src/merkle.rs` on `main` exactly — golden-tested):
//! `hash_pair(left, right) = keccak256(left ++ right)`; an odd level's last node is paired with a
//! domain-separated `EMPTY` sentinel (`keccak256(b"rome-zk/merkle/empty/v1")`), never duplicated against
//! itself; leaves are taken as given (already the caller's per-leaf content hash). Empty input's root is
//! `[0u8; 32]`; a single leaf's root is that leaf, unreduced.
//!
//! Each leaf is additionally bound to its position: the zk-inbox leaf value is `keccak(idx_le[4] ‖
//! chunk_hash[32])`, not the bare chunk hash — [`indexed_leaf`] computes that.

/// Domain-separation string for the padding sentinel used on an odd Merkle level.
pub const EMPTY_DOMAIN: &[u8] = b"rome-zk/merkle/empty/v1";

/// The one `keccak256(&[part0, part1, ...]) = keccak256(concat(parts))` in this workspace. On
/// `target_os = "solana"` this is the syscall (`solana_program::keccak::hashv`); everywhere else it is a pure-Rust
/// `sha3::Keccak256` — both must agree byte-for-byte, pinned by
/// [`tests::production_keccak_matches_an_independently_computed_digest`].
#[cfg(target_os = "solana")]
#[inline(always)]
pub fn keccak256(parts: &[&[u8]]) -> [u8; 32] {
    solana_program::keccak::hashv(parts).to_bytes()
}

/// Off-chain (host, non-SBF) `keccak256` — see [`keccak256`]'s own doc above.
#[cfg(not(target_os = "solana"))]
pub fn keccak256(parts: &[&[u8]]) -> [u8; 32] {
    use sha3::{Digest, Keccak256};
    let mut k = Keccak256::new();
    for p in parts {
        k.update(p);
    }
    k.finalize().into()
}

/// A keccak256-shaped hash function: `hashv(&[part0, part1, ...]) = keccak256(concat(parts))`. Matches
/// `solana_program::keccak::hashv`'s signature (and [`keccak256`]'s own) so an on-chain caller can pass
/// the syscall directly, and any caller can pass [`keccak256`] itself.
pub trait HashV {
    fn hashv(&self, parts: &[&[u8]]) -> [u8; 32];
}

impl<F: Fn(&[&[u8]]) -> [u8; 32]> HashV for F {
    fn hashv(&self, parts: &[&[u8]]) -> [u8; 32] {
        self(parts)
    }
}

/// `keccak256(EMPTY_DOMAIN)` — the odd-level padding sentinel. Not the hash of any real leaf or internal
/// node (its preimage is a fixed ASCII domain string with nothing else folded in).
pub fn empty(h: &impl HashV) -> [u8; 32] {
    h.hashv(&[EMPTY_DOMAIN])
}

/// `keccak256(idx_le[4] ++ hash)` — the per-leaf value the zk-inbox accumulator commits (each leaf is `idx ‖
/// hash`), binding a leaf to its position so it cannot be silently reordered without changing its hash.
pub fn indexed_leaf(h: &impl HashV, idx: u32, hash: &[u8; 32]) -> [u8; 32] {
    h.hashv(&[&idx.to_le_bytes(), hash])
}

fn hash_pair(h: &impl HashV, left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    h.hashv(&[left, right])
}

/// Reduce `leaves` (already the domain leaf values, in the order they should be committed) to a single
/// Merkle root by repeated pairwise hashing. `[0u8; 32]` for an empty list; the leaf itself for a
/// single-leaf list; otherwise level-by-level reduction, odd levels padded with [`empty`].
pub fn root(h: &impl HashV, leaves: &[[u8; 32]]) -> [u8; 32] {
    if leaves.is_empty() {
        return [0u8; 32];
    }
    if leaves.len() == 1 {
        return leaves[0];
    }
    let e = empty(h);
    let mut level: Vec<[u8; 32]> = leaves.to_vec();
    while level.len() > 1 {
        level = next_level(h, &level, &e);
    }
    level[0]
}

fn next_level(h: &impl HashV, level: &[[u8; 32]], empty_pad: &[u8; 32]) -> Vec<[u8; 32]> {
    let mut parents = Vec::with_capacity(level.len().div_ceil(2));
    let mut i = 0;
    while i < level.len() {
        let left = level[i];
        let right = if i + 1 < level.len() {
            level[i + 1]
        } else {
            *empty_pad
        };
        parents.push(hash_pair(h, &left, &right));
        i += 2;
    }
    parents
}

/// Reduce the leaves stored in-place in `buf` (a byte buffer whose first `n * 32` bytes are `n` 32-byte
/// leaf values, in order) to a single root, overwriting `buf` level by level so no allocation beyond a
/// caller-owned scratch buffer for the parent level is required from `buf` itself. Returns the root.
/// Used by the on-chain program, which already holds `leaf_hashes` as raw account bytes and would
/// otherwise have to copy them into a `Vec` just to call [`root`].
///
/// `n` must be `>= 1` and `buf.len() >= n * 32`.
pub fn root_in_place(h: &impl HashV, buf: &mut [u8], n: usize) -> [u8; 32] {
    assert!(n >= 1 && buf.len() >= n * 32);
    let read = |b: &[u8], i: usize| -> [u8; 32] { b[i * 32..i * 32 + 32].try_into().unwrap() };
    if n == 1 {
        return read(buf, 0);
    }
    let e = empty(h);
    let mut len = n;
    loop {
        let next_len = len.div_ceil(2);
        for i in 0..next_len {
            let left = read(buf, 2 * i);
            let right = if 2 * i + 1 < len {
                read(buf, 2 * i + 1)
            } else {
                e
            };
            let parent = hash_pair(h, &left, &right);
            buf[i * 32..i * 32 + 32].copy_from_slice(&parent);
        }
        if next_len == 1 {
            return read(buf, 0);
        }
        len = next_len;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha3::{Digest, Keccak256};

    /// Golden test (contract): `keccak256` against a digest computed independently (pycryptodome
    /// `Crypto.Hash.keccak`, digest_bits=256 — not this crate, not the `sha3` crate's own test suite) —
    /// the same input/output pair `zk-inbox-client::chunk_body_hash` is golden-tested against, so this
    /// pins the single production `keccak256` both crates now share.
    #[test]
    fn production_keccak_matches_an_independently_computed_digest() {
        let body = b"rome-zk chunk body: the quick brown fox jumps over the lazy dog";
        assert_eq!(
            hex(&keccak256(&[body])),
            "c2d88bce252087e45b6d5eb9382c95d454a6b923aea1b62ba9cd24fc054621e9",
        );
    }

    fn leaf(b: u8) -> [u8; 32] {
        [b; 32]
    }

    #[test]
    fn empty_root_is_zero() {
        assert_eq!(root(&keccak256, &[]), [0u8; 32]);
    }

    #[test]
    fn one_leaf_root_is_the_leaf_itself() {
        let l0 = leaf(1);
        assert_eq!(root(&keccak256, &[l0]), l0);
    }

    #[test]
    fn two_leaves_matches_independent_reconstruction() {
        let l0 = leaf(1);
        let l1 = leaf(2);
        let mut preimage = [0u8; 64];
        preimage[..32].copy_from_slice(&l0);
        preimage[32..].copy_from_slice(&l1);
        let expected: [u8; 32] = Keccak256::digest(preimage).into();
        assert_eq!(root(&keccak256, &[l0, l1]), expected);
    }

    #[test]
    fn three_leaves_odd_level_pads_with_empty_not_duplicate() {
        let l0 = leaf(1);
        let l1 = leaf(2);
        let l2 = leaf(3);
        let e = empty(&keccak256);
        let h01: [u8; 32] = Keccak256::digest([l0, l1].concat()).into();
        let h2e: [u8; 32] = Keccak256::digest([l2, e].concat()).into();
        let expected: [u8; 32] = Keccak256::digest([h01, h2e].concat()).into();
        assert_eq!(root(&keccak256, &[l0, l1, l2]), expected);
    }

    /// Golden test (contract): the reduction rule here must match
    /// `crates/rome-zk-sequencer/src/merkle.rs`'s `root()` byte-for-byte. That module's own tests pin
    /// concrete hex roots for 2 and 3 leaves (independently reconstructed); reproduce those same
    /// constants here against opaque, non-domain-bound leaves (this crate's `root`, not `indexed_leaf` —
    /// the sequencer's leaves carry no positional prefix either) to prove the two implementations agree.
    #[test]
    fn matches_sequencer_merkle_two_leaf_golden_hex() {
        let l0 = leaf(1);
        let l1 = leaf(2);
        let r = root(&keccak256, &[l0, l1]);
        assert_eq!(
            hex(&r),
            "346d8c96a2454213fcc0daff3c96ad0398148181b9fa6488f7ae2c0af5b20aa0"
        );
    }

    fn hex(b: &[u8; 32]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// `root` (Vec-based) and `root_in_place` (in-place over a raw byte buffer, the shape the on-chain
    /// program actually has to work with — account data is `&mut [u8]`, not `Vec<[u8; 32]>`) must agree
    /// for every size in the same range the on-chain tests exercise: 1, 2, 3, 5, 900.
    #[test]
    fn root_in_place_matches_vec_based_root_for_1_2_3_5_900_leaves() {
        for n in [1usize, 2, 3, 5, 900] {
            let leaves: Vec<[u8; 32]> = (0..n).map(|i| leaf((i % 251) as u8 + 1)).collect();
            let expected = root(&keccak256, &leaves);
            let mut buf = vec![0u8; n * 32];
            for (i, l) in leaves.iter().enumerate() {
                buf[i * 32..i * 32 + 32].copy_from_slice(l);
            }
            let got = root_in_place(&keccak256, &mut buf, n);
            assert_eq!(got, expected, "mismatch at n={n}");
        }
    }

    #[test]
    fn indexed_leaf_binds_position_into_the_leaf_hash() {
        let hash = [7u8; 32];
        let l0 = indexed_leaf(&keccak256, 0, &hash);
        let l1 = indexed_leaf(&keccak256, 1, &hash);
        assert_ne!(
            l0, l1,
            "same chunk hash at a different idx must produce a different leaf"
        );
    }
}
