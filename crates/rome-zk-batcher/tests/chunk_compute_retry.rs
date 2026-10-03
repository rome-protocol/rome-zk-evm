//! A chunk-lane frame that runs out of compute units is resent once at a higher limit and lands, instead of
//! failing the batch (`Sender::send_and_confirm_many_retrying_compute`, used by the batcher's chunk lane).
//!
//! The forced path: the base limit is set below what any frame costs (a chunk transaction costs about 16,400 CU
//! before any extra bump attempt on the real compiled `zk_inbox.so`, see `bump_search_cu_limit.rs`), so every first attempt fails on
//! the real runtime with a compute-overrun error and only the retry can land it. The runtime reports an overrun
//! two ways (`ComputationalBudgetExceeded` from a syscall that cannot charge its cost, `ProgramFailedToComplete`
//! from the program's own instruction meter), and both are seen here: the retry keys on either.

use rome_zk_batcher::channel::Frame;
use rome_zk_batcher::pipeline::{self, BatchTarget};
use rome_zk_batcher::sender::{
    is_compute_exceeded, sender_error_transaction_error, SendTuning, Sender, SenderError,
};
use rome_zk_testkit::{cursor_account, root_account_with_authority};
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_program_test::{BanksClient, ProgramTestContext};
use solana_sdk::{
    account::Account,
    instruction::Instruction,
    signature::{Keypair, Signer},
    transaction::Transaction,
};
use solana_system_interface::program as system_program;
use std::time::Duration;

/// `solana_sdk`'s `TransactionError` -> the `solana_transaction_error` one `SenderError::StepFailed` carries.
/// Only the two compute-overrun shapes this file expects (see `is_compute_exceeded`); anything else panics loudly.
fn to_v1_tx_error(
    e: solana_sdk::transaction::TransactionError,
) -> solana_transaction_error::TransactionError {
    use solana_instruction_error::InstructionError as V1;
    use solana_sdk::instruction::InstructionError as Legacy;
    match e {
        solana_sdk::transaction::TransactionError::InstructionError(
            idx,
            Legacy::ComputationalBudgetExceeded,
        ) => solana_transaction_error::TransactionError::InstructionError(
            idx,
            V1::ComputationalBudgetExceeded,
        ),
        solana_sdk::transaction::TransactionError::InstructionError(
            idx,
            Legacy::ProgramFailedToComplete,
        ) => solana_transaction_error::TransactionError::InstructionError(
            idx,
            V1::ProgramFailedToComplete,
        ),
        other => panic!("unexpected transaction error: {other:?}"),
    }
}

fn to_v1_signature(s: solana_sdk::signature::Signature) -> solana_signature::Signature {
    let bytes: [u8; 64] = s.into();
    solana_signature::Signature::from(bytes)
}

/// Bridges [`Sender`] to a real `BanksClient`; `tuning.compute_unit_limit` becomes a real compute-unit-limit
/// instruction enforced by the real runtime. Records every limit it was asked to send with.
struct BanksSender {
    banks_client: BanksClient,
    payer: Keypair,
    limits: std::sync::Mutex<Vec<u32>>,
}

impl Sender for BanksSender {
    async fn send_and_confirm(
        &self,
        instructions: &[Instruction],
        tuning: SendTuning,
    ) -> Result<solana_signature::Signature, SenderError> {
        self.limits.lock().unwrap().push(tuning.compute_unit_limit);
        let mut ixs = vec![
            ComputeBudgetInstruction::set_compute_unit_limit(tuning.compute_unit_limit),
            ComputeBudgetInstruction::set_compute_unit_price(tuning.priority_fee_micro_lamports),
        ];
        ixs.extend_from_slice(instructions);
        let mut banks_client = self.banks_client.clone();
        let recent = banks_client.get_latest_blockhash().await.unwrap();
        let tx = Transaction::new_signed_with_payer(
            &ixs,
            Some(&self.payer.pubkey()),
            &[&self.payer],
            recent,
        );
        let sig = tx.signatures[0];
        rome_zk_testkit::send_checked(&mut banks_client, tx)
            .await
            .map_err(|err| SenderError::StepFailed {
                frame_index: 0,
                stage_index: 0,
                tx_index: 0,
                err: to_v1_tx_error(err),
            })?;
        Ok(to_v1_signature(sig))
    }
}

fn tuning(compute_unit_limit: u32) -> SendTuning {
    SendTuning {
        compute_unit_limit,
        loaded_accounts_data_size_limit: 131_072,
        priority_fee_micro_lamports: 1_000,
        max_priority_fee_micro_lamports: 200_000,
        confirm_timeout: Duration::from_secs(30),
        ..Default::default()
    }
}

fn frames(n: u32) -> Vec<Frame> {
    (0..n)
        .map(|i| Frame {
            channel_id: [0u8; 16],
            frame_no: i as u16,
            is_last: i + 1 == n,
            body: vec![0xA5; 200],
        })
        .collect()
}

struct Fixture {
    ctx: ProgramTestContext,
    sender: BanksSender,
    target: BatchTarget,
}

/// A real inbox program with a batch of `n` leaves opened (at a generous limit) and ready for chunk sends.
async fn fixture(n: u32) -> Fixture {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        false,
    );
    let chain_id = 200_198u64;
    let authority = Keypair::new();
    pt.add_account(
        zk_inbox_client::root_pda(&settlement_program, chain_id).0,
        root_account_with_authority(chain_id, &authority.pubkey(), settlement_program),
    );
    pt.add_account(
        zk_inbox_client::cursor_pda(&program_id, &settlement_program, chain_id).0,
        cursor_account(program_id, chain_id, 0),
    );
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
    let ctx = pt.start_with_context().await;
    let sender = BanksSender {
        banks_client: ctx.banks_client.clone(),
        payer: authority.insecure_clone(),
        limits: Default::default(),
    };
    let target = BatchTarget {
        program_id,
        settlement_program,
        payer: authority.pubkey(),
        chain_id,
        batch: 0,
    };
    pipeline::open_and_grow_batch(&sender, target, n, tuning(400_000))
        .await
        .expect("OpenBatch(+Grow)");
    sender.limits.lock().unwrap().clear();
    Fixture {
        ctx,
        sender,
        target,
    }
}

async fn leaves_present(f: &mut Fixture) -> u32 {
    let (batch_pda, _) = zk_inbox_client::batch_pda(
        &f.target.program_id,
        &f.target.settlement_program,
        f.target.chain_id,
        f.target.batch,
    );
    let data = f
        .ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .expect("batch account")
        .data;
    zk_inbox_client::decode_batch_account(&data)
        .expect("decode")
        .leaves_present
}

/// Every first attempt (12,000 CU, below the fixed cost of any chunk transaction, about 16,400 CU before any extra bump attempt) ran out of compute units; each frame is
/// resent once at 100,000 and lands. Exactly one retry per frame, and every leaf is present on chain.
#[tokio::test]
async fn frames_that_run_out_of_compute_units_are_resent_once_at_the_retry_limit_and_land() {
    let mut f = fixture(3).await;
    let jobs = pipeline::build_frame_jobs(f.target, &frames(3));
    let outcome = f
        .sender
        .send_and_confirm_many_retrying_compute(
            &jobs,
            tuning(12_000),
            100_000,
            4,
            Duration::from_millis(1),
            256,
        )
        .await
        .expect("every frame must land on its retry");
    assert_eq!(outcome.frames.len(), 3);
    let mut limits = f.sender.limits.lock().unwrap().clone();
    limits.sort_unstable();
    assert_eq!(
        limits,
        vec![12_000, 12_000, 12_000, 100_000, 100_000, 100_000],
        "each frame: one attempt at the base limit, then exactly one retry at the retry limit"
    );
    assert_eq!(leaves_present(&mut f).await, 3);
}

/// Without the retry the same frames fail, and the failure is the compute overrun the retry keys on: the control
/// that shows the retry is what made the frames above land.
#[tokio::test]
async fn without_the_retry_the_same_frames_fail_with_a_compute_overrun() {
    let f = fixture(1).await;
    let jobs = pipeline::build_frame_jobs(f.target, &frames(1));
    let err = f
        .sender
        .send_and_confirm_many(&jobs, tuning(12_000), 4, Duration::from_millis(1), 256)
        .await
        .expect_err("12,000 CU is below a chunk transaction's cost");
    let tx_err = sender_error_transaction_error(&err).expect("an on-chain failure");
    assert!(is_compute_exceeded(&tx_err), "got {tx_err:?}");
}

/// A frame that needs more than the retry limit fails the call after exactly one retry.
#[tokio::test]
async fn a_frame_that_also_overruns_the_retry_limit_fails_after_one_retry() {
    let f = fixture(1).await;
    let jobs = pipeline::build_frame_jobs(f.target, &frames(1));
    let err = f
        .sender
        .send_and_confirm_many_retrying_compute(
            &jobs,
            tuning(12_000),
            13_000,
            4,
            Duration::from_millis(1),
            256,
        )
        .await
        .expect_err("13,000 CU is still below a chunk transaction's cost");
    assert!(rome_zk_batcher::sender::sender_error_is_compute_exceeded(
        &err
    ));
    assert_eq!(*f.sender.limits.lock().unwrap(), vec![12_000, 13_000]);
}
