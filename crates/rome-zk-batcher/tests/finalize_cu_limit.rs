//! `finalize_and_verify` used to send `FinalizeBatch` under `tuning.compute_unit_limit` — the same 200,000 default
//! every other (cheap) instruction uses — but a 900-leaf `FinalizeBatch` measures 334,459 CU on SBPF v3 (~370 CU/leaf;
//! it was 372,951 before the dependency bump), so every batch above ~530 frames fails all 120 confirm-polls
//! (`LeavesNeverComplete`), stuck sealed-but-never-finalized. This drives the real `pipeline::open_and_grow_batch` +
//! `pipeline::finalize_and_verify` functions (not just raw instructions) against the real, `cargo build-sbf`-compiled
//! `zk_inbox.so`, through a `Sender` bridged to `solana-program-test`'s `BanksClient` (so `tuning.compute_unit_limit`
//! is a real, enforced compute budget, not a fake) and an `RpcClient` bridged to the same `BanksClient` for
//! `finalize_and_verify`'s own account reads (`RpcClient::new_sender` — see `pipeline.rs`'s own
//! `finalize_and_verify_gate` tests for why a fixed-response version of this same bridge already exists there; this one
//! instead answers `getAccountInfo` from the live bank, since a real `FinalizeBatch` send genuinely mutates the
//! account).
//!
//! Leaf state is seeded directly onto the just-opened, just-grown account (same trick
//! `programs/zk-inbox/tests/growbatch_cursor.rs`'s 900-leaf test uses) rather than sending 900 real chunk
//! transactions — this test's subject is the CU-limit plumbing around `FinalizeBatch`, not the chunk lane
//! (separately covered).
//!
//! ## V1 note
//! `BanksSender` below still builds a **legacy** (`ComputeBudgetInstruction`-prefixed) transaction, on
//! purpose: these `solana-program-test` suites have always run legacy transactions, and the move to
//! `solana-program-test = 4.3.0` did not change that (same for `tests/hop_collapse.rs` and
//! `tests/pipeline.rs` — see their module docs). This file's whole subject is the *CU-limit selection* business logic
//! (`finalize_compute_unit_limit` vs `chunk_compute_unit_limit`), not the wire format (that proof is
//! `sender.rs`'s own `design_frame_v1_tx_fits_4096_and_carries_both_header_limits` test). The `Sender`
//! trait itself now returns the V1-generation `Signature`/`SenderError::StepFailed::err`
//! (`solana_transaction_error::TransactionError`), so `BanksSender` converts its own legacy-generation
//! signature/error at the one point they cross that boundary — same "one conversion" rule as
//! `sender::compat`, just local to this test file since program-test never needs the reverse direction.

use rome_zk_batcher::channel::Frame;
use rome_zk_batcher::metrics::Metrics;
use rome_zk_batcher::pipeline::{self, BatchTarget, FinalizePoll};
use rome_zk_batcher::sender::{SendTuning, Sender, SenderError};
use rome_zk_testkit::{cursor_account, root_account_with_authority};
use solana_client::client_error::Result as ClientResult;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_client::RpcClientConfig;
use solana_client::rpc_request::RpcRequest;
use solana_client::rpc_sender::{RpcSender as LowLevelRpcSender, RpcTransportStats};
use solana_program::{keccak, pubkey::Pubkey};
// `system_program`/`commitment_config`/`compute_budget` all moved out of
// `solana_program`'s/`solana_sdk`'s root re-exports in the Agave 4.x line (API fallout).
use solana_commitment_config::CommitmentConfig;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_program_test::BanksClient;
use solana_sdk::{
    account::{Account, AccountSharedData},
    instruction::Instruction,
    signature::{Keypair, Signer},
    transaction::Transaction,
};
use solana_system_interface::program as system_program;
use std::time::Duration;

/// `solana_sdk::transaction::TransactionError` (what `BanksClientError::TransactionError` carries)
/// -> `solana_transaction_error::TransactionError` (what `Sender`/`SenderError::StepFailed` use).
/// Deliberately narrow: this test file only ever produces (and only ever needs to assert on)
/// `InstructionError(index, ComputationalBudgetExceeded)` — the exact shape a real CU-exceeded
/// `FinalizeBatch` returns — so every other variant panics loudly rather than silently misrepresenting an
/// error this file was never written to expect. A general-purpose `TransactionError` converter
/// is out of scope for a test-only bridge; see `sender::compat`'s own module doc for the production
/// conversion boundary this crate actually ships.
fn to_v1_tx_error(
    e: solana_sdk::transaction::TransactionError,
) -> solana_transaction_error::TransactionError {
    match e {
        solana_sdk::transaction::TransactionError::InstructionError(idx, inner) => {
            let v1_inner = match inner {
                solana_sdk::instruction::InstructionError::ComputationalBudgetExceeded => {
                    solana_instruction_error::InstructionError::ComputationalBudgetExceeded
                }
                other => panic!(
                    "to_v1_tx_error: unhandled InstructionError variant {other:?} — this test-only \
                     bridge only converts ComputationalBudgetExceeded"
                ),
            };
            solana_transaction_error::TransactionError::InstructionError(idx, v1_inner)
        }
        other => panic!(
            "to_v1_tx_error: unhandled TransactionError variant {other:?} — this test-only bridge only \
             converts InstructionError"
        ),
    }
}

/// A `solana_sdk` `Signature` -> the `solana_signature::Signature` `Sender::send_and_confirm` returns —
/// a plain 64-byte round-trip, same shape as `sender::compat::to_legacy_signature`'s reverse direction.
fn to_v1_signature(s: solana_sdk::signature::Signature) -> solana_signature::Signature {
    let bytes: [u8; 64] = s.into();
    solana_signature::Signature::from(bytes)
}

/// Bridges [`Sender`] to a real `BanksClient` — `tuning.compute_unit_limit` becomes a real
/// `ComputeBudgetInstruction::SetComputeUnitLimit` in the sent transaction, enforced by the real runtime
/// exactly as `sender::RpcSender`'s own `build_tx` would build it.
struct BanksSender {
    banks_client: BanksClient,
    payer: Keypair,
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
            .map_err(|err| SenderError::StepFailed {
                frame_index: 0,
                stage_index: 0,
                tx_index: 0,
                err: to_v1_tx_error(err),
            })?;
        Ok(to_v1_signature(sig))
    }
}

/// Bridges `finalize_and_verify`'s `&RpcClient` account reads to the same live `BanksClient` — every
/// `getAccountInfo` call is answered from the bank's actual current state, so this sees the real effect
/// of `BanksSender`'s sends (unlike `pipeline.rs`'s own `FixedAccountRpc`, which answers a frozen
/// snapshot).
struct BanksAccountRpc {
    banks_client: BanksClient,
}

#[async_trait::async_trait]
impl LowLevelRpcSender for BanksAccountRpc {
    async fn send(
        &self,
        request: RpcRequest,
        params: serde_json::Value,
    ) -> ClientResult<serde_json::Value> {
        assert_eq!(
            request,
            RpcRequest::GetAccountInfo,
            "this test's finalize_and_verify call never needs any other RPC method"
        );
        let pubkey_str = params[0].as_str().expect("getAccountInfo pubkey param");
        let pubkey: Pubkey = pubkey_str.parse().expect("valid pubkey string");
        let banks_client = self.banks_client.clone();
        let account = banks_client
            .get_account(pubkey)
            .await
            .expect("BanksClient::get_account");
        let value = match account {
            Some(a) => serde_json::json!({
                "lamports": a.lamports,
                "data": bs58::encode(&a.data).into_string(),
                "owner": a.owner.to_string(),
                "executable": a.executable,
                "rentEpoch": a.rent_epoch,
                "space": a.data.len(),
            }),
            None => serde_json::Value::Null,
        };
        Ok(serde_json::json!({
            "context": {"slot": 1, "apiVersion": null},
            "value": value,
        }))
    }

    fn get_transport_stats(&self) -> RpcTransportStats {
        RpcTransportStats::default()
    }

    fn url(&self) -> String {
        "banks-account-rpc".to_string()
    }
}

fn banks_rpc_client(banks_client: BanksClient) -> RpcClient {
    RpcClient::new_sender(
        BanksAccountRpc { banks_client },
        RpcClientConfig::with_commitment(CommitmentConfig::confirmed()),
    )
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

/// 900 tiny frames (frame_no 0..900) — enough to reproduce the measured ~373k CU `FinalizeBatch` cost;
/// content is irrelevant (never re-derived against any source blocks here), only that each frame's own
/// `to_bytes()` hashes to what gets seeded as that leaf's on-chain (pre-finalize) hash.
fn synthetic_frames(n: u32) -> Vec<Frame> {
    (0..n)
        .map(|i| Frame {
            channel_id: [0u8; 16],
            frame_no: i as u16,
            is_last: i + 1 == n,
            body: i.to_le_bytes().to_vec(),
        })
        .collect()
}

/// Opens+grows a real `n`-leaf batch (via `pipeline::open_and_grow_batch`, driven by `BanksSender` — a
/// real transaction, real CU accounting), then seeds every leaf as already-sealed directly on the
/// resulting account (skipping `n` real chunk `{Open,Write,Seal}` + `SealLeaf` sends, exactly as
/// `programs/zk-inbox/tests/growbatch_cursor.rs`'s own 900-leaf test does) — preserving every header field
/// `OpenBatch` itself wrote (in particular `open_slot`, which `verify_acc` needs to match).
async fn open_grown_and_presealed_batch(
    ctx: &mut solana_program_test::ProgramTestContext,
    sender: &BanksSender,
    target: BatchTarget,
    settlement_program: &Pubkey,
    frames: &[Frame],
) {
    pipeline::open_and_grow_batch(sender, target, frames.len() as u32, tuning(200_000))
        .await
        .expect("OpenBatch(+Grow) must succeed");

    let (batch_pda, _) = zk_inbox_client::batch_pda(
        &target.program_id,
        settlement_program,
        target.chain_id,
        target.batch,
    );
    let account = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .expect("batch account must exist after OpenBatch(+Grow)");
    let target_len = rome_zk_layouts::batch::account_len_for(
        rome_zk_layouts::batch::VERSION,
        frames.len() as u32,
    )
    .unwrap();
    assert_eq!(account.data.len(), target_len);

    let mut data = account.data.clone();
    let bitmap_off = rome_zk_layouts::batch::HEADER_LEN_V2;
    let leaves_off = rome_zk_layouts::batch::leaves_offset_for(
        rome_zk_layouts::batch::VERSION,
        frames.len() as u32,
    )
    .unwrap();
    for (i, frame) in frames.iter().enumerate() {
        data[bitmap_off + i / 8] |= 1 << (i % 8);
        let hash = keccak::hashv(&[&frame.to_bytes()]).to_bytes();
        let slot = leaves_off + 32 * i;
        data[slot..slot + 32].copy_from_slice(&hash);
    }
    data[rome_zk_layouts::batch::OFF_LEAVES_PRESENT
        ..rome_zk_layouts::batch::OFF_LEAVES_PRESENT + 4]
        .copy_from_slice(&(frames.len() as u32).to_le_bytes());
    let mut new_account = account;
    new_account.data = data;
    ctx.set_account(&batch_pda, &AccountSharedData::from(new_account));
}

/// Under the batcher's general 200,000 CU limit, `finalize_and_verify` fails a 900-leaf `FinalizeBatch` —
/// `TransactionError` for exceeding the compute budget, surfaced through `SenderError::StepFailed` as
/// `PipelineError::Send`. This is why `FinalizeBatch` has its own, higher limit.
#[tokio::test]
async fn finalize_and_verify_fails_a_900_leaf_batch_at_the_general_200k_cu_limit() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        false,
    );
    let chain_id = 200_198u64;
    let batch = 0u64;
    let authority = Keypair::new();
    let (root, _) = zk_inbox_client::root_pda(&settlement_program, chain_id);
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &authority.pubkey(), settlement_program),
    );
    pt.add_account(
        zk_inbox_client::cursor_pda(&program_id, &settlement_program, chain_id).0,
        cursor_account(program_id, chain_id, batch),
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
    let mut ctx = pt.start_with_context().await;

    let sender = BanksSender {
        banks_client: ctx.banks_client.clone(),
        payer: authority.insecure_clone(),
    };
    let target = BatchTarget {
        program_id,
        settlement_program,
        payer: authority.pubkey(),
        chain_id,
        batch,
    };
    let frames = synthetic_frames(900);
    open_grown_and_presealed_batch(&mut ctx, &sender, target, &settlement_program, &frames).await;

    let rpc = banks_rpc_client(ctx.banks_client.clone());
    let metrics = Metrics::new();
    let err = pipeline::finalize_and_verify(
        &sender,
        &rpc,
        &metrics,
        target,
        tuning(200_000),
        FinalizePoll {
            expected_count: frames.len() as u32,
            poll_interval: Duration::from_millis(1),
            max_polls: 1,
        },
        &frames,
    )
    .await
    .expect_err(
        "FinalizeBatch(900 leaves) must exceed a 200,000 CU limit — this is the exact failure \
         the CU-limit selection exists to avoid",
    );
    let msg = format!("{err:?}");
    eprintln!("finalize_and_verify at 200,000 CU failed as expected (RED reproduction): {msg}");
    assert!(
        matches!(
            err,
            rome_zk_batcher::pipeline::PipelineError::Send(SenderError::StepFailed {
                err: solana_transaction_error::TransactionError::InstructionError(
                    _,
                    solana_instruction_error::InstructionError::ComputationalBudgetExceeded
                ),
                ..
            })
        ),
        "expected ComputationalBudgetExceeded, got {msg}"
    );
}

/// GREEN: the same 900-leaf batch, `finalize_and_verify` called with `config.finalize_compute_unit_limit`
/// (600,000, the new default) instead of the general 200,000 default — succeeds, and `acc` matches the
/// off-chain reference exactly as it does at every other leaf count.
#[tokio::test]
async fn finalize_and_verify_succeeds_a_900_leaf_batch_at_the_configured_finalize_cu_limit() {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        false,
    );
    let chain_id = 200_199u64;
    let batch = 0u64;
    let authority = Keypair::new();
    let (root, _) = zk_inbox_client::root_pda(&settlement_program, chain_id);
    pt.add_account(
        root,
        root_account_with_authority(chain_id, &authority.pubkey(), settlement_program),
    );
    pt.add_account(
        zk_inbox_client::cursor_pda(&program_id, &settlement_program, chain_id).0,
        cursor_account(program_id, chain_id, batch),
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
    let mut ctx = pt.start_with_context().await;

    let sender = BanksSender {
        banks_client: ctx.banks_client.clone(),
        payer: authority.insecure_clone(),
    };
    let target = BatchTarget {
        program_id,
        settlement_program,
        payer: authority.pubkey(),
        chain_id,
        batch,
    };
    let frames = synthetic_frames(900);
    open_grown_and_presealed_batch(&mut ctx, &sender, target, &settlement_program, &frames).await;

    let rpc = banks_rpc_client(ctx.banks_client.clone());
    let metrics = Metrics::new();
    // 600,000 — `rome_zk_batcher::config::Config`'s `finalize_compute_unit_limit` default.
    let finalize_cu_limit = 600_000u32;
    let decoded = pipeline::finalize_and_verify(
        &sender,
        &rpc,
        &metrics,
        target,
        tuning(finalize_cu_limit),
        FinalizePoll {
            expected_count: frames.len() as u32,
            poll_interval: Duration::from_millis(1),
            max_polls: 5,
        },
        &frames,
    )
    .await
    .expect("FinalizeBatch(900 leaves) must succeed at the configured 600,000 CU finalize limit");
    assert!(decoded.finalized);
    let reference_acc =
        pipeline::verify_acc(&decoded, &frames).expect("on-chain acc must match reference");
    assert_eq!(decoded.acc, reference_acc);
}
