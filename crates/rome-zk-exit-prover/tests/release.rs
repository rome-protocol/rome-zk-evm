//! The release step: once an exit is proved, the exit prover pays the user out with the same instructions the
//! operator's `release-exit` command sends. Every test here runs against fakes, with no network.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rome_zk_exit_prover::metrics::Metrics;
use rome_zk_exit_prover::release::{
    release_loaded_accounts_limit, release_one, AccountReader, ProvedExit, ReleaseContext,
    ReleaseOutcome, ReleaseRefusal, Releaser, MAX_RELEASE_FAILURES, SENT_GRACE_POLLS,
};
use rome_zk_exit_prover::settlement::{ReadError, SettlementReader};
use rome_zk_solana_sender::{SendTuning, Sender, SenderError};
use solana_program::instruction::Instruction;
use solana_program::pubkey::Pubkey;

const CHAIN_ID: u64 = 7;
const HASH: [u8; 32] = [9; 32];
const MIN: u64 = 10_000_000;

fn settlement() -> Pubkey {
    Pubkey::new_from_array([1; 32])
}
fn bridge() -> Pubkey {
    Pubkey::new_from_array([2; 32])
}
fn mint() -> Pubkey {
    Pubkey::new_from_array([3; 32])
}
fn recipient() -> Pubkey {
    Pubkey::new_from_array([4; 32])
}
fn payer() -> Pubkey {
    Pubkey::new_from_array([5; 32])
}
fn record_payer() -> Pubkey {
    payer()
}

fn tuning() -> SendTuning {
    SendTuning {
        compute_unit_limit: 150_000,
        loaded_accounts_data_size_limit: 64 * 1024,
        priority_fee_micro_lamports: 0,
        max_priority_fee_micro_lamports: 0,
        confirm_timeout: std::time::Duration::from_secs(1),
        ..Default::default()
    }
}

fn ctx(min: u64) -> ReleaseContext {
    ReleaseContext {
        program_id: settlement(),
        chain_id: CHAIN_ID,
        payer: payer(),
        create_account_min_lamports: min,
    }
}

/// Accounts and the exit config, served from memory. `fail_reads` makes every account read fail.
struct Chain {
    accounts: Mutex<HashMap<Pubkey, Vec<u8>>>,
    fail_reads: bool,
    reads: AtomicU32,
    /// Batched reads: how many calls, and the most keys one call asked for.
    batch_calls: AtomicU32,
    max_batch: AtomicU32,
}

impl Chain {
    fn new() -> Self {
        Self {
            accounts: Mutex::new(HashMap::new()),
            fail_reads: false,
            reads: AtomicU32::new(0),
            batch_calls: AtomicU32::new(0),
            max_batch: AtomicU32::new(0),
        }
    }
    fn put(&self, key: Pubkey, data: Vec<u8>) {
        self.accounts.lock().unwrap().insert(key, data);
    }
    fn remove(&self, key: &Pubkey) {
        self.accounts.lock().unwrap().remove(key);
    }
    fn read_count(&self) -> u32 {
        self.reads.load(Ordering::SeqCst)
    }
}

impl AccountReader for Chain {
    fn read_account(&self, key: &Pubkey) -> Result<Option<Vec<u8>>, ReadError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if self.fail_reads {
            return Err(ReadError::Rpc("down".into()));
        }
        Ok(self.accounts.lock().unwrap().get(key).cloned())
    }
    fn read_accounts(&self, keys: &[Pubkey]) -> Result<Vec<Option<Vec<u8>>>, ReadError> {
        self.batch_calls.fetch_add(1, Ordering::SeqCst);
        self.max_batch
            .fetch_max(keys.len() as u32, Ordering::SeqCst);
        self.reads.fetch_add(keys.len() as u32, Ordering::SeqCst);
        if self.fail_reads {
            return Err(ReadError::Rpc("down".into()));
        }
        let accounts = self.accounts.lock().unwrap();
        Ok(keys.iter().map(|k| accounts.get(k).cloned()).collect())
    }
}

impl Chain {
    fn batch_calls(&self) -> u32 {
        self.batch_calls.load(Ordering::SeqCst)
    }
}

impl SettlementReader for Chain {
    fn read_root(&self) -> Result<zk_settlement_client::RootAccount, ReadError> {
        unimplemented!("a release never reads the root")
    }
    fn read_pending(&self, _: u64) -> Result<zk_settlement_client::PendingAccount, ReadError> {
        unimplemented!("a release never reads a pending batch")
    }
    fn read_exit_config(&self) -> Result<zk_settlement_client::ExitConfigAccount, ReadError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if self.fail_reads {
            return Err(ReadError::Rpc("down".into()));
        }
        Ok(zk_settlement_client::ExitConfigAccount {
            chain_id: CHAIN_ID,
            exit_portal: [0xaa; 20],
            bridge_program: bridge(),
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
        _: u64,
    ) -> Result<Option<zk_settlement_client::NullifierPageAccount>, ReadError> {
        unimplemented!("a release never reads a nullifier page")
    }
    fn read_exit_window(
        &self,
        _: u64,
    ) -> Result<Option<zk_settlement_client::ExitWindowAccount>, ReadError> {
        unimplemented!("a release never reads a window")
    }
    fn read_slot(&self) -> Result<u64, ReadError> {
        unimplemented!("a release never reads the slot")
    }
}

/// Records every instruction list it is asked to send. `fail` makes the send fail on chain.
struct Recorder {
    sent: Mutex<Vec<(Vec<Instruction>, SendTuning)>>,
    fail: bool,
}

impl Recorder {
    fn ok() -> Self {
        Self {
            sent: Mutex::new(vec![]),
            fail: false,
        }
    }
    fn failing() -> Self {
        Self {
            sent: Mutex::new(vec![]),
            fail: true,
        }
    }
    fn count(&self) -> usize {
        self.sent.lock().unwrap().len()
    }
}

impl Sender for Recorder {
    async fn send_and_confirm(
        &self,
        instructions: &[Instruction],
        tuning: SendTuning,
    ) -> Result<solana_signature::Signature, SenderError> {
        self.sent
            .lock()
            .unwrap()
            .push((instructions.to_vec(), tuning));
        if self.fail {
            return Err(SenderError::StepFailed {
                frame_index: 0,
                stage_index: 0,
                tx_index: 0,
                err: solana_transaction_error::TransactionError::AccountNotFound,
            });
        }
        Ok(solana_signature::Signature::default())
    }
}

fn record_pda() -> Pubkey {
    zk_settlement_client::exit_record_pda(&settlement(), CHAIN_ID, HASH).0
}

fn record_bytes(status: u8, amount_wei: u128) -> Vec<u8> {
    rome_zk_layouts::exit::exit_record::write(
        &rome_zk_layouts::exit::exit_record::ExitRecordFields {
            chain_id: CHAIN_ID,
            batch: 1,
            message_hash: HASH,
            sol_recipient: recipient().to_bytes(),
            amount: amount_wei,
            window_index: 0,
            proved_slot: 10,
            status,
            payer: record_payer().to_bytes(),
            asset: [0; 20],
        },
    )
    .to_vec()
}

fn vault_bytes(settlement_program: Pubkey, decimals: u8) -> Vec<u8> {
    zk_bridge::state::vault_config::write(&zk_bridge::state::vault_config::VaultConfigFields {
        chain_id: CHAIN_ID,
        settlement_program,
        mint: mint(),
        mint_decimals: decimals,
        authority: Pubkey::new_from_array([6; 32]),
    })
    .to_vec()
}

fn vault_pda() -> Pubkey {
    zk_bridge_client::vault_config_pda(&bridge(), &settlement(), CHAIN_ID).0
}

fn ata() -> Pubkey {
    zk_bridge_client::recipient_ata(&recipient(), &mint())
}

/// A chain holding a PROVED record worth `lamports` (wrapped SOL has nine decimals, the portal counts in wei), the
/// vault, and, if `with_token_account`, the recipient's token account.
fn chain_with(lamports: u128, with_token_account: bool) -> Chain {
    let chain = Chain::new();
    chain.put(
        record_pda(),
        record_bytes(
            rome_zk_layouts::exit::exit_record::STATUS_PROVED,
            lamports * 1_000_000_000,
        ),
    );
    chain.put(vault_pda(), vault_bytes(settlement(), 9));
    if with_token_account {
        chain.put(ata(), vec![0u8; 165]);
    }
    chain
}

fn expected_release_ix() -> Instruction {
    zk_bridge_client::release_exit_ix(
        &bridge(),
        &settlement(),
        CHAIN_ID,
        HASH,
        &mint(),
        &zk_settlement_client::exit_config_pda(&settlement(), CHAIN_ID).0,
        &record_pda(),
        &record_payer(),
        &recipient(),
    )
}

#[tokio::test]
async fn an_open_record_is_released_with_the_instruction_the_operator_command_builds() {
    let chain = chain_with(50_000_000, true);
    let sender = Recorder::ok();

    let out = release_one(&chain, &sender, &ctx(MIN), HASH, tuning()).await;

    assert_eq!(out, ReleaseOutcome::Released);
    let sent = sender.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].0,
        vec![expected_release_ix()],
        "the token account exists, so only the release goes out"
    );
}

#[tokio::test]
async fn a_missing_token_account_is_created_in_the_same_transaction_when_the_payout_is_large_enough(
) {
    let chain = chain_with(MIN as u128, false);
    let sender = Recorder::ok();

    let out = release_one(&chain, &sender, &ctx(MIN), HASH, tuning()).await;

    assert_eq!(out, ReleaseOutcome::Released);
    let sent = sender.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].0,
        vec![
            zk_bridge_client::create_recipient_ata_idempotent_ix(&payer(), &recipient(), &mint()),
            expected_release_ix()
        ],
        "a payout exactly at the minimum is released, and the exit payer funds the token account"
    );
}

#[tokio::test]
async fn a_payout_below_the_minimum_to_a_recipient_without_a_token_account_waits() {
    let chain = chain_with(MIN as u128 - 1, false);
    let sender = Recorder::ok();

    let out = release_one(&chain, &sender, &ctx(MIN), HASH, tuning()).await;

    assert_eq!(
        out,
        ReleaseOutcome::Waiting {
            payout: MIN as u128 - 1
        }
    );
    assert_eq!(sender.count(), 0, "nothing is sent, so no rent is spent");
}

#[tokio::test]
async fn a_small_payout_to_a_recipient_with_a_token_account_is_released() {
    let chain = chain_with(1, true);
    let sender = Recorder::ok();

    let out = release_one(&chain, &sender, &ctx(MIN), HASH, tuning()).await;

    assert_eq!(out, ReleaseOutcome::Released);
    assert_eq!(
        sender.sent.lock().unwrap()[0].0,
        vec![expected_release_ix()]
    );
}

#[tokio::test]
async fn a_closed_record_means_someone_else_released_it() {
    let chain = chain_with(50_000_000, true);
    chain.remove(&record_pda());
    let sender = Recorder::ok();

    let out = release_one(&chain, &sender, &ctx(MIN), HASH, tuning()).await;

    assert_eq!(out, ReleaseOutcome::Gone);
    assert_eq!(sender.count(), 0);
}

#[tokio::test]
async fn a_record_that_is_not_proved_is_refused_by_name() {
    let chain = chain_with(50_000_000, true);
    chain.put(
        record_pda(),
        record_bytes(rome_zk_layouts::exit::exit_record::STATUS_RELEASED, 1),
    );
    let sender = Recorder::ok();

    let out = release_one(&chain, &sender, &ctx(MIN), HASH, tuning()).await;

    assert_eq!(
        out,
        ReleaseOutcome::Refused(ReleaseRefusal::RecordNotProved {
            status: rome_zk_layouts::exit::exit_record::STATUS_RELEASED
        })
    );
    assert_eq!(sender.count(), 0);
}

#[tokio::test]
async fn an_undecodable_record_is_refused_by_name() {
    let chain = chain_with(50_000_000, true);
    chain.put(record_pda(), vec![1, 2, 3]);
    let sender = Recorder::ok();

    let out = release_one(&chain, &sender, &ctx(MIN), HASH, tuning()).await;

    assert!(matches!(
        out,
        ReleaseOutcome::Refused(ReleaseRefusal::RecordUndecodable(_))
    ));
    assert_eq!(sender.count(), 0);
}

#[tokio::test]
async fn a_chain_without_a_vault_is_refused_by_name() {
    let chain = chain_with(50_000_000, true);
    chain.remove(&vault_pda());
    let sender = Recorder::ok();

    let out = release_one(&chain, &sender, &ctx(MIN), HASH, tuning()).await;

    assert_eq!(
        out,
        ReleaseOutcome::Refused(ReleaseRefusal::VaultConfigNotFound)
    );
    assert_eq!(sender.count(), 0);
}

#[tokio::test]
async fn a_vault_for_another_settlement_program_is_refused_by_name() {
    let chain = chain_with(50_000_000, true);
    chain.put(vault_pda(), vault_bytes(Pubkey::new_unique(), 9));
    let sender = Recorder::ok();

    let out = release_one(&chain, &sender, &ctx(MIN), HASH, tuning()).await;

    assert!(matches!(
        out,
        ReleaseOutcome::Refused(ReleaseRefusal::VaultSettlementMismatch { .. })
    ));
    assert_eq!(sender.count(), 0);
}

#[tokio::test]
async fn a_vault_with_impossible_decimals_is_refused_by_name() {
    let chain = chain_with(50_000_000, true);
    chain.put(vault_pda(), vault_bytes(settlement(), 19));
    let sender = Recorder::ok();

    let out = release_one(&chain, &sender, &ctx(MIN), HASH, tuning()).await;

    assert_eq!(
        out,
        ReleaseOutcome::Refused(ReleaseRefusal::VaultDecimalsInvalid { decimals: 19 })
    );
}

#[tokio::test]
async fn a_failed_read_is_a_failure_to_retry_and_sends_nothing() {
    let mut chain = chain_with(50_000_000, true);
    chain.fail_reads = true;
    let sender = Recorder::ok();

    let out = release_one(&chain, &sender, &ctx(MIN), HASH, tuning()).await;

    assert!(matches!(out, ReleaseOutcome::Failed(_)));
    assert_eq!(sender.count(), 0);
}

#[tokio::test]
async fn a_failed_send_is_a_failure_to_retry() {
    let chain = chain_with(50_000_000, true);
    let sender = Recorder::failing();

    let out = release_one(&chain, &sender, &ctx(MIN), HASH, tuning()).await;

    assert!(matches!(out, ReleaseOutcome::Failed(_)));
    assert_eq!(sender.count(), 1);
}

#[tokio::test]
async fn the_release_asks_for_enough_room_for_the_bridge_program_and_enough_compute() {
    let chain = chain_with(50_000_000, true);
    chain.put(bridge(), vec![0u8; 40_000]);
    chain.put(
        zk_settlement_client::program_data_pda(&bridge()),
        vec![0u8; 100_000],
    );
    let sender = Recorder::ok();
    let mut small = tuning();
    small.compute_unit_limit = 40_000;

    release_one(&chain, &sender, &ctx(MIN), HASH, small).await;

    let sent = sender.sent.lock().unwrap();
    let used = sent[0].1;
    assert!(used.compute_unit_limit >= 150_000);
    assert!(
        used.loaded_accounts_data_size_limit
            >= release_loaded_accounts_limit(
                small.loaded_accounts_data_size_limit,
                40_000,
                100_000
            ),
    );
    assert!(used.loaded_accounts_data_size_limit > small.loaded_accounts_data_size_limit + 140_000);
}

#[test]
fn the_loaded_accounts_limit_adds_the_bridge_program_in_whole_pages_and_stays_under_the_network_cap(
) {
    const PAGE: u32 = 32 * 1024;
    let base = 10 * PAGE;
    let grown = release_loaded_accounts_limit(base, 1, 1);
    assert!(grown > base);
    assert_eq!(grown % PAGE, 0);
    assert_eq!(
        release_loaded_accounts_limit(base, 10_000_000, 100_000_000),
        64 * 1024 * 1024
    );
}

fn proved(just_sent: bool) -> ProvedExit {
    ProvedExit {
        message_hash: HASH,
        just_sent,
    }
}

#[tokio::test]
async fn a_released_exit_is_counted_and_forgotten() {
    let chain = chain_with(50_000_000, true);
    let sender = Recorder::ok();
    let metrics = Metrics::new();
    let mut releaser = Releaser::new(true);
    releaser.note(&proved(true));

    releaser
        .run(&chain, &sender, &ctx(MIN), tuning(), &metrics)
        .await;
    assert_eq!(metrics.exits_released_total.get(), 1);
    assert_eq!(releaser.open_count(), 0);

    releaser
        .run(&chain, &sender, &ctx(MIN), tuning(), &metrics)
        .await;
    assert_eq!(sender.count(), 1, "nothing is sent twice");
}

#[tokio::test]
async fn a_waiting_exit_is_counted_then_released_once_the_recipient_has_a_token_account() {
    let chain = chain_with(5, false);
    let sender = Recorder::ok();
    let metrics = Metrics::new();
    let mut releaser = Releaser::new(true);
    releaser.note(&proved(false));

    let t0 = Instant::now();
    releaser
        .run_at(t0, &chain, &sender, &ctx(MIN), tuning(), &metrics)
        .await;
    assert_eq!(metrics.exit_release_waiting.get(), 1);
    assert_eq!(metrics.exits_released_total.get(), 0);
    assert_eq!(sender.count(), 0);

    chain.put(ata(), vec![0u8; 165]);
    // A waiting exit is looked at again after its backoff, not on every poll.
    releaser
        .run_at(
            t0 + Duration::from_secs(31),
            &chain,
            &sender,
            &ctx(MIN),
            tuning(),
            &metrics,
        )
        .await;
    assert_eq!(metrics.exit_release_waiting.get(), 0);
    assert_eq!(metrics.exits_released_total.get(), 1);
}

#[tokio::test]
async fn with_auto_release_off_nothing_is_read_or_sent() {
    let chain = chain_with(50_000_000, true);
    let sender = Recorder::ok();
    let metrics = Metrics::new();
    let mut releaser = Releaser::new(false);
    releaser.note(&proved(true));

    releaser
        .run(&chain, &sender, &ctx(MIN), tuning(), &metrics)
        .await;

    assert_eq!(releaser.open_count(), 0);
    assert_eq!(chain.read_count(), 0);
    assert_eq!(sender.count(), 0);
}

#[tokio::test]
async fn an_exit_proved_by_an_earlier_run_whose_record_is_gone_is_dropped_at_once() {
    let chain = chain_with(50_000_000, true);
    chain.remove(&record_pda());
    let sender = Recorder::ok();
    let metrics = Metrics::new();
    let mut releaser = Releaser::new(true);
    releaser.note(&proved(false));

    releaser
        .run(&chain, &sender, &ctx(MIN), tuning(), &metrics)
        .await;

    assert_eq!(releaser.open_count(), 0);
}

#[tokio::test]
async fn an_exit_this_run_just_sent_is_given_time_to_show_up() {
    let chain = chain_with(50_000_000, true);
    chain.remove(&record_pda());
    let sender = Recorder::ok();
    let metrics = Metrics::new();
    let mut releaser = Releaser::new(true);
    releaser.note(&proved(true));

    for _ in 0..SENT_GRACE_POLLS {
        releaser
            .run(&chain, &sender, &ctx(MIN), tuning(), &metrics)
            .await;
        assert_eq!(releaser.open_count(), 1, "still waiting for the record");
    }
    // The record shows up: it is released.
    chain.put(
        record_pda(),
        record_bytes(
            rome_zk_layouts::exit::exit_record::STATUS_PROVED,
            50_000_000 * 1_000_000_000,
        ),
    );
    releaser
        .run(&chain, &sender, &ctx(MIN), tuning(), &metrics)
        .await;
    assert_eq!(metrics.exits_released_total.get(), 1);
}

#[tokio::test]
async fn an_exit_that_keeps_failing_is_given_up_on_after_a_bounded_number_of_polls() {
    let chain = chain_with(50_000_000, true);
    let sender = Recorder::failing();
    let metrics = Metrics::new();
    let mut releaser = Releaser::new(true);
    releaser.note(&proved(false));

    for i in 0..MAX_RELEASE_FAILURES {
        assert_eq!(
            releaser.open_count(),
            1,
            "still retrying before attempt {i}"
        );
        releaser
            .run(&chain, &sender, &ctx(MIN), tuning(), &metrics)
            .await;
    }
    assert_eq!(releaser.open_count(), 0);
    assert_eq!(sender.count() as u32, MAX_RELEASE_FAILURES);
    assert_eq!(metrics.exits_released_total.get(), 0);
}

#[test]
fn the_new_metrics_render_under_their_names() {
    let metrics = Metrics::new();
    metrics.exits_released_total.inc();
    metrics.exit_release_waiting.set(2);
    let text = String::from_utf8(metrics.render()).unwrap();
    assert!(text.contains("rome_zk_exits_released_total 1"));
    assert!(text.contains("rome_zk_exit_release_waiting 2"));
}

fn hash_n(n: u8) -> [u8; 32] {
    [n; 32]
}

/// A proved, small record for the exit with message hash `hash`, paying `recipient()`.
fn put_waiting_record(chain: &Chain, hash: [u8; 32]) {
    // The release step only looks at the account at the PDA of this hash.
    chain.put(
        zk_settlement_client::exit_record_pda(&settlement(), CHAIN_ID, hash).0,
        record_bytes(
            rome_zk_layouts::exit::exit_record::STATUS_PROVED,
            5 * 1_000_000_000,
        ),
    );
}

fn proved_hash(hash: [u8; 32]) -> ProvedExit {
    ProvedExit {
        message_hash: hash,
        just_sent: false,
    }
}

#[tokio::test]
async fn a_waiting_exit_costs_few_reads_over_many_polls_and_is_still_released_when_its_token_account_appears(
) {
    let chain = chain_with(5, false);
    let sender = Recorder::ok();
    let metrics = Metrics::new();
    let mut releaser = Releaser::new(true);
    releaser.note(&proved(false));
    let t0 = Instant::now();

    for i in 0..100u64 {
        releaser
            .run_at(
                t0 + Duration::from_secs(2 * i),
                &chain,
                &sender,
                &ctx(MIN),
                tuning(),
                &metrics,
            )
            .await;
    }
    assert!(
        chain.read_count() < 40,
        "100 polls read {} accounts for one waiting exit (4 reads a poll was 400)",
        chain.read_count()
    );
    assert_eq!(
        metrics.exit_release_waiting.get(),
        1,
        "still counted as waiting"
    );
    assert_eq!(sender.count(), 0);

    chain.put(ata(), vec![0u8; 165]);
    releaser
        .run_at(
            t0 + Duration::from_secs(400),
            &chain,
            &sender,
            &ctx(MIN),
            tuning(),
            &metrics,
        )
        .await;
    assert_eq!(metrics.exits_released_total.get(), 1);
    assert_eq!(metrics.exit_release_waiting.get(), 0);
    assert_eq!(releaser.open_count(), 0);
}

#[tokio::test]
async fn waiting_exits_due_together_are_read_in_batches_of_at_most_a_hundred_accounts() {
    let chain = Chain::new();
    chain.put(vault_pda(), vault_bytes(settlement(), 9));
    let sender = Recorder::ok();
    let metrics = Metrics::new();
    let mut releaser = Releaser::new(true);
    for n in 1..=60u8 {
        put_waiting_record(&chain, hash_n(n));
        releaser.note(&proved_hash(hash_n(n)));
    }
    let t0 = Instant::now();
    releaser
        .run_at(t0, &chain, &sender, &ctx(MIN), tuning(), &metrics)
        .await;
    assert_eq!(metrics.exit_release_waiting.get(), 60);
    assert_eq!(
        chain.batch_calls(),
        0,
        "the first look reads each exit on its own"
    );

    releaser
        .run_at(
            t0 + Duration::from_secs(31),
            &chain,
            &sender,
            &ctx(MIN),
            tuning(),
            &metrics,
        )
        .await;
    assert!(chain.batch_calls() >= 1 && chain.batch_calls() <= 2);
    assert!(chain.max_batch.load(Ordering::SeqCst) <= 100);
    assert_eq!(metrics.exit_release_waiting.get(), 60);
}

#[tokio::test]
async fn a_waiting_exit_whose_record_is_gone_is_dropped_at_its_next_look() {
    let chain = chain_with(5, false);
    let sender = Recorder::ok();
    let metrics = Metrics::new();
    let mut releaser = Releaser::new(true);
    releaser.note(&proved(false));
    let t0 = Instant::now();
    releaser
        .run_at(t0, &chain, &sender, &ctx(MIN), tuning(), &metrics)
        .await;
    assert_eq!(metrics.exit_release_waiting.get(), 1);

    chain.remove(&record_pda());
    releaser
        .run_at(
            t0 + Duration::from_secs(31),
            &chain,
            &sender,
            &ctx(MIN),
            tuning(),
            &metrics,
        )
        .await;
    assert_eq!(releaser.open_count(), 0);
    assert_eq!(metrics.exit_release_waiting.get(), 0);
}
