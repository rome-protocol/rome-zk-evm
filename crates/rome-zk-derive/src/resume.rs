//! Resume anchor: where a derivation node starts after a restart, instead of always walking from batch 0.
//!
//! **The problem `Self::FromGenesis` alone cannot solve.** `zk-inbox`'s `CloseBatch`/chunk `Close`
//! (`programs/zk-inbox/src/batch.rs`) reclaim a finalized batch's rent once its covering root is final
//! — the account is reallocated to 0 bytes and reassigned to the system program. A restart that always
//! starts [`crate::traversal::SolanaTraversal`] at batch 0 cannot tell "batch 0 was closed" apart from
//! "batch 0 was abandoned and never posted" (both: no account at that PDA) — worse, on a chain where
//! settlement has run for any length of time the walk-forward is O(total historical chunks/txs), never
//! catching up (≥ 900 RTTs + 20-40k `ecrecover`s per 10 s of history at the design point).
//!
//! **The fix: the settlement root account is already the answer.** `advance_root_final`
//! (`programs/zk-settlement/src/settle.rs`) writes `(head_final_batch, number, block_hash, state_root)`
//! every time a batch finalizes — `number`/`block_hash` name the exact block (a **real** EVM height,
//! same space [`crate::engine::EngineApi::block_at`] is queried in) this chain's settlement layer has
//! already confirmed. [`resume_anchor`] reads that account and asks the engine itself to confirm it
//! (the engine's own chain is the one durable truth — an anchor the engine cannot
//! independently confirm is discarded, not trusted). By construction no batch beyond `head_final_batch`
//! can have been closed (`CloseBatch`/chunk `Close` both require `check_final_root`, i.e.
//! `head_final_batch >= batch`), so resuming traversal at `head_final_batch + 1` never lands on the
//! closed-vs-abandoned ambiguity above — that batch id (if it even needed rent-reclaiming at all) is
//! strictly behind the anchor, never in front of it.
//!
//! Only "no root account at all" falls back to [`ResumeAnchor::FromGenesis`], the always-correct (if
//! potentially slower) path [`crate::engine::EngineController::from_engine_head`] already implements:
//! walk from the engine's own real genesis, consolidating every already-built block along the way.
//! `--from-batch <N>` (the binary's own flag) bypasses this module entirely for an explicit
//! re-derivation.
//!
//! **A decoded root the engine cannot confirm is refused, never
//! silently re-walked.** A root account that decodes but names a block this engine's own chain lacks at
//! that real height (a fresh derivation node, or one whose engine DB was lost, once `CloseBatch` has
//! reclaimed the batches that would let it re-derive from batch 0 — [`crate::traversal`]'s closed-id
//! ambiguity this whole module exists to route around) is [`PipelineError::Critical`]: falling back to
//! genesis here would silently skip every rent-recycled batch and hand the operator a misleading
//! "design premise violated" error from deep inside [`crate::engine::EngineController::advance`] instead
//! of the real, actionable one. Likewise a **hash mismatch** — the engine has a block at that height, but
//! it disagrees with what the root names — is equivocation (a stale/foreign root, or a genuine chain
//! fork) and gets its own named refusal, never a silent re-derivation from 0. The refused engine cannot
//! be trusted to consolidate a chain it may have equivocated against — restoring it from a snapshot at
//! or after the anchor's height, or running with `--from-batch` while DA is retained, are the
//! sanctioned recoveries.
//!
//! **The settlement genesis sentinel (`root.number == 0`) has no continuity
//! to offset.** `InitChain` (`programs/zk-settlement/src/chain.rs`) writes the operator's real genesis
//! `(number, block_hash)` into the root account and leaves `head_final_batch = 0` — "none/genesis", the
//! very first `PostRoot` treats as its predecessor. The old `last_design_block = root.number - 1` offset
//! (`saturating_sub`) collapsed this case to `Some(0)`, demanding a fresh chain's first-ever batch start
//! at design block 1 when its real first block is design 0 — no chain could ever derive its first batch.
//! [`ResumeAnchor::Confirmed::last_design_block`] is `None` here (nothing has been derived yet),
//! `Some(number)` otherwise — now that the sequencer numbers its first sealed block 1
//! (`BlockEnv::number` IS the real height), the last finalized block's real height and its design number
//! are the same value, so no offset is applied at all.

use alloy_primitives::B256;
use solana_program::pubkey::Pubkey;

use crate::engine::EngineApi;
use crate::reader::AccountReader;
use crate::PipelineError;

/// Where a pipeline should resume, as decided by [`resume_anchor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeAnchor {
    /// A settlement root the engine itself confirmed: `crate::traversal::SolanaTraversal` starts at
    /// `start_at_batch` (= `head_final_batch + 1`), `crate::pipeline::DerivePipeline`'s
    /// `last_design_block` starts at `Some(last_design_block)`
    /// (`BlockEnv::number` is the real height, so `last_design_block = number`, no offset), and the
    /// engine itself seeds from `(block_hash, number)` directly (`EngineController::new`), never from
    /// genesis.
    Confirmed {
        start_at_batch: u64,
        /// `None` at the settlement genesis sentinel (`number == 0` — nothing derived yet),
        /// `Some(number)` otherwise (no offset — design number == real height).
        last_design_block: Option<u64>,
        number: u64,
        block_hash: B256,
    },
    /// No root account exists yet (a genuinely fresh chain, or settlement not yet deployed in its
    /// current shape) — resume the always-correct way: genesis-first
    /// ([`crate::engine::EngineController::from_engine_head`]), batch 0. A decoded root the engine
    /// cannot confirm is a named [`PipelineError::Critical`] instead — see this module's
    /// doc.
    FromGenesis,
}

/// Reads the settlement root account (`zk_settlement_client::root_pda(settlement_program_id,
/// chain_id)`, via `reader` — callers use the same FINALIZED-commitment [`AccountReader`] every other
/// read in this crate uses) and verifies it against `engine`'s own chain before trusting it (module
/// doc). A decode failure (e.g. root.rs's own documented forward-compat case: an account still in the
/// earlier 88-byte shape) is treated as "no confirmable anchor", never a fault — the engine is the truth
/// and an unconfirmable hint is simply discarded. A **decoded** account naming
/// the wrong chain, by contrast, can only mean this crate's own PDA derivation drifted from
/// `zk-settlement-client`'s (a build-time bug, mirroring `crate::traversal`'s own equivalent check) —
/// that is [`PipelineError::Critical`]. Once a root account **does** decode for the right
/// chain, the engine's own confirmation is mandatory, not optional — a block the engine lacks at the
/// named height, or one whose hash disagrees, is refused by name (never a silent re-walk from genesis);
/// see this module's doc.
pub async fn resume_anchor<R: AccountReader, E: EngineApi>(
    reader: &mut R,
    settlement_program_id: &Pubkey,
    chain_id: u64,
    engine: &mut E,
) -> Result<ResumeAnchor, PipelineError> {
    let (root_pda, _) = zk_settlement_client::root_pda(settlement_program_id, chain_id);
    let Some(data) = reader.get_account_data(root_pda).await? else {
        return Ok(ResumeAnchor::FromGenesis);
    };
    let root = match zk_settlement_client::decode_root_account(&data) {
        Ok(root) => root,
        Err(_) => return Ok(ResumeAnchor::FromGenesis),
    };
    if root.chain_id != chain_id {
        return Err(PipelineError::Critical(format!(
            "settlement root account at the expected PDA for chain {chain_id} reports chain {} instead",
            root.chain_id
        )));
    }

    let expected_hash = B256::from(root.block_hash);
    // A decoded root this engine cannot confirm is refused, never silently re-walked — see
    // this module's doc for why (the closed-vs-abandoned ambiguity `resume_anchor` exists to route
    // around reappears the moment a fresh/rebuilt engine falls back to genesis here instead).
    let Some(existing) = engine.block_at(root.number).await? else {
        return Err(PipelineError::Critical(format!(
            "resume anchor: the settlement root for chain {chain_id} names real height {} (hash {expected_hash:#x}, head_final_batch {}) but this engine has no block there — restore the engine from a snapshot at/after height {0}, or run with --from-batch while DA is retained",
            root.number, root.head_final_batch
        )));
    };
    if existing.block_hash != expected_hash {
        return Err(PipelineError::Critical(format!(
            "resume anchor equivocation: the settlement root for chain {chain_id} names block {} as hash {expected_hash:#x}, but this engine's own block at that height has hash {:#x} — refusing to silently re-derive from genesis",
            root.number, existing.block_hash
        )));
    }

    Ok(ResumeAnchor::Confirmed {
        // Batch ids are 1-based per chain; 0 is the sentinel everywhere
        // (`head_pending_batch`/`head_final_batch` == 0 means "none", `InitChain`'s own initial value).
        // History: the first Tiber reset's inbox also started counting batches at 0, so this arm
        // used to special-case the settlement genesis sentinel (`number == 0`) to `start_at_batch: 0` —
        // `+ 1` there skipped that inbox's real first batch forever. Settlement's own continuity check
        // (`PostRootProved`/`PostRoot`, `settle.rs:246-256`) requires the first postable batch to be
        // `head_pending_batch + 1 = 1`, so the inbox's own first batch was moved to 1 to match —
        // tooling now bootstraps a fresh chain's cursor at 1 (the chain registration script / `init_cursor`), not 0.
        // The expression collapses to the same arithmetic in both cases: at the sentinel
        // `head_final_batch` is already 0, so `0 + 1 == 1` is the inbox's real first batch id.
        start_at_batch: root.head_final_batch + 1,
        // EVM header number == design number (`crate::engine`'s module
        // doc) — no offset to apply; at the settlement genesis sentinel (`number == 0`) nothing has
        // been derived yet, so there is no design number at all — `None`, not `Some(0)` (`saturating_sub`'s
        // old collapse).
        last_design_block: if root.number == 0 {
            None
        } else {
            Some(root.number)
        },
        number: root.number,
        block_hash: expected_hash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::mock::MockEngineApi;
    use crate::engine::ExistingBlock;
    use crate::testutil::FakeAccountReader;

    const CHAIN_ID: u64 = 200_101;

    fn root_account_bytes(
        chain_id: u64,
        number: u64,
        block_hash: [u8; 32],
        head_final_batch: u64,
    ) -> Vec<u8> {
        rome_zk_layouts::root::write(&rome_zk_layouts::root::RootFields {
            chain_id,
            number,
            parent_hash: [0u8; 32],
            state_root: [0u8; 32],
            block_hash,
            updates: 0,
            profile: 0,
            challenge_window_slots: 0,
            prove_window_slots: 0,
            proving_policy: 0,
            poster_bond: 0,
            exit_cap_per_window: 0,
            authority: [0u8; 32],
            head_pending_batch: 0,
            head_final_batch,
            pending_count: 0,
            max_pending: 0,
        })
        .to_vec()
    }

    fn existing(block_hash: B256) -> ExistingBlock {
        ExistingBlock {
            block_hash,
            timestamp: 0,
            prev_randao: B256::ZERO,
            gas_limit: 0,
            state_root: B256::ZERO,
            tx_hashes: vec![],
            // This module's tests only ever query `block_at` directly (confirming the settlement
            // root's own anchor block), never through `EngineController::advance`'s consolidation
            // check — so these are harmless placeholders, not asserted-against values.
            beneficiary: alloy_primitives::Address::ZERO,
            extra_data: alloy_primitives::Bytes::new(),
            withdrawals_root: rome_zk_executor_api::EMPTY_WITHDRAWALS,
            parent_beacon_block_root: B256::ZERO,
            blob_gas_used: 0,
            excess_blob_gas: 0,
        }
    }

    /// (c): no root account at all (a fresh chain, or settlement not yet deployed in its current shape) —
    /// ordinary "nothing to confirm", never an error.
    #[tokio::test]
    async fn no_root_account_falls_back_to_genesis() {
        let program_id = Pubkey::new_unique();
        let mut reader = FakeAccountReader::default();
        let mut engine = MockEngineApi::default();
        let anchor = resume_anchor(&mut reader, &program_id, CHAIN_ID, &mut engine)
            .await
            .unwrap();
        assert_eq!(anchor, ResumeAnchor::FromGenesis);
    }

    /// Closed batch 0 + fresh engine + root at height 5: a root naming a block
    /// this engine's own chain does not have at all — the fresh-node-after-rent-recycling case — is a
    /// named refusal, never a silent re-walk from genesis.
    #[tokio::test]
    async fn a_root_naming_a_block_the_fresh_engine_lacks_is_a_named_refusal() {
        let program_id = Pubkey::new_unique();
        let mut reader = FakeAccountReader::default();
        let (root_pda, _) = zk_settlement_client::root_pda(&program_id, CHAIN_ID);
        reader
            .accounts
            .insert(root_pda, root_account_bytes(CHAIN_ID, 5, [0xab; 32], 0));
        let mut engine = MockEngineApi::default(); // fresh engine: block_at(5) -> None
        let err = resume_anchor(&mut reader, &program_id, CHAIN_ID, &mut engine)
            .await
            .unwrap_err();
        match err {
            PipelineError::Critical(msg) => {
                assert!(
                    msg.contains('5'),
                    "expected the anchor's real height named in the refusal, got {msg:?}"
                );
                assert!(
                    msg.contains("snapshot") || msg.contains("from-batch"),
                    "expected the operator's recovery instruction in the refusal, got {msg:?}"
                );
            }
            other => panic!("expected Critical, got {other:?}"),
        }
    }

    /// The root's `block_hash` disagreeing with what the engine actually has at that real
    /// height is equivocation (or a stale/foreign root) — its own named refusal, never a silent re-walk
    /// from genesis.
    #[tokio::test]
    async fn a_hash_mismatch_is_a_named_equivocation_refusal() {
        let program_id = Pubkey::new_unique();
        let mut reader = FakeAccountReader::default();
        let (root_pda, _) = zk_settlement_client::root_pda(&program_id, CHAIN_ID);
        reader
            .accounts
            .insert(root_pda, root_account_bytes(CHAIN_ID, 5, [0xab; 32], 0));
        let mut engine = MockEngineApi::default();
        engine
            .existing_blocks
            .insert(5, existing(B256::repeat_byte(0xcd))); // != root's [0xab; 32]
        let err = resume_anchor(&mut reader, &program_id, CHAIN_ID, &mut engine)
            .await
            .unwrap_err();
        match err {
            PipelineError::Critical(msg) => assert!(
                msg.to_lowercase().contains("equivocation"),
                "expected the refusal to name itself as equivocation, got {msg:?}"
            ),
            other => panic!("expected Critical, got {other:?}"),
        }
    }

    /// (a): a root the engine confirms resumes right after the last final batch, with
    /// `last_design_block` equal to the root's real height (no offset).
    #[tokio::test]
    async fn a_confirmed_root_resumes_right_after_the_last_final_batch() {
        let program_id = Pubkey::new_unique();
        let mut reader = FakeAccountReader::default();
        let hash = B256::repeat_byte(0xab);
        let (root_pda, _) = zk_settlement_client::root_pda(&program_id, CHAIN_ID);
        reader
            .accounts
            .insert(root_pda, root_account_bytes(CHAIN_ID, 5, hash.0, 0));
        let mut engine = MockEngineApi::default();
        engine.existing_blocks.insert(5, existing(hash));
        let anchor = resume_anchor(&mut reader, &program_id, CHAIN_ID, &mut engine)
            .await
            .unwrap();
        assert_eq!(
            anchor,
            ResumeAnchor::Confirmed {
                start_at_batch: 1,
                last_design_block: Some(5),
                number: 5,
                block_hash: hash,
            }
        );
    }

    /// At the settlement genesis sentinel (`number == 0`, `head_final_batch == 0` — "none/genesis",
    /// `InitChain`'s own initial values) `last_design_block` must be `None` — nothing has been derived yet — never
    /// `Some(0)` (the old `saturating_sub` collapse that made a fresh chain's first batch demand design block 1).
    ///
    /// Batch ids are 1-based; 0 is the sentinel everywhere. At the sentinel
    /// `start_at_batch` must be 1, not 0 — settlement's own `PostRootProved` requires the first postable
    /// batch to be `head_pending_batch + 1 = 1` (`settle.rs:246-256`), so an inbox whose first real batch
    /// is 0 can never be finalized. `resume_anchor`'s sentinel arm used to return `start_at_batch: 0`;
    /// the test requires 1.
    #[tokio::test]
    async fn a_confirmed_root_at_the_genesis_sentinel_has_no_last_design_block() {
        let program_id = Pubkey::new_unique();
        let mut reader = FakeAccountReader::default();
        let genesis_hash = B256::repeat_byte(0x11);
        let (root_pda, _) = zk_settlement_client::root_pda(&program_id, CHAIN_ID);
        reader
            .accounts
            .insert(root_pda, root_account_bytes(CHAIN_ID, 0, genesis_hash.0, 0));
        let mut engine = MockEngineApi::default();
        engine.existing_blocks.insert(0, existing(genesis_hash));
        let anchor = resume_anchor(&mut reader, &program_id, CHAIN_ID, &mut engine)
            .await
            .unwrap();
        assert_eq!(
            anchor,
            ResumeAnchor::Confirmed {
                start_at_batch: 1,
                last_design_block: None,
                number: 0,
                block_hash: genesis_hash,
            }
        );
    }

    /// A decoded root reporting a different chain than the one this PDA was derived for can only mean
    /// this crate's own PDA derivation drifted from `zk-settlement-client`'s (mirrors
    /// `crate::traversal::SolanaTraversal`'s equivalent check) — fail loudly, never silently resume
    /// against the wrong chain's settlement state.
    #[tokio::test]
    async fn a_chain_id_mismatch_on_the_root_account_is_critical() {
        let program_id = Pubkey::new_unique();
        let mut reader = FakeAccountReader::default();
        let (root_pda, _) = zk_settlement_client::root_pda(&program_id, CHAIN_ID);
        reader
            .accounts
            .insert(root_pda, root_account_bytes(CHAIN_ID + 1, 5, [0xab; 32], 0));
        let mut engine = MockEngineApi::default();
        let err = resume_anchor(&mut reader, &program_id, CHAIN_ID, &mut engine)
            .await
            .unwrap_err();
        assert!(matches!(err, PipelineError::Critical(_)));
    }
}
