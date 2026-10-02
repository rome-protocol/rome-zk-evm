//! Signed-tx fixtures shared by this crate's unit tests and the `tests/` integration suite.
//! Not part of the sequencer's product surface.

use alloy::consensus::transaction::SignerRecoverable;
use alloy::consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy::primitives::{Address, Bytes, TxKind, U256};
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::SignerSync;
use alloy_eips::eip2718::Encodable2718;

/// Build and sign a minimal EIP-1559 transfer, returning its EIP-2718 raw encoding (what
/// `eth_sendRawTransaction` accepts) — a flat 21 000 gas transfer, matching `MockExecutor`'s gas model.
pub fn signed_raw_tx(signer: &PrivateKeySigner, chain_id: u64, nonce: u64) -> Bytes {
    let tx = TxEip1559 {
        chain_id,
        nonce,
        gas_limit: 21_000,
        max_fee_per_gas: 1_000_000_000,
        max_priority_fee_per_gas: 1_000_000_000,
        to: TxKind::Call(Address::ZERO),
        value: U256::ZERO,
        access_list: Default::default(),
        input: Bytes::new(),
    };
    let sig_hash = tx.signature_hash();
    let signature = signer.sign_hash_sync(&sig_hash).expect("sign fixture tx");
    let signed = tx.into_signed(signature);
    let envelope = TxEnvelope::from(signed);
    Bytes::from(envelope.encoded_2718())
}

/// Recover the sender + tx hash of a raw tx produced by [`signed_raw_tx`], for test assertions.
pub fn sender_and_hash(raw: &Bytes) -> (Address, alloy::primitives::TxHash) {
    use alloy_eips::eip2718::Decodable2718;
    let mut slice = raw.as_ref();
    let envelope = TxEnvelope::decode_2718(&mut slice).expect("decode fixture tx");
    (
        envelope.recover_signer().expect("recover fixture signer"),
        *envelope.tx_hash(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_raw_tx_recovers_to_signer() {
        let signer = PrivateKeySigner::random();
        let raw = signed_raw_tx(&signer, 200_101, 0);
        let (sender, _hash) = sender_and_hash(&raw);
        assert_eq!(sender, signer.address());
    }
}
