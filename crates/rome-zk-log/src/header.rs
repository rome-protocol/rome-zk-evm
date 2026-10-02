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
//!   tx_root: bytes32, receipts_root: bytes32, gas_used: u64, prev_hash: bytes32 ]
//! ```
//!
//! Each integer is RLP-encoded as its minimal big-endian byte string (`alloy_rlp`'s standard integer
//! encoding); each `bytes32` field is a fixed 32-byte RLP string. `header_hash = keccak256(rlp(header))`.
//! Golden bytes for a fixed header are pinned in this module's tests so a future field reorder, a width
//! change, or an `alloy_rlp` upgrade that changes integer trimming is caught immediately.

use alloy::primitives::{keccak256, B256};
use alloy_rlp::{RlpDecodable, RlpEncodable};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct SubBlockHeader {
    pub chain_id: u64,
    pub block: u64,
    pub index: u16,
    pub timestamp_us: u64,
    pub tx_root: B256,
    pub receipts_root: B256,
    pub gas_used: u64,
    pub prev_hash: B256,
}

impl SubBlockHeader {
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
