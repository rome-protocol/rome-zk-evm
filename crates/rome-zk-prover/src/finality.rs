//! The finalize sweep and the retention-gated close: pure planning functions,
//! never touching the network themselves — the CLI sends whatever [`plan_finalize`]/[`should_close`]
//! decide, through the same `Sender` the poster uses.

/// One `FinalizeBatch` call's plan: the in-order batch (`head_final_batch + 1`) plus every already-Final
/// successor to walk past in the same call, capped by `finalize_walk` (`MAX_FINALITY_WALK` on chain,
/// `settle.rs`) and by `head_pending_batch` (never walk past the chain's own pending head).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizePlan {
    pub batch: u64,
    pub walk: Vec<u64>,
}

/// `None` when already caught up (`head_final_batch >= head_pending_batch`) — the loop's own idle case,
/// no send attempted. Otherwise the next in-order batch plus every successor up to
/// `min(head_pending_batch, batch + finalize_walk)`.
pub fn plan_finalize(
    head_pending_batch: u64,
    head_final_batch: u64,
    finalize_walk: u64,
) -> Option<FinalizePlan> {
    if head_final_batch >= head_pending_batch {
        return None;
    }
    let batch = head_final_batch + 1;
    let last = head_pending_batch.min(batch.saturating_add(finalize_walk));
    let walk = ((batch + 1)..=last).collect();
    Some(FinalizePlan { batch, walk })
}

/// Retention gate: `close_pending_after_batches == None` (Tiber's
/// default) never closes anything — every pending PDA stays readable by `RootView` forever. `Some(k)`
/// proposes closing a batch once it is BOTH strictly behind `head_pending_batch - k` (the program itself
/// refuses to close the head batch regardless, `IsHeadPendingBatch`, so this gate does not need to
/// special-case it, but it never proposes closing the head either — `checked_sub` protects the case
/// `k >= head_pending_batch`, where nothing is old enough to close yet) AND its own pending status —
/// read from the SAME snapshot the caller already has, never assumed — is `STATUS_FINAL`
/// (`rome_zk_layouts::pending::STATUS_FINAL`). A batch a challenge-window post (the
/// `proving_policy`, or any path other than `PostRootProved`) left `STATUS_PENDING` is never proposed
/// for close: the program would refuse it (`NotFinal`, `settle.rs`), but this gate stops the CLI from
/// ever building that doomed send in the first place.
pub fn should_close(
    head_pending_batch: u64,
    batch: u64,
    status: u8,
    close_pending_after_batches: Option<u64>,
) -> bool {
    if status != rome_zk_layouts::pending::STATUS_FINAL {
        return false;
    }
    match close_pending_after_batches {
        None => false,
        Some(k) => match head_pending_batch.checked_sub(k) {
            Some(threshold) => batch < threshold,
            None => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== Pure planning tests =====

    #[test]
    fn no_plan_when_already_caught_up() {
        assert_eq!(plan_finalize(5, 5, 8), None);
        assert_eq!(plan_finalize(5, 6, 8), None); // head_final ahead is also "nothing to do" here
    }

    #[test]
    fn walks_every_successor_up_to_head_pending_when_within_the_walk_cap() {
        assert_eq!(
            plan_finalize(3, 0, 8),
            Some(FinalizePlan {
                batch: 1,
                walk: vec![2, 3]
            })
        );
    }

    #[test]
    fn the_walk_is_capped_by_finalize_walk_even_if_head_pending_is_further_ahead() {
        assert_eq!(
            plan_finalize(20, 0, 2),
            Some(FinalizePlan {
                batch: 1,
                walk: vec![2, 3]
            })
        );
    }

    #[test]
    fn a_single_batch_behind_produces_an_empty_walk() {
        assert_eq!(
            plan_finalize(1, 0, 8),
            Some(FinalizePlan {
                batch: 1,
                walk: vec![]
            })
        );
    }

    #[test]
    fn retention_none_never_closes_anything() {
        assert!(!should_close(
            100,
            1,
            rome_zk_layouts::pending::STATUS_FINAL,
            None
        ));
    }

    #[test]
    fn retention_some_k_closes_only_strictly_behind_head_minus_k() {
        let final_status = rome_zk_layouts::pending::STATUS_FINAL;
        assert!(should_close(3, 1, final_status, Some(1))); // 1 < 3-1=2
        assert!(!should_close(3, 2, final_status, Some(1))); // 2 < 2 is false
        assert!(!should_close(3, 3, final_status, Some(1))); // never proposes closing the head
    }

    #[test]
    fn retention_some_k_at_or_above_head_pending_closes_nothing_yet() {
        assert!(!should_close(
            3,
            0,
            rome_zk_layouts::pending::STATUS_FINAL,
            Some(5)
        ));
    }

    /// A batch that is otherwise old enough to close but whose own status is `STATUS_PENDING`
    /// (a challenge-window post, not this CLI's proved path) must never be proposed for close — the
    /// program would refuse it (`NotFinal`), but this gate stops the send before it is ever built.
    #[test]
    fn a_pending_status_batch_is_never_proposed_for_close_even_when_old_enough() {
        assert!(!should_close(
            3,
            1,
            rome_zk_layouts::pending::STATUS_PENDING,
            Some(1)
        ));
    }

    // ===== Real-BPF: the sweep + retention gate against genuine zk_settlement.so =====

    use rome_zk_testkit::{
        funded_keypair, program_test, rent_exempt, send_measuring_cu, ProgramSpec,
    };
    use solana_program::pubkey::Pubkey;
    use solana_sdk::{
        instruction::InstructionError, signature::Signer, transaction::TransactionError,
    };

    const CHAIN_ID: u64 = 200_202;

    fn encode_pending_with_status(
        batch: u64,
        last_block: u64,
        state_root: [u8; 32],
        status: u8,
    ) -> Vec<u8> {
        use rome_zk_layouts::pending::*;
        let mut d = vec![0u8; PENDING_LEN];
        d[OFF_BATCH..OFF_BATCH + 8].copy_from_slice(&batch.to_le_bytes());
        d[OFF_PREV_BATCH..OFF_PREV_BATCH + 8].copy_from_slice(&(batch - 1).to_le_bytes());
        d[OFF_LAST_BLOCK..OFF_LAST_BLOCK + 8].copy_from_slice(&last_block.to_le_bytes());
        d[OFF_STATE_ROOT..OFF_STATE_ROOT + 32].copy_from_slice(&state_root);
        d[OFF_STATUS] = status;
        d[OFF_PARENT_HASH..OFF_PARENT_HASH + 32].copy_from_slice(&[batch as u8; 32]);
        d[OFF_LAST_BLOCK_HASH..OFF_LAST_BLOCK_HASH + 32]
            .copy_from_slice(&[(batch + 100) as u8; 32]);
        d
    }

    fn encode_pending_final(batch: u64, last_block: u64, state_root: [u8; 32]) -> Vec<u8> {
        encode_pending_with_status(
            batch,
            last_block,
            state_root,
            rome_zk_layouts::pending::STATUS_FINAL,
        )
    }

    fn encode_root(head_pending: u64, head_final: u64, authority: Pubkey) -> Vec<u8> {
        use rome_zk_layouts::root::*;
        let mut d = vec![0u8; MIN_LEN];
        d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
        d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].copy_from_slice(&CHAIN_ID.to_le_bytes());
        d[OFF_AUTHORITY..OFF_AUTHORITY + 32].copy_from_slice(authority.as_ref());
        d[OFF_HEAD_PENDING_BATCH..OFF_HEAD_PENDING_BATCH + 8]
            .copy_from_slice(&head_pending.to_le_bytes());
        d[OFF_HEAD_FINAL_BATCH..OFF_HEAD_FINAL_BATCH + 8]
            .copy_from_slice(&head_final.to_le_bytes());
        d[OFF_MAX_PENDING..OFF_MAX_PENDING + 4].copy_from_slice(&16u32.to_le_bytes());
        d
    }

    struct FinalityRig {
        ctx: solana_program_test::ProgramTestContext,
        settlement_program: Pubkey,
        authority: solana_sdk::signature::Keypair,
    }
    impl FinalityRig {
        /// Seeds root (`head_pending_batch = 3`, `head_final_batch = 0`) and pending(1..=3), all already
        /// `Final` — the shape a run of out-of-order proved posts (or, simplest to construct directly, a
        /// hand-seeded fixture) leaves behind: `finalize_batch` on batch 1 is then a pure head-advance
        /// walk, exactly the scenario the finalize sweep exists for ("needed when a post
        /// landed with advances_head == false").
        async fn start() -> Self {
            let settlement_program = Pubkey::new_unique();
            let authority = funded_keypair();
            let mut pt = program_test(
                &[ProgramSpec::new("zk_settlement", settlement_program)],
                true,
            );
            pt.add_account(
                authority.pubkey(),
                solana_sdk::account::Account {
                    lamports: 50_000_000_000,
                    data: vec![],
                    owner: solana_system_interface::program::id(),
                    executable: false,
                    rent_epoch: 0,
                },
            );
            let (root_pda, _) = zk_settlement_client::root_pda(&settlement_program, CHAIN_ID);
            pt.add_account(
                root_pda,
                solana_sdk::account::Account {
                    lamports: rent_exempt(rome_zk_layouts::root::MIN_LEN),
                    data: encode_root(3, 0, authority.pubkey()),
                    owner: settlement_program,
                    executable: false,
                    rent_epoch: 0,
                },
            );
            for batch in 1..=3u64 {
                let (pending_pda, _) =
                    zk_settlement_client::pending_pda(&settlement_program, CHAIN_ID, batch);
                let data = encode_pending_final(batch, batch * 10, [batch as u8; 32]);
                pt.add_account(
                    pending_pda,
                    solana_sdk::account::Account {
                        lamports: rent_exempt(data.len()),
                        data,
                        owner: settlement_program,
                        executable: false,
                        rent_epoch: 0,
                    },
                );
            }
            let ctx = pt.start_with_context().await;
            FinalityRig {
                ctx,
                settlement_program,
                authority,
            }
        }

        async fn root(&mut self) -> zk_settlement_client::RootAccount {
            let (root_pda, _) = zk_settlement_client::root_pda(&self.settlement_program, CHAIN_ID);
            let data = self
                .ctx
                .banks_client
                .get_account(root_pda)
                .await
                .unwrap()
                .unwrap();
            zk_settlement_client::decode_root_account(&data.data).unwrap()
        }
    }

    fn custom_code(err: &TransactionError) -> Option<u32> {
        match err {
            TransactionError::InstructionError(_, InstructionError::Custom(c)) => Some(*c),
            _ => None,
        }
    }

    #[tokio::test]
    async fn the_sweep_advances_head_final_past_every_already_final_successor_in_one_tx() {
        let mut rig = FinalityRig::start().await;
        let root = rig.root().await;
        assert_eq!(root.head_pending_batch, 3);
        assert_eq!(root.head_final_batch, 0);

        let plan = plan_finalize(root.head_pending_batch, root.head_final_batch, 8)
            .expect("there is a sweep to do");
        assert_eq!(
            plan,
            FinalizePlan {
                batch: 1,
                walk: vec![2, 3]
            }
        );

        let ix = zk_settlement_client::finalize_batch_ix(
            &rig.settlement_program,
            CHAIN_ID,
            plan.batch,
            &plan.walk,
        );
        let payer = rig.authority.insecure_clone();
        let (result, cu, _logs) = send_measuring_cu(&mut rig.ctx, &[ix], &payer, &[]).await;
        result.expect("finalize sweep must succeed");
        println!("finalize sweep CU: {cu}");

        let root = rig.root().await;
        assert_eq!(
            root.head_final_batch, 3,
            "must advance past every Final successor in one tx"
        );
        assert_eq!(root.number, 30);
    }

    /// Real BPF: a finalize walk that reaches a batch
    /// still `STATUS_PENDING` (not `Final` — a challenge-window post, or one not yet through the dispute
    /// window) stops there rather than refusing the whole instruction — `settle.rs`'s own trailing-walk
    /// is a best-effort stop, pinned here rather than assumed. `head_final_batch` must advance only to
    /// the last genuinely `Final` successor (batch 2), never past the `Pending` one (batch 3).
    #[tokio::test]
    async fn a_walk_that_meets_a_pending_pda_stops_there_and_still_succeeds() {
        let settlement_program = Pubkey::new_unique();
        let authority = funded_keypair();
        let mut pt = program_test(
            &[ProgramSpec::new("zk_settlement", settlement_program)],
            true,
        );
        pt.add_account(
            authority.pubkey(),
            solana_sdk::account::Account {
                lamports: 50_000_000_000,
                data: vec![],
                owner: solana_system_interface::program::id(),
                executable: false,
                rent_epoch: 0,
            },
        );
        let (root_pda, _) = zk_settlement_client::root_pda(&settlement_program, CHAIN_ID);
        pt.add_account(
            root_pda,
            solana_sdk::account::Account {
                lamports: rent_exempt(rome_zk_layouts::root::MIN_LEN),
                data: encode_root(3, 0, authority.pubkey()),
                owner: settlement_program,
                executable: false,
                rent_epoch: 0,
            },
        );
        // Batches 1 and 2 are genuinely Final; batch 3 (the head) is still Pending — a shape that never
        // arises on the proved path alone (`PostRootProved` always born Final) but does when a
        // challenge-window post (or any non-`PostRootProved` path) shares the same chain.
        for (batch, status) in [
            (1u64, rome_zk_layouts::pending::STATUS_FINAL),
            (2, rome_zk_layouts::pending::STATUS_FINAL),
            (3, rome_zk_layouts::pending::STATUS_PENDING),
        ] {
            let (pending_pda, _) =
                zk_settlement_client::pending_pda(&settlement_program, CHAIN_ID, batch);
            let data = encode_pending_with_status(batch, batch * 10, [batch as u8; 32], status);
            pt.add_account(
                pending_pda,
                solana_sdk::account::Account {
                    lamports: rent_exempt(data.len()),
                    data,
                    owner: settlement_program,
                    executable: false,
                    rent_epoch: 0,
                },
            );
        }
        let mut ctx = pt.start_with_context().await;

        let plan = plan_finalize(3, 0, 8).expect("there is a sweep to do");
        assert_eq!(
            plan,
            FinalizePlan {
                batch: 1,
                walk: vec![2, 3]
            }
        );
        let ix = zk_settlement_client::finalize_batch_ix(
            &settlement_program,
            CHAIN_ID,
            plan.batch,
            &plan.walk,
        );
        let payer = authority.insecure_clone();
        let (result, cu, _logs) = send_measuring_cu(&mut ctx, &[ix], &payer, &[]).await;
        result.expect("a walk meeting a Pending successor must still succeed (best-effort stop)");
        println!("finalize walk (stops at a Pending successor) CU: {cu}");

        let root_data = ctx
            .banks_client
            .get_account(root_pda)
            .await
            .unwrap()
            .unwrap();
        let root = zk_settlement_client::decode_root_account(&root_data.data).unwrap();
        assert_eq!(
            root.head_final_batch, 2,
            "the walk must stop at the last genuinely Final successor, never adopting the Pending one"
        );
    }

    #[tokio::test]
    async fn root_view_of_batch_1_still_succeeds_after_a_sweep_with_no_retention() {
        let mut rig = FinalityRig::start().await;
        let root = rig.root().await;
        let plan = plan_finalize(root.head_pending_batch, root.head_final_batch, 8).unwrap();
        let ix = zk_settlement_client::finalize_batch_ix(
            &rig.settlement_program,
            CHAIN_ID,
            plan.batch,
            &plan.walk,
        );
        let payer = rig.authority.insecure_clone();
        send_measuring_cu(&mut rig.ctx, &[ix], &payer, &[])
            .await
            .0
            .expect("sweep succeeds");

        // close_pending_after_batches == None (Tiber's default) — should_close never fires, batch 1's
        // pending PDA is never closed, so RootView(1) still reads it.
        assert!(!should_close(
            3,
            1,
            rome_zk_layouts::pending::STATUS_FINAL,
            None
        ));
        let view_ix = zk_settlement_client::root_view_ix(&rig.settlement_program, CHAIN_ID, 1);
        let (result, _cu, _logs) = send_measuring_cu(&mut rig.ctx, &[view_ix], &payer, &[]).await;
        result.expect("RootView(1) must still succeed with no retention gate");
    }

    /// With `close_pending_after_batches = Some(1)`, only batches strictly behind `head_pending - 1 = 2`
    /// are closed — batch 1 qualifies, batch 2 (== the threshold) does not, batch 3 is the head (the
    /// program refuses that regardless). After closing batch 1, `RootView(1)` is refused (the PDA no
    /// longer exists / is not program-owned); `RootView(2)` still succeeds.
    ///
    /// **Mutation: sweep ignores the knob → this test goes red.** If `should_close` is
    /// mutated to always return `false`, batch 1 is never closed and `RootView(1)` unexpectedly
    /// SUCCEEDS instead of being refused — this test's own final assertion catches that.
    #[tokio::test]
    async fn retention_some_1_closes_only_batch_1_and_root_view_of_it_is_then_refused() {
        let mut rig = FinalityRig::start().await;
        let root = rig.root().await;
        let plan = plan_finalize(root.head_pending_batch, root.head_final_batch, 8).unwrap();
        let ix = zk_settlement_client::finalize_batch_ix(
            &rig.settlement_program,
            CHAIN_ID,
            plan.batch,
            &plan.walk,
        );
        let payer = rig.authority.insecure_clone();
        send_measuring_cu(&mut rig.ctx, &[ix], &payer, &[])
            .await
            .0
            .expect("sweep succeeds");

        let close_pending_after_batches = Some(1u64);
        let head_pending = 3u64;
        let final_status = rome_zk_layouts::pending::STATUS_FINAL;
        for batch in 1..=3u64 {
            if should_close(
                head_pending,
                batch,
                final_status,
                close_pending_after_batches,
            ) {
                let close_ix = zk_settlement_client::close_pending_ix(
                    &rig.settlement_program,
                    &rig.authority.pubkey(),
                    CHAIN_ID,
                    batch,
                );
                let payer2 = rig.authority.insecure_clone();
                send_measuring_cu(&mut rig.ctx, &[close_ix], &payer2, &[])
                    .await
                    .0
                    .unwrap_or_else(|e| {
                        panic!("ClosePending(batch {batch}) should succeed: {e:?}")
                    });
            }
        }
        assert!(should_close(
            head_pending,
            1,
            final_status,
            close_pending_after_batches
        ));
        assert!(!should_close(
            head_pending,
            2,
            final_status,
            close_pending_after_batches
        ));
        assert!(!should_close(
            head_pending,
            3,
            final_status,
            close_pending_after_batches
        ));

        let view1 = zk_settlement_client::root_view_ix(&rig.settlement_program, CHAIN_ID, 1);
        let payer3 = rig.authority.insecure_clone();
        let (result1, _cu, _logs) = send_measuring_cu(&mut rig.ctx, &[view1], &payer3, &[]).await;
        let err = result1.expect_err("RootView(1) must be refused once its PDA is closed");
        assert!(
            custom_code(&err).is_none(),
            "a closed PDA fails a native account check (IncorrectProgramId), not a SettleError custom code"
        );

        let view2 = zk_settlement_client::root_view_ix(&rig.settlement_program, CHAIN_ID, 2);
        let payer4 = rig.authority.insecure_clone();
        let (result2, _cu, _logs) = send_measuring_cu(&mut rig.ctx, &[view2], &payer4, &[]).await;
        result2.expect("RootView(2) must still succeed — only batch 1 was closed");
    }
}
