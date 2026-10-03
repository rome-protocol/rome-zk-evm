//! A not-finalized batch anywhere in the pending window
//! `[root.head_final_batch, cursor.next_batch)` (inclusive lower bound: `head_final_batch` itself is
//! re-checked, never assumed safe to skip) at startup is always
//! `AbandonBatch`'d and its chunk PDAs
//! closed — proved here against the real, `cargo build-sbf`-compiled `zk_inbox.so` (not a scripted fake):
//! `OpenBatch` + a partial seal (one of two chunks) leaves batch 0 open-not-finalized;
//! `pipeline::abandon_open_batches_in_pending_window` (the "new process" in the contract's own words) must
//! abandon it — the batch account gone, its one opened chunk PDA gone too — and leave the cursor exactly
//! where `OpenBatch` had already advanced it, so the next post lands under a fresh id (1), never batch 0
//! again.

use rome_zk_batcher::pipeline::{self, BatchTarget};
use rome_zk_batcher::resolve::{AccountOps, ResolveError};
use rome_zk_batcher::sender::{SendTuning, Sender, SenderError};
use rome_zk_testkit::{cursor_account, root_account_with_authority};
use solana_program::{instruction::Instruction, pubkey::Pubkey};
use solana_program_test::BanksClient;
use solana_system_interface::program as system_program;
// `compute_budget` moved out of `solana_sdk`'s root re-export in the Agave 4.x
// line (API fallout) — now its own crate.
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_sdk::{
    account::Account,
    signature::{Keypair, Signer},
    transaction::Transaction,
};
use std::time::Duration;

const CHAIN_ID: u64 = 200_198;

/// [`AccountOps`] bridged straight to a real `BanksClient` (see `once_dry_run.rs`'s own identical bridge
/// — duplicated here rather than shared, matching this workspace's existing per-test-file convention for
/// small BanksClient bridges, e.g. `finalize_cu_limit.rs`'s `BanksAccountRpc`).
#[derive(Clone)]
struct BanksAccountOps {
    banks_client: BanksClient,
}

impl AccountOps for BanksAccountOps {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Vec<u8>>, ResolveError> {
        let bc = self.banks_client.clone();
        let account = bc
            .get_account(*pubkey)
            .await
            .expect("BanksClient::get_account");
        Ok(account.map(|a| a.data))
    }

    async fn accounts_exist(&self, pubkeys: &[Pubkey]) -> Result<Vec<bool>, ResolveError> {
        let mut out = Vec::with_capacity(pubkeys.len());
        for pubkey in pubkeys {
            out.push(self.get_account(pubkey).await?.is_some());
        }
        Ok(out)
    }
}

/// [`Sender`] bridged straight to a real `BanksClient` (legacy transaction, matching
/// `finalize_cu_limit.rs`'s own `BanksSender`; this file's subject is the abandon/close instruction plan, not the wire format).
struct BanksSender {
    banks_client: BanksClient,
    payer: Keypair,
}

fn to_v1_signature(s: solana_sdk::signature::Signature) -> solana_signature::Signature {
    let bytes: [u8; 64] = s.into();
    solana_signature::Signature::from(bytes)
}

impl Sender for BanksSender {
    async fn send_and_confirm(
        &self,
        instructions: &[Instruction],
        tuning: SendTuning,
    ) -> Result<solana_signature::Signature, SenderError> {
        let mut ixs = vec![
            ComputeBudgetInstruction::set_compute_unit_limit(tuning.compute_unit_limit),
            ComputeBudgetInstruction::set_compute_unit_price(tuning.priority_fee_micro_lamports),
        ];
        ixs.extend_from_slice(instructions);
        let mut banks_client = self.banks_client.clone();
        let recent = banks_client
            .get_latest_blockhash()
            .await
            .expect("get_latest_blockhash");
        let tx = Transaction::new_signed_with_payer(
            &ixs,
            Some(&self.payer.pubkey()),
            &[&self.payer],
            recent,
        );
        let sig = tx.signatures[0];
        rome_zk_testkit::send_checked(&mut banks_client, tx)
            .await
            .unwrap_or_else(|e| panic!("transaction failed: {e}"));
        Ok(to_v1_signature(sig))
    }
}

fn tuning() -> SendTuning {
    SendTuning {
        compute_unit_limit: 200_000,
        loaded_accounts_data_size_limit: 131_072,
        priority_fee_micro_lamports: 1_000,
        max_priority_fee_micro_lamports: 200_000,
        confirm_timeout: Duration::from_secs(30),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_half_written_batch_is_abandoned_and_its_chunks_closed_before_the_next_post() {
    let program_id = Pubkey::new_unique();
    let settlement_program = Pubkey::new_unique();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let authority = Keypair::new();
    pt.add_account(
        authority.pubkey(),
        Account {
            lamports: 50_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    let (root, _) = zk_inbox_client::root_pda(&settlement_program, CHAIN_ID);
    pt.add_account(
        root,
        root_account_with_authority(CHAIN_ID, &authority.pubkey(), settlement_program),
    );
    pt.add_account(
        zk_inbox_client::cursor_pda(&program_id, &settlement_program, CHAIN_ID).0,
        cursor_account(program_id, CHAIN_ID, 0),
    );
    let ctx = pt.start_with_context().await;
    let banks_client = ctx.banks_client.clone();

    // --- OpenBatch(0) + Grow to expected_count=2, one transaction (advances cursor.next_batch to 1) ---
    let sender = BanksSender {
        banks_client: banks_client.clone(),
        payer: authority.insecure_clone(),
    };
    let open_and_grow_ixs = zk_inbox_client::open_and_grow_batch_ixs(
        &program_id,
        &authority.pubkey(),
        CHAIN_ID,
        0,
        2,
        &settlement_program,
    );
    sender
        .send_and_confirm(&open_and_grow_ixs, tuning())
        .await
        .unwrap();

    // --- Seal only chunk 0 of 2 (a partial seal — the crash-mid-post shape) ---
    let payload = b"one sealed chunk, one never opened".to_vec();
    let chunk_plan = pipeline::plan_chunk(
        &program_id,
        &authority.pubkey(),
        &settlement_program,
        CHAIN_ID,
        0,
        0,
        &payload,
    );
    sender
        .send_and_confirm(&chunk_plan, tuning())
        .await
        .unwrap();

    let (batch_pda, _) = zk_inbox_client::batch_pda(&program_id, &settlement_program, CHAIN_ID, 0);
    let (chunk0_pda, _) =
        zk_inbox_client::chunk_pda(&program_id, &settlement_program, CHAIN_ID, 0, 0);
    let (chunk1_pda, _) =
        zk_inbox_client::chunk_pda(&program_id, &settlement_program, CHAIN_ID, 0, 1);
    let before = banks_client
        .clone()
        .get_account(batch_pda)
        .await
        .unwrap()
        .unwrap();
    let decoded_before = zk_inbox_client::decode_batch_account(&before.data).unwrap();
    assert!(
        !decoded_before.finalized,
        "batch 0 must not be finalized yet"
    );
    assert_eq!(decoded_before.leaves_present, 1, "only chunk 0 was sealed");
    assert!(
        banks_client
            .clone()
            .get_account(chunk0_pda)
            .await
            .unwrap()
            .is_some(),
        "chunk 0 must exist (it was opened)"
    );
    assert!(
        banks_client
            .clone()
            .get_account(chunk1_pda)
            .await
            .unwrap()
            .is_none(),
        "chunk 1 must not exist (it was never opened)"
    );

    // ===== The "new process": abandon the half-written batch before posting anything =====
    let accounts = BanksAccountOps {
        banks_client: banks_client.clone(),
    };
    let abandoned = pipeline::abandon_open_batches_in_pending_window(
        &accounts,
        &sender,
        &program_id,
        &settlement_program,
        CHAIN_ID,
        authority.pubkey(),
        tuning(),
    )
    .await
    .expect("abandon_open_batches_in_pending_window must succeed");
    assert_eq!(abandoned, vec![0], "batch 0 must be the one abandoned");

    // Batch 0's account is gone, and its one opened chunk is gone too (closed, not merely orphaned).
    assert!(
        banks_client
            .clone()
            .get_account(batch_pda)
            .await
            .unwrap()
            .is_none(),
        "batch 0's account must be gone after AbandonBatch"
    );
    assert!(
        banks_client
            .clone()
            .get_account(chunk0_pda)
            .await
            .unwrap()
            .is_none(),
        "chunk 0 must be closed, not left stranded"
    );

    // A second call is a no-op (nothing left open in the pending window to abandon) — never double-abandons.
    let second = pipeline::abandon_open_batches_in_pending_window(
        &accounts,
        &sender,
        &program_id,
        &settlement_program,
        CHAIN_ID,
        authority.pubkey(),
        tuning(),
    )
    .await
    .expect("a second call must be a clean no-op");
    assert_eq!(second, Vec::<u64>::new());

    // ===== batch 1 (a fresh id, never batch 0 again) posts and finalizes normally =====
    let cursor_data = banks_client
        .clone()
        .get_account(zk_inbox_client::cursor_pda(&program_id, &settlement_program, CHAIN_ID).0)
        .await
        .unwrap()
        .unwrap();
    let cursor = zk_inbox_client::decode_batch_cursor(&cursor_data.data).unwrap();
    assert_eq!(
        cursor.next_batch, 1,
        "AbandonBatch must not touch the cursor — the next post lands at 1, never reusing 0"
    );

    let target = BatchTarget {
        program_id,
        settlement_program,
        payer: authority.pubkey(),
        chain_id: CHAIN_ID,
        batch: 1,
    };
    let compressed = rome_zk_batcher::channel::encode_stream(&[rome_zk_batcher::channel::Block {
        number: 0,
        timestamp: 1_757_000_000,
        gas_limit: 100_000_000,
        txs: vec![],
    }]);
    let frames = rome_zk_batcher::channel::cut_frames(CHAIN_ID, 1, &compressed, 3_200);
    let open_and_grow_ixs = zk_inbox_client::open_and_grow_batch_ixs(
        &program_id,
        &authority.pubkey(),
        CHAIN_ID,
        1,
        frames.len() as u32,
        &settlement_program,
    );
    sender
        .send_and_confirm(&open_and_grow_ixs, tuning())
        .await
        .unwrap();
    let frame_jobs = pipeline::build_frame_jobs(target, &frames);
    for stages in &frame_jobs {
        for stage in stages {
            for ixs in stage {
                sender.send_and_confirm(ixs, tuning()).await.unwrap();
            }
        }
    }
    let finalize_ix = zk_inbox_client::finalize_batch_ix(
        &program_id,
        &authority.pubkey(),
        &settlement_program,
        CHAIN_ID,
        1,
        0,
    );
    sender
        .send_and_confirm(std::slice::from_ref(&finalize_ix), tuning())
        .await
        .unwrap();

    let (batch1_pda, _) = zk_inbox_client::batch_pda(&program_id, &settlement_program, CHAIN_ID, 1);
    let account1 = banks_client
        .clone()
        .get_account(batch1_pda)
        .await
        .unwrap()
        .unwrap();
    let decoded1 = zk_inbox_client::decode_batch_account(&account1.data).unwrap();
    assert!(decoded1.finalized, "batch 1 must be finalized");
    pipeline::verify_acc(&decoded1, &frames)
        .unwrap_or_else(|e| panic!("batch 1's on-chain acc must match: {e}"));

    // The chain-anchor walk must now decode batch 1's own last block, skipping the
    // abandoned batch 0 entirely — the exact "derive's traversal skips N" property, proved at the
    // batcher's own resume-anchor layer.
    let anchor = rome_zk_batcher::anchor::resolve_anchor(
        &accounts,
        &program_id,
        &settlement_program,
        CHAIN_ID,
        std::path::Path::new("/nonexistent"), // never reached: batch 1 is Finalized, no closed-batch fallback
        20,
        100_000_000,
    )
    .await
    .expect(
        "resolve_anchor must decode batch 1's own chunks, never touching the abandoned batch 0",
    );
    assert_eq!(anchor.from_block, 1);
}
