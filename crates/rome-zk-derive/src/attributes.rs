//! `AttributesQueue`: turns one decoded [`rome_zk_channel::Block`] into the
//! committed [`BlockEnv`] the sequencer itself would have opened this block with (the block
//! environment is published, not inferred), plus the block's ordered raw tx list — the two things
//! [`crate::engine::EngineController`] needs to drive `forkchoiceUpdated`/`getPayload`/`newPayload`.
//!
//! Every field here is *reused* from `rome_zk_executor_api`, never recomputed independently — in
//! particular `prev_randao`, whose exact formula (`keccak256(chain_id ‖ number ‖ channel_id)`) is
//! pinned by that crate's own golden-vector test; recomputing it here would risk a silent drift between
//! what the sequencer commits and what this node derives, exactly the kind of divergence the strict
//! validity checks exist to catch.

use alloy_primitives::{Address, Bytes};
use rome_zk_channel::Block;
use rome_zk_executor_api::BlockEnv;

/// One block's derived attributes: the committed env plus its ordered, still-raw (not yet decoded) txs
/// — [`crate::engine::EngineController`] passes `txs` to the engine's forced-transaction-list payload
/// attributes verbatim (`noTxPool`: arrival order is execution order). `chain_id` rides
/// alongside `env` (rather than requiring `EngineController` to carry its own copy) so
/// [`crate::engine::EngineController::advance`] can build the SAME
/// [`rome_zk_executor_api::canonical_header_rule`] this struct's own `env.prev_randao` already came from
/// — one chain id, carried with the block it derives, never a second
/// independently-threaded copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attributes {
    pub chain_id: u64,
    pub env: BlockEnv,
    pub txs: Vec<Bytes>,
}

/// Builds this block's [`Attributes`] (the published block-environment fields): `coinbase` is the chain's
/// own fee recipient — the genesis `coinbase` (the SAME value the guest embeds at compile time and the
/// sequencer's `SealerState`/`replay_into_executor` read from their own loaded genesis — `Address::ZERO`
/// on Tiber, never a hardcoded literal here), passed in rather than re-read by this function so callers
/// own where their genesis comes from. `base_fee: None` lets the engine derive EIP-1559 base fee from its own parent
/// header, exactly as the design specifies.
pub fn attributes_for_block(chain_id: u64, fee_recipient: Address, block: &Block) -> Attributes {
    Attributes {
        chain_id,
        env: BlockEnv {
            number: block.number,
            timestamp_secs: block.timestamp,
            gas_limit: block.gas_limit,
            coinbase: fee_recipient,
            prev_randao: rome_zk_executor_api::prev_randao(chain_id, block.number),
            base_fee: None,
        },
        txs: block.txs.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_the_committed_env_using_the_shared_prev_randao_formula() {
        let block = Block {
            number: 5,
            timestamp: 1_757_000_005,
            gas_limit: 100_000_000,
            txs: vec![Bytes::from_static(b"tx")],
        };
        let attrs = attributes_for_block(200_101, Address::ZERO, &block);
        assert_eq!(attrs.chain_id, 200_101);
        assert_eq!(attrs.env.number, 5);
        assert_eq!(attrs.env.timestamp_secs, 1_757_000_005);
        assert_eq!(attrs.env.gas_limit, 100_000_000);
        assert_eq!(attrs.env.coinbase, Address::ZERO);
        assert_eq!(attrs.env.base_fee, None);
        assert_eq!(
            attrs.env.prev_randao,
            rome_zk_executor_api::prev_randao(200_101, 5),
            "must reuse the shared formula, not recompute an equivalent one"
        );
        assert_eq!(attrs.txs, block.txs);
    }

    /// A non-zero `fee_recipient` (a chain other than Tiber) must flow
    /// through to `env.coinbase` — and from there, via `engine::payload_attrs_from` /
    /// `canonical_header_rule`, into what the guest/derive/sequencer all check as the ONE rule-fixed
    /// `beneficiary`. `attributes_for_block` used to take no `fee_recipient` argument and hardcode
    /// `Address::ZERO`; the test requires the passed-in address to reach `env.coinbase`.
    #[test]
    fn a_non_zero_fee_recipient_flows_into_env_coinbase() {
        let block = Block {
            number: 5,
            timestamp: 1_757_000_005,
            gas_limit: 100_000_000,
            txs: vec![],
        };
        let fee_recipient = Address::repeat_byte(0x77);
        let attrs = attributes_for_block(200_101, fee_recipient, &block);
        assert_eq!(attrs.env.coinbase, fee_recipient);
    }
}
