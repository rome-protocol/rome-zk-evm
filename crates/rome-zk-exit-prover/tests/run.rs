//! `run::poll_once`'s own RED tests: an `eth_getLogs` failure and a `get_slot`
//! failure must each skip the current poll — never crash the loop, never substitute a fabricated value —
//! and both are counted on `rome_zk_exit_rpc_errors_total{method}`.

use std::sync::atomic::{AtomicU32, Ordering};

use rome_zk_exit_prover::follower::{Follower, Pending, Wait};
use rome_zk_exit_prover::metrics::Metrics;
use rome_zk_exit_prover::rpc::{
    BlockByNumber, GetProofResult, LogEntry, VerifierError, VerifierRpc,
};
use rome_zk_exit_prover::run::{exit_gate, exit_gate_with_tuning, poll_once, ExitGate};
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
    fn eth_block_number(&self) -> Result<u64, VerifierError> {
        Ok(1_000)
    }
    fn eth_get_logs(
        &self,
        _portal: &str,
        _from: u64,
        _to: u64,
    ) -> Result<Vec<LogEntry>, VerifierError> {
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
        10_000,
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
        10_000,
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
        10_000,
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
        10_000,
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

/// The same chain as `WindowSeamSettlement`, except that every nullifier bit is already set: every exit is already
/// proved.
struct AllProvedSettlement;
impl SettlementReader for AllProvedSettlement {
    fn read_root(&self) -> Result<zk_settlement_client::RootAccount, ReadError> {
        WindowSeamSettlement.read_root()
    }
    fn read_pending(&self, batch: u64) -> Result<zk_settlement_client::PendingAccount, ReadError> {
        WindowSeamSettlement.read_pending(batch)
    }
    fn read_exit_config(&self) -> Result<zk_settlement_client::ExitConfigAccount, ReadError> {
        WindowSeamSettlement.read_exit_config()
    }
    fn read_nullifier_page(
        &self,
        page: u64,
    ) -> Result<Option<zk_settlement_client::NullifierPageAccount>, ReadError> {
        Ok(Some(zk_settlement_client::NullifierPageAccount {
            chain_id: 200_101,
            page,
            bits: [0xFF; rome_zk_layouts::exit::exit_nullifier::BITS_LEN],
        }))
    }
    fn read_exit_window(
        &self,
        window_index: u64,
    ) -> Result<Option<zk_settlement_client::ExitWindowAccount>, ReadError> {
        WindowSeamSettlement.read_exit_window(window_index)
    }
    fn read_slot(&self) -> Result<u64, ReadError> {
        WindowSeamSettlement.read_slot()
    }
}

/// An exit found already proved is handed to the release step, marked as not sent by this run, and a message that is
/// merely queued is not.
#[tokio::test]
async fn poll_once_hands_an_already_proved_exit_to_the_release_step() {
    let proved = msg(13);
    let hash = proved.message_hash();
    let verifier = EmptyLogsVerifier;
    let sender = FakeSender::panics_if_called();
    let mut follower = Follower::new(0, 5, 3);
    follower.pending.insert(
        hash,
        Pending {
            message: proved,
            seen_block: 0,
            wait: Wait::Now,
            send_attempts: 0,
            window_requeues: 0,
        },
    );
    let metrics = Metrics::new();

    let report = poll_once(
        &AllProvedSettlement,
        &verifier,
        &sender,
        &mut follower,
        &metrics,
        "0xabababababababababababababababababababab",
        &Pubkey::new_unique(),
        &Pubkey::new_unique(),
        4096,
        default_tuning(),
        10_000,
    )
    .await;

    assert_eq!(
        report.proved,
        vec![rome_zk_exit_prover::release::ProvedExit {
            message_hash: hash,
            just_sent: false
        }]
    );
    assert_eq!(sender.call_count(), 0);
}

// ---------------------------------------------------------------------------------------------
// The log scan is done in chunks
// ---------------------------------------------------------------------------------------------

/// An L2 node at a given head that refuses any log range wider than its limit, as reth does, and holds one exit
/// log at `log_block`. It records every range it was asked for.
struct LimitedL2 {
    head: u64,
    limit: u64,
    log_block: u64,
    message: ExitMessage,
    fail_from: Option<u64>,
    ranges: std::sync::Mutex<Vec<(u64, u64)>>,
}

impl LimitedL2 {
    fn new(head: u64, limit: u64, log_block: u64) -> Self {
        Self {
            head,
            limit,
            log_block,
            message: msg(7),
            fail_from: None,
            ranges: std::sync::Mutex::new(Vec::new()),
        }
    }
    fn ranges(&self) -> Vec<(u64, u64)> {
        self.ranges.lock().unwrap().clone()
    }
}

impl VerifierRpc for LimitedL2 {
    fn eth_block_number(&self) -> Result<u64, VerifierError> {
        Ok(self.head)
    }
    fn eth_get_logs(
        &self,
        _portal: &str,
        from: u64,
        to: u64,
    ) -> Result<Vec<LogEntry>, VerifierError> {
        self.ranges.lock().unwrap().push((from, to));
        if to < from || to - from + 1 > self.limit || to > self.head {
            return Err(VerifierError::RpcError {
                method: "eth_getLogs",
                error: "query exceeds max block range".to_string(),
            });
        }
        if self.fail_from == Some(from) {
            return Err(VerifierError::RpcError {
                method: "eth_getLogs",
                error: "connection reset".to_string(),
            });
        }
        if (from..=to).contains(&self.log_block) {
            return Ok(vec![LogEntry {
                address: "0x0000000000000000000000000000000000000000".to_string(),
                topics: vec![
                    "0x0000000000000000000000000000000000000000000000000000000000000000"
                        .to_string(),
                    format!("0x{}", hex::encode(self.message.message_hash())),
                ],
                data: format!("0x{}", hex::encode(self.message.message_preimage())),
                block_number: format!("0x{:x}", self.log_block),
                transaction_hash: "0x00".to_string(),
                log_index: "0x0".to_string(),
            }]);
        }
        Ok(vec![])
    }
    fn eth_get_proof(
        &self,
        _address: &str,
        _slot: &str,
        _block: u64,
    ) -> Result<GetProofResult, VerifierError> {
        unimplemented!("not exercised by the scan tests")
    }
    fn eth_get_block_by_number(&self, _block: u64) -> Result<BlockByNumber, VerifierError> {
        unimplemented!("not exercised by the scan tests")
    }
}

/// A chain far longer than the node's range limit is scanned in pieces the node accepts, the exit in the middle
/// one is found, and the cursor ends one past the head even though the last pieces held no log.
#[tokio::test]
async fn a_long_chain_is_scanned_in_chunks_the_node_accepts() {
    let verifier = LimitedL2::new(25_000, 10_000, 12_345);
    let mut follower = Follower::new(0, 5, 3);
    let metrics = Metrics::new();
    let report = poll_once(
        &FailingSlotSettlement,
        &verifier,
        &FakeSender::panics_if_called(),
        &mut follower,
        &metrics,
        "0x0000000000000000000000000000000000000000",
        &Pubkey::new_unique(),
        &Pubkey::new_unique(),
        4096,
        default_tuning(),
        10_000,
    )
    .await;
    assert!(!report.logs_error);
    assert_eq!(
        verifier.ranges(),
        vec![(0, 9_999), (10_000, 19_999), (20_000, 25_000)]
    );
    assert_eq!(follower.pending.len(), 1, "the exit in the middle chunk");
    assert_eq!(report.ingest.added, 1);
    assert_eq!(follower.scan_from_block, 25_001);
}

/// A chunk that fails leaves the cursor at the end of the last chunk that worked, so the next poll asks for the same
/// stretch again and nothing is skipped.
#[tokio::test]
async fn a_failed_chunk_leaves_the_cursor_at_the_last_good_chunk() {
    let mut verifier = LimitedL2::new(25_000, 10_000, 3);
    verifier.fail_from = Some(10_000);
    let mut follower = Follower::new(0, 5, 3);
    let metrics = Metrics::new();
    let report = poll_once(
        &FailingSlotSettlement,
        &verifier,
        &FakeSender::panics_if_called(),
        &mut follower,
        &metrics,
        "0x0000000000000000000000000000000000000000",
        &Pubkey::new_unique(),
        &Pubkey::new_unique(),
        4096,
        default_tuning(),
        10_000,
    )
    .await;
    assert!(report.logs_error);
    assert_eq!(follower.scan_from_block, 10_000);
    assert_eq!(follower.pending.len(), 1, "the first chunk's exit is kept");
    assert!(String::from_utf8(metrics.render())
        .unwrap()
        .contains(r#"method="eth_getLogs""#));
}

/// A cursor already past the head asks the node for nothing.
#[tokio::test]
async fn a_cursor_past_the_head_asks_for_no_logs() {
    let verifier = LimitedL2::new(500, 10_000, 3);
    let mut follower = Follower::new(501, 5, 3);
    let metrics = Metrics::new();
    poll_once(
        &FailingSlotSettlement,
        &verifier,
        &FakeSender::panics_if_called(),
        &mut follower,
        &metrics,
        "0x0000000000000000000000000000000000000000",
        &Pubkey::new_unique(),
        &Pubkey::new_unique(),
        4096,
        default_tuning(),
        10_000,
    )
    .await;
    assert!(verifier.ranges().is_empty(), "{:?}", verifier.ranges());
    assert_eq!(follower.scan_from_block, 501);
}

// ---------------------------------------------------------------------------------------------
// The exit gate's gauges
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum GateMode {
    Active,
    NoExitConfig,
    ConfigReadFails,
    RootReadFails,
}

/// A settlement reader whose exit config and root answer as the current mode says.
struct GateSettlement {
    mode: std::sync::Mutex<GateMode>,
}

impl GateSettlement {
    fn new(mode: GateMode) -> Self {
        Self {
            mode: std::sync::Mutex::new(mode),
        }
    }
    fn set(&self, mode: GateMode) {
        *self.mode.lock().unwrap() = mode;
    }
    fn mode(&self) -> GateMode {
        *self.mode.lock().unwrap()
    }
}

const GATE_PORTAL: [u8; 20] = [0x42; 20];

impl SettlementReader for GateSettlement {
    fn read_root(&self) -> Result<zk_settlement_client::RootAccount, ReadError> {
        if matches!(self.mode(), GateMode::RootReadFails) {
            return Err(ReadError::Rpc("connection reset".to_string()));
        }
        let data = rome_zk_layouts::root::write(&rome_zk_layouts::root::RootFields {
            chain_id: 1,
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
            authority: [0; 32],
            head_pending_batch: 0,
            head_final_batch: 0,
            pending_count: 0,
            max_pending: 0,
        });
        Ok(zk_settlement_client::decode_root_account(&data).unwrap())
    }
    fn read_pending(&self, _batch: u64) -> Result<zk_settlement_client::PendingAccount, ReadError> {
        unimplemented!()
    }
    fn read_exit_config(&self) -> Result<zk_settlement_client::ExitConfigAccount, ReadError> {
        match self.mode() {
            GateMode::NoExitConfig => Err(ReadError::NotFound(Pubkey::new_unique())),
            GateMode::ConfigReadFails => Err(ReadError::Rpc("connection reset".to_string())),
            _ => Ok(exit_config_with_portal(GATE_PORTAL)),
        }
    }
    fn read_nullifier_page(
        &self,
        _page: u64,
    ) -> Result<Option<zk_settlement_client::NullifierPageAccount>, ReadError> {
        unimplemented!()
    }
    fn read_exit_window(
        &self,
        _window_index: u64,
    ) -> Result<Option<zk_settlement_client::ExitWindowAccount>, ReadError> {
        unimplemented!()
    }
    fn read_slot(&self) -> Result<u64, ReadError> {
        Ok(1)
    }
}

fn exit_config_with_portal(portal: [u8; 20]) -> zk_settlement_client::ExitConfigAccount {
    let data = rome_zk_layouts::exit::exit_config::write(
        &rome_zk_layouts::exit::exit_config::ExitConfigFields {
            chain_id: 1,
            exit_portal: portal,
            bridge_program: [3; 32],
            pending_exit_portal: [0; 20],
            pending_bridge_program: [0; 32],
            pending_exit_cap: 0,
            pending_poster_bond: 0,
            activation_slot: 0,
            pending_mask: 0,
        },
    );
    zk_settlement_client::decode_exit_config_account(&data).unwrap()
}

/// A failed read says nothing about whether exits are on: the active gauge keeps its last value and the read gauge
/// drops to zero, so a check can tell "cannot read the chain" from "exits are off".
#[test]
fn a_failed_gate_read_leaves_the_active_gauge_and_drops_the_read_gauge() {
    let settlement = GateSettlement::new(GateMode::Active);
    let metrics = Metrics::new();
    assert!(matches!(
        exit_gate(&settlement, &metrics),
        ExitGate::Active { .. }
    ));
    assert_eq!(metrics.exit_active.get(), 1);
    assert_eq!(metrics.exit_gate_read_ok.get(), 1);

    for failing in [GateMode::ConfigReadFails, GateMode::RootReadFails] {
        settlement.set(failing);
        assert!(matches!(
            exit_gate(&settlement, &metrics),
            ExitGate::Idle(_)
        ));
        assert_eq!(
            metrics.exit_active.get(),
            1,
            "active kept at its last value"
        );
        assert_eq!(metrics.exit_gate_read_ok.get(), 0);
    }

    settlement.set(GateMode::Active);
    exit_gate(&settlement, &metrics);
    assert_eq!(
        metrics.exit_gate_read_ok.get(),
        1,
        "recovers with the next good read"
    );
}

/// The program accounts are part of what the prover needs from the chain: when they cannot be read the process is
/// idle, and the read gauge must say so even though the gate reads worked.
#[test]
fn unreadable_program_accounts_drop_the_read_gauge_too() {
    let settlement = GateSettlement::new(GateMode::Active);
    let metrics = Metrics::new();
    let tuning = SendTuning::default();

    let gate = exit_gate_with_tuning(&settlement, Some(&tuning), &metrics);
    assert!(matches!(gate, ExitGate::Active { .. }));
    assert_eq!(metrics.exit_gate_read_ok.get(), 1);

    let gate = exit_gate_with_tuning(&settlement, None, &metrics);
    assert!(
        matches!(gate, ExitGate::Active { .. }),
        "the gate itself is unchanged"
    );
    assert_eq!(metrics.exit_active.get(), 1);
    assert_eq!(metrics.exit_gate_read_ok.get(), 0);

    exit_gate_with_tuning(&settlement, Some(&tuning), &metrics);
    assert_eq!(
        metrics.exit_gate_read_ok.get(),
        1,
        "recovers with the next good read"
    );
}

/// A chain with no exit config is a good read that says exits are off.
#[test]
fn a_missing_exit_config_is_a_good_read_with_exits_off() {
    let settlement = GateSettlement::new(GateMode::Active);
    let metrics = Metrics::new();
    exit_gate(&settlement, &metrics);
    settlement.set(GateMode::NoExitConfig);
    assert!(matches!(
        exit_gate(&settlement, &metrics),
        ExitGate::Idle(_)
    ));
    assert_eq!(metrics.exit_active.get(), 0);
    assert_eq!(metrics.exit_gate_read_ok.get(), 1);
}

/// The gauge shows up on the metrics page under its agreed name.
#[test]
fn the_read_gauge_is_rendered() {
    let metrics = Metrics::new();
    let text = String::from_utf8(metrics.render()).unwrap();
    assert!(text.contains("rome_zk_exit_gate_read_ok"), "{text}");
}

// ---------------------------------------------------------------------------------------------
// A portal that changes while the prover runs
// ---------------------------------------------------------------------------------------------

async fn poll_portal(
    verifier: &LimitedL2,
    follower: &mut Follower,
    portal_hex: &str,
) -> rome_zk_exit_prover::run::PollReport {
    poll_once(
        &FailingSlotSettlement,
        verifier,
        &FakeSender::panics_if_called(),
        follower,
        &Metrics::new(),
        portal_hex,
        &Pubkey::new_unique(),
        &Pubkey::new_unique(),
        4096,
        default_tuning(),
        10_000,
    )
    .await
}

const PORTAL_A: &str = "0x4242424242424242424242424242424242424242";
const PORTAL_B: &str = "0x4343434343434343434343434343434343434343";

/// When the exit config names a different portal, the follower starts over: the cursor goes back to the configured
/// start block and what it was waiting on (all from the old portal) is dropped, so the new portal's logs are scanned
/// from the start.
#[tokio::test]
async fn a_new_portal_resets_the_cursor_and_drops_what_was_pending() {
    let verifier = LimitedL2::new(500, 10_000, 300);
    let mut follower = Follower::new(100, 5, 3);
    poll_portal(&verifier, &mut follower, PORTAL_A).await;
    assert_eq!(follower.scan_from_block, 501);
    let stale = msg(99);
    follower.pending.insert(
        stale.message_hash(),
        Pending {
            message: stale,
            seen_block: 5,
            wait: Wait::Now,
            send_attempts: 0,
            window_requeues: 0,
        },
    );

    poll_portal(&verifier, &mut follower, PORTAL_B).await;

    assert!(
        !follower.pending.contains_key(&stale.message_hash()),
        "a message from the old portal survived"
    );
    assert_eq!(
        verifier.ranges(),
        vec![(100, 500), (100, 500)],
        "the new portal was not scanned from the start block"
    );
    assert_eq!(follower.scan_from_block, 501);
    assert_eq!(follower.pending.len(), 1, "the new portal's own log");
}

/// The same portal on the next poll changes nothing: the cursor stays and nothing is scanned twice.
#[tokio::test]
async fn the_same_portal_keeps_the_cursor() {
    let verifier = LimitedL2::new(500, 10_000, 300);
    let mut follower = Follower::new(100, 5, 3);
    poll_portal(&verifier, &mut follower, PORTAL_A).await;
    poll_portal(&verifier, &mut follower, PORTAL_A).await;
    assert_eq!(verifier.ranges(), vec![(100, 500)]);
    assert_eq!(follower.pending.len(), 1);
}

/// Both RPC methods `poll_once` counts show up as their own lines, so a check can add them up.
#[test]
fn both_rpc_error_methods_are_rendered() {
    let metrics = Metrics::new();
    metrics.record_rpc_error("eth_blockNumber");
    metrics.record_rpc_error("eth_getLogs");
    let text = String::from_utf8(metrics.render()).unwrap();
    assert!(text.contains(r#"method="eth_blockNumber""#), "{text}");
    assert!(text.contains(r#"method="eth_getLogs""#), "{text}");
}
