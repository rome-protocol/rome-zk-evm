//! Sub-block header signing and the pre-confirmation shape.
//!
//! Every sub-block header is signed secp256k1 by the sequencer key
//! (`alloy_signer_local::PrivateKeySigner`, backed by `k256`) before it is appended to the ordered log.
//! A signed pre-confirmation whose tx is absent or misplaced in the finalized inbox is a slashable
//! proof against the poster bond — so the signature is over exactly the fields a challenger needs to
//! reconstruct that claim: which header (by hash) placed which tx at which position.

use alloy::primitives::{Address, Signature, SignatureError, B256};
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::SignerSync;

use crate::header::SubBlockHeader;

/// Sign a sub-block header with the sequencer key, over its **domain-separated** signing hash
/// ([`SubBlockHeader::signing_hash`]) — never the bare header hash (a bare
/// `keccak(rlp)` signature would be replayable across any future signed structure using the same key).
///
/// Signing a fixed-size hash with a valid secp256k1 signing key cannot fail (the only failure mode in
/// `alloy_signer_local` is a signer that refuses to sign, which `PrivateKeySigner` never does), so callers
/// get a `Signature` directly rather than threading a `Result` through the hot path.
pub fn sign_header(signer: &PrivateKeySigner, header: &SubBlockHeader) -> Signature {
    signer
        .sign_hash_sync(&header.signing_hash())
        .expect("PrivateKeySigner::sign_hash_sync over a prehash never fails")
}

/// Recover the signer address from a header's domain-separated signing hash and a signature. Used by
/// tests and by anyone verifying a pre-confirmation or a log record.
pub fn recover_header_signer(
    signing_hash: B256,
    signature: &Signature,
) -> Result<Address, SignatureError> {
    signature.recover_address_from_prehash(&signing_hash)
}

/// What the sequencer hands back to a sender once their tx's sub-block has been signed and fsynced to
/// the ordered log (only then is the pre-confirmation returned). This is also the shape
/// streamed per-tx on the `rome_subscribe(preconfirmations)` feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preconfirmation {
    pub tx_hash: B256,
    pub block: u64,
    pub sub_block_index: u16,
    /// Position of this tx within its sub-block (0-based, arrival order).
    pub position: u32,
    pub header_hash: B256,
    pub signature: Signature,
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::B256 as B256Type;

    fn fixture_header() -> SubBlockHeader {
        SubBlockHeader {
            chain_id: 200_101,
            block: 1,
            index: 0,
            timestamp_us: 1_757_000_000_000_000,
            tx_root: B256Type::repeat_byte(0x11),
            receipts_root: B256Type::repeat_byte(0x22),
            gas_used: 21_000,
            prev_hash: B256Type::ZERO,
        }
    }

    #[test]
    fn signature_recovers_to_signer_address() {
        let signer = PrivateKeySigner::random();
        let header = fixture_header();
        let sig = sign_header(&signer, &header);
        let recovered = recover_header_signer(header.signing_hash(), &sig).expect("recover");
        assert_eq!(recovered, signer.address());
    }

    #[test]
    fn signature_does_not_recover_to_a_different_header() {
        let signer = PrivateKeySigner::random();
        let header = fixture_header();
        let sig = sign_header(&signer, &header);
        let mut tampered = header;
        tampered.gas_used += 1;
        let recovered = recover_header_signer(tampered.signing_hash(), &sig).expect("recover");
        assert_ne!(recovered, signer.address());
    }

    /// A signature made over the domain-separated signing hash must not recover correctly if verified
    /// against the bare header hash instead — proves the domain separation actually changes what is
    /// signed, not just that a new method exists that happens to return the same value.
    #[test]
    fn signature_does_not_recover_against_the_bare_header_hash() {
        let signer = PrivateKeySigner::random();
        let header = fixture_header();
        let sig = sign_header(&signer, &header);
        let recovered = recover_header_signer(header.hash(), &sig).expect("recover");
        assert_ne!(
            recovered,
            signer.address(),
            "signature over signing_hash() must not also recover against the bare hash()"
        );
    }
}
