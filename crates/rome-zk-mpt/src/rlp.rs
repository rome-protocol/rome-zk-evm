//! A small, bounded RLP decoder — this crate's own, not `alloy-rlp`.
//!
//! **Why not `alloy-rlp`:** the security contract this crate needs is bound-first — every trie node this verifier
//! ever looks at has already been checked against [`crate::MAX_NODE_BYTES`]/[`crate::MAX_NODES`] *before* a single
//! byte of it is parsed (`crate::check_bounds`), and the only shapes this verifier ever has to recognise are the
//! four fixed Ethereum trie-node forms (branch: 17 items; leaf/extension: 2 items; a value string; a hash-or-inline
//! child reference). A general-purpose RLP library decodes arbitrary nesting and length and would need its own
//! bounds re-imposed after the fact to get back to the same contract — a purpose-built decoder that only ever
//! recognises "one flat list of items, each either a string or a nested list, over a byte slice this crate has
//! already size-checked" is smaller to audit than wrapping a general decoder and re-deriving the same bound.
//! `alloy-rlp` was not pulled in to test against this crate's actual target set either (SBF via `cargo build-sbf`,
//! plus a future zkVM guest that stays `solana-program`-free) — the bounded decoder below has no dependency at all,
//! so it carries zero risk on either target by construction.
//!
//! **What this decoder recognises**, per the Yellow Paper's RLP grammar (single-byte forms are handled by
//! the `0x00..=0x7f` case; see [`decode_header`]):
//! - a byte string, short (`0x80..=0xb7`, length 0..55) or long (`0xb8..=0xbf`, a length-of-length prefix
//!   then the length, then the payload);
//! - a list, short (`0xc0..=0xf7`) or long (`0xf8..=0xff`), same length-of-length shape;
//! - the long forms are refused (canonical-RLP check) when the encoded length is `< 56` — a length that
//!   short form could already have expressed is not a length any real Ethereum encoder ever produces, and
//!   accepting it would let two different byte strings decode to the same node-and-hash under a laxer
//!   reader while a stricter one produces a different reading — never a case this crate's own bounds
//!   forbid outright, but neither is it a shape a real `eth_getProof` output ever contains, so refusing it
//!   costs nothing against the fixtures this crate is exercised on.
//!
//! A list item's own encoding is preserved whole ([`Item::List`] carries the header *and* payload) so a
//! caller can re-decode it as a standalone node without another allocation — this is exactly the
//! "embedded/inline child" case (Yellow Paper: a child node whose own RLP encoding is under 32 bytes is
//! embedded directly in its parent rather than referenced by a 32-byte keccak hash); [`crate::walk`]
//! treats an [`Item::List`] child reference as a node to decode in place, never a hash to look up.

/// One item inside a decoded RLP list: either a byte string's payload, or a nested list's *entire* own
/// encoding (header + payload) — the shape [`crate::walk`] needs to treat an embedded child as a node in
/// its own right without a further allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Item<'a> {
    Str(&'a [u8]),
    List(&'a [u8]),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Str,
    List,
}

/// A node's bytes were not well-formed RLP by this decoder's rules (truncated, a claimed length past the
/// slice, or a non-canonical long form) — every caller maps this to [`crate::MptError::BadRlp`] (with
/// whichever node index it was decoding) or [`crate::MptError::BadAccountRlp`], never propagates it as-is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RlpError;

/// Decodes the RLP header at the start of `data`: returns `(kind, payload_start, payload_len,
/// total_consumed)`. Every offset is checked against `data.len()` before use — a length field is never
/// trusted past the slice that actually backs it, however large it claims to be.
fn decode_header(data: &[u8]) -> Result<(Kind, usize, usize, usize), RlpError> {
    let b0 = *data.first().ok_or(RlpError)?;
    if b0 < 0x80 {
        // A single byte in [0x00, 0x7f] is its own one-byte RLP string encoding.
        Ok((Kind::Str, 0, 1, 1))
    } else if b0 <= 0xb7 {
        let len = (b0 - 0x80) as usize;
        let end = 1usize.checked_add(len).ok_or(RlpError)?;
        if end > data.len() {
            return Err(RlpError);
        }
        Ok((Kind::Str, 1, len, end))
    } else if b0 <= 0xbf {
        let (start, len) = decode_long_len(data, b0 - 0xb7)?;
        let end = start.checked_add(len).ok_or(RlpError)?;
        if end > data.len() {
            return Err(RlpError);
        }
        Ok((Kind::Str, start, len, end))
    } else if b0 <= 0xf7 {
        let len = (b0 - 0xc0) as usize;
        let end = 1usize.checked_add(len).ok_or(RlpError)?;
        if end > data.len() {
            return Err(RlpError);
        }
        Ok((Kind::List, 1, len, end))
    } else {
        let (start, len) = decode_long_len(data, b0 - 0xf7)?;
        let end = start.checked_add(len).ok_or(RlpError)?;
        if end > data.len() {
            return Err(RlpError);
        }
        Ok((Kind::List, start, len, end))
    }
}

/// Decodes a long-form (string or list) length-of-length prefix: `len_of_len` bytes right after the
/// leading tag byte give the payload length, big-endian, no leading zero (canonical). Returns `(payload
/// start offset, payload length)`; refuses a length `< 56` (short form could have encoded it).
fn decode_long_len(data: &[u8], len_of_len: u8) -> Result<(usize, usize), RlpError> {
    let len_of_len = len_of_len as usize;
    if len_of_len == 0 || 1 + len_of_len > data.len() {
        return Err(RlpError);
    }
    let len_bytes = &data[1..1 + len_of_len];
    if len_bytes[0] == 0 {
        return Err(RlpError); // non-canonical: a leading zero length byte
    }
    if len_bytes.len() > 8 {
        return Err(RlpError);
    }
    let mut v: u64 = 0;
    for &b in len_bytes {
        v = (v << 8) | b as u64;
    }
    let len = usize::try_from(v).map_err(|_| RlpError)?;
    if len < 56 {
        return Err(RlpError); // non-canonical: short form could have expressed this length
    }
    Ok((1 + len_of_len, len))
}

/// Decodes `data` as exactly one top-level RLP **list** (every trie node is a list — a branch, a leaf or
/// an extension), refusing any trailing byte after it. Returns the list's items in order.
pub(crate) fn decode_top_level_list(data: &[u8]) -> Result<Vec<Item<'_>>, RlpError> {
    let (kind, start, len, consumed) = decode_header(data)?;
    if kind != Kind::List || consumed != data.len() {
        return Err(RlpError);
    }
    decode_items(&data[start..start + len])
}

/// Decodes a flat run of RLP items filling `data` exactly (a list's own payload). Bounded implicitly: a
/// node's total byte length is already checked by `crate::check_bounds` before this ever runs, so the
/// number of items this can produce is bounded by that same byte count.
fn decode_items(mut data: &[u8]) -> Result<Vec<Item<'_>>, RlpError> {
    let mut items = Vec::new();
    while !data.is_empty() {
        let (kind, start, len, consumed) = decode_header(data)?;
        match kind {
            Kind::Str => items.push(Item::Str(&data[start..start + len])),
            Kind::List => items.push(Item::List(&data[..consumed])),
        }
        data = &data[consumed..];
    }
    Ok(items)
}

/// Decodes `data` as exactly one top-level RLP **string** (used for the double-RLP-wrapped storage
/// value — see `crate::decode_storage_value`'s doc), refusing any trailing byte after it.
pub(crate) fn decode_top_level_string(data: &[u8]) -> Result<&[u8], RlpError> {
    let (kind, start, len, consumed) = decode_header(data)?;
    if kind != Kind::Str || consumed != data.len() {
        return Err(RlpError);
    }
    Ok(&data[start..start + len])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_byte_below_0x80_is_its_own_string() {
        let items = decode_top_level_list(&[0xc1, 0x05]).unwrap();
        assert_eq!(items, vec![Item::Str(&[0x05])]);
    }

    #[test]
    fn short_string_and_short_list_round_trip() {
        // A list of two short strings: [0x82, b'h', b'i']
        let data = [0xc2, 0x81, b'h'];
        let items = decode_top_level_list(&data).unwrap();
        assert_eq!(items, vec![Item::Str(b"h")]);
    }

    #[test]
    fn nested_list_item_preserves_its_whole_encoding() {
        // Outer list containing one inner list [ [0x01] ]: 0xc2 0xc1 0x01
        let data = [0xc2, 0xc1, 0x01];
        let items = decode_top_level_list(&data).unwrap();
        assert_eq!(items.len(), 1);
        match items[0] {
            Item::List(raw) => assert_eq!(raw, &[0xc1, 0x01]),
            Item::Str(_) => panic!("expected a nested list item"),
        }
    }

    #[test]
    fn trailing_byte_after_the_top_level_value_is_refused() {
        // A single string 0x05 followed by a stray extra byte.
        assert!(decode_top_level_list(&[0xc1, 0x05, 0xff]).is_err());
    }

    #[test]
    fn truncated_length_prefix_is_refused_not_panicking() {
        assert!(decode_header(&[0xb8]).is_err());
        assert!(decode_header(&[0xf8]).is_err());
        assert!(decode_header(&[]).is_err());
    }

    #[test]
    fn claimed_length_past_the_slice_is_refused() {
        // 0x83 says "3-byte string" but only one byte follows.
        assert!(decode_header(&[0x83, 0x01]).is_err());
    }

    #[test]
    fn non_canonical_long_form_for_a_short_length_is_refused() {
        // 0xb8 0x01 'x' encodes a 1-byte string the short form (0x81 'x') could already express.
        assert!(decode_header(&[0xb8, 0x01, b'x']).is_err());
    }

    #[test]
    fn top_level_string_helper_round_trips_and_refuses_a_list() {
        assert_eq!(decode_top_level_string(&[0x01]).unwrap(), &[0x01]);
        assert!(decode_top_level_string(&[0xc1, 0x01]).is_err());
    }
}
