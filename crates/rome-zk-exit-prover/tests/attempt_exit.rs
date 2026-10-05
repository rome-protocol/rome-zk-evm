//! `attempt_exit`'s own RED tests: every named pre-send refusal, the V1 send
//! classification (`ExitAlreadyProved`/`ExitCapExceeded`), the `StateUnavailableAtRoot` retry, and the
//! measured tx-size report — all against fixtures (`fixtures/exit/*.json`) and small scripted fakes, never
//! a live cluster or verifier node.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use rome_zk_exit_prover::core::{attempt_exit, AttemptParams, Outcome, Refusal};
use rome_zk_exit_prover::proof::VerifyRefusal;
use rome_zk_exit_prover::rpc::{GetProofResult, LogEntry, VerifierError, VerifierRpc};
use rome_zk_exit_prover::settlement::{ReadError, SettlementReader};
use rome_zk_layouts::exit::ExitMessage;
use rome_zk_solana_sender::{SendTuning, Sender, SenderError};
use serde::Deserialize;
// `Sender::send_and_confirm`'s own `instructions` type is `solana_program::instruction::Instruction`
// (see `rome-zk-solana-sender`'s trait doc), not `solana_instruction::Instruction` directly.
use solana_program::instruction::Instruction;
use solana_program::pubkey::Pubkey;
use solana_transaction_error::TransactionError;

const ANVIL_GET_PROOF: &str = include_str!("../../../fixtures/exit/anvil_getProof.json");
const ANVIL_STATE_ROOT: &str = include_str!("../../../fixtures/exit/anvil_state_root.json");
const MESSAGE_HASH_JSON: &str = include_str!("../../../fixtures/exit/message_hash.json");

#[derive(Deserialize)]
struct StateRootFixture {
    number: String,
    state_root: String,
}
#[derive(Deserialize)]
struct MessageHashFixture {
    nonce: u64,
    l2_sender: String,
    sol_recipient: String,
    asset: String,
    amount_wei: String,
    portal: String,
}

fn strip_0x(s: &str) -> &str {
    s.strip_prefix("0x").unwrap_or(s)
}
fn hex32(s: &str) -> [u8; 32] {
    let h = strip_0x(s);
    let v = hex::decode(format!("{:0>64}", h)).unwrap();
    v.try_into().unwrap()
}
fn hex20(s: &str) -> [u8; 20] {
    hex::decode(strip_0x(s)).unwrap().try_into().unwrap()
}
fn hex_u64(s: &str) -> u64 {
    u64::from_str_radix(strip_0x(s), 16).unwrap()
}

fn fixture_message() -> ExitMessage {
    let m: MessageHashFixture = serde_json::from_str(MESSAGE_HASH_JSON).unwrap();
    ExitMessage {
        nonce: m.nonce,
        l2_sender: hex20(&m.l2_sender),
        sol_recipient: hex32(&m.sol_recipient),
        asset: hex20(&m.asset),
        amount: m.amount_wei.parse().unwrap(),
    }
}

fn fixture_portal() -> [u8; 20] {
    let m: MessageHashFixture = serde_json::from_str(MESSAGE_HASH_JSON).unwrap();
    hex20(&m.portal)
}

fn fixture_state_root_and_block() -> ([u8; 32], u64) {
    let r: StateRootFixture = serde_json::from_str(ANVIL_STATE_ROOT).unwrap();
    (hex32(&r.state_root), hex_u64(&r.number))
}

fn fixture_get_proof() -> GetProofResult {
    serde_json::from_str(ANVIL_GET_PROOF).unwrap()
}

// ---------------------------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
struct FakeRoot {
    head_final_batch: u64,
    challenge_window_slots: u64,
    chain_id: u64,
    /// `root.exit_cap_per_window` (`FakeRoot` did not carry this field before, which is
    /// why the missing on-chain `ExitCapUnset` mirror was invisible). Every existing happy-path literal
    /// below sets a non-zero value so a valid exit still proves; only the new `ExitCapUnset` RED test sets
    /// it to `0`.
    exit_cap_per_window: u64,
}

#[derive(Clone)]
struct FakePending {
    state_root: [u8; 32],
    last_block: u64,
}

/// A `SettlementReader` scripted with an ordered sequence of roots (one consulted per `read_root` call,
/// the last one repeating once exhausted — mirrors a chain head that advances between polls) and a
/// per-batch pending map. `Cell` is not `Sync`, so the index lives behind a `Mutex` instead — this fake
/// must satisfy `SettlementReader`'s `Send + Sync` bound.
struct FakeSettlementInner {
    roots: Vec<FakeRoot>,
    index: usize,
}

struct FakeSettlementReader {
    inner: Mutex<FakeSettlementInner>,
    pending: HashMap<u64, FakePending>,
    exit_portal: [u8; 20],
    /// `window_index -> spent_cap_units` — a window absent from this map reads as
    /// `Ok(None)` (0 spent), same "absent = zero" convention `read_nullifier_page` already uses. Empty by
    /// default via `single`/`sequence`; tests that need a spent window use [`FakeSettlementReader::with_windows`].
    windows: HashMap<u64, u64>,
}

impl FakeSettlementReader {
    fn single(root: FakeRoot, pending: FakePending, exit_portal: [u8; 20]) -> Self {
        let batch = root.head_final_batch;
        Self {
            inner: Mutex::new(FakeSettlementInner {
                roots: vec![root],
                index: 0,
            }),
            pending: HashMap::from([(batch, pending)]),
            exit_portal,
            windows: HashMap::new(),
        }
    }

    fn sequence(
        roots: Vec<FakeRoot>,
        pending: HashMap<u64, FakePending>,
        exit_portal: [u8; 20],
    ) -> Self {
        Self {
            inner: Mutex::new(FakeSettlementInner { roots, index: 0 }),
            pending,
            exit_portal,
            windows: HashMap::new(),
        }
    }

    fn with_windows(mut self, windows: HashMap<u64, u64>) -> Self {
        self.windows = windows;
        self
    }
}

impl SettlementReader for FakeSettlementReader {
    fn read_root(&self) -> Result<zk_settlement_client::RootAccount, ReadError> {
        let mut inner = self.inner.lock().unwrap();
        let idx = inner.index.min(inner.roots.len() - 1);
        let r = inner.roots[idx].clone();
        inner.index += 1;
        Ok(zk_settlement_client::RootAccount {
            chain_id: r.chain_id,
            number: 0,
            parent_hash: [0; 32],
            state_root: [0; 32],
            block_hash: [0; 32],
            updates: 0,
            profile: 0,
            challenge_window_slots: r.challenge_window_slots as u32,
            prove_window_slots: 0,
            proving_policy: 0,
            poster_bond: 0,
            exit_cap_per_window: r.exit_cap_per_window,
            authority: Pubkey::new_unique(),
            head_pending_batch: r.head_final_batch,
            head_final_batch: r.head_final_batch,
            pending_count: 0,
            max_pending: 0,
        })
    }

    fn read_pending(&self, batch: u64) -> Result<zk_settlement_client::PendingAccount, ReadError> {
        let p = self
            .pending
            .get(&batch)
            .unwrap_or_else(|| panic!("fake has no pending account for batch {batch}"));
        Ok(zk_settlement_client::PendingAccount {
            batch,
            prev_batch: batch.saturating_sub(1),
            pre_state_root: [0; 32],
            first_block: 0,
            last_block: p.last_block,
            state_root: p.state_root,
            block_roots_merkle: [0; 32],
            inbox_commitment: [0; 32],
            forced_outcome_commitment: [0; 32],
            posted_slot: 0,
            status: 0,
            disputes_open: 0,
            deadline_slot: 0,
            parent_hash: [0; 32],
            last_block_hash: [0; 32],
        })
    }

    fn read_exit_config(&self) -> Result<zk_settlement_client::ExitConfigAccount, ReadError> {
        Ok(zk_settlement_client::ExitConfigAccount {
            chain_id: 200_101,
            exit_portal: self.exit_portal,
            bridge_program: Pubkey::new_unique(),
            pending_exit_portal: [0; 20],
            pending_bridge_program: Pubkey::default(),
            pending_exit_cap: 0,
            pending_poster_bond: 0,
            activation_slot: 0,
            pending_mask: 0,
        })
    }

    /// Every test in this file proves a message that has never been proved before — the account is
    /// absent (bit clear), same as a real chain that has never touched this nullifier page.
    fn read_nullifier_page(
        &self,
        _page: u64,
    ) -> Result<Option<zk_settlement_client::NullifierPageAccount>, ReadError> {
        Ok(None)
    }

    fn read_exit_window(
        &self,
        window_index: u64,
    ) -> Result<Option<zk_settlement_client::ExitWindowAccount>, ReadError> {
        Ok(self.windows.get(&window_index).map(|&spent_cap_units| {
            zk_settlement_client::ExitWindowAccount {
                chain_id: 200_101,
                window_index,
                spent_cap_units,
                exits: 0,
            }
        }))
    }

    fn read_slot(&self) -> Result<u64, ReadError> {
        unimplemented!("not exercised by attempt_exit — read_slot is a run::poll_once-only read")
    }
}

/// A `VerifierRpc` keyed by requested block number — each call consults the same script (never mutated),
/// so it's safe to share across the two `attempt_exit` calls a "retries against a later poll" test makes.
struct FakeVerifier {
    by_block: HashMap<u64, Result<GetProofResult, String>>,
    calls: AtomicU32,
}

impl FakeVerifier {
    fn new(by_block: HashMap<u64, Result<GetProofResult, String>>) -> Self {
        Self {
            by_block,
            calls: AtomicU32::new(0),
        }
    }
}

impl VerifierRpc for FakeVerifier {
    fn eth_block_number(&self) -> Result<u64, VerifierError> {
        Ok(1_000)
    }
    fn eth_get_logs(
        &self,
        _portal: &str,
        _from: u64,
        _to: u64,
    ) -> Result<Vec<LogEntry>, VerifierError> {
        Ok(vec![])
    }
    fn eth_get_proof(
        &self,
        _address: &str,
        _slot: &str,
        block: u64,
    ) -> Result<GetProofResult, VerifierError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.by_block.get(&block) {
            Some(Ok(r)) => Ok(r.clone()),
            Some(Err(msg)) => Err(VerifierError::RpcError {
                method: "eth_getProof",
                error: msg.clone(),
            }),
            None => panic!("FakeVerifier has no script for block {block}"),
        }
    }
    fn eth_get_block_by_number(
        &self,
        _block: u64,
    ) -> Result<rome_zk_exit_prover::rpc::BlockByNumber, VerifierError> {
        unimplemented!("not exercised by attempt_exit")
    }
}

/// Records how many times `send_and_confirm` was called, and returns a scripted single outcome — the
/// "0 sends" crux every pre-send refusal test proves, and the "exactly 1 send, no internal resend" crux
/// every send-classification test proves.
struct FakeSender {
    calls: AtomicU32,
    result: Mutex<Option<Result<solana_signature::Signature, TransactionError>>>,
}

impl FakeSender {
    fn ok() -> Self {
        Self {
            calls: AtomicU32::new(0),
            result: Mutex::new(Some(Ok(solana_signature::Signature::new_unique()))),
        }
    }
    fn failing_with(code: u32) -> Self {
        Self {
            calls: AtomicU32::new(0),
            result: Mutex::new(Some(Err(TransactionError::InstructionError(
                0,
                solana_instruction_error::InstructionError::Custom(code),
            )))),
        }
    }
    fn panics_if_called() -> Self {
        Self {
            calls: AtomicU32::new(0),
            result: Mutex::new(None),
        }
    }
    fn call_count(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Sender for FakeSender {
    async fn send_and_confirm(
        &self,
        _instructions: &[Instruction],
        _tuning: SendTuning,
    ) -> Result<solana_signature::Signature, SenderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.result.lock().unwrap().take() {
            None => {
                panic!("FakeSender::send_and_confirm must never be called for a pre-send refusal")
            }
            Some(Ok(sig)) => Ok(sig),
            Some(Err(err)) => Err(SenderError::StepFailed {
                frame_index: 0,
                stage_index: 0,
                tx_index: 0,
                err,
            }),
        }
    }
}

/// A cap comfortably above the fixture message's own unit count (`cap_units(1e18 wei) ==
/// 1_000_000_000`), so every existing happy-path test below — none of which is exercising the
/// cap checks on purpose — still proves cleanly under them.
const AMPLE_CAP: u64 = 5_000_000_000;

fn default_tuning() -> SendTuning {
    SendTuning {
        compute_unit_limit: 150_000,
        loaded_accounts_data_size_limit: 16 * 1024,
        priority_fee_micro_lamports: 0,
        max_priority_fee_micro_lamports: 0,
        confirm_timeout: std::time::Duration::from_secs(1),
        ..Default::default()
    }
}

fn default_params(now_slot: u64, max_tx_bytes: usize) -> AttemptParams {
    AttemptParams {
        program_id: Pubkey::new_unique(),
        payer: Pubkey::new_unique(),
        now_slot,
        max_tx_bytes,
        tuning: default_tuning(),
        local_spent_units: 0,
    }
}

// ---------------------------------------------------------------------------------------------
// RED tests
// ---------------------------------------------------------------------------------------------

/// RED: `local_verify_refuses_on_root_mismatch_before_send` — a fake verifier returns a genuine proof
/// (the real anvil fixture) but the fake `pending.state_root` is NOT what that proof's own account nodes
/// hash to; the prover refuses `RootMismatch` and the fake `Sender` records ZERO sends (the crux: a
/// doomed proof never spends a fee). Mutation target: skip the pre-send local verify.
#[tokio::test]
async fn local_verify_refuses_on_root_mismatch_before_send() {
    let (_real_root, block) = fixture_state_root_and_block();
    let wrong_root = [0x99u8; 32]; // NOT the fixture's real state_root
    let settlement = FakeSettlementReader::single(
        FakeRoot {
            head_final_batch: 5,
            challenge_window_slots: 100,
            chain_id: 200_101,
            exit_cap_per_window: AMPLE_CAP,
        },
        FakePending {
            state_root: wrong_root,
            last_block: block,
        },
        fixture_portal(),
    );
    let verifier = FakeVerifier::new(HashMap::from([(block, Ok(fixture_get_proof()))]));
    let sender = FakeSender::panics_if_called();

    let outcome = attempt_exit(
        &settlement,
        &verifier,
        &sender,
        fixture_message(),
        &default_params(1000, 4096),
    )
    .await
    .unwrap();

    assert_eq!(
        outcome,
        Outcome::Refused(Refusal::Verify(VerifyRefusal::RootMismatch))
    );
    assert_eq!(
        sender.call_count(),
        0,
        "a doomed proof must never spend a fee"
    );
}

/// RED: `proof_too_large_is_refused_before_send` — a synthetic `ExitProof` (16 account + 8 storage
/// nodes, ~300 B each — well over the V1 envelope once wrapped in a `ProveExit` instruction) is refused
/// `ProofTooLarge` before the `Sender` is ever touched. Mutation target: drop the byte_len size check.
#[tokio::test]
async fn proof_too_large_is_refused_before_send() {
    let (state_root, block) = fixture_state_root_and_block();
    let synthetic_node = format!("0x{}", "ab".repeat(300));
    let synthetic = GetProofResult {
        address: "0x0000000000000000000000000000000000000000".to_string(),
        balance: "0x0".to_string(),
        nonce: "0x0".to_string(),
        storage_hash: "0x0".to_string(),
        code_hash: "0x0".to_string(),
        account_proof: vec![synthetic_node.clone(); 16],
        storage_proof: vec![rome_zk_exit_prover::rpc::StorageProofEntry {
            key: "0x0".to_string(),
            value: "0x1".to_string(),
            proof: vec![synthetic_node; 8],
        }],
    };

    let settlement = FakeSettlementReader::single(
        FakeRoot {
            head_final_batch: 5,
            challenge_window_slots: 100,
            chain_id: 200_101,
            exit_cap_per_window: AMPLE_CAP,
        },
        FakePending {
            state_root,
            last_block: block,
        },
        fixture_portal(),
    );
    let verifier = FakeVerifier::new(HashMap::from([(block, Ok(synthetic))]));
    let sender = FakeSender::panics_if_called();

    let outcome = attempt_exit(
        &settlement,
        &verifier,
        &sender,
        fixture_message(),
        &default_params(1000, 4096),
    )
    .await
    .unwrap();

    match outcome {
        Outcome::Refused(Refusal::ProofTooLarge { len, max }) => {
            assert!(len > max, "reported len {len} must exceed max {max}");
            assert_eq!(max, 4096);
        }
        other => panic!("expected ProofTooLarge, got {other:?}"),
    }
    assert_eq!(sender.call_count(), 0);
}

/// RED: `over_cap_response_enters_queued_state_with_next_index` — a fake `Sender` returning
/// `ExitCapExceeded` (68) puts the prover in `QueuedForWindow{next_index = current+1}` — NOT a halt —
/// and the send happens exactly once (no internal resend loop). Mutation target: treat `ExitCapExceeded`
/// as a halt.
#[tokio::test]
async fn over_cap_response_enters_queued_state_with_next_index() {
    let (state_root, block) = fixture_state_root_and_block();
    let challenge_window_slots = 100u64;
    let now_slot = 250u64; // window_index = 2
    let settlement = FakeSettlementReader::single(
        FakeRoot {
            head_final_batch: 5,
            challenge_window_slots,
            chain_id: 200_101,
            exit_cap_per_window: AMPLE_CAP,
        },
        FakePending {
            state_root,
            last_block: block,
        },
        fixture_portal(),
    );
    let verifier = FakeVerifier::new(HashMap::from([(block, Ok(fixture_get_proof()))]));
    let sender = FakeSender::failing_with(68);

    let outcome = attempt_exit(
        &settlement,
        &verifier,
        &sender,
        fixture_message(),
        &default_params(now_slot, 4096),
    )
    .await
    .unwrap();

    assert_eq!(
        outcome,
        Outcome::QueuedForWindow {
            next_index: 3,
            retry_slot: 300,
        }
    );
    assert_eq!(
        sender.call_count(),
        1,
        "exactly one send, no internal resend"
    );
}

/// RED: `already_proved_is_treated_as_done` — a fake `Sender` returning `ExitAlreadyProved` (65) ->
/// advance, zero re-sends.
#[tokio::test]
async fn already_proved_is_treated_as_done() {
    let (state_root, block) = fixture_state_root_and_block();
    let settlement = FakeSettlementReader::single(
        FakeRoot {
            head_final_batch: 5,
            challenge_window_slots: 100,
            chain_id: 200_101,
            exit_cap_per_window: AMPLE_CAP,
        },
        FakePending {
            state_root,
            last_block: block,
        },
        fixture_portal(),
    );
    let verifier = FakeVerifier::new(HashMap::from([(block, Ok(fixture_get_proof()))]));
    let sender = FakeSender::failing_with(65);

    let outcome = attempt_exit(
        &settlement,
        &verifier,
        &sender,
        fixture_message(),
        &default_params(1000, 4096),
    )
    .await
    .unwrap();

    assert_eq!(outcome, Outcome::AlreadyProved);
    assert_eq!(sender.call_count(), 1);
}

/// RED: `state_unavailable_retries_newest_final_root` — a fake verifier errors "distance to target block
/// exceeds maximum proof window" on an OLD final batch's block; a second, independent `attempt_exit` call
/// against a NEWER final batch (the chain head having advanced between polls) succeeds. Two separate
/// calls, sharing one `FakeVerifier` script keyed by block and one `FakeSettlementReader` whose root
/// sequence advances — the loop's own retry is simply "call again once the newest Final batch changed".
#[tokio::test]
async fn state_unavailable_retries_newest_final_root() {
    let (real_state_root, real_block) = fixture_state_root_and_block();
    let old_block = real_block + 1000; // an old batch's block the verifier no longer serves state for

    let verifier = FakeVerifier::new(HashMap::from([
        (
            old_block,
            Err("distance to target block exceeds maximum proof window".to_string()),
        ),
        (real_block, Ok(fixture_get_proof())),
    ]));

    let mut pending = HashMap::new();
    pending.insert(
        1,
        FakePending {
            state_root: [0x11; 32], // never read: this attempt refuses before any verify
            last_block: old_block,
        },
    );
    pending.insert(
        2,
        FakePending {
            state_root: real_state_root,
            last_block: real_block,
        },
    );
    let settlement = FakeSettlementReader::sequence(
        vec![
            FakeRoot {
                head_final_batch: 1,
                challenge_window_slots: 100,
                chain_id: 200_101,
                exit_cap_per_window: AMPLE_CAP,
            },
            FakeRoot {
                head_final_batch: 2,
                challenge_window_slots: 100,
                chain_id: 200_101,
                exit_cap_per_window: AMPLE_CAP,
            },
        ],
        pending,
        fixture_portal(),
    );
    let sender = FakeSender::ok();

    // First poll: the newest Final batch (1) points at a block the verifier no longer serves.
    let outcome1 = attempt_exit(
        &settlement,
        &verifier,
        &sender,
        fixture_message(),
        &default_params(1000, 4096),
    )
    .await
    .unwrap();
    assert_eq!(
        outcome1,
        Outcome::Refused(Refusal::StateUnavailableAtRoot { number: old_block })
    );
    assert_eq!(sender.call_count(), 0);

    // Second poll: the chain moved on (head_final_batch is now 2); the SAME message proves cleanly.
    let outcome2 = attempt_exit(
        &settlement,
        &verifier,
        &sender,
        fixture_message(),
        &default_params(1000, 4096),
    )
    .await
    .unwrap();
    assert!(matches!(outcome2, Outcome::Sent { .. }), "{outcome2:?}");
    assert_eq!(sender.call_count(), 1);
}

/// RED: `v1_tx_size_within_envelope` — the anvil-fixture `ProveExit` signed V1 tx measures within the
/// 4,096-B envelope; also reports the size of a synthetic 16-account+8-storage-node proof (the shape
/// `--max-proof-bytes`'s default must accommodate or refuse).
#[test]
fn v1_tx_size_within_envelope() {
    use rome_zk_exit_prover::core::measure_prove_exit_tx_len;
    use rome_zk_exit_prover::proof::exit_proof_from_get_proof;

    let program_id = Pubkey::new_unique();
    let message = fixture_message();
    let message_arg = zk_settlement_client::ExitMessageArg {
        nonce: message.nonce,
        l2_sender: message.l2_sender,
        sol_recipient: message.sol_recipient,
        asset: message.asset,
        amount: message.amount,
    };
    let anvil_proof = exit_proof_from_get_proof(&fixture_get_proof()).unwrap();

    let anvil_len = measure_prove_exit_tx_len(
        &program_id,
        200_101,
        5,
        message_arg,
        anvil_proof,
        2,
        0,
        default_tuning(),
    )
    .unwrap();
    println!("anvil-fixture ProveExit signed V1 tx: {anvil_len} bytes");
    assert!(
        anvil_len <= 4096,
        "anvil-fixture ProveExit tx {anvil_len} B exceeds the 4,096 B V1 envelope"
    );

    let synthetic_node = vec![0xabu8; 300];
    let synthetic_proof = rome_zk_mpt::ExitProof {
        account_nodes: vec![synthetic_node.clone(); 16],
        storage_nodes: vec![synthetic_node; 8],
    };
    let synthetic_len = measure_prove_exit_tx_len(
        &program_id,
        200_101,
        5,
        message_arg,
        synthetic_proof,
        2,
        0,
        default_tuning(),
    )
    .unwrap();
    println!("synthetic 16+8-node ProveExit signed V1 tx: {synthetic_len} bytes (grounds --max-proof-bytes)");
    assert!(
        synthetic_len > 4096,
        "the 16+8-node synthetic proof ({synthetic_len} B) should exceed the V1 envelope — it's the \
         shape that motivates a pre-send ProofTooLarge refusal in the first place"
    );
}

/// RED: `recorded_getproof_fixture_decodes` end-to-end via `attempt_exit` itself (the unit version lives
/// in `src/proof.rs`) — a clean fixture with an `Ok` `Sender` sends successfully.
#[tokio::test]
async fn recorded_getproof_fixture_decodes_end_to_end() {
    let (state_root, block) = fixture_state_root_and_block();
    let settlement = FakeSettlementReader::single(
        FakeRoot {
            head_final_batch: 5,
            challenge_window_slots: 100,
            chain_id: 200_101,
            exit_cap_per_window: AMPLE_CAP,
        },
        FakePending {
            state_root,
            last_block: block,
        },
        fixture_portal(),
    );
    let verifier = FakeVerifier::new(HashMap::from([(block, Ok(fixture_get_proof()))]));
    let sender = FakeSender::ok();

    let outcome = attempt_exit(
        &settlement,
        &verifier,
        &sender,
        fixture_message(),
        &default_params(1000, 4096),
    )
    .await
    .unwrap();
    assert!(matches!(outcome, Outcome::Sent { .. }), "{outcome:?}");
    assert_eq!(sender.call_count(), 1);
}

/// RED: `exit_cap_unset_is_refused_before_send` — mirrors the on-chain `ExitCapUnset`
/// gate (`root.exit_cap_per_window == 0`, error 64). Governance can activate `exit_portal`
/// alone (an independent `pending_mask` bit from the cap), so `(exit_portal set, exit_cap_per_window ==
/// 0)` is reachable on any chain. The fake verifier carries NO script at all (`FakeVerifier::new` with an
/// empty map) — it panics if `eth_get_proof` is ever called, proving the gate fires BEFORE any fetch, not
/// just before the send — and `FakeSender::panics_if_called` proves zero sends.
/// Mutation target: delete the `root.exit_cap_per_window == 0` check in `attempt_exit` — this test then
/// fails: `FakeVerifier` panics ("has no script for block ...") because the gate no longer stops the
/// pre-fetch, i.e. a doomed proof would be fetched and sent.
#[tokio::test]
async fn exit_cap_unset_is_refused_before_send() {
    let (state_root, block) = fixture_state_root_and_block();
    let settlement = FakeSettlementReader::single(
        FakeRoot {
            head_final_batch: 5,
            challenge_window_slots: 100,
            chain_id: 200_101,
            exit_cap_per_window: 0, // the reachable (portal set, cap unset) state
        },
        FakePending {
            state_root,
            last_block: block,
        },
        fixture_portal(), // exit_portal IS set
    );
    // No script for any block — a genuine inclusion proof would be a lie if fetched; the gate must
    // refuse before `eth_get_proof` is ever called.
    let verifier = FakeVerifier::new(HashMap::new());
    let sender = FakeSender::panics_if_called();

    let outcome = attempt_exit(
        &settlement,
        &verifier,
        &sender,
        fixture_message(),
        &default_params(1000, 4096),
    )
    .await
    .unwrap();

    assert_eq!(outcome, Outcome::Refused(Refusal::ExitCapUnset));
    assert_eq!(
        verifier.calls.load(Ordering::SeqCst),
        0,
        "the cap gate must fire before any eth_getProof fetch"
    );
    assert_eq!(
        sender.call_count(),
        0,
        "a doomed proof must never spend a fee"
    );
}

// ---------------------------------------------------------------------------------------------
// Cap pre-checks before `read_pending`/`eth_getProof`/`Sender`
// ---------------------------------------------------------------------------------------------

/// RED: `an_amount_over_the_whole_cap_is_stuck_before_any_send` — a cap smaller than
/// this single message's own unit count refuses `ExceedsWindowCap` before ANY read past `exit_config`
/// (no `eth_getProof` fetch, no window read needed, no send). Mutation target: drop pre-check (a) in
/// `core::attempt_exit`.
#[tokio::test]
async fn an_amount_over_the_whole_cap_is_stuck_before_any_send() {
    let (state_root, block) = fixture_state_root_and_block();
    // fixture message = 1e18 wei = 1_000_000_000 cap units; a cap of 500 can never admit it, in any window.
    let settlement = FakeSettlementReader::single(
        FakeRoot {
            head_final_batch: 5,
            challenge_window_slots: 100,
            chain_id: 200_101,
            exit_cap_per_window: 500,
        },
        FakePending {
            state_root,
            last_block: block,
        },
        fixture_portal(),
    );
    // No script for any block, and no window entry either — reaching either would itself be the bug.
    let verifier = FakeVerifier::new(HashMap::new());
    let sender = FakeSender::panics_if_called();

    let outcome = attempt_exit(
        &settlement,
        &verifier,
        &sender,
        fixture_message(),
        &default_params(1000, 4096),
    )
    .await
    .unwrap();

    assert_eq!(
        outcome,
        Outcome::Refused(Refusal::ExceedsWindowCap {
            units: 1_000_000_000,
            cap: 500,
        })
    );
    assert_eq!(
        verifier.calls.load(Ordering::SeqCst),
        0,
        "the whole-cap gate must fire before any eth_getProof fetch"
    );
    assert_eq!(
        sender.call_count(),
        0,
        "a doomed exit must never spend a fee"
    );
}

/// RED: `a_spent_window_queues_without_touching_the_sender` — the window this attempt
/// would land in has already spent enough that this message's own units would overflow it; queues to the
/// next window WITHOUT ever calling `eth_getProof` or the `Sender`. Mutation target: drop pre-check (b)
/// (the `read_exit_window` call and its comparison) in `core::attempt_exit`.
#[tokio::test]
async fn a_spent_window_queues_without_touching_the_sender() {
    let (state_root, block) = fixture_state_root_and_block();
    let challenge_window_slots = 100u64;
    let now_slot = 250u64; // window_index = 2
    let settlement = FakeSettlementReader::single(
        FakeRoot {
            head_final_batch: 5,
            challenge_window_slots,
            chain_id: 200_101,
            // Exactly the fixture message's own unit count — passes pre-check (a) alone, but the window
            // below has already spent 1 unit, so (a)+(b) together overflow it.
            exit_cap_per_window: 1_000_000_000,
        },
        FakePending {
            state_root,
            last_block: block,
        },
        fixture_portal(),
    )
    .with_windows(HashMap::from([(2, 1)]));
    let verifier = FakeVerifier::new(HashMap::new());
    let sender = FakeSender::panics_if_called();

    let outcome = attempt_exit(
        &settlement,
        &verifier,
        &sender,
        fixture_message(),
        &default_params(now_slot, 4096),
    )
    .await
    .unwrap();

    assert_eq!(
        outcome,
        Outcome::QueuedForWindow {
            next_index: 3,
            retry_slot: 300,
        }
    );
    assert_eq!(
        verifier.calls.load(Ordering::SeqCst),
        0,
        "a spent window must queue before any eth_getProof fetch"
    );
    assert_eq!(
        sender.call_count(),
        0,
        "queuing must never touch the Sender"
    );
}

/// RED: `own_confirmed_sends_count_against_the_window_before_the_chain_sees_them`.
/// The chain's `exit_window` (read at `finalized`) still shows 0 spent, but THIS process already sent 600
/// of a 1_000_000_000-unit cap in this window... here modelled as: cap == the fixture's own unit count,
/// chain spent 0, `local_spent_units` 1 — the follower's own accounting alone must make the attempt queue
/// WITHOUT any `eth_getProof` fetch or `Sender` call. Mutation target: drop the
/// `.max(params.local_spent_units)` in `core::attempt_exit`'s window pre-check.
#[tokio::test]
async fn own_confirmed_sends_count_against_the_window_before_the_chain_sees_them() {
    let (state_root, block) = fixture_state_root_and_block();
    let challenge_window_slots = 100u64;
    let now_slot = 250u64; // window_index = 2
    let settlement = FakeSettlementReader::single(
        FakeRoot {
            head_final_batch: 5,
            challenge_window_slots,
            chain_id: 200_101,
            exit_cap_per_window: 1_000_000_000,
        },
        FakePending {
            state_root,
            last_block: block,
        },
        fixture_portal(),
    ); // no `.with_windows(..)`: the chain reads 0 spent for window 2
    let verifier = FakeVerifier::new(HashMap::new());
    let sender = FakeSender::panics_if_called();

    let mut params = default_params(now_slot, 4096);
    params.local_spent_units = 1;
    let outcome = attempt_exit(&settlement, &verifier, &sender, fixture_message(), &params)
        .await
        .unwrap();

    assert_eq!(
        outcome,
        Outcome::QueuedForWindow {
            next_index: 3,
            retry_slot: 300,
        }
    );
    assert_eq!(verifier.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        sender.call_count(),
        0,
        "our own confirmed send must be enough to refuse locally"
    );
}

/// RED: `an_unsupported_asset_is_refused_before_any_read_of_the_proof` — the
/// local mirror of the on-chain `UnsupportedAsset` gate (`programs/zk-settlement/src/exit.rs`, v1 is
/// native-asset only). Mutation target: drop the `message.asset != [0; 20]` check in `core::attempt_exit`.
#[tokio::test]
async fn an_unsupported_asset_is_refused_before_any_read_of_the_proof() {
    let (state_root, block) = fixture_state_root_and_block();
    let settlement = FakeSettlementReader::single(
        FakeRoot {
            head_final_batch: 5,
            challenge_window_slots: 100,
            chain_id: 200_101,
            exit_cap_per_window: AMPLE_CAP,
        },
        FakePending {
            state_root,
            last_block: block,
        },
        fixture_portal(),
    );
    let verifier = FakeVerifier::new(HashMap::new());
    let sender = FakeSender::panics_if_called();
    let mut message = fixture_message();
    message.asset = [0xAA; 20];

    let outcome = attempt_exit(
        &settlement,
        &verifier,
        &sender,
        message,
        &default_params(250, 4096),
    )
    .await
    .unwrap();

    assert_eq!(outcome, Outcome::Refused(Refusal::UnsupportedAsset));
    assert_eq!(verifier.calls.load(Ordering::SeqCst), 0);
    assert_eq!(sender.call_count(), 0);
}
