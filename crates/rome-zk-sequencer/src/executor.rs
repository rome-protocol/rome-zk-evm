//! The `Executor` trait — the executor seam.
//!
//! Shaped after reth's `PayloadJob` contract (`reth_payload_builder::PayloadJob`,
//! `crates/payload/builder/src/lib.rs`: `best_payload()` must always return a currently-valid payload,
//! never an error for "nothing built yet") so that an in-process reth, driven like reth's `LocalMiner`,
//! can implement this trait without changing anything above this seam: admission, the log, the sealer
//! and the RPC layer all depend only on this trait.
//!
//! The trait and its associated types now live in `rome-zk-executor-api` (re-exported here in full) —
//! `rome-zk-executor-reth` (the real implementation) must depend on the trait it implements,
//! and a `rome-zk-sequencer <-> rome-zk-executor-reth` cycle is not something Cargo allows. Every existing
//! caller in this crate keeps writing `crate::executor::Executor` etc. unchanged.
//!
//! The other implementation is [`MockExecutor`] — deterministic, no real EVM.

pub use rome_zk_executor_api::{
    prev_randao, BlockEnv, BlockOutcome, BlockSealInputs, Executor, ExecutorError, Head, Reason,
    RejectedTx, SubBlockLimits, SubBlockOutcome, PREV_RANDAO_EPOCH_BLOCKS,
};

use alloy::primitives::{Address, Bytes, TxHash, B256};
use std::collections::HashMap;
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;

/// Deterministic mock executor: no real EVM. Every tx costs a flat 21 000 gas; a tx whose nonce does not
/// match the sender's expected next nonce is rejected (not executed, not counted); balances are
/// irrelevant. `receipts_root`/`state_root`/`block_hash` are `keccak256` commitments over the outcome's
/// own content — opaque but deterministic and content-addressed, which is all that is needed here (the real
/// state root comes from reth).
#[derive(Debug, Default)]
pub struct MockExecutor {
    nonces: HashMap<Address, u64>,
    head: Head,
    /// Rolling digest of everything executed so far, folded into `block_hash`/`state_root` so a replay
    /// that executes a different tx sequence provably diverges (a real invariant check, not a decorative
    /// one — see `tests/crash_replay.rs`).
    digest: B256,
}

const GAS_PER_TX: u64 = 21_000;

/// Executor cap 100M gas/s × 50 ms sub-block cadence = 5,000,000 gas per sub-block. The
/// default `SubBlockLimits::gas_limit`; configurable per chain. Owned by the standalone
/// `rome-zk-profile` crate now ([`rome_zk_profile::DEFAULT_SUB_BLOCK_GAS_LIMIT`]); re-exported here under
/// this crate's historical name.
pub const DEFAULT_SUB_BLOCK_GAS_LIMIT: u64 = rome_zk_profile::DEFAULT_SUB_BLOCK_GAS_LIMIT;

impl MockExecutor {
    pub fn new() -> Self {
        Self::default()
    }

    fn recover_sender_and_nonce(tx: &Bytes) -> Result<(Address, u64, TxHash), ExecutorError> {
        let parsed =
            crate::tx::parse(tx.clone()).map_err(|e| ExecutorError::Malformed(e.to_string()))?;
        Ok((parsed.sender, parsed.nonce, parsed.tx_hash))
    }
}

impl Executor for MockExecutor {
    /// Folds `env`'s fields into the rolling digest immediately, so a
    /// test can prove the opened env actually reaches the sealed outcome (`self.digest` is what
    /// `seal_block` commits into `state_root`/`block_hash`) — a decorative no-op here would defeat the
    /// point of testing this seam at all.
    async fn open_block(&mut self, env: BlockEnv) -> Result<(), ExecutorError> {
        let mut preimage = Vec::new();
        preimage.extend_from_slice(b"open_block");
        preimage.extend_from_slice(&env.number.to_be_bytes());
        preimage.extend_from_slice(&env.timestamp_secs.to_be_bytes());
        preimage.extend_from_slice(&env.gas_limit.to_be_bytes());
        preimage.extend_from_slice(env.coinbase.as_slice());
        preimage.extend_from_slice(env.prev_randao.as_slice());
        if let Some(base_fee) = env.base_fee {
            preimage.extend_from_slice(&base_fee.to_be_bytes());
        }
        self.digest = alloy::primitives::keccak256([self.digest.as_slice(), &preimage].concat());
        Ok(())
    }

    async fn execute_sub_block(
        &mut self,
        txs: &[Bytes],
        limits: SubBlockLimits,
    ) -> Result<SubBlockOutcome, ExecutorError> {
        let mut included = Vec::new();
        let mut rejected = Vec::new();
        let mut gas_used = 0u64;
        let mut fold_input = Vec::new();
        let mut not_executed: Vec<Bytes> = Vec::new();

        for (i, tx) in txs.iter().enumerate() {
            // Checked before touching this tx at all: a tx the budget can't afford, or that the deadline
            // has already passed for, is never reached — not included, not rejected, just carried
            // forward untouched.
            if Instant::now() >= limits.deadline || gas_used + GAS_PER_TX > limits.gas_limit {
                not_executed = txs[i..].to_vec();
                break;
            }

            let (sender, nonce, tx_hash) = Self::recover_sender_and_nonce(tx)?;
            let expected = *self.nonces.get(&sender).unwrap_or(&0);
            if nonce != expected {
                let reason = if nonce < expected {
                    Reason::NonceTooLow { expected }
                } else {
                    Reason::NonceTooHigh { expected }
                };
                rejected.push(RejectedTx {
                    tx_hash,
                    sender,
                    reason,
                });
                continue;
            }
            self.nonces.insert(sender, expected + 1);
            gas_used += GAS_PER_TX;
            included.push(tx_hash);
            fold_input.extend_from_slice(tx_hash.as_slice());
        }

        self.digest = alloy::primitives::keccak256([self.digest.as_slice(), &fold_input].concat());
        let receipts_root = alloy::primitives::keccak256(
            [
                b"receipts".as_slice(),
                self.digest.as_slice(),
                &gas_used.to_be_bytes(),
            ]
            .concat(),
        );

        self.head.sub_block_index += 1;
        Ok(SubBlockOutcome {
            included,
            rejected,
            receipts_root,
            gas_used,
            not_executed,
        })
    }

    async fn seal_block(&mut self, inputs: BlockSealInputs) -> Result<BlockOutcome, ExecutorError> {
        let mut preimage = Vec::new();
        preimage.extend_from_slice(&inputs.block.to_be_bytes());
        preimage.extend_from_slice(&inputs.timestamp_secs.to_be_bytes());
        for h in &inputs.sub_block_header_hashes {
            preimage.extend_from_slice(h.as_slice());
        }
        preimage.extend_from_slice(&inputs.total_gas_used.to_be_bytes());
        preimage.extend_from_slice(self.digest.as_slice());

        let state_root = alloy::primitives::keccak256([b"state".as_slice(), &preimage].concat());
        let block_hash = alloy::primitives::keccak256([b"block".as_slice(), &preimage].concat());
        let receipts_root =
            alloy::primitives::keccak256([b"receipts_root".as_slice(), &preimage].concat());

        self.head = Head {
            block: inputs.block,
            sub_block_index: 0,
            state_root,
            block_hash,
        };
        Ok(BlockOutcome {
            state_root,
            block_hash,
            receipts_root,
        })
    }

    fn head(&self) -> Head {
        self.head
    }

    fn nonce(&self, addr: Address) -> u64 {
        *self.nonces.get(&addr).unwrap_or(&0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::signed_raw_tx;
    use alloy::signers::local::PrivateKeySigner;

    #[tokio::test]
    async fn included_txs_advance_nonce_and_count_flat_gas() {
        let mut ex = MockExecutor::new();
        let signer = PrivateKeySigner::random();
        let tx0 = signed_raw_tx(&signer, 1, 0);
        let tx1 = signed_raw_tx(&signer, 1, 1);
        let outcome = ex
            .execute_sub_block(&[tx0, tx1], SubBlockLimits::unbounded())
            .await
            .unwrap();
        assert_eq!(outcome.included.len(), 2);
        assert!(outcome.rejected.is_empty());
        assert!(outcome.not_executed.is_empty());
        assert_eq!(outcome.gas_used, 2 * GAS_PER_TX);
        assert_eq!(ex.nonce(signer.address()), 2);
    }

    #[tokio::test]
    async fn wrong_nonce_is_rejected_not_executed() {
        let mut ex = MockExecutor::new();
        let signer = PrivateKeySigner::random();
        let skip_ahead = signed_raw_tx(&signer, 1, 5); // expected next nonce is 0
        let outcome = ex
            .execute_sub_block(&[skip_ahead], SubBlockLimits::unbounded())
            .await
            .unwrap();
        assert!(outcome.included.is_empty());
        assert_eq!(outcome.rejected.len(), 1);
        assert_eq!(outcome.rejected[0].sender, signer.address());
        assert_eq!(
            outcome.rejected[0].reason,
            Reason::NonceTooHigh { expected: 0 },
            "a future nonce (tx nonce > expected) must be classified NonceTooHigh"
        );
        assert_eq!(outcome.gas_used, 0);
        assert_eq!(
            ex.nonce(signer.address()),
            0,
            "a rejected tx must not advance the nonce cache"
        );
    }

    /// A nonce BELOW the expected next value (already spent) must be
    /// classified `NonceTooLow`, distinct from `NonceTooHigh` — admission's reconcile path treats
    /// these differently (see `sequencer::tests`).
    #[tokio::test]
    async fn stale_nonce_is_rejected_as_nonce_too_low() {
        let mut ex = MockExecutor::new();
        let signer = PrivateKeySigner::random();
        let first = signed_raw_tx(&signer, 1, 0);
        ex.execute_sub_block(&[first], SubBlockLimits::unbounded())
            .await
            .unwrap();
        assert_eq!(ex.nonce(signer.address()), 1);

        let replay = signed_raw_tx(&signer, 1, 0); // already spent
        let outcome = ex
            .execute_sub_block(&[replay], SubBlockLimits::unbounded())
            .await
            .unwrap();
        assert_eq!(outcome.rejected.len(), 1);
        assert_eq!(
            outcome.rejected[0].reason,
            Reason::NonceTooLow { expected: 1 }
        );
    }

    #[tokio::test]
    async fn seal_block_is_deterministic_given_the_same_executed_txs() {
        let signer = PrivateKeySigner::random();
        let tx = signed_raw_tx(&signer, 1, 0);

        let mut a = MockExecutor::new();
        a.execute_sub_block(std::slice::from_ref(&tx), SubBlockLimits::unbounded())
            .await
            .unwrap();
        let out_a = a
            .seal_block(BlockSealInputs {
                block: 1,
                timestamp_secs: 1_757_000_000,
                sub_block_header_hashes: vec![B256::repeat_byte(1)],
                total_gas_used: GAS_PER_TX,
            })
            .await
            .unwrap();

        let mut b = MockExecutor::new();
        b.execute_sub_block(&[tx], SubBlockLimits::unbounded())
            .await
            .unwrap();
        let out_b = b
            .seal_block(BlockSealInputs {
                block: 1,
                timestamp_secs: 1_757_000_000,
                sub_block_header_hashes: vec![B256::repeat_byte(1)],
                total_gas_used: GAS_PER_TX,
            })
            .await
            .unwrap();

        assert_eq!(
            out_a, out_b,
            "same tx sequence + same seal inputs must yield the same roots"
        );
        assert_eq!(a.head(), b.head());
    }

    /// 300 txs at a flat 21 000 gas each against a 5,000,000 gas budget —
    /// `floor(5_000_000 / 21_000) = 238` reached and included, the remaining 62 never reached at all.
    #[tokio::test]
    async fn gas_limit_cuts_off_after_the_affordable_prefix() {
        let mut ex = MockExecutor::new();
        let signer = PrivateKeySigner::random();
        let txs: Vec<_> = (0..300u64).map(|n| signed_raw_tx(&signer, 1, n)).collect();

        let outcome = ex
            .execute_sub_block(
                &txs,
                SubBlockLimits {
                    gas_limit: 5_000_000,
                    deadline: Instant::now() + Duration::from_secs(3600),
                },
            )
            .await
            .unwrap();

        assert_eq!(outcome.included.len(), 238);
        assert!(outcome.rejected.is_empty());
        assert_eq!(outcome.not_executed.len(), 62);
        assert_eq!(outcome.gas_used, 238 * GAS_PER_TX);
        assert_eq!(
            outcome.not_executed,
            txs[238..],
            "not_executed must be exactly the unreached suffix, in order"
        );
        assert_eq!(
            ex.nonce(signer.address()),
            238,
            "an unreached tx must not advance the nonce cache"
        );
    }

    /// A deadline that has already passed before execution starts means
    /// every tx is carried — none reached, none included.
    #[tokio::test]
    async fn deadline_already_passed_carries_every_tx() {
        let mut ex = MockExecutor::new();
        let signer = PrivateKeySigner::random();
        let txs: Vec<_> = (0..5u64).map(|n| signed_raw_tx(&signer, 1, n)).collect();

        let already_passed = Instant::now();
        std::thread::sleep(Duration::from_millis(1));

        let outcome = ex
            .execute_sub_block(
                &txs,
                SubBlockLimits {
                    gas_limit: u64::MAX,
                    deadline: already_passed,
                },
            )
            .await
            .unwrap();

        assert!(outcome.included.is_empty());
        assert!(outcome.rejected.is_empty());
        assert_eq!(outcome.not_executed, txs);
        assert_eq!(outcome.gas_used, 0);
    }
}
