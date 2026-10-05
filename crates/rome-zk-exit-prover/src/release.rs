//! Paying the user out. `ProveExit` records that a withdrawal is valid; the money moves when someone sends
//! `ReleaseExit`, which is open to anyone and has no waiting period. The exit prover sends it itself, right after its
//! own `ProveExit` lands and for any proved record it finds still open (after a restart, or when someone else proved
//! the exit but nobody released it), so a user is paid without anyone running a command.
//!
//! The instructions are the ones the operator's `release-exit` command sends, built by `zk_bridge_client` (nothing
//! is derived twice here): create the recipient's wrapped SOL token account if it is missing, then release.
//!
//! One protection: when the token account does not exist, creating it costs the exit payer rent (about 2 million
//! lamports). A payout smaller than `release_create_account_min_lamports` is not worth that, and anyone could send
//! dust to a fresh address to drain the payer, so such a withdrawal waits for a manual release. It is counted in
//! `rome_zk_exit_release_waiting` and its message hash is logged. If the recipient later gets a token account, the next
//! poll pays it out.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use rome_zk_solana_sender::{SendTuning, Sender};
use solana_program::pubkey::Pubkey;

use crate::metrics::Metrics;
use crate::settlement::{ReadError, SettlementReader};

/// The smallest release the exit prover pays a token-account rent for, in raw units of the vault's mint (lamports
/// for wrapped SOL).
pub const DEFAULT_CREATE_ACCOUNT_MIN_LAMPORTS: u64 = 10_000_000;

/// How many polls an exit this process has just proved is given for its record to appear in the reads (they are at
/// `finalized`, a send may be confirmed at a lower level). A record that was already proved before this run and is
/// gone has been released, and is dropped at once.
pub const SENT_GRACE_POLLS: u32 = 10;

/// Consecutive polls a release may fail (a read or a send, or a refusal) before the exit prover stops trying it in
/// this run. It is tried again after a restart, and `release-exit` always works.
pub const MAX_RELEASE_FAILURES: u32 = 5;

/// A waiting exit (payout below the minimum, no token account) is looked at again this long after it was found
/// waiting, then after twice that, and so on up to `WAIT_BACKOFF_MAX`. A change in its record or its token account
/// starts it over. Looking at it on every poll cost four reads each, for as long as the process ran.
pub const WAIT_BACKOFF_START: Duration = Duration::from_secs(30);
pub const WAIT_BACKOFF_MAX: Duration = Duration::from_secs(30 * 60);

/// A release is at least this many compute units: the release, its inner call into the settlement program and the
/// token transfer take about 65,000, and creating the token account about 30,000 more. The proving limit is often
/// lower than that.
const MIN_RELEASE_COMPUTE_UNITS: u32 = 150_000;

/// Room for the token program, the associated token program and the small accounts a release loads, on top of the
/// bridge program itself.
const RELEASE_LOADED_SLACK_BYTES: u32 = 512 * 1024;

/// The network's ceiling on the data one transaction may load.
const MAX_LOADED_ACCOUNTS_BYTES: u32 = 64 * 1024 * 1024;

/// Reads one account's data at the settlement commitment; `Ok(None)` when it does not exist.
pub trait AccountReader: Send + Sync {
    fn read_account(&self, key: &Pubkey) -> Result<Option<Vec<u8>>, ReadError>;

    /// Reads up to `MAX_BATCH_KEYS` accounts in one request; the answers come back in the order of `keys`. The default
    /// reads them one by one, a reader on a real endpoint overrides it with one `getMultipleAccounts` call.
    fn read_accounts(&self, keys: &[Pubkey]) -> Result<Vec<Option<Vec<u8>>>, ReadError> {
        keys.iter().map(|k| self.read_account(k)).collect()
    }
}

/// The most accounts one batched read asks for (the endpoint's own limit).
pub const MAX_BATCH_KEYS: usize = 100;

/// An exit that is proved on chain: handed from the poll to the release step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProvedExit {
    pub message_hash: [u8; 32],
    /// `true` when this run's own `ProveExit` landed, `false` when the exit was found already proved.
    pub just_sent: bool,
}

/// What one release needs besides the chain and the sender.
#[derive(Debug, Clone, Copy)]
pub struct ReleaseContext {
    pub program_id: Pubkey,
    pub chain_id: u64,
    /// The exit payer: it signs the transaction, funds a missing token account, and gets the record rent back.
    pub payer: Pubkey,
    pub create_account_min_lamports: u64,
}

/// Why a release was not attempted. Each one is named in the log.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReleaseRefusal {
    #[error("ExitRecordUndecodable: {0}")]
    RecordUndecodable(String),
    #[error("ExitRecordNotProved: the record is at status {status}, and only a freshly proved record can be released")]
    RecordNotProved { status: u8 },
    #[error("BridgeProgramUnset: the exit config names no bridge program")]
    BridgeProgramUnset,
    #[error("VaultConfigNotFound: the chain has no vault for this settlement program")]
    VaultConfigNotFound,
    #[error("VaultConfigUndecodable: {0}")]
    VaultConfigUndecodable(String),
    #[error(
        "VaultSettlementMismatch: the vault records settlement program {actual}, expected {expected}"
    )]
    VaultSettlementMismatch { expected: Pubkey, actual: Pubkey },
    #[error("VaultDecimalsInvalid: the vault's mint has {decimals} decimals, more than 18")]
    VaultDecimalsInvalid { decimals: u8 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseOutcome {
    /// The release landed.
    Released,
    /// There is no exit record: it was never proved at this commitment, or it is already released and closed.
    Gone,
    /// The payout is below the token-account minimum and the recipient has no token account. Nothing was sent.
    Waiting { payout: u128 },
    /// Not attempted, by name.
    Refused(ReleaseRefusal),
    /// A read or the send failed; worth trying again.
    Failed(String),
}

/// The total data a release loads: the proving figure (it already covers the settlement program and the settlement
/// accounts) plus the bridge program and its program data, rounded up to the 32 KiB pages a transaction is charged in,
/// plus room for the token programs. Never above the network's ceiling.
pub fn release_loaded_accounts_limit(
    base: u32,
    bridge_program_len: usize,
    bridge_program_data_len: usize,
) -> u32 {
    const PAGE: u64 = 32 * 1024;
    let bridge = (bridge_program_len as u64).saturating_add(bridge_program_data_len as u64);
    let bridge_pages = bridge.div_ceil(PAGE) * PAGE;
    let total = (base as u64)
        .saturating_add(bridge_pages)
        .saturating_add(RELEASE_LOADED_SLACK_BYTES as u64);
    total.min(MAX_LOADED_ACCOUNTS_BYTES as u64) as u32
}

fn pow10(exp: u8) -> u128 {
    10u128.pow(exp as u32)
}

/// One release attempt for the exit with `message_hash`. Reads the record, the exit config, the vault and the
/// recipient's token account; sends nothing unless every check holds.
pub async fn release_one<C, S>(
    chain: &C,
    sender: &S,
    ctx: &ReleaseContext,
    message_hash: [u8; 32],
    tuning: SendTuning,
) -> ReleaseOutcome
where
    C: SettlementReader + AccountReader,
    S: Sender,
{
    release_inner(chain, sender, ctx, message_hash, tuning, &mut None).await
}

/// What a waiting exit is watched by: the record's bytes and the recipient's token account.
struct WaitInfo {
    record: Vec<u8>,
    token_account: Pubkey,
}

async fn release_inner<C, S>(
    chain: &C,
    sender: &S,
    ctx: &ReleaseContext,
    message_hash: [u8; 32],
    tuning: SendTuning,
    wait: &mut Option<WaitInfo>,
) -> ReleaseOutcome
where
    C: SettlementReader + AccountReader,
    S: Sender,
{
    let (record_pda, _) =
        zk_settlement_client::exit_record_pda(&ctx.program_id, ctx.chain_id, message_hash);
    let record_data = match chain.read_account(&record_pda) {
        Ok(Some(d)) => d,
        Ok(None) => return ReleaseOutcome::Gone,
        Err(e) => return ReleaseOutcome::Failed(format!("reading the exit record: {e}")),
    };
    let record = match zk_settlement_client::decode_exit_record_account(&record_data) {
        Ok(r) => r,
        Err(e) => return ReleaseOutcome::Refused(ReleaseRefusal::RecordUndecodable(e.to_string())),
    };
    if record.status != rome_zk_layouts::exit::exit_record::STATUS_PROVED {
        return ReleaseOutcome::Refused(ReleaseRefusal::RecordNotProved {
            status: record.status,
        });
    }

    let exit_config = match chain.read_exit_config() {
        Ok(c) => c,
        Err(e) => return ReleaseOutcome::Failed(format!("reading the exit config: {e}")),
    };
    let bridge = exit_config.bridge_program;
    if bridge == Pubkey::default() {
        return ReleaseOutcome::Refused(ReleaseRefusal::BridgeProgramUnset);
    }

    let (vault_pda, _) = zk_bridge_client::vault_config_pda(&bridge, &ctx.program_id, ctx.chain_id);
    let vault_data = match chain.read_account(&vault_pda) {
        Ok(Some(d)) => d,
        Ok(None) => return ReleaseOutcome::Refused(ReleaseRefusal::VaultConfigNotFound),
        Err(e) => return ReleaseOutcome::Failed(format!("reading the vault: {e}")),
    };
    let vault = match zk_bridge_client::decode_vault_config_account(&vault_data) {
        Ok(v) => v,
        Err(e) => {
            return ReleaseOutcome::Refused(ReleaseRefusal::VaultConfigUndecodable(e.to_string()))
        }
    };
    if let Err(e) = zk_bridge_client::check_vault_settlement(&vault, &ctx.program_id) {
        return ReleaseOutcome::Refused(ReleaseRefusal::VaultSettlementMismatch {
            expected: e.expected,
            actual: e.actual,
        });
    }
    if vault.mint_decimals > 18 {
        return ReleaseOutcome::Refused(ReleaseRefusal::VaultDecimalsInvalid {
            decimals: vault.mint_decimals,
        });
    }

    let recipient = Pubkey::new_from_array(record.sol_recipient);
    let token_account = zk_bridge_client::recipient_ata(&recipient, &vault.mint);
    let token_account_exists = match chain.read_account(&token_account) {
        Ok(found) => found.is_some(),
        Err(e) => return ReleaseOutcome::Failed(format!("reading the token account: {e}")),
    };
    // What the program pays out: the 18-decimal amount scaled down to the mint's decimals, rounding down.
    let payout = record.amount / pow10(18 - vault.mint_decimals);
    if !token_account_exists && payout < ctx.create_account_min_lamports as u128 {
        *wait = Some(WaitInfo {
            record: record_data,
            token_account,
        });
        return ReleaseOutcome::Waiting { payout };
    }

    let mut instructions = Vec::with_capacity(2);
    if !token_account_exists {
        instructions.push(zk_bridge_client::create_recipient_ata_idempotent_ix(
            &ctx.payer,
            &recipient,
            &vault.mint,
        ));
    }
    let (exit_config_pda, _) = zk_settlement_client::exit_config_pda(&ctx.program_id, ctx.chain_id);
    instructions.push(zk_bridge_client::release_exit_ix(
        &bridge,
        &ctx.program_id,
        ctx.chain_id,
        message_hash,
        &vault.mint,
        &exit_config_pda,
        &record_pda,
        &record.payer,
        &recipient,
    ));

    // The bridge program and its program data are loaded on top of what proving loads. A missing one counts as zero
    // bytes: the send then fails by itself with the network's own error.
    let program_len = |key: &Pubkey| match chain.read_account(key) {
        Ok(Some(d)) => d.len(),
        _ => 0,
    };
    let bridge_len = program_len(&bridge);
    let bridge_data_len = program_len(&zk_settlement_client::program_data_pda(&bridge));
    let tuning = SendTuning {
        compute_unit_limit: tuning.compute_unit_limit.max(MIN_RELEASE_COMPUTE_UNITS),
        loaded_accounts_data_size_limit: release_loaded_accounts_limit(
            tuning.loaded_accounts_data_size_limit,
            bridge_len,
            bridge_data_len,
        ),
        ..tuning
    };

    match sender.send_and_confirm(&instructions, tuning).await {
        Ok(_) => ReleaseOutcome::Released,
        Err(e) => ReleaseOutcome::Failed(format!("sending the release: {e}")),
    }
}

#[derive(Debug, Clone)]
struct Open {
    /// Polls left in which an absent record is still expected to appear.
    grace: u32,
    /// Consecutive polls this exit's release failed or was refused.
    failures: u32,
    /// Logged as waiting already, so the line is written once.
    waiting: bool,
    /// Set while the exit waits for a token account: when it is next looked at, and what it is watched by.
    wait: Option<WaitState>,
}

#[derive(Debug, Clone)]
struct WaitState {
    delay: Duration,
    next_check: Instant,
    record: Vec<u8>,
    token_account: Pubkey,
}

/// The proved exits this process still has to release. Owned by the binary's loop; the poll hands it what it proved.
#[derive(Debug)]
pub struct Releaser {
    auto_release: bool,
    open: BTreeMap<[u8; 32], Open>,
}

impl Releaser {
    pub fn new(auto_release: bool) -> Self {
        Self {
            auto_release,
            open: BTreeMap::new(),
        }
    }

    /// How many proved exits are still to be released (including the waiting ones).
    pub fn open_count(&self) -> usize {
        self.open.len()
    }

    /// Takes note of a proved exit. With `auto_release` off, nothing is kept.
    pub fn note(&mut self, proved: &ProvedExit) {
        if !self.auto_release {
            return;
        }
        self.open.entry(proved.message_hash).or_insert(Open {
            grace: if proved.just_sent {
                SENT_GRACE_POLLS
            } else {
                0
            },
            failures: 0,
            waiting: false,
            wait: None,
        });
    }

    /// Waiting exits are not tried on every poll. Those whose time has come have their record and token account read
    /// together, in as few batched reads as possible. An exit whose record and token account are as they were has its
    /// wait doubled and is returned to be skipped this poll. One whose record changed or whose token account
    /// appeared is tried in full (it forgets its wait). A failed batch skips its exits until the next poll.
    fn look_at_waiting<C: AccountReader>(
        &mut self,
        now: Instant,
        chain: &C,
        ctx: &ReleaseContext,
    ) -> BTreeSet<[u8; 32]> {
        let mut skip = BTreeSet::new();
        let mut due = Vec::new();
        for (hash, open) in &self.open {
            match &open.wait {
                Some(w) if now >= w.next_check => due.push(*hash),
                Some(_) => {
                    skip.insert(*hash);
                }
                None => {}
            }
        }
        // Two accounts per exit, so 50 exits fill a batch.
        for page in due.chunks(MAX_BATCH_KEYS / 2) {
            let keys: Vec<Pubkey> = page
                .iter()
                .flat_map(|hash| {
                    let record =
                        zk_settlement_client::exit_record_pda(&ctx.program_id, ctx.chain_id, *hash)
                            .0;
                    [record, self.open[hash].wait.as_ref().unwrap().token_account]
                })
                .collect();
            let found = match chain.read_accounts(&keys) {
                Ok(f) if f.len() == keys.len() => f,
                other => {
                    tracing::warn!(error = ?other.err(), "reading the waiting exits failed, will retry");
                    skip.extend(page.iter().copied());
                    continue;
                }
            };
            for (hash, pair) in page.iter().zip(found.chunks(2)) {
                let Some(w) = self.open.get_mut(hash).and_then(|o| o.wait.as_mut()) else {
                    continue;
                };
                if pair[0].as_deref() == Some(w.record.as_slice()) && pair[1].is_none() {
                    w.delay = (w.delay * 2).min(WAIT_BACKOFF_MAX);
                    w.next_check = now + w.delay;
                    skip.insert(*hash);
                }
            }
        }
        skip
    }

    /// Tries to release every open exit once, and updates the release metrics.
    pub async fn run<C, S>(
        &mut self,
        chain: &C,
        sender: &S,
        ctx: &ReleaseContext,
        tuning: SendTuning,
        metrics: &Metrics,
    ) where
        C: SettlementReader + AccountReader,
        S: Sender,
    {
        self.run_at(Instant::now(), chain, sender, ctx, tuning, metrics)
            .await
    }

    /// `run` with the poll's time given, so a test can move the clock.
    pub async fn run_at<C, S>(
        &mut self,
        now: Instant,
        chain: &C,
        sender: &S,
        ctx: &ReleaseContext,
        tuning: SendTuning,
        metrics: &Metrics,
    ) where
        C: SettlementReader + AccountReader,
        S: Sender,
    {
        let hashes: Vec<[u8; 32]> = self.open.keys().copied().collect();
        let skip = self.look_at_waiting(now, chain, ctx);
        for hash in hashes {
            if skip.contains(&hash) {
                continue;
            }
            let mut wait = None;
            let outcome = release_inner(chain, sender, ctx, hash, tuning, &mut wait).await;
            let label = format!("0x{}", hex::encode(hash));
            let Some(open) = self.open.get_mut(&hash) else {
                continue;
            };
            let mut done = false;
            let mut gave_up = false;
            open.wait = None;
            if !matches!(outcome, ReleaseOutcome::Waiting { .. }) {
                open.waiting = false;
            }
            match outcome {
                ReleaseOutcome::Released => {
                    metrics.exits_released_total.inc();
                    tracing::info!(message_hash = %label, "exit released to its recipient");
                    done = true;
                }
                ReleaseOutcome::Gone => {
                    if open.grace == 0 {
                        tracing::debug!(message_hash = %label, "exit record is gone, already released");
                        done = true;
                    } else {
                        open.grace -= 1;
                    }
                }
                ReleaseOutcome::Waiting { payout } => {
                    open.failures = 0;
                    open.wait = wait.map(|w| WaitState {
                        delay: WAIT_BACKOFF_START,
                        next_check: now + WAIT_BACKOFF_START,
                        record: w.record,
                        token_account: w.token_account,
                    });
                    if !open.waiting {
                        open.waiting = true;
                        tracing::warn!(
                            message_hash = %label,
                            payout,
                            minimum = ctx.create_account_min_lamports,
                            "exit not released automatically: the payout is below the minimum for creating the recipient's token account, which does not exist; run release-exit for it"
                        );
                    }
                }
                ReleaseOutcome::Refused(refusal) => {
                    open.failures += 1;
                    tracing::warn!(message_hash = %label, %refusal, "exit release refused");
                    gave_up = open.failures >= MAX_RELEASE_FAILURES;
                }
                ReleaseOutcome::Failed(reason) => {
                    open.failures += 1;
                    tracing::warn!(message_hash = %label, reason, "exit release failed, will retry");
                    gave_up = open.failures >= MAX_RELEASE_FAILURES;
                }
            }
            if gave_up {
                tracing::warn!(message_hash = %label, "giving up on automatic release for this run; run release-exit for it");
            }
            if done || gave_up {
                self.open.remove(&hash);
            }
        }
        let waiting = self.open.values().filter(|o| o.waiting).count();
        metrics.exit_release_waiting.set(waiting as i64);
    }
}
