//! The trie walk shared by [`crate::verify_account`] and [`crate::verify_storage`]: follow `nodes` from
//! `root` down to `key_nibbles`, verifying every hash-referenced node's own keccak against the reference
//! that led to it before ever looking inside it, and following an embedded (inline) child directly by its
//! own bytes with no hash check at all — its integrity is already covered by its parent's own hash, which
//! *was* checked, exactly as the Yellow Paper's rule intends.

use crate::nibbles::decode_compact_path;
use crate::rlp::{decode_top_level_list, Item};
use crate::MptError;

/// Where the *next* node's bytes come from: pulled from the caller's `nodes` array at `index` (and its
/// keccak checked against `expected_hash` before it is read), or already in hand as `bytes` — an inline
/// child embedded directly in its parent's own encoding, requiring no array slot and no hash check.
enum Cur<'a> {
    FromArray {
        index: usize,
        expected_hash: [u8; 32],
    },
    Inline {
        bytes: &'a [u8],
    },
}

/// The walk's outcome once it reaches a value or a point where the key provably is not in the trie.
pub(crate) enum WalkOutcome<'a> {
    /// The full key path was matched to a leaf (or a branch's own value slot); `value` is that leaf's/
    /// branch's raw value bytes (not yet RLP-decoded into an account or a storage scalar).
    Found(&'a [u8]),
    /// The key path diverged from the trie at some point (an empty branch slot, a leaf whose stored path
    /// disagrees, or an extension whose prefix disagrees) — proof of *exclusion*, not proof of anything
    /// about `nodes` being malformed.
    Diverged,
}

/// Walks `nodes` from `root` along `key_nibbles`. Returns the outcome plus how many entries of `nodes`
/// were consumed from the front — callers compare that count against `nodes.len()` themselves
/// (`MptError::TrailingNodes` when a proof carries more nodes than the walk ever needed).
pub(crate) fn walk<'a>(
    root: [u8; 32],
    key_nibbles: &[u8],
    nodes: &'a [Vec<u8>],
) -> Result<(WalkOutcome<'a>, usize), MptError> {
    let mut cur = Cur::FromArray {
        index: 0,
        expected_hash: root,
    };
    let mut remaining: &[u8] = key_nibbles;
    // Index of the next `nodes` entry the walk has not yet consumed — tracked independently of `cur` so
    // it stays correct while `cur` is `Inline` (an inline child consumes nothing from the array).
    let mut next_index: usize = 0;

    loop {
        let (node_bytes, err_index): (&'a [u8], usize) = match cur {
            Cur::FromArray {
                index,
                expected_hash,
            } => {
                let Some(n) = nodes.get(index) else {
                    // The path calls for another hash-referenced node but the proof ran out of them —
                    // an incomplete/malformed proof, reported the same way any other unparsable node
                    // shape is: this crate's error set has no dedicated "proof too short" variant, and
                    // "the RLP data this index names is missing" is exactly what BadRlp means here.
                    return Err(MptError::BadRlp { index });
                };
                let h = rome_zk_merkle::keccak256(&[n.as_slice()]);
                if h != expected_hash {
                    return Err(if index == 0 {
                        MptError::RootMismatch
                    } else {
                        MptError::HashMismatch { index }
                    });
                }
                next_index = index + 1;
                (n.as_slice(), index)
            }
            Cur::Inline { bytes } => (bytes, next_index.saturating_sub(1)),
        };

        let items =
            decode_top_level_list(node_bytes).map_err(|_| MptError::BadRlp { index: err_index })?;

        match items.len() {
            2 => match decode_leaf_or_extension(&items, remaining, err_index)? {
                LeafOrExt::Leaf(outcome) => return Ok((outcome, next_index)),
                LeafOrExt::Extension { consumed, child } => {
                    remaining = &remaining[consumed..];
                    cur = resolve_child(child, next_index, err_index)?;
                }
            },
            17 => {
                if remaining.is_empty() {
                    return match items[16] {
                        Item::Str([]) => Ok((WalkOutcome::Diverged, next_index)),
                        Item::Str(s) => Ok((WalkOutcome::Found(s), next_index)),
                        Item::List(_) => Err(MptError::BadRlp { index: err_index }),
                    };
                }
                let nib = remaining[0] as usize;
                remaining = &remaining[1..];
                match items[nib] {
                    Item::Str([]) => return Ok((WalkOutcome::Diverged, next_index)),
                    other => cur = resolve_child(other, next_index, err_index)?,
                }
            }
            n => {
                return Err(MptError::BadNodeShape {
                    index: err_index,
                    items: n,
                })
            }
        }
    }
}

enum LeafOrExt<'a> {
    Leaf(WalkOutcome<'a>),
    Extension { consumed: usize, child: Item<'a> },
}

/// Handles a 2-item node (leaf or extension, distinguished by the hex-prefix terminator flag on its
/// first item). A path/prefix mismatch is [`WalkOutcome::Diverged`] (exclusion), never an error — the
/// caller (`verify_account`/`verify_storage`) decides whether divergence is itself refused.
fn decode_leaf_or_extension<'a>(
    items: &[Item<'a>],
    remaining: &[u8],
    err_index: usize,
) -> Result<LeafOrExt<'a>, MptError> {
    let path_bytes = match items[0] {
        Item::Str(s) => s,
        Item::List(_) => return Err(MptError::BadRlp { index: err_index }),
    };
    let (is_leaf, path_nibbles) =
        decode_compact_path(path_bytes).ok_or(MptError::BadRlp { index: err_index })?;
    if is_leaf {
        let value = match items[1] {
            Item::Str(s) => s,
            Item::List(_) => return Err(MptError::BadRlp { index: err_index }),
        };
        if path_nibbles == remaining {
            Ok(LeafOrExt::Leaf(WalkOutcome::Found(value)))
        } else {
            Ok(LeafOrExt::Leaf(WalkOutcome::Diverged))
        }
    } else if remaining.len() >= path_nibbles.len()
        && remaining[..path_nibbles.len()] == path_nibbles[..]
    {
        Ok(LeafOrExt::Extension {
            consumed: path_nibbles.len(),
            child: items[1],
        })
    } else {
        Ok(LeafOrExt::Leaf(WalkOutcome::Diverged))
    }
}

/// Turns a branch/extension child-reference item into the next [`Cur`]: a 32-byte string is a keccak
/// reference to the next array entry (`next_index`, checked against `expected_hash` on the next loop
/// turn); any other-length string is malformed; a nested list is an embedded child, followed directly
/// with no hash check (its bytes are already covered by its parent's own hash).
fn resolve_child<'a>(
    item: Item<'a>,
    next_index: usize,
    err_index: usize,
) -> Result<Cur<'a>, MptError> {
    match item {
        Item::Str(s) if s.len() == 32 => {
            let mut h = [0u8; 32];
            h.copy_from_slice(s);
            Ok(Cur::FromArray {
                index: next_index,
                expected_hash: h,
            })
        }
        Item::Str(_) => Err(MptError::BadRlp { index: err_index }),
        Item::List(raw) => Ok(Cur::Inline { bytes: raw }),
    }
}
