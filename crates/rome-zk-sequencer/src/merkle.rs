//! Binary Merkle tree over an ordered list of leaf hashes.
//!
//! `tx_root` used to be `keccak256(concat(tx_hashes))` — a single flat hash a challenger cannot use to
//! prove "tx `t` at position `p` is in sub-block `h`" without revealing every other tx in the sub-block.
//! This module replaces it with a real binary Merkle tree so that claim has a `log2(n)`-size proof.
//!
//! ## Construction
//!
//! Leaves are the tx hashes themselves, in inclusion order — no leaf-domain prefix (the leaves are
//! already 32-byte content hashes with nothing else that could collide with an internal node's preimage
//! shape by accident of length, since every node at every level is exactly 64 bytes hashed to 32).
//!
//! One level's parents are computed by pairing adjacent nodes left-to-right and hashing
//! `keccak256(left || right)`. When a level has an odd number of nodes, the last node is paired with
//! [`EMPTY`] — a fixed, domain-separated sentinel — rather than duplicated against itself.
//! Levels repeat until exactly one node remains: that node is the root. An empty leaf
//! list's root is defined as [`B256::ZERO`] (there is no sub-block with zero levels to reduce).
//!
//! A one-leaf tree's root is that leaf itself — zero rounds of pairing, matching "reduce until one node
//! remains".
//!
//! ## Odd-node padding and proof capacity
//!
//! Duplicating the last node on an odd level used to mean `root([a,b,c])` and `root([a,b,c,c])` were
//! identical — a position proof for "tx `c` at index 3" would verify against a sub-block that actually had
//! only 3 txs, which is ambiguous for a slashing claim keyed on `(root, position)`. [`EMPTY`] fixes that:
//! it is not the hash of any real leaf or internal node (it's a domain-separated constant with nothing
//! feeding into it), so a genuinely-duplicated leaf and a padded-with-EMPTY leaf produce different parent
//! hashes and therefore different roots.
//!
//! That alone is not sufficient, though: a proof of length `L` (as produced by [`proof`]) reconstructs the
//! root correctly not just for the index it was built for, but for any index that shares the same
//! low-`L`-bits hash path — e.g. a 3-leaf tree's proof for index 2 (length 2) also reconstructs the root
//! at index 6, since `2` and `6` agree in their low 2 bits. [`verify`] therefore also rejects any index
//! that is at or beyond `2^proof.len()` outright, before touching the hash chain — the header does not
//! carry a tx count, so this capacity bound (not the leaf list length) is what makes a proof self-limiting
//! to the indices it could actually have been built for.

use alloy::primitives::{keccak256, B256};
use std::sync::LazyLock;

/// Domain-separated padding constant used in place of "duplicate the last node" on an odd Merkle level.
/// Not the hash of any real leaf or internal node — its preimage is a
/// fixed ASCII domain string with nothing else folded in — so it can never collide with real sub-block
/// content.
pub static EMPTY: LazyLock<B256> = LazyLock::new(|| keccak256(b"rome-zk/merkle/empty/v1"));

/// The Merkle root of `leaves`, taken as an ordered list of 32-byte hashes. `B256::ZERO` for an empty
/// list; the leaf itself for a single-leaf list; otherwise the tree is reduced level by level as
/// described in the module doc.
pub fn root(leaves: &[B256]) -> B256 {
    if leaves.is_empty() {
        return B256::ZERO;
    }
    let mut level: Vec<B256> = leaves.to_vec();
    while level.len() > 1 {
        level = next_level(&level);
    }
    level[0]
}

fn next_level(level: &[B256]) -> Vec<B256> {
    let mut parents = Vec::with_capacity(level.len().div_ceil(2));
    let mut i = 0;
    while i < level.len() {
        let left = level[i];
        let right = if i + 1 < level.len() {
            level[i + 1]
        } else {
            *EMPTY // odd level: pad with the domain-separated sentinel, never duplicate
        };
        parents.push(hash_pair(left, right));
        i += 2;
    }
    parents
}

fn hash_pair(left: B256, right: B256) -> B256 {
    let mut preimage = [0u8; 64];
    preimage[..32].copy_from_slice(left.as_slice());
    preimage[32..].copy_from_slice(right.as_slice());
    keccak256(preimage)
}

/// A Merkle inclusion proof: sibling hashes from the leaf's level up to (but not including) the root, in
/// bottom-to-top order. Position (left/right at each level) is reconstructed from `index` alone during
/// [`verify`] — it is not stored in the proof.
pub type Proof = Vec<B256>;

/// Build the inclusion proof for the leaf at `index` in `leaves`. `None` if `index` is out of range.
pub fn proof(leaves: &[B256], index: usize) -> Option<Proof> {
    if index >= leaves.len() {
        return None;
    }
    let mut path = Vec::new();
    let mut level: Vec<B256> = leaves.to_vec();
    let mut idx = index;
    while level.len() > 1 {
        let sibling = if idx.is_multiple_of(2) {
            if idx + 1 < level.len() {
                level[idx + 1]
            } else {
                *EMPTY // odd level: this node's sibling is the padding sentinel, not itself
            }
        } else {
            level[idx - 1]
        };
        path.push(sibling);
        level = next_level(&level);
        idx /= 2;
    }
    Some(path)
}

/// Verify that `leaf` at `index` is included under `root`, given `proof` (as produced by [`proof`]).
///
/// A proof of length `L` only ever certifies indices `0..2^L` — `index`
/// is checked against that capacity *before* the hash chain runs, since the hash chain alone would
/// otherwise also accept any index sharing the same low-`L`-bits path as the one the proof was built for
/// (see the module doc's "Odd-node padding and proof capacity" section).
pub fn verify(root: B256, leaf: B256, index: usize, proof: &Proof) -> bool {
    if proof.len() >= usize::BITS as usize || index >= (1usize << proof.len()) {
        return false;
    }
    let mut node = leaf;
    let mut idx = index;
    for sibling in proof {
        node = if idx.is_multiple_of(2) {
            hash_pair(node, *sibling)
        } else {
            hash_pair(*sibling, node)
        };
        idx /= 2;
    }
    node == root
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(byte: u8) -> B256 {
        B256::repeat_byte(byte)
    }

    #[test]
    fn empty_root_is_zero() {
        assert_eq!(root(&[]), B256::ZERO);
    }

    #[test]
    fn one_leaf_root_is_the_leaf_itself() {
        let l0 = leaf(1);
        assert_eq!(root(&[l0]), l0);
    }

    /// Golden: pins the exact pairing/hash construction for 2 leaves — reconstructed independently
    /// (directly from `keccak256`, not by calling `root()` twice) so a bug in `root()`'s pairing or
    /// concatenation order would actually be caught.
    #[test]
    fn two_leaves_root_matches_independent_reconstruction() {
        let l0 = leaf(1);
        let l1 = leaf(2);
        let mut preimage = [0u8; 64];
        preimage[..32].copy_from_slice(l0.as_slice());
        preimage[32..].copy_from_slice(l1.as_slice());
        let expected = keccak256(preimage);
        assert_eq!(root(&[l0, l1]), expected);
        assert_eq!(
            hex::encode(root(&[l0, l1])),
            "346d8c96a2454213fcc0daff3c96ad0398148181b9fa6488f7ae2c0af5b20aa0"
        );
    }

    /// Independent reconstruction of the domain-separated padding constant — computed here from its raw
    /// preimage bytes, not by referencing [`EMPTY`] itself, so a bug in the constant's definition would
    /// still be caught.
    fn independent_empty() -> B256 {
        keccak256(*b"rome-zk/merkle/empty/v1")
    }

    /// Golden: 3 leaves — odd bottom level pads leaf 2 with the domain-separated `EMPTY` sentinel
    /// (it used to be duplicated against itself), then the two level-1 nodes pair to the root.
    /// Reconstructed independently of `root()`.
    #[test]
    fn three_leaves_root_matches_independent_reconstruction() {
        let l0 = leaf(1);
        let l1 = leaf(2);
        let l2 = leaf(3);
        let empty = independent_empty();
        let h01 = keccak256([l0.as_slice(), l1.as_slice()].concat());
        let h2e = keccak256([l2.as_slice(), empty.as_slice()].concat());
        let expected = keccak256([h01.as_slice(), h2e.as_slice()].concat());
        assert_eq!(root(&[l0, l1, l2]), expected);
    }

    /// Golden: 5 leaves — two levels of `EMPTY` padding (level 0 has 5 → pairs (0,1) (2,3) (4,EMPTY);
    /// level 1 has 3 → pairs (01,23) (4E,EMPTY); level 2 has 2 → root). Reconstructed independently of
    /// `root()`.
    #[test]
    fn five_leaves_root_matches_independent_reconstruction() {
        let leaves: Vec<B256> = (1..=5u8).map(leaf).collect();
        let empty = independent_empty();
        let h01 = keccak256([leaves[0].as_slice(), leaves[1].as_slice()].concat());
        let h23 = keccak256([leaves[2].as_slice(), leaves[3].as_slice()].concat());
        let h4e = keccak256([leaves[4].as_slice(), empty.as_slice()].concat());
        let h0123 = keccak256([h01.as_slice(), h23.as_slice()].concat());
        let h4e_empty = keccak256([h4e.as_slice(), empty.as_slice()].concat());
        let expected = keccak256([h0123.as_slice(), h4e_empty.as_slice()].concat());
        assert_eq!(root(&leaves), expected);
    }

    #[test]
    fn proof_out_of_range_is_none() {
        let leaves = vec![leaf(1), leaf(2)];
        assert!(proof(&leaves, 2).is_none());
    }

    /// The old "duplicate the last node" odd-level padding made
    /// `root([a,b,c])` collide with `root([a,b,c,c])` — a position proof for "c at index 3" would verify
    /// against a sub-block that actually had only 3 txs. The fix pads with a domain-separated `EMPTY`
    /// sentinel instead, so the two roots must now differ.
    #[test]
    fn three_leaves_and_four_leaves_with_a_duplicated_last_leaf_have_different_roots() {
        let l0 = leaf(1);
        let l1 = leaf(2);
        let l2 = leaf(3);
        let root_three = root(&[l0, l1, l2]);
        let root_four_duplicated = root(&[l0, l1, l2, l2]);
        assert_ne!(
            root_three, root_four_duplicated,
            "root([a,b,c]) must differ from root([a,b,c,c]) — the old duplicate-padding scheme made \
             these collide"
        );
    }

    /// A proof of length 2 (as produced for a 3-leaf tree) has capacity
    /// for 4 index slots (0..=3), but only indices 0..=2 are real. `verify` must reject an out-of-range
    /// index even when the hash chain would otherwise reconstruct the same root (the aliasing case: the
    /// real proof for index 2 also happens to reconstruct the root at index 6, since 6 and 2 share the
    /// same low bits through this proof's two levels) — the fix is an explicit `index < 2^proof.len()`
    /// bound, not just the padding-constant swap above.
    #[test]
    fn proof_verification_rejects_an_index_beyond_the_proofs_capacity() {
        let l0 = leaf(1);
        let l1 = leaf(2);
        let l2 = leaf(3);
        let leaves = vec![l0, l1, l2];
        let r = root(&leaves);
        let p = proof(&leaves, 2).expect("index 2 is in range for 3 leaves");
        assert_eq!(p.len(), 2, "a 3-leaf tree's proof must have 2 levels");

        // The legitimate claim: leaf l2 really is at index 2.
        assert!(verify(r, l2, 2, &p));

        // Index 3 doesn't exist (only 3 leaves) — even the harmless EMPTY sentinel must not verify at an
        // index a real proof was never built for.
        assert!(
            !verify(r, l2, 3, &p),
            "index 3 must not verify against a 3-leaf root"
        );

        // The aliasing case: index 6 shares the same low bits as index 2 through a 2-level proof
        // (6 = 0b110, 2 = 0b010 — both land on the same hash path), so without an explicit capacity
        // check the hash chain alone would reconstruct the same root.
        assert!(
            !verify(r, l2, 6, &p),
            "index 6 must not verify via a proof whose capacity (2^2 = 4) it exceeds"
        );
    }

    /// Round trip: every leaf's proof, against every tree size from 1 to 9 (covering even, odd, and
    /// power-of-two boundaries), verifies against the tree's own root; a proof for the wrong leaf value at
    /// the same index must fail.
    #[test]
    fn every_leaf_proof_round_trips_for_a_range_of_tree_sizes() {
        for n in 1..=9usize {
            let leaves: Vec<B256> = (0..n).map(|i| leaf(i as u8 + 1)).collect();
            let r = root(&leaves);
            for i in 0..n {
                let p = proof(&leaves, i).expect("index in range");
                assert!(
                    verify(r, leaves[i], i, &p),
                    "leaf {i} of {n} must verify under the tree's root"
                );
                assert!(
                    !verify(r, leaf(200), i, &p),
                    "a wrong leaf value at the same index must not verify"
                );
            }
        }
    }
}
