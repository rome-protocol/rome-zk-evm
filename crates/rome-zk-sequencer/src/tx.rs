//! Raw tx parsing shared by [`crate::admission`] (gate-time checks) and [`crate::executor`]
//! (execution-time checks) so the two layers never disagree about what a tx's sender/nonce/hash are.

use alloy::consensus::transaction::SignerRecoverable;
use alloy::consensus::{Transaction as _, TxEnvelope};
use alloy::primitives::{Address, Bytes, TxHash};
use alloy_eips::eip2718::Decodable2718;

#[derive(Debug, Clone)]
pub struct ParsedTx {
    pub raw: Bytes,
    pub tx_hash: TxHash,
    pub sender: Address,
    pub nonce: u64,
    /// `None` for a pre-EIP-155 legacy tx that carries no chain id at all.
    pub chain_id: Option<u64>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("rlp decode: {0}")]
    Decode(String),
    #[error("recover signer: {0}")]
    Recover(String),
}

pub fn parse(raw: Bytes) -> Result<ParsedTx, ParseError> {
    let mut slice = raw.as_ref();
    let envelope =
        TxEnvelope::decode_2718(&mut slice).map_err(|e| ParseError::Decode(e.to_string()))?;
    let sender = envelope
        .recover_signer()
        .map_err(|e| ParseError::Recover(e.to_string()))?;
    Ok(ParsedTx {
        tx_hash: *envelope.tx_hash(),
        sender,
        nonce: envelope.nonce(),
        chain_id: envelope.chain_id(),
        raw,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::signed_raw_tx;
    use alloy::signers::local::PrivateKeySigner;

    #[test]
    fn parses_sender_nonce_and_chain_id() {
        let signer = PrivateKeySigner::random();
        let raw = signed_raw_tx(&signer, 200_101, 3);
        let parsed = parse(raw).unwrap();
        assert_eq!(parsed.sender, signer.address());
        assert_eq!(parsed.nonce, 3);
        assert_eq!(parsed.chain_id, Some(200_101));
    }

    #[test]
    fn malformed_bytes_are_rejected() {
        let err = parse(Bytes::from_static(&[0xFF, 0x00, 0x01])).unwrap_err();
        assert!(matches!(err, ParseError::Decode(_)));
    }
}
