//! `run::poll_once`'s own RED tests: an `eth_getLogs` failure and a `get_slot`
//! failure must each skip the current poll — never crash the loop, never substitute a fabricated value —
//! and both are counted on `rome_zk_exit_rpc_errors_total{method}`.

use std::sync::atomic::{AtomicU32, Ordering};

use rome_zk_exit_prover::follower::{Follower, Pending, Wait};
use rome_zk_exit_prover::metrics::Metrics;
use rome_zk_exit_prover::rpc::{
    BlockByNumber, GetProofResult, LogEntry, VerifierError, VerifierRpc,
};
use rome_zk_exit_prover::run::poll_once;
use rome_zk_exit_prover::settlement::{ReadError, SettlementReader};
use rome_zk_layouts::exit::ExitMessage;
use rome_zk_solana_sender::{SendTuning, Sender, SenderError};
use solana_program::instruction::Instruction;
use solana_program::pubkey::Pubkey;

fn msg(nonce: u64) -> ExitMessage {
    ExitMessage {
        nonce,
        l2_sender: [0x33; 20],
        sol_recipient: [0x44; 32],
        asset: [0u8; 20],
        amount: 1_000,
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

/// A `SettlementReader` every one of whose methods panics — used where reaching ANY of them would itself
/// be the bug (both RED tests here refuse before touching settlement state at all: the first on
/// `eth_getLogs` failing before `read_slot` is ever called, the second exactly at `read_slot` failing
/// before `read_root`/`read_exit_config`/etc. are ever reached).
struct PanicsIfCalledSettlement;
impl SettlementReader for PanicsIfCalledSettlement {
    fn read_root(&self) -> Result<zk_settlement_client::RootAccount, ReadError> {
        panic!("read_root must never be called")
    }
    fn read_pending(&self, _batch: u64) -> Result<zk_settlement_client::PendingAccount, ReadError> {
        panic!("read_pending must never be called")
    }
    fn read_exit_config(&self) -> Result<zk_settlement_client::ExitConfigAccount, ReadError> {
        panic!("read_exit_config must never be called")
    }
    fn read_nullifier_page(
        &self,
        _page: u64,
    ) -> Result<Option<zk_settlement_client::NullifierPageAccount>, ReadError> {
        panic!("read_nullifier_page must never be called")
    }
    fn read_exit_window(
        &self,
        _window_index: u64,
    ) -> Result<Option<zk_settlement_client::ExitWindowAccount>, ReadError> {
        panic!("read_exit_window must never be called")
    }
    fn read_slot(&self) -> Result<u64, ReadError> {
        panic!("read_slot must never be called")
    }
}

/// Same as [`PanicsIfCalledSettlement`], except `read_slot` returns a scripted error instead of panicking
/// — the second RED test's own trigger.
struct FailingSlotSettlement;
impl SettlementReader for FailingSlotSettlement {
    fn read_root(&self) -> Result<zk_settlement_client::RootAccount, ReadError> {
        panic!("read_root must never be called once get_slot has already failed")
    }
    fn read_pending(&self, _batch: u64) -> Result<zk_settlement_client::PendingAccount, ReadError> {
        panic!("read_pending must never be called once get_slot has already failed")
    }
    fn read_exit_config(&self) -> Result<zk_settlement_client::ExitConfigAccount, ReadError> {
        panic!("read_exit_config must never be called once get_slot has already failed")
    }
    fn read_nullifier_page(
        &self,
        _page: u64,
    ) -> Result<Option<zk_settlement_client::NullifierPageAccount>, ReadError> {
        panic!("read_nullifier_page must never be called once get_slot has already failed")
    }
    fn read_exit_window(
        &self,
        _window_index: u64,
    ) -> Result<Option<zk_settlement_client::ExitWindowAccount>, ReadError> {
        panic!("read_exit_window must never be called once get_slot has already failed")
    }
    fn read_slot(&self) -> Result<u64, ReadError> {
        Err(ReadError::Rpc("connection reset".to_string()))
    }
}

struct FailingLogsVerifier;
impl VerifierRpc for FailingLogsVerifier {
    fn eth_get_logs(&self, _portal: &str, _from: u64) -> Result<Vec<LogEntry>, VerifierError> {
        Err(VerifierError::RpcError {
            method: "eth_getLogs",
            error: "connection reset".to_string(),
        })
    }
    fn eth_get_proof(
        &self,
        _address: &str,
        _slot: &str,
        _block: u64,
    ) -> Result<GetProofResult, VerifierError> {
        panic!("eth_getProof must never be called once eth_getLogs has already failed")
    }
    fn eth_get_block_by_number(&self, _block: u64) -> Result<BlockByNumber, VerifierError> {
        unimplemented!("not exercised by these RED tests")
    }
}

/// Returns an empty log set — the second RED test's own poll ingests nothing new; the pending entry it
/// asserts on is seeded directly into the `Follower` instead.
struct EmptyLogsVerifier;
impl VerifierRpc for EmptyLogsVerifier {
    fn eth_get_logs(&self, _portal: &str, _from: u64) -> Result<Vec<LogEntry>, VerifierError> {
        Ok(vec![])
    }
    fn eth_get_proof(
        &self,
        _address: &str,
        _slot: &str,
        _block: u64,
    ) -> Result<GetProofResult, VerifierError> {
        panic!("eth_getProof must never be called once get_slot has already failed")
    }
    fn eth_get_block_by_number(&self, _block: u64) -> Result<BlockByNumber, VerifierError> {
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
        panic!("send_and_confirm must never be called by either RED test in this file")
    }
}

// ---------------------------------------------------------------------------------------------
// RED tests
// ---------------------------------------------------------------------------------------------

/// RED: `a_get_logs_error_skips_the_poll_and_counts_it`. Mutation target: restore `?` on the
/// `eth_getLogs` call in `run::poll_once` — the failure would then propagate as a panicking `.unwrap()`
/// in this test's `poll_once(...).await` (no `Result` to unwrap any more, since `poll_once` returns a
/// plain `PollReport`) instead of being caught and reported.
#[tokio::test]
async fn a_get_logs_error_skips_the_poll_and_counts_it() {
    let settlement = PanicsIfCalledSettlement;
    let verifier = FailingLogsVerifier;
    let sender = FakeSender::panics_if_called();
    let mut follower = Follower::new(0, 5, 3);
    let metrics = Metrics::new();

    let report = poll_once(
        &settlement,
        &verifier,
        &sender,
        &mut follower,
        &metrics,
        "0x0000000000000000000000000000000000000000",
        &Pubkey::new_unique(),
        &Pubkey::new_unique(),
        4096,
        default_tuning(),
    )
    .await;

    assert!(
        report.logs_error,
        "the poll must report the eth_getLogs failure"
    );
    assert_eq!(report.attempted, 0);
    assert_eq!(sender.call_count(), 0);
    assert_eq!(
        metrics
            .exit_rpc_errors_total
            .with_label_values(&["eth_getLogs"])
            .get(),
        1,
        "the eth_getLogs failure must be counted"
    );
    assert!(follower.pending.is_empty(), "nothing was ever ingested");
}

/// RED: `a_slot_read_failure_skips_attempts_and_never_uses_slot_zero`. A `Wait::Now` pending message
/// (unconditionally due, regardless of slot) is seeded directly; if `get_slot`'s failure were papered over
/// with `now_slot = 0` (as the old bin's `.unwrap_or(0)` did) instead of skipping the round, `attempt_exit`
/// would run and reach the `Sender` — which panics if called. Mutation target: restore `.unwrap_or(0)`
/// (or any other fallback) on the `read_slot` call in `run::poll_once`.
#[tokio::test]
async fn a_slot_read_failure_skips_attempts_and_never_uses_slot_zero() {
    let message = msg(11);
    let hash = message.message_hash();

    let settlement = FailingSlotSettlement;
    let verifier = EmptyLogsVerifier;
    let sender = FakeSender::panics_if_called();
    let mut follower = Follower::new(0, 5, 3);
    follower.pending.insert(
        hash,
        Pending {
            message,
            seen_block: 0,
            wait: Wait::Now,
            send_attempts: 0,
            window_requeues: 0,
        },
    );
    let metrics = Metrics::new();

    let report = poll_once(
        &settlement,
        &verifier,
        &sender,
        &mut follower,
        &metrics,
        "0x0000000000000000000000000000000000000000",
        &Pubkey::new_unique(),
        &Pubkey::new_unique(),
        4096,
        default_tuning(),
    )
    .await;

    assert!(
        report.slot_error,
        "the poll must report the get_slot failure"
    );
    assert_eq!(
        report.attempted, 0,
        "no message may be attempted this poll — never against a fabricated slot"
    );
    assert_eq!(sender.call_count(), 0);
    assert_eq!(
        metrics
            .exit_rpc_errors_total
            .with_label_values(&["get_slot"])
            .get(),
        1,
        "the get_slot failure must be counted"
    );
    assert!(
        follower.pending.contains_key(&hash),
        "the pending message must be untouched — still due next poll, not silently dropped"
    );
    // `due()` alone was never re-evaluated with a fabricated slot: `Now` mocks the current absence of the
    // check by construction. What actually matters (`attempted == 0`, `sender.call_count() == 0`) is
    // already asserted above.
}

/// Same as [`FailingSlotSettlement`], except `read_slot` succeeds and `read_root` is the one returning a
/// scripted error — this test's own trigger (`read_root` was the one remaining `.unwrap_or(0)` in
/// `poll_once`).
struct FailingRootSettlement;
impl SettlementReader for FailingRootSettlement {
    fn read_root(&self) -> Result<zk_settlement_client::RootAccount, ReadError> {
        Err(ReadError::Rpc("connection reset".to_string()))
    }
    fn read_pending(&self, _batch: u64) -> Result<zk_settlement_client::PendingAccount, ReadError> {
        panic!("read_pending must never be called once read_root has failed")
    }
    fn read_exit_config(&self) -> Result<zk_settlement_client::ExitConfigAccount, ReadError> {
        panic!("read_exit_config must never be called once read_root has failed")
    }
    fn read_nullifier_page(
        &self,
        _page: u64,
    ) -> Result<Option<zk_settlement_client::NullifierPageAccount>, ReadError> {
        panic!("read_nullifier_page must never be called once read_root has failed")
    }
    fn read_exit_window(
        &self,
        _window_index: u64,
    ) -> Result<Option<zk_settlement_client::ExitWindowAccount>, ReadError> {
        panic!("read_exit_window must never be called once read_root has failed")
    }
    fn read_slot(&self) -> Result<u64, ReadError> {
        Ok(1_000)
    }
}

/// RED: `a_root_read_failure_skips_attempts_and_never_uses_head_final_zero`. A
/// `Wait::Now` message is due regardless of the Final root, so if `read_root`'s failure were papered over
/// with `head_final_batch = 0` (the old `.unwrap_or(0)`), `attempt_exit` would run — and `apply` would
/// record `seen_head_final: 0` for whatever it parks, making it due again on the very next poll. Mutation
/// target: restore `.unwrap_or(0)` on the `read_root` call in `run::poll_once`.
#[tokio::test]
async fn a_root_read_failure_skips_attempts_and_never_uses_head_final_zero() {
    let message = msg(12);
    let hash = message.message_hash();

    let settlement = FailingRootSettlement;
    let verifier = EmptyLogsVerifier;
    let sender = FakeSender::panics_if_called();
    let mut follower = Follower::new(0, 5, 3);
    follower.pending.insert(
        hash,
        Pending {
            message,
            seen_block: 0,
            wait: Wait::Now,
            send_attempts: 0,
            window_requeues: 0,
        },
    );
    let metrics = Metrics::new();

    let report = poll_once(
        &settlement,
        &verifier,
        &sender,
        &mut follower,
        &metrics,
        "0x0000000000000000000000000000000000000000",
        &Pubkey::new_unique(),
        &Pubkey::new_unique(),
        4096,
        default_tuning(),
    )
    .await;

    assert!(
        report.root_error,
        "the poll must report the read_root failure"
    );
    assert_eq!(
        report.attempted, 0,
        "no attempt against a fabricated head_final_batch"
    );
    assert_eq!(sender.call_count(), 0);
    assert_eq!(
        metrics
            .exit_rpc_errors_total
            .with_label_values(&["read_root"])
            .get(),
        1,
        "the read_root failure must be counted"
    );
    assert!(follower.pending.contains_key(&hash));
}

/// A settlement reader configured far enough for `attempt_exit` to reach the pre-send WINDOW check and no
/// further: root (Final batch 5, 100-slot windows, cap 1000 units), a set portal, a clear nullifier page,
/// and an `exit_window` the chain has not seen spent yet (`Ok(None)`). `read_pending` panics: reaching the
/// proof fetch means the follower's own-send accounting was NOT applied.
struct WindowSeamSettlement;
impl SettlementReader for WindowSeamSettlement {
    fn read_root(&self) -> Result<zk_settlement_client::RootAccount, ReadError> {
        Ok(zk_settlement_client::RootAccount {
            chain_id: 200_101,
            number: 0,
            parent_hash: [0; 32],
            state_root: [0; 32],
            block_hash: [0; 32],
            updates: 0,
            profile: 0,
            challenge_window_slots: 100,
            prove_window_slots: 0,
            proving_policy: 0,
            poster_bond: 0,
            exit_cap_per_window: 1_000,
            authority: Pubkey::new_unique(),
            head_pending_batch: 5,
            head_final_batch: 5,
            pending_count: 0,
            max_pending: 0,
        })
    }
    fn read_pending(&self, _batch: u64) -> Result<zk_settlement_client::PendingAccount, ReadError> {
        panic!("read_pending must never be reached: the follower's own sent_units fill this window")
    }
    fn read_exit_config(&self) -> Result<zk_settlement_client::ExitConfigAccount, ReadError> {
        Ok(zk_settlement_client::ExitConfigAccount {
            chain_id: 200_101,
            exit_portal: [0xAB; 20],
            bridge_program: Pubkey::new_unique(),
            pending_exit_portal: [0; 20],
            pending_bridge_program: Pubkey::new_unique(),
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
        Ok(None)
    }
    fn read_exit_window(
        &self,
        _window_index: u64,
    ) -> Result<Option<zk_settlement_client::ExitWindowAccount>, ReadError> {
        Ok(None) // the chain (at `finalized`) has not seen our sends yet
    }
    fn read_slot(&self) -> Result<u64, ReadError> {
        Ok(250) // window_index = 250 / 100 = 2
    }
}

/// RED: `poll_once_feeds_the_followers_own_sends_into_the_window_check` —
/// pins the SEAM between `Follower::sent_units_in`, `run::poll_once`'s `window_index`, and
/// `core::attempt_exit`'s `local_spent_units`: the follower already sent the whole 1000-unit cap in window
/// 2 (the chain still reads 0), so the next due message must queue to window 3 WITHOUT any proof fetch or
/// send. Mutation target: `local_spent_units: 0` (or a wrong `window_index`) in `poll_once`.
#[tokio::test]
async fn poll_once_feeds_the_followers_own_sends_into_the_window_check() {
    let message = msg(13); // amount 1_000 wei = 1 cap unit (rounded up)
    let hash = message.message_hash();

    let settlement = WindowSeamSettlement;
    let verifier = EmptyLogsVerifier;
    let sender = FakeSender::panics_if_called();
    let mut follower = Follower::new(0, 5, 3);
    follower.sent_units.insert(2, 1_000);
    follower.pending.insert(
        hash,
        Pending {
            message,
            seen_block: 0,
            wait: Wait::Now,
            send_attempts: 0,
            window_requeues: 0,
        },
    );
    let metrics = Metrics::new();

    let report = poll_once(
        &settlement,
        &verifier,
        &sender,
        &mut follower,
        &metrics,
        "0xabababababababababababababababababababab",
        &Pubkey::new_unique(),
        &Pubkey::new_unique(),
        4096,
        default_tuning(),
    )
    .await;

    assert_eq!(report.attempted, 1);
    assert_eq!(
        sender.call_count(),
        0,
        "our own sends alone must refuse locally"
    );
    assert_eq!(
        follower
            .pending
            .get(&hash)
            .map(|p| (p.wait, p.window_requeues)),
        Some((Wait::Slot(300), 1)),
        "queued to window 3 (retry at its first slot), one requeue counted"
    );
}
