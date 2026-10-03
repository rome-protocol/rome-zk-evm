//! Sub-block header: canonical encoding and hash.
//!
//! ## Byte layout
//!
//! `SubBlockHeader` is RLP-encoded (`alloy_rlp::Encodable`, derived) as a **list** whose items are the
//! struct fields in declaration order — this order is the canonical layout and must never be reordered
//! without a design review (a reorder silently changes every header hash and signature):
//!
//! ```text
//! [ chain_id: u64, block: u64, index: u16, timestamp_us: u64,
//!   tx_root: bytes32, receipts_root: bytes32, gas_used: u64, prev_hash: bytes32,
//!   (deposits_end: u64)? ]
//! ```
//!
//! The ninth item, `deposits_end`, is optional and is only ever present on the index-0 header of a block that credits
//! deposits: it is the deposit queue's index after that block. `None` is the absence of the item, so a header without
//! deposits encodes to exactly the eight-item list it always did (the golden bytes below do not change). The
//! encoding is written by hand, not derived, for the same reason as the channel block's fifth field: the derive's
//! trailing mode would accept a present-but-empty item as `None`, a second encoding of the same header.
//!
//! Each integer is RLP-encoded as its minimal big-endian byte string (`alloy_rlp`'s standard integer
//! encoding); each `bytes32` field is a fixed 32-byte RLP string. `header_hash = keccak256(rlp(header))`.
//! Golden bytes for a fixed header are pinned in this module's tests so a future field reorder, a width
//! change, or an `alloy_rlp` upgrade that changes integer trimming is caught immediately.

use alloy::primitives::{keccak256, B256};
use alloy_rlp::{Decodable, Encodable, Header};

/// Domain separator for sub-block header signatures: the sequencer key
/// signs `keccak256(SIGNING_DOMAIN || header_hash)`, never the bare `header_hash`, so a signature over a
/// sub-block header can never be replayed as a valid signature over some other structure this key might
/// sign in the future that happens to reuse the same bare RLP hash.
pub const SIGNING_DOMAIN: &[u8] = b"rome-zk/sub-block/v1";

/// One sub-block's header. `block` = 20 sub-blocks; `index` is this sub-block's position within its
/// block, `0..20`. `timestamp_us` is a microsecond wall-clock timestamp (sub-blocks are not EVM blocks
/// and are not bound to the integer-second rule); the EVM block's own timestamp is defined as the
/// **first** sub-block's timestamp, truncated to seconds (the EVM block timestamp has a 1 s
/// cadence; sub-blocks carry µs).
///
/// `deposits_end` is the deposit queue's index after this block, set only on the index-0 header of a block that credits
/// deposits (see the module doc). The header hash and the signature cover it, so a signed pre-confirmation commits
/// to the range the block credits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubBlockHeader {
    pub chain_id: u64,
    pub block: u64,
    pub index: u16,
    pub timestamp_us: u64,
    pub tx_root: B256,
    pub receipts_root: B256,
    pub gas_used: u64,
    pub prev_hash: B256,
    pub deposits_end: Option<u64>,
}

impl SubBlockHeader {
    fn payload_length(&self) -> usize {
        self.chain_id.length()
            + self.block.length()
            + self.index.length()
            + self.timestamp_us.length()
            + self.tx_root.length()
            + self.receipts_root.length()
            + self.gas_used.length()
            + self.prev_hash.length()
            + self.deposits_end.map_or(0, |d| d.length())
    }

    /// Canonical RLP encoding of this header (see module doc for the byte layout).
    pub fn encode_canonical(&self) -> Vec<u8> {
        alloy_rlp::encode(self)
    }

    /// `keccak256` of the canonical encoding — a content-addressed identifier for this header (used for
    /// `prev_hash` chaining and as the value everything else names the header by). **Not** what gets
    /// signed — see [`Self::signing_hash`].
    pub fn hash(&self) -> B256 {
        keccak256(self.encode_canonical())
    }

    /// The value the sequencer key actually signs: `keccak256(SIGNING_DOMAIN || hash())`. Domain
    /// separation means a signature here can never be confused with, or replayed as, a signature over any
    /// other structure signed by the same key.
    pub fn signing_hash(&self) -> B256 {
        keccak256([SIGNING_DOMAIN, self.hash().as_slice()].concat())
    }

    /// The EVM block timestamp derived from this sub-block, valid only when `index == 0` (the block timestamp is
    /// its first sub-block's second).
    pub fn block_timestamp_secs(&self) -> u64 {
        self.timestamp_us / 1_000_000
    }
}

impl Encodable for SubBlockHeader {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        Header {
            list: true,
            payload_length: self.payload_length(),
        }
        .encode(out);
        self.chain_id.encode(out);
        self.block.encode(out);
        self.index.encode(out);
        self.timestamp_us.encode(out);
        self.tx_root.encode(out);
        self.receipts_root.encode(out);
        self.gas_used.encode(out);
        self.prev_hash.encode(out);
        if let Some(d) = self.deposits_end {
            d.encode(out);
        }
    }

    fn length(&self) -> usize {
        let payload_length = self.payload_length();
        Header {
            list: true,
            payload_length,
        }
        .length()
            + payload_length
    }
}

impl Decodable for SubBlockHeader {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }
        if buf.len() < header.payload_length {
            return Err(alloy_rlp::Error::InputTooShort);
        }
        let (mut payload, rest) = buf.split_at(header.payload_length);
        let chain_id = u64::decode(&mut payload)?;
        let block = u64::decode(&mut payload)?;
        let index = u16::decode(&mut payload)?;
        let timestamp_us = u64::decode(&mut payload)?;
        let tx_root = B256::decode(&mut payload)?;
        let receipts_root = B256::decode(&mut payload)?;
        let gas_used = u64::decode(&mut payload)?;
        let prev_hash = B256::decode(&mut payload)?;
        // `u64::decode` is canonical (no leading zero, no wrapped single byte), and a present item is always `Some`.
        let deposits_end = if payload.is_empty() {
            None
        } else {
            Some(u64::decode(&mut payload)?)
        };
        // Exactly eight or nine items: anything after the optional ninth is refused.
        if !payload.is_empty() {
            return Err(alloy_rlp::Error::ListLengthMismatch {
                expected: header.payload_length - payload.len(),
                got: header.payload_length,
            });
        }
        *buf = rest;
        Ok(SubBlockHeader {
            chain_id,
            block,
            index,
            timestamp_us,
            tx_root,
            receipts_root,
            gas_used,
            prev_hash,
            deposits_end,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_header() -> SubBlockHeader {
        SubBlockHeader {
            chain_id: 200_101,
            block: 42,
            index: 7,
            timestamp_us: 1_757_000_000_123_456,
            tx_root: B256::repeat_byte(0xAB),
            receipts_root: B256::repeat_byte(0xCD),
            gas_used: 462_000,
            prev_hash: B256::repeat_byte(0xEF),
            deposits_end: None,
        }
    }

    /// Golden bytes: pins the exact RLP layout described in the module doc. If this fails after an
    /// intentional field change, regenerate it deliberately — do not "fix" it by copying new output in
    /// without checking the field order against the header tuple in the lane design.
    #[test]
    fn header_canonical_encoding_is_stable() {
        let header = fixture_header();
        let encoded = header.encode_canonical();
        let expected_hex = "\
f87583030da52a0787063dfb70e0b240a0ababababababababababababababababababababababababababababababababa0cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd83070cb0a0efefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefef";
        // Decoded independently below rather than trusted blind: confirms the encoding round-trips
        // and that decoding recovers the exact same struct (canonical-ness, not just "some bytes").
        assert_eq!(hex::encode(&encoded), expected_hex);
        let decoded: SubBlockHeader = alloy_rlp::decode_exact(&encoded).expect("round trip decode");
        assert_eq!(decoded, header);
    }

    /// A header with `deposits_end` is the golden eight-item list with one more item appended and the list prefix
    /// grown by one byte, pinned here against the golden hex above, not re-derived through the encoder.
    #[test]
    fn deposits_end_appends_one_item_to_the_golden_bytes() {
        let none_hex = hex::encode(fixture_header().encode_canonical());
        assert!(none_hex.starts_with("f875"));
        let header = SubBlockHeader {
            deposits_end: Some(7),
            ..fixture_header()
        };
        let encoded = header.encode_canonical();
        assert_eq!(hex::encode(&encoded), format!("f876{}07", &none_hex[4..]));
        let decoded: SubBlockHeader = alloy_rlp::decode_exact(&encoded).expect("round trip decode");
        assert_eq!(decoded, header);
        // The signed hash covers it.
        assert_ne!(header.hash(), fixture_header().hash());
        assert_ne!(header.signing_hash(), fixture_header().signing_hash());
    }

    #[test]
    fn deposits_end_round_trips_across_integer_widths() {
        for d in [0u64, 1, 127, 128, 255, 256, 70_000, u64::MAX] {
            let header = SubBlockHeader {
                deposits_end: Some(d),
                ..fixture_header()
            };
            let decoded: SubBlockHeader =
                alloy_rlp::decode_exact(header.encode_canonical()).expect("decode");
            assert_eq!(decoded, header, "deposits_end {d}");
        }
    }

    /// Only eight or nine items decode: a tenth item, and a non-canonical ninth (leading zero), are refused.
    #[test]
    fn header_with_a_tenth_or_non_canonical_ninth_item_is_refused() {
        let base = fixture_header().encode_canonical();
        let payload = &base[2..];
        let build = |extra: &[u8]| {
            let mut p = payload.to_vec();
            p.extend_from_slice(extra);
            let mut out = vec![0xf8, p.len() as u8];
            out.extend_from_slice(&p);
            out
        };
        // A ninth item holding 7, then a tenth item.
        assert!(alloy_rlp::decode_exact::<SubBlockHeader>(&build(&[0x07, 0x08])).is_err());
        // Ninth item encoded with a leading zero byte: `0x82 0x00 0x07`.
        assert!(alloy_rlp::decode_exact::<SubBlockHeader>(&build(&[0x82, 0x00, 0x07])).is_err());
        // A one-byte value wrapped in a string prefix: `0x81 0x07`.
        assert!(alloy_rlp::decode_exact::<SubBlockHeader>(&build(&[0x81, 0x07])).is_err());
        // The well-formed shapes still decode.
        assert!(alloy_rlp::decode_exact::<SubBlockHeader>(&build(&[])).is_ok());
        assert!(alloy_rlp::decode_exact::<SubBlockHeader>(&build(&[0x07])).is_ok());
    }

    #[test]
    fn hash_is_keccak_of_canonical_encoding() {
        let header = fixture_header();
        let expected = keccak256(header.encode_canonical());
        assert_eq!(header.hash(), expected);
    }

    #[test]
    fn block_timestamp_truncates_to_seconds() {
        let header = fixture_header();
        assert_eq!(header.block_timestamp_secs(), 1_757_000_000);
    }

    /// Pins the exact `domain || header_hash` construction against the known fixture header's hash, so a
    /// future refactor of `signing_hash()` (e.g. swapping the concatenation order, or hashing the domain
    /// separately) is caught immediately.
    #[test]
    fn signing_hash_is_keccak_of_domain_concat_header_hash() {
        let header = fixture_header();
        let expected = keccak256([SIGNING_DOMAIN, header.hash().as_slice()].concat());
        assert_eq!(header.signing_hash(), expected);
    }

    /// The signing hash must differ from the bare header hash — this is the whole point of domain
    /// separation: a signature over `signing_hash()` must not be mistakable for a signature over
    /// `hash()` alone, so a key that signs sub-block headers cannot have its signature replayed as a
    /// valid signature over some future structure that happens to reuse the bare RLP hash.
    #[test]
    fn signing_hash_differs_from_bare_header_hash() {
        let header = fixture_header();
        assert_ne!(header.signing_hash(), header.hash());
    }

    #[test]
    fn signing_domain_constant_is_stable() {
        assert_eq!(SIGNING_DOMAIN, b"rome-zk/sub-block/v1");
    }
}
