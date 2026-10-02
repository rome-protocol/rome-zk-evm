//! RED tests: the pre-send nullifier check in `core::attempt_exit` (the first test) and the full stateful
//! wire loop `ingest → due → attempt_exit → apply` after a restart from genesis (the second test). The
//! [`rome_zk_exit_prover::follower::Follower`] state machine's own pure tests
//! (ingest/due/apply against hand-built `Outcome`/`CoreError` values, no `attempt_exit`) live in
//! `src/follower.rs`'s own `#[cfg(test)]` module — this file is the two tests that need a real
//! `attempt_exit` call: `already_proved_bit_refuses_before_the_sender_is_touched` and
//! `restart_from_genesis_sends_nothing_for_a_set_nullifier_bit`.

use std::sync::atomic::{AtomicU32, Ordering};

use rome_zk_exit_prover::core::{attempt_exit, AttemptParams, Outcome};
use rome_zk_exit_prover::follower::{Follower, Now};
use rome_zk_exit_prover::rpc::{GetProofResult, LogEntry, VerifierError, VerifierRpc};
use rome_zk_exit_prover::settlement::{ReadError, SettlementReader};
use rome_zk_layouts::exit::{exit_nullifier, nullifier_page, set_bit, ExitMessage};
use rome_zk_solana_sender::{SendTuning, Sender, SenderError};
use serde::Deserialize;
use solana_program::instruction::Instruction;
use solana_program::pubkey::Pubkey;

const MESSAGE_HASH_JSON: &str = include_str!("../../../fixtures/exit/message_hash.json");

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

/// A fully-set `exit_nullifier` page account (every bit set, including `nonce`'s) for `chain_id`/`page` —
/// the "this exit (and every other on the page) was already proved" fixture both RED tests need.
fn nullifier_page_account_with_bit_set(
    chain_id: u64,
    nonce: u64,
) -> zk_settlement_client::NullifierPageAccount {
    let page = nullifier_page(nonce);
    let mut bits = [0u8; exit_nullifier::BITS_LEN];
    set_bit(&mut bits, page, nonce).unwrap();
    zk_settlement_client::NullifierPageAccount {
        chain_id,
        page,
        bits,
    }
}

// ---------------------------------------------------------------------------------------------
// Fakes (a trimmed copy of tests/attempt_exit.rs's own shapes, plus nullifier-page control and a
// panic-if-called guard on `read_pending` — proving the nullifier check runs strictly BEFORE it).
// ---------------------------------------------------------------------------------------------

struct FakeSettlementReader {
    chain_id: u64,
    head_final_batch: u64,
    challenge_window_slots: u64,
    exit_cap_per_window: u64,
    exit_portal: [u8; 20],
    nullifier_page: Option<zk_settlement_client::NullifierPageAccount>,
    /// When true, `read_pending` panics instead of returning — proves a caller never reaches it (used
    /// only by the already-proved test, where reaching `read_pending` would itself be the bug).
    panic_on_read_pending: bool,
}

impl SettlementReader for FakeSettlementReader {
    fn read_root(&self) -> Result<zk_settlement_client::RootAccount, ReadError> {
        Ok(zk_settlement_client::RootAccount {
            chain_id: self.chain_id,
            number: 0,
            parent_hash: [0; 32],
            state_root: [0; 32],
            block_hash: [0; 32],
            updates: 0,
            profile: 0,
            challenge_window_slots: self.challenge_window_slots as u32,
            prove_window_slots: 0,
            proving_policy: 0,
            poster_bond: 0,
            exit_cap_per_window: self.exit_cap_per_window,
            authority: Pubkey::new_unique(),
            head_pending_batch: self.head_final_batch,
            head_final_batch: self.head_final_batch,
            pending_count: 0,
            max_pending: 0,
        })
    }

    fn read_pending(&self, _batch: u64) -> Result<zk_settlement_client::PendingAccount, ReadError> {
        if self.panic_on_read_pending {
            panic!("read_pending must never be called once the nullifier bit is already set");
        }
        unimplemented!("not exercised by these RED tests")
    }

    fn read_exit_config(&self) -> Result<zk_settlement_client::ExitConfigAccount, ReadError> {
        Ok(zk_settlement_client::ExitConfigAccount {
            chain_id: self.chain_id,
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

    fn read_nullifier_page(
        &self,
        _page: u64,
    ) -> Result<Option<zk_settlement_client::NullifierPageAccount>, ReadError> {
        Ok(self.nullifier_page)
    }

    fn read_exit_window(
        &self,
        _window_index: u64,
    ) -> Result<Option<zk_settlement_client::ExitWindowAccount>, ReadError> {
        // Both RED tests in this file short-circuit at `Outcome::AlreadyProved` (the nullifier bit is
        // already set), well before `core::attempt_exit` would ever reach the cap pre-checks — an absent
        // window (0 spent) is a correct, unreached default either way.
        Ok(None)
    }

    fn read_slot(&self) -> Result<u64, ReadError> {
        unimplemented!("not exercised by attempt_exit — read_slot is a run::poll_once-only read")
    }
}

struct FakeVerifier {
    calls: AtomicU32,
}
impl FakeVerifier {
    fn never_called() -> Self {
        Self {
            calls: AtomicU32::new(0),
        }
    }
}
impl VerifierRpc for FakeVerifier {
    fn eth_get_logs(&self, _portal: &str, _from: u64) -> Result<Vec<LogEntry>, VerifierError> {
        Ok(vec![])
    }
    fn eth_get_proof(
        &self,
        _address: &str,
        _slot: &str,
        _block: u64,
    ) -> Result<GetProofResult, VerifierError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        panic!("eth_getProof must never be called once the nullifier bit is already set")
    }
    fn eth_get_block_by_number(
        &self,
        _block: u64,
    ) -> Result<rome_zk_exit_prover::rpc::BlockByNumber, VerifierError> {
        unimplemented!("not exercised by these RED tests")
    }
}

struct FakeSender {
    calls: AtomicU32,
}
impl FakeSender {
    fn panics_if_called() -> Self {
        Self {
            calls: AtomicU32::new(0),
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
        panic!("send_and_confirm must never be called once the nullifier bit is already set")
    }
}

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

/// RED: `already_proved_bit_refuses_before_the_sender_is_touched`. Dropping the pre-send nullifier read in
/// `core::attempt_exit` makes this test fail: `read_pending` panics (it is reached because the gate that
/// should have stopped things first is gone).
#[tokio::test]
async fn already_proved_bit_refuses_before_the_sender_is_touched() {
    let message = fixture_message();
    let chain_id = 200_101;
    let settlement = FakeSettlementReader {
        chain_id,
        head_final_batch: 5,
        challenge_window_slots: 100,
        exit_cap_per_window: 1_000,
        exit_portal: fixture_portal(),
        nullifier_page: Some(nullifier_page_account_with_bit_set(chain_id, message.nonce)),
        panic_on_read_pending: true,
    };
    let verifier = FakeVerifier::never_called();
    let sender = FakeSender::panics_if_called();

    let outcome = attempt_exit(
        &settlement,
        &verifier,
        &sender,
        message,
        &default_params(1000, 4096),
    )
    .await
    .unwrap();

    assert_eq!(outcome, Outcome::AlreadyProved);
    assert_eq!(
        verifier.calls.load(Ordering::SeqCst),
        0,
        "eth_getProof must never run once the nullifier bit is already set"
    );
    assert_eq!(
        sender.call_count(),
        0,
        "a doomed proof must never spend a fee"
    );
}

/// RED: `restart_from_genesis_sends_nothing_for_a_set_nullifier_bit`. A fresh `Follower`
/// (`portal_from_block = 0`, as after a restart) ingests one historical `ExitInitiated` log for an exit
/// that was ALREADY proved before this run started (the on-chain nullifier bit is set); the full
/// `ingest → due → attempt_exit → apply` wire proves it as `AlreadyProved` and ends with the follower
/// tracking nothing at all — a restart from genesis costs one `eth_getLogs` + one nullifier-page read,
/// zero fees. Mutation target: same as the first test (drop the nullifier read) — this test would
/// additionally fail on `sender.call_count() == 0`.
#[tokio::test]
async fn restart_from_genesis_sends_nothing_for_a_set_nullifier_bit() {
    let message = fixture_message();
    let chain_id = 200_101;
    let log = LogEntry {
        address: "0x0000000000000000000000000000000000000000".to_string(),
        topics: vec![
            "0x0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            format!("0x{}", hex::encode(message.message_hash())),
        ],
        data: format!("0x{}", hex::encode(message.message_preimage())),
        block_number: "0x2".to_string(),
        transaction_hash: "0x00".to_string(),
        log_index: "0x0".to_string(),
    };

    let mut follower = Follower::new(0, 5, 3);
    let report = follower.ingest(&[log]);
    assert_eq!(report.added, 1);
    assert_eq!(follower.pending.len(), 1);

    let settlement = FakeSettlementReader {
        chain_id,
        head_final_batch: 5,
        challenge_window_slots: 100,
        exit_cap_per_window: 1_000,
        exit_portal: fixture_portal(),
        nullifier_page: Some(nullifier_page_account_with_bit_set(chain_id, message.nonce)),
        panic_on_read_pending: true,
    };
    let verifier = FakeVerifier::never_called();
    let sender = FakeSender::panics_if_called();

    let now = Now {
        slot: 1000,
        head_final_batch: 5,
    };
    let due = follower.due(now);
    assert_eq!(due, vec![message.message_hash()]);

    for hash in due {
        let params = default_params(now.slot, 4096);
        let outcome = attempt_exit(&settlement, &verifier, &sender, message, &params).await;
        assert_eq!(outcome.as_ref().unwrap(), &Outcome::AlreadyProved);
        follower.apply(hash, now, outcome);
    }

    assert!(
        follower.pending.is_empty(),
        "an already-proved exit must be fully forgotten, not retried"
    );
    assert!(follower.stuck.is_empty());
    assert_eq!(
        sender.call_count(),
        0,
        "a restart must never re-send an already-proved exit"
    );
}
