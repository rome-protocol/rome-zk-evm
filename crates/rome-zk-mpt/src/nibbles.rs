//! Nibble-path helpers: turning a 32-byte trie key into its 64 nibbles, and decoding the hex-prefix
//! ("compact") encoding a leaf/extension node's path is stored in (Yellow Paper appendix D).

/// Splits `bytes` into big-endian nibbles, high nibble first per byte (a 32-byte key becomes 64 nibbles —
/// every Ethereum state/storage trie key is `keccak256(...)`, always 32 bytes).
pub(crate) fn to_nibbles(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(b >> 4);
        out.push(b & 0x0f);
    }
    out
}

/// Decodes a hex-prefix-encoded path: the first byte's high nibble carries two flag bits (bit 1 = terminator/leaf,
/// bit 0 = odd nibble count); an odd count folds its first nibble into that same byte's low nibble, an even count
/// leaves it as padding — and the Yellow Paper fixes that padding nibble at `0`, so an even-count encoding whose
/// low nibble is nonzero is refused (`None`), not silently decoded with the padding discarded (a
/// belt-and-suspenders canonical-form check — every real node is keccak-bound to a trusted root, so this is a
/// laxity, not a soundness gap, but a future caller with a laxer trust model should not inherit it by accident).
/// Returns `(is_leaf, nibbles)`, or `None` for an empty input (every real encoding is at least one byte, even for a
/// zero-length path) or a non-canonical even-path padding nibble.
pub(crate) fn decode_compact_path(encoded: &[u8]) -> Option<(bool, Vec<u8>)> {
    let first = *encoded.first()?;
    let is_leaf = (first & 0x20) != 0;
    let is_odd = (first & 0x10) != 0;
    if !is_odd && (first & 0x0f) != 0 {
        return None;
    }
    let mut nibbles = Vec::with_capacity(encoded.len() * 2);
    if is_odd {
        nibbles.push(first & 0x0f);
    }
    for &b in &encoded[1..] {
        nibbles.push(b >> 4);
        nibbles.push(b & 0x0f);
    }
    Some((is_leaf, nibbles))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_nibbles_splits_high_then_low() {
        assert_eq!(to_nibbles(&[0xab, 0x12]), vec![0xa, 0xb, 0x1, 0x2]);
    }

    #[test]
    fn compact_path_even_leaf() {
        // flag nibble 0x2 (leaf, even) + padding nibble 0, then bytes 0xab 0xcd.
        let (is_leaf, nibbles) = decode_compact_path(&[0x20, 0xab, 0xcd]).unwrap();
        assert!(is_leaf);
        assert_eq!(nibbles, vec![0xa, 0xb, 0xc, 0xd]);
    }

    #[test]
    fn compact_path_odd_extension() {
        // flag nibble 0x1 (extension, odd) folded with first nibble 0xa into one byte, then 0xbc.
        let (is_leaf, nibbles) = decode_compact_path(&[0x1a, 0xbc]).unwrap();
        assert!(!is_leaf);
        assert_eq!(nibbles, vec![0xa, 0xb, 0xc]);
    }

    #[test]
    fn empty_input_is_none() {
        assert!(decode_compact_path(&[]).is_none());
    }

    /// An even-path HP byte's low nibble is padding and the Yellow Paper's hex-prefix encoding fixes it at 0 — a
    /// non-canonical `0x2f` (even/leaf flag `0x2`, padding nibble `0xf`) must be refused, not silently accepted
    /// with the padding nibble discarded.
    #[test]
    fn non_canonical_even_padding_nibble_is_refused() {
        assert!(
            decode_compact_path(&[0x2f]).is_none(),
            "an even-path HP byte with a nonzero low (padding) nibble must be refused, not decoded"
        );
    }

    /// The zero-padding case (already covered by `compact_path_even_leaf` above) must keep decoding —
    /// the new check only refuses a *nonzero* padding nibble.
    #[test]
    fn canonical_even_padding_nibble_still_decodes() {
        let (is_leaf, nibbles) = decode_compact_path(&[0x20, 0xab, 0xcd]).unwrap();
        assert!(is_leaf);
        assert_eq!(nibbles, vec![0xa, 0xb, 0xc, 0xd]);
    }
}
