//! `deposit-queue init|propose|activate|show` and `close-deposit`: set up a chain's deposit queue, change its
//! parameters under the activation delay, read it, and give a credited deposit record's rent back to its depositor.
//!
//! Every refusal is checked here, by name, before anything is sent, and each one mirrors a refusal the bridge
//! program makes (the program stays the guard; the command says why early). Sends go through
//! [`crate::commands::execute_many`], so each is one V1 transaction. `show` prints `key=value` lines that
//! `./rollup check` reads.

use crate::chain::Chain;
use crate::commands::{execute_many, read, read_slot, Read};
use crate::error::{Mode, OpsError, Report};
use crate::keys::{self, Signers};
use rome_zk_layouts::deposit_queue::{
    bridge_config, deposit_queue as queue_layout, deposit_record,
};
use solana_program::pubkey::Pubkey;
use std::path::PathBuf;
use zk_bridge_client::DepositParamsArgs;

/// The parameter values a queue starts with when no flag says otherwise.
pub const DEFAULT_DEADLINE_SECS: u32 = 43_200;
pub const DEFAULT_MAX_PER_BATCH: u16 = 256;
pub const DEFAULT_MAX_PER_BLOCK: u16 = 4;
/// 0.001 SOL, in lamports.
pub const DEFAULT_MIN_AMOUNT: u64 = 1_000_000;
/// 0.0001 SOL, in lamports.
pub const DEFAULT_FEE_LAMPORTS: u64 = 100_000;

/// The parameter flags as given. A flag that was not given is `None`.
#[derive(Debug, Clone, Default)]
pub struct ParamFlags {
    pub deadline_secs: Option<u32>,
    pub max_per_batch: Option<u16>,
    pub max_per_block: Option<u16>,
    pub min_amount: Option<u64>,
    pub fee_lamports: Option<u64>,
    pub fee_recipient: Option<Pubkey>,
    /// The sequencer's blocks per batch. When given, `max_per_block x blocks_per_batch` must fit `max_per_batch`.
    pub blocks_per_batch: Option<u32>,
}

impl ParamFlags {
    /// The flags laid over `base`: a flag that was given wins, the rest come from `base`.
    fn over(&self, base: DepositParamsArgs) -> DepositParamsArgs {
        DepositParamsArgs {
            inclusion_deadline_secs: self.deadline_secs.unwrap_or(base.inclusion_deadline_secs),
            max_per_batch: self.max_per_batch.unwrap_or(base.max_per_batch),
            max_per_block: self.max_per_block.unwrap_or(base.max_per_block),
            min_amount: self.min_amount.unwrap_or(base.min_amount),
            fee_lamports: self.fee_lamports.unwrap_or(base.fee_lamports),
            fee_recipient: self.fee_recipient.unwrap_or(base.fee_recipient),
        }
    }
}

fn defaults(fee_recipient: Pubkey) -> DepositParamsArgs {
    DepositParamsArgs {
        inclusion_deadline_secs: DEFAULT_DEADLINE_SECS,
        max_per_batch: DEFAULT_MAX_PER_BATCH,
        max_per_block: DEFAULT_MAX_PER_BLOCK,
        min_amount: DEFAULT_MIN_AMOUNT,
        fee_lamports: DEFAULT_FEE_LAMPORTS,
        fee_recipient,
    }
}

fn from_layout(p: &queue_layout::DepositParams) -> DepositParamsArgs {
    DepositParamsArgs {
        inclusion_deadline_secs: p.inclusion_deadline_secs,
        max_per_batch: p.max_per_batch,
        max_per_block: p.max_per_block,
        min_amount: p.min_amount,
        fee_lamports: p.fee_lamports,
        fee_recipient: Pubkey::new_from_array(p.fee_recipient),
    }
}

/// The program's bounds on one parameter set, and the per-block rule against the sequencer's blocks per batch.
/// Each refusal carries the name the program gives the same refusal (`PerBlockExceedsBatch` is the one only this
/// command can check, because the programs cannot see `blocks_per_batch`).
pub fn validate_params(
    p: &DepositParamsArgs,
    blocks_per_batch: Option<u32>,
) -> Result<(), OpsError> {
    use zk_bridge_client::{
        MAX_DEADLINE_SECS, MAX_FEE_LAMPORTS, MAX_PER_BATCH_CEILING, MIN_AMOUNT_FLOOR,
        MIN_DEADLINE_SECS,
    };
    if p.inclusion_deadline_secs < MIN_DEADLINE_SECS {
        return Err(OpsError::usage(
            "DeadlineBelowFloor",
            format!(
                "the deadline is {} s; it must be at least {MIN_DEADLINE_SECS} s (1 hour)",
                p.inclusion_deadline_secs
            ),
        ));
    }
    if p.inclusion_deadline_secs > MAX_DEADLINE_SECS {
        return Err(OpsError::usage(
            "DeadlineAboveCeiling",
            format!(
                "the deadline is {} s; it must be at most {MAX_DEADLINE_SECS} s (24 hours)",
                p.inclusion_deadline_secs
            ),
        ));
    }
    if p.max_per_batch > MAX_PER_BATCH_CEILING {
        return Err(OpsError::usage(
            "MaxPerBatchTooLarge",
            format!(
                "max_per_batch is {}; it must be at most {MAX_PER_BATCH_CEILING}",
                p.max_per_batch
            ),
        ));
    }
    if p.max_per_block == 0 || p.max_per_block > p.max_per_batch {
        return Err(OpsError::usage(
            "MaxPerBlockOutOfRange",
            format!(
                "max_per_block is {}; it must be at least 1 and at most max_per_batch ({})",
                p.max_per_block, p.max_per_batch
            ),
        ));
    }
    if p.min_amount < MIN_AMOUNT_FLOOR {
        return Err(OpsError::usage(
            "MinAmountZero",
            "min_amount must be at least 1 base unit",
        ));
    }
    if p.fee_lamports > MAX_FEE_LAMPORTS {
        return Err(OpsError::usage(
            "FeeTooHigh",
            format!(
                "the fee is {} lamports; it must be at most {MAX_FEE_LAMPORTS} (0.01 SOL)",
                p.fee_lamports
            ),
        ));
    }
    if let Some(blocks) = blocks_per_batch {
        let per_batch = p.max_per_block as u64 * blocks as u64;
        if per_batch > p.max_per_batch as u64 {
            return Err(OpsError::usage(
                "PerBlockExceedsBatch",
                format!(
                    "max_per_block {} x blocks_per_batch {blocks} = {per_batch}, which is more than max_per_batch {}; \
lower max_per_block to at most {} or raise max_per_batch",
                    p.max_per_block,
                    p.max_per_batch,
                    p.max_per_batch as u64 / blocks.max(1) as u64
                ),
            ));
        }
    }
    Ok(())
}

fn print_params(report: &mut Report, prefix: &str, p: &queue_layout::DepositParams) {
    report.line(format!(
        "{prefix}inclusion_deadline_secs={}",
        p.inclusion_deadline_secs
    ));
    report.line(format!("{prefix}max_per_batch={}", p.max_per_batch));
    report.line(format!("{prefix}max_per_block={}", p.max_per_block));
    report.line(format!("{prefix}min_amount={}", p.min_amount));
    report.line(format!("{prefix}fee_lamports={}", p.fee_lamports));
    report.line(format!(
        "{prefix}fee_recipient={}",
        Pubkey::new_from_array(p.fee_recipient)
    ));
}

fn print_args(report: &mut Report, p: &DepositParamsArgs) {
    report.line(format!("  deadline_secs  {}", p.inclusion_deadline_secs));
    report.line(format!("  max_per_batch  {}", p.max_per_batch));
    report.line(format!("  max_per_block  {}", p.max_per_block));
    report.line(format!("  min_amount     {}", p.min_amount));
    report.line(format!("  fee_lamports   {}", p.fee_lamports));
    report.line(format!("  fee_recipient  {}", p.fee_recipient));
}

fn print_accounts(report: &mut Report, ix: &solana_program::instruction::Instruction) {
    for m in &ix.accounts {
        report.line(format!(
            "      {}{}{}",
            m.pubkey,
            if m.is_writable { "  (writable)" } else { "" },
            if m.is_signer { "  (signer)" } else { "" },
        ));
    }
}

fn decode_queue(d: &[u8], pda: &Pubkey) -> Result<queue_layout::DepositQueueFields, OpsError> {
    queue_layout::read(d).map_err(|e| {
        OpsError::chain(
            "DepositQueueUndecodable",
            format!("deposit_queue {pda} does not decode: {e:?}"),
        )
    })
}

fn decode_config(d: &[u8], pda: &Pubkey) -> Result<bridge_config::BridgeConfigFields, OpsError> {
    bridge_config::read(d).map_err(|e| {
        OpsError::chain(
            "BridgeConfigUndecodable",
            format!("bridge_config {pda} does not decode: {e:?}"),
        )
    })
}

/// Reads the bridge config; a missing one is a refusal, because every deposit command depends on it.
async fn read_config<C: Chain>(
    chain: &C,
    mode: Mode,
    bridge: &Pubkey,
    report: &mut Report,
) -> Result<Option<bridge_config::BridgeConfigFields>, OpsError> {
    let (pda, _) = zk_bridge_client::bridge_config_pda(bridge);
    match read(
        chain,
        mode,
        &pda,
        "the bridge config",
        "BridgeConfigLookupFailed",
        report,
    )
    .await?
    {
        Read::Found(d) => Ok(Some(decode_config(&d, &pda)?)),
        Read::Missing => Err(OpsError::chain(
            "BridgeConfigNotFound",
            format!(
                "bridge_config {pda} not found; run `bridge-config init` for this bridge first"
            ),
        )),
        Read::Unavailable => Ok(None),
    }
}

/// The chain's root, checked against the key that is about to sign as the chain authority. `Ok(None)` is a dry run
/// that could not read it.
async fn read_root_for_authority<C: Chain>(
    chain: &C,
    mode: Mode,
    settlement: &Pubkey,
    chain_id: u64,
    authority: &Pubkey,
    report: &mut Report,
) -> Result<Option<zk_settlement_client::RootAccount>, OpsError> {
    let (root_pda, _) = zk_settlement_client::root_pda(settlement, chain_id);
    match read(
        chain,
        mode,
        &root_pda,
        "the root account",
        "RootLookupFailed",
        report,
    )
    .await?
    {
        Read::Found(d) => {
            let root = zk_settlement_client::decode_root_account(&d).map_err(|e| {
                OpsError::chain(
                    "RootUndecodable",
                    format!("the root account {root_pda} does not decode: {e}"),
                )
            })?;
            if root.authority != *authority {
                return Err(OpsError::chain(
                    "WrongChainAuthority",
                    format!(
                        "--authority-keypair is {authority}, but chain {chain_id} belongs to {}",
                        root.authority
                    ),
                ));
            }
            Ok(Some(root))
        }
        Read::Missing => Err(OpsError::chain(
            "ChainNotRegistered",
            format!(
                "chain {chain_id} has no root account {root_pda} under this settlement program"
            ),
        )),
        Read::Unavailable => Ok(None),
    }
}

// ---- deposit-queue init ----

#[derive(Debug, Clone)]
pub struct InitRequest {
    pub settlement: Pubkey,
    pub bridge: Pubkey,
    pub chain_id: u64,
    /// The chain authority's keypair file. It signs, pays, and is the fee recipient unless one is given.
    pub authority_keypair: PathBuf,
    pub flags: ParamFlags,
}

/// `deposit-queue init`: create the chain's deposit queue with the ruled defaults, or the values given. A queue that
/// exists is reported and nothing is sent.
pub async fn init<C: Chain>(chain: &C, req: InitRequest, mode: Mode) -> Result<Report, OpsError> {
    let authority = keys::load(&req.authority_keypair, "--authority-keypair")?;
    let authority_key = keys::pubkey(&authority);
    let params = req.flags.over(defaults(authority_key));
    validate_params(&params, req.flags.blocks_per_batch)?;
    if rome_zk_layouts::chainid::is_reserved(req.chain_id) {
        return Err(OpsError::usage(
            "ReservedChainId",
            format!(
                "chain {} is a reserved id; deposit queues are for permissionless chains",
                req.chain_id
            ),
        ));
    }

    let mut report = Report::default();
    let (queue_pda, _) =
        zk_bridge_client::deposit_queue_pda(&req.bridge, &req.settlement, req.chain_id);
    if let Read::Found(d) = read(
        chain,
        mode,
        &queue_pda,
        "the deposit queue",
        "DepositQueueLookupFailed",
        &mut report,
    )
    .await?
    {
        let q = decode_queue(&d, &queue_pda)?;
        report.line(format!(
            "deposit_queue {queue_pda} already exists, nothing sent:"
        ));
        print_params(&mut report, "  ", &q.params);
        report.line(format!("  count={}", q.count));
        return Ok(report);
    }

    if let Some(cfg) = read_config(chain, mode, &req.bridge, &mut report).await? {
        if cfg.settlement_program != req.settlement.to_bytes() {
            return Err(OpsError::chain(
                "WrongSettlementProgram",
                format!(
                    "the bridge config names settlement program {}, not {}",
                    Pubkey::new_from_array(cfg.settlement_program),
                    req.settlement
                ),
            ));
        }
    }
    if let Some(root) = read_root_for_authority(
        chain,
        mode,
        &req.settlement,
        req.chain_id,
        &authority_key,
        &mut report,
    )
    .await?
    {
        if root.head_pending_batch == 0 {
            return Err(OpsError::chain(
                "ChainNeverPosted",
                format!(
                    "chain {} has not posted a root yet; the program refuses a queue until it has. Post one batch first, then run this again",
                    req.chain_id
                ),
            ));
        }
    }
    let (vault_pda, _) =
        zk_bridge_client::vault_config_pda(&req.bridge, &req.settlement, req.chain_id);
    if let Read::Missing = read(
        chain,
        mode,
        &vault_pda,
        "the vault config",
        "VaultConfigLookupFailed",
        &mut report,
    )
    .await?
    {
        return Err(OpsError::chain(
            "VaultConfigNotFound",
            format!("vault_config {vault_pda} not found; run `vault init` first"),
        ));
    }

    let ix = zk_bridge_client::init_deposit_queue_ix(
        &req.bridge,
        &authority_key,
        &authority_key,
        &req.settlement,
        req.chain_id,
        params,
    );
    report.line(format!(
        "-- deposit-queue init: chain {}, settlement {}, bridge {}",
        req.chain_id, req.settlement, req.bridge
    ));
    report.line(format!("  deposit_queue (not yet created): {queue_pda}"));
    print_args(&mut report, &params);
    report.line("  instruction: InitDepositQueue");
    print_accounts(&mut report, &ix);
    let signers = Signers::new(authority, vec![]);
    if let Some(sig) = execute_many(
        chain,
        mode,
        "InitDepositQueue",
        std::slice::from_ref(&ix),
        &signers,
        &mut report,
    )
    .await?
    {
        report.line(format!("-- sent: {sig}"));
    }
    Ok(report)
}

// ---- deposit-queue propose ----

#[derive(Debug, Clone)]
pub struct ProposeRequest {
    pub settlement: Pubkey,
    pub bridge: Pubkey,
    pub chain_id: u64,
    pub authority_keypair: PathBuf,
    /// A flag not given keeps the queue's live value.
    pub flags: ParamFlags,
    pub activation_slot: Option<u64>,
    pub activation_delay_slots: Option<u64>,
}

/// The slots added past one challenge window when no activation is given, so the proposal still lands at least one
/// window out when the transaction takes a few slots to confirm. Never more than the window itself, the room the
/// program leaves.
pub fn default_margin_slots(challenge_window: u64) -> u64 {
    (challenge_window / 10).max(10).min(challenge_window)
}

/// `deposit-queue propose`: record new parameters with an activation slot. A new proposal replaces a pending one.
pub async fn propose<C: Chain>(
    chain: &C,
    req: ProposeRequest,
    mode: Mode,
) -> Result<Report, OpsError> {
    if req.activation_slot.is_some() && req.activation_delay_slots.is_some() {
        return Err(OpsError::usage(
            "ActivationFlagsConflict",
            "pass at most one of --activation-slot or --activation-delay-slots",
        ));
    }
    let authority = keys::load(&req.authority_keypair, "--authority-keypair")?;
    let authority_key = keys::pubkey(&authority);
    let mut report = Report::default();

    let (queue_pda, _) =
        zk_bridge_client::deposit_queue_pda(&req.bridge, &req.settlement, req.chain_id);
    let queue = match read(
        chain,
        mode,
        &queue_pda,
        "the deposit queue",
        "DepositQueueLookupFailed",
        &mut report,
    )
    .await?
    {
        Read::Found(d) => Some(decode_queue(&d, &queue_pda)?),
        Read::Missing => {
            return Err(OpsError::chain(
                "DepositQueueNotFound",
                format!("deposit_queue {queue_pda} not found; run `deposit-queue init` first"),
            ))
        }
        Read::Unavailable => None,
    };
    let base = queue
        .as_ref()
        .map(|q| from_layout(&q.params))
        .unwrap_or_else(|| defaults(authority_key));
    let params = req.flags.over(base);
    validate_params(&params, req.flags.blocks_per_batch)?;

    let root = read_root_for_authority(
        chain,
        mode,
        &req.settlement,
        req.chain_id,
        &authority_key,
        &mut report,
    )
    .await?;
    let window = root.map(|r| r.challenge_window_slots as u64);
    let now = read_slot(chain, mode, &mut report).await?;
    if window == Some(0) {
        return Err(OpsError::chain(
            "ChallengeWindowZero",
            "the chain's challenge window is 0 slots, so there is no activation delay to propose under",
        ));
    }

    let activation_slot = match (req.activation_slot, req.activation_delay_slots) {
        (Some(slot), _) => slot,
        (None, Some(delay)) => match now {
            Some(n) => n.saturating_add(delay),
            None => {
                report.line("(dry run: current_slot assumed 0, no live cluster to read)");
                delay
            }
        },
        (None, None) => match (now, window) {
            (Some(n), Some(w)) => n + w + default_margin_slots(w),
            _ => {
                return Err(OpsError::usage(
                    "ActivationUnknown",
                    "without the chain's challenge window and the current slot there is no default activation; give --activation-slot",
                ))
            }
        },
    };
    if let (Some(n), Some(w)) = (now, window) {
        if activation_slot < n.saturating_add(w) {
            return Err(OpsError::chain(
                "ActivationTooSoon",
                format!(
                    "activation slot {activation_slot} is under one challenge window from now (slot {n}, window {w}); the earliest is {}",
                    n + w
                ),
            ));
        }
        if activation_slot > n.saturating_add(2 * w) {
            return Err(OpsError::chain(
                "ActivationTooLate",
                format!(
                    "activation slot {activation_slot} is over two challenge windows from now (slot {n}, window {w}); the latest is {}",
                    n + 2 * w
                ),
            ));
        }
    }

    report.line(format!(
        "-- deposit-queue propose: chain {}, settlement {}, bridge {}",
        req.chain_id, req.settlement, req.bridge
    ));
    if let Some(q) = &queue {
        if q.activation_slot != 0 {
            report.line(format!(
                "  a proposal is already pending (activation slot {}); this one replaces it",
                q.activation_slot
            ));
        }
    }
    print_args(&mut report, &params);
    report.line(format!("  activation_slot  {activation_slot}"));
    let ix = zk_bridge_client::propose_deposit_params_ix(
        &req.bridge,
        &authority_key,
        &req.settlement,
        req.chain_id,
        activation_slot,
        params,
    );
    report.line("  instruction: ProposeDepositParams");
    print_accounts(&mut report, &ix);
    let signers = Signers::new(authority, vec![]);
    if let Some(sig) = execute_many(
        chain,
        mode,
        "ProposeDepositParams",
        std::slice::from_ref(&ix),
        &signers,
        &mut report,
    )
    .await?
    {
        report.line(format!("-- sent: {sig}"));
    }
    Ok(report)
}

// ---- deposit-queue activate ----

#[derive(Debug, Clone)]
pub struct ActivateRequest {
    pub settlement: Pubkey,
    pub bridge: Pubkey,
    pub chain_id: u64,
    pub payer_keypair: PathBuf,
}

/// `deposit-queue activate`: make the pending parameters the live ones once their slot has passed. Anyone may.
pub async fn activate<C: Chain>(
    chain: &C,
    req: ActivateRequest,
    mode: Mode,
) -> Result<Report, OpsError> {
    let payer = keys::load(&req.payer_keypair, "--payer-keypair")?;
    let mut report = Report::default();
    let (queue_pda, _) =
        zk_bridge_client::deposit_queue_pda(&req.bridge, &req.settlement, req.chain_id);
    let pending_recipient =
        match read(
            chain,
            mode,
            &queue_pda,
            "the deposit queue",
            "DepositQueueLookupFailed",
            &mut report,
        )
        .await?
        {
            Read::Found(d) => {
                let q = decode_queue(&d, &queue_pda)?;
                if q.activation_slot == 0 {
                    return Err(OpsError::chain(
                        "NoPendingParams",
                        format!(
                            "chain {} has no pending deposit parameters: nothing to activate",
                            req.chain_id
                        ),
                    ));
                }
                if let Some(now) = read_slot(chain, mode, &mut report).await? {
                    if now < q.activation_slot {
                        return Err(OpsError::chain(
                            "ActivationNotReached",
                            format!(
                                "not yet: {} slots to go (current slot {now}, activation_slot {})",
                                q.activation_slot - now,
                                q.activation_slot
                            ),
                        ));
                    }
                }
                Pubkey::new_from_array(q.pending.fee_recipient)
            }
            Read::Missing => {
                return Err(OpsError::chain(
                    "DepositQueueNotFound",
                    format!("deposit_queue {queue_pda} not found; nothing to activate"),
                ))
            }
            Read::Unavailable => return Err(OpsError::usage(
                "PendingRecipientUnknown",
                "the pending fee recipient is read from the queue, and the queue could not be read",
            )),
        };
    let ix = zk_bridge_client::activate_deposit_params_ix(
        &req.bridge,
        &req.settlement,
        req.chain_id,
        &pending_recipient,
    );
    report.line(format!(
        "-- deposit-queue activate: chain {}, settlement {}, bridge {}",
        req.chain_id, req.settlement, req.bridge
    ));
    report.line("  instruction: ActivateDepositParams");
    print_accounts(&mut report, &ix);
    let signers = Signers::new(payer, vec![]);
    if let Some(sig) = execute_many(
        chain,
        mode,
        "ActivateDepositParams",
        std::slice::from_ref(&ix),
        &signers,
        &mut report,
    )
    .await?
    {
        report.line(format!("-- sent: {sig}"));
    }
    Ok(report)
}

// ---- deposit-queue show ----

/// `deposit-queue show`: the queue's live and pending parameters, its count, the cursor's `deposit_next`, and the
/// oldest waiting deposit's age against the deadline, as `key=value` lines. Reads only; a failed read is a named
/// refusal, never a missing queue. `now_unix` is the clock the age is measured on.
pub async fn show<C: Chain>(
    chain: &C,
    settlement: &Pubkey,
    bridge: &Pubkey,
    chain_id: u64,
    now_unix: i64,
) -> Result<Report, OpsError> {
    let mut report = Report::default();
    let mode = Mode::Confirm; // strict reads
    let (queue_pda, _) = zk_bridge_client::deposit_queue_pda(bridge, settlement, chain_id);
    report.line(format!("chain_id={chain_id}"));
    report.line(format!("deposit_queue={queue_pda}"));
    let queue = match read(
        chain,
        mode,
        &queue_pda,
        "the deposit queue",
        "DepositQueueLookupFailed",
        &mut report,
    )
    .await?
    {
        Read::Found(d) => decode_queue(&d, &queue_pda)?,
        _ => {
            report.line("queue_exists=false");
            return Ok(report);
        }
    };
    report.line("queue_exists=true");
    report.line(format!("count={}", queue.count));
    print_params(&mut report, "", &queue.params);
    let slot = read_slot(chain, mode, &mut report).await?.unwrap_or(0);
    report.line(format!("slot={slot}"));
    if queue.activation_slot == 0 {
        report.line("pending=false");
    } else {
        report.line("pending=true");
        print_params(&mut report, "pending_", &queue.pending);
        report.line(format!("activation_slot={}", queue.activation_slot));
        if slot >= queue.activation_slot {
            report.line("activation_reached=true");
        } else {
            report.line("activation_reached=false");
            report.line(format!(
                "activation_slots_left={}",
                queue.activation_slot - slot
            ));
        }
    }

    // The cursor lives under the config's inbox program.
    let cfg = read_config(chain, mode, bridge, &mut report)
        .await?
        .expect("a strict read either answers or refuses");
    let inbox = Pubkey::new_from_array(cfg.inbox_program);
    let (cursor_pda, _) = zk_inbox_client::cursor_pda(&inbox, settlement, chain_id);
    let cursor = match read(
        chain,
        mode,
        &cursor_pda,
        "the batch cursor",
        "CursorLookupFailed",
        &mut report,
    )
    .await?
    {
        Read::Found(d) => Some(rome_zk_layouts::cursor::read(&d).map_err(|e| {
            OpsError::chain(
                "CursorUndecodable",
                format!("the batch cursor {cursor_pda} does not decode: {e:?}"),
            )
        })?),
        _ => None,
    };
    let Some(cursor) = cursor else {
        report.line("cursor_version=none");
        return Ok(report);
    };
    let Some(dep) = cursor.deposit else {
        report.line("cursor_version=1");
        return Ok(report);
    };
    report.line("cursor_version=2");
    report.line(format!("deposit_next={}", dep.next));
    report.line(format!("deposit_final={}", dep.final_));
    let backlog = queue.count.saturating_sub(dep.next);
    report.line(format!("backlog={backlog}"));
    if backlog == 0 {
        report.line("oldest_waiting=none");
        return Ok(report);
    }
    let (rec_pda, _) = zk_bridge_client::deposit_record_pda(bridge, settlement, chain_id, dep.next);
    match read(
        chain,
        mode,
        &rec_pda,
        "the oldest waiting deposit record",
        "DepositRecordLookupFailed",
        &mut report,
    )
    .await?
    {
        Read::Found(d) => {
            let rec = deposit_record::read(&d).map_err(|e| {
                OpsError::chain(
                    "DepositRecordUndecodable",
                    format!("deposit record {rec_pda} does not decode: {e:?}"),
                )
            })?;
            let age = now_unix.saturating_sub(rec.enqueue_unix_ts).max(0);
            report.line(format!("oldest_waiting_index={}", dep.next));
            report.line(format!(
                "oldest_waiting_enqueue_unix={}",
                rec.enqueue_unix_ts
            ));
            report.line(format!("oldest_waiting_age_secs={age}"));
            report.line(format!(
                "oldest_waiting_deadline_secs={}",
                queue.params.inclusion_deadline_secs
            ));
        }
        _ => report.line("oldest_waiting=unreadable"),
    }
    Ok(report)
}

// ---- close-deposit ----

#[derive(Debug, Clone)]
pub struct CloseDepositRequest {
    pub settlement: Pubkey,
    pub bridge: Pubkey,
    pub chain_id: u64,
    pub index: u64,
    pub payer_keypair: PathBuf,
}

/// `close-deposit`: refund a deposit record's rent to its depositor once the batch that credited it is final. The
/// refund goes to the record's sender whoever pays. The command names the program's refusals before sending.
pub async fn close_deposit<C: Chain>(
    chain: &C,
    req: CloseDepositRequest,
    mode: Mode,
) -> Result<Report, OpsError> {
    let payer = keys::load(&req.payer_keypair, "--payer-keypair")?;
    let mut report = Report::default();
    let cfg = read_config(chain, mode, &req.bridge, &mut report)
        .await?
        .ok_or_else(|| {
            OpsError::usage(
                "BridgeConfigUnknown",
                "the inbox program comes from the bridge config, and it could not be read",
            )
        })?;
    let inbox = Pubkey::new_from_array(cfg.inbox_program);

    let (rec_pda, _) =
        zk_bridge_client::deposit_record_pda(&req.bridge, &req.settlement, req.chain_id, req.index);
    let record = match read(
        chain,
        mode,
        &rec_pda,
        "the deposit record",
        "DepositRecordLookupFailed",
        &mut report,
    )
    .await?
    {
        Read::Found(d) => deposit_record::read(&d).map_err(|e| {
            OpsError::chain(
                "DepositRecordUndecodable",
                format!("deposit record {rec_pda} does not decode: {e:?}"),
            )
        })?,
        Read::Missing => {
            return Err(OpsError::chain(
                "DepositRecordNotFound",
                format!(
                    "deposit record {} of chain {} ({rec_pda}) does not exist: never made, or already closed",
                    req.index, req.chain_id
                ),
            ))
        }
        Read::Unavailable => {
            return Err(OpsError::usage(
                "DepositRecordUnknown",
                "the depositor is read from the record, and the record could not be read",
            ))
        }
    };

    let (cursor_pda, _) = zk_inbox_client::cursor_pda(&inbox, &req.settlement, req.chain_id);
    match read(
        chain,
        mode,
        &cursor_pda,
        "the batch cursor",
        "CursorLookupFailed",
        &mut report,
    )
    .await?
    {
        Read::Found(d) => {
            let cursor = rome_zk_layouts::cursor::read(&d).map_err(|e| {
                OpsError::chain(
                    "CursorUndecodable",
                    format!("the batch cursor {cursor_pda} does not decode: {e:?}"),
                )
            })?;
            let Some(dep) = cursor.deposit else {
                return Err(OpsError::chain(
                    "CursorNotV2",
                    format!(
                        "the batch cursor {cursor_pda} is version 1, so no batch has credited a deposit yet"
                    ),
                ));
            };
            if dep.next <= req.index {
                return Err(OpsError::chain(
                    "DepositNotCredited",
                    format!(
                        "no finalized batch has credited deposit {} yet (the cursor's deposit_next is {})",
                        req.index, dep.next
                    ),
                ));
            }
            if req.index >= dep.final_ {
                return Err(OpsError::chain(
                    "DepositNotFinal",
                    format!(
                        "the batch that credited deposit {} is not final yet (the cursor's deposit_final is {})",
                        req.index, dep.final_
                    ),
                ));
            }
        }
        Read::Missing => {
            return Err(OpsError::chain(
                "CursorNotFound",
                format!("the batch cursor {cursor_pda} does not exist"),
            ))
        }
        Read::Unavailable => {}
    }

    let sender = Pubkey::new_from_array(record.sender);
    let ix = zk_bridge_client::close_deposit_ix(
        &req.bridge,
        &inbox,
        &req.settlement,
        req.chain_id,
        req.index,
        &sender,
    );
    report.line(format!(
        "-- close-deposit: chain {}, record {}, settlement {}, bridge {}",
        req.chain_id, req.index, req.settlement, req.bridge
    ));
    report.line(format!("  rent goes to  {sender}"));
    report.line("  instruction: CloseDeposit");
    print_accounts(&mut report, &ix);
    let signers = Signers::new(payer, vec![]);
    if let Some(sig) = execute_many(
        chain,
        mode,
        "CloseDeposit",
        std::slice::from_ref(&ix),
        &signers,
        &mut report,
    )
    .await?
    {
        report.line(format!("-- sent: {sig}"));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::fake::*;
    use rome_zk_layouts::cursor::{self, CursorDeposit, CursorFields};
    use rome_zk_layouts::deposit_queue::deposit_queue::{DepositParams, DepositQueueFields};
    use rome_zk_layouts::deposit_queue::deposit_record::DepositRecordFields;

    const CHAIN: u64 = 4_294_967_307;
    const NOW_UNIX: i64 = 2_000_000;

    fn bridge() -> Pubkey {
        Pubkey::new_from_array([7u8; 32])
    }

    fn queue_key() -> Pubkey {
        zk_bridge_client::deposit_queue_pda(&bridge(), &program(), CHAIN).0
    }

    fn root_key() -> Pubkey {
        zk_settlement_client::root_pda(&program(), CHAIN).0
    }

    fn vault_key() -> Pubkey {
        zk_bridge_client::vault_config_pda(&bridge(), &program(), CHAIN).0
    }

    fn config_key() -> Pubkey {
        zk_bridge_client::bridge_config_pda(&bridge()).0
    }

    fn cursor_key() -> Pubkey {
        zk_inbox_client::cursor_pda(&inbox(), &program(), CHAIN).0
    }

    fn record_key(index: u64) -> Pubkey {
        zk_bridge_client::deposit_record_pda(&bridge(), &program(), CHAIN, index).0
    }

    fn config_bytes(settlement: Pubkey) -> Vec<u8> {
        let mut d = vec![0u8; bridge_config::LEN];
        bridge_config::write(
            &mut d,
            &bridge_config::BridgeConfigFields {
                settlement_program: settlement.to_bytes(),
                inbox_program: inbox().to_bytes(),
            },
        );
        d
    }

    fn vault_bytes() -> Vec<u8> {
        zk_bridge::state::vault_config::write(&zk_bridge::state::vault_config::VaultConfigFields {
            chain_id: CHAIN,
            settlement_program: program(),
            mint: Pubkey::new_unique(),
            mint_decimals: 9,
            authority: Pubkey::new_from_array([5u8; 32]),
        })
        .to_vec()
    }

    fn params(fee_recipient: Pubkey) -> DepositParams {
        DepositParams {
            inclusion_deadline_secs: 43_200,
            max_per_batch: 256,
            max_per_block: 4,
            min_amount: 1_000_000,
            fee_lamports: 100_000,
            fee_recipient: fee_recipient.to_bytes(),
        }
    }

    fn queue_bytes(
        count: u64,
        live: DepositParams,
        pending: Option<(u64, DepositParams)>,
    ) -> Vec<u8> {
        let mut d = vec![0u8; queue_layout::LEN];
        queue_layout::write(
            &mut d,
            &DepositQueueFields {
                count,
                head_hash: [0; 32],
                params: live,
                pending: pending.map(|p| p.1).unwrap_or_default(),
                activation_slot: pending.map(|p| p.0).unwrap_or(0),
            },
        );
        d
    }

    fn cursor_bytes(next: u64, final_: u64) -> Vec<u8> {
        cursor::write_v2(&CursorFields {
            chain_id: CHAIN,
            next_batch: 3,
            deposit: Some(CursorDeposit {
                next,
                hash: [0; 32],
                final_,
            }),
        })
        .unwrap()
        .to_vec()
    }

    fn cursor_v1_bytes() -> Vec<u8> {
        cursor::write(&CursorFields {
            chain_id: CHAIN,
            next_batch: 3,
            deposit: None,
        })
        .to_vec()
    }

    fn record_bytes(index: u64, ts: i64, sender: Pubkey) -> Vec<u8> {
        let mut d = vec![0u8; deposit_record::LEN];
        deposit_record::write(
            &mut d,
            &DepositRecordFields {
                index,
                enqueue_unix_ts: ts,
                sender: sender.to_bytes(),
                recipient: [1; 20],
                amount_gwei: 1_000_000,
                hash_after: [0; 32],
            },
        );
        d
    }

    /// A chain ready for `init`: config, a root the key owns that has posted, and a vault. No queue.
    fn init_chain(authority: Pubkey, head_pending: u64) -> FakeChain {
        FakeChain::default()
            .with(config_key(), config_bytes(program()))
            .with(root_key(), root_bytes(CHAIN, authority, head_pending, 0))
            .with(vault_key(), vault_bytes())
    }

    fn init_req(path: PathBuf, flags: ParamFlags) -> InitRequest {
        InitRequest {
            settlement: program(),
            bridge: bridge(),
            chain_id: CHAIN,
            authority_keypair: path,
            flags,
        }
    }

    fn name_of<T: std::fmt::Debug>(r: Result<T, OpsError>) -> &'static str {
        r.unwrap_err().name
    }

    // ---- the bounds ----

    #[test]
    fn the_ruled_defaults_pass_every_bound() {
        validate_params(&defaults(Pubkey::new_unique()), None).unwrap();
        // 4 per block x 64 blocks is exactly 256.
        validate_params(&defaults(Pubkey::new_unique()), Some(64)).unwrap();
    }

    #[test]
    fn each_bound_refuses_by_name() {
        let ok = defaults(Pubkey::new_unique());
        let with = |f: &dyn Fn(&mut DepositParamsArgs)| {
            let mut p = ok;
            f(&mut p);
            p
        };
        let name = |p: DepositParamsArgs, blocks: Option<u32>| name_of(validate_params(&p, blocks));
        assert_eq!(
            name(with(&|p| p.inclusion_deadline_secs = 3_599), None),
            "DeadlineBelowFloor"
        );
        assert_eq!(
            name(with(&|p| p.inclusion_deadline_secs = 86_401), None),
            "DeadlineAboveCeiling"
        );
        assert_eq!(
            name(with(&|p| p.max_per_batch = 257), None),
            "MaxPerBatchTooLarge"
        );
        assert_eq!(
            name(with(&|p| p.max_per_block = 0), None),
            "MaxPerBlockOutOfRange"
        );
        assert_eq!(
            name(
                with(&|p| {
                    p.max_per_batch = 8;
                    p.max_per_block = 9
                }),
                None
            ),
            "MaxPerBlockOutOfRange"
        );
        assert_eq!(name(with(&|p| p.min_amount = 0), None), "MinAmountZero");
        assert_eq!(
            name(with(&|p| p.fee_lamports = 10_000_001), None),
            "FeeTooHigh"
        );
        assert_eq!(name(ok, Some(65)), "PerBlockExceedsBatch");
        // The edges are allowed.
        validate_params(&with(&|p| p.inclusion_deadline_secs = 3_600), None).unwrap();
        validate_params(&with(&|p| p.inclusion_deadline_secs = 86_400), None).unwrap();
        validate_params(&with(&|p| p.fee_lamports = 10_000_000), None).unwrap();
        validate_params(&with(&|p| p.min_amount = 1), None).unwrap();
    }

    // ---- init ----

    #[tokio::test]
    async fn init_refuses_out_of_bounds_values_before_reading_the_chain() {
        let (_, path) = key_file("dq-bounds");
        let chain = FakeChain::default(); // empty: any read would find nothing
        for flags in [
            ParamFlags {
                deadline_secs: Some(10),
                ..Default::default()
            },
            ParamFlags {
                fee_lamports: Some(u64::MAX),
                ..Default::default()
            },
            ParamFlags {
                max_per_batch: Some(300),
                ..Default::default()
            },
            ParamFlags {
                min_amount: Some(0),
                ..Default::default()
            },
            ParamFlags {
                blocks_per_batch: Some(100),
                ..Default::default()
            },
        ] {
            let err = init(&chain, init_req(path.clone(), flags), Mode::Confirm)
                .await
                .unwrap_err();
            assert_eq!(err.exit_code, 2, "{err}");
        }
        remove(&path);
        assert_eq!(chain.sent_count(), 0);
    }

    #[tokio::test]
    async fn init_refuses_a_per_block_cap_that_does_not_fit_the_batch_cap() {
        let (k, path) = key_file("dq-fit");
        let chain = init_chain(key_pubkey(&k), 1);
        let flags = ParamFlags {
            blocks_per_batch: Some(65),
            ..Default::default()
        };
        let err = init(&chain, init_req(path.clone(), flags), Mode::Confirm).await;
        remove(&path);
        assert_eq!(name_of(err), "PerBlockExceedsBatch");
        assert_eq!(chain.sent_count(), 0);
    }

    #[tokio::test]
    async fn init_refuses_a_chain_that_never_posted_and_says_so() {
        let (k, path) = key_file("dq-never");
        let chain = init_chain(key_pubkey(&k), 0);
        let err = init(
            &chain,
            init_req(path.clone(), ParamFlags::default()),
            Mode::Confirm,
        )
        .await
        .unwrap_err();
        remove(&path);
        assert_eq!(err.name, "ChainNeverPosted");
        assert!(err.detail.contains("Post one batch first"), "{err}");
        assert_eq!(chain.sent_count(), 0);
    }

    #[tokio::test]
    async fn init_refuses_a_wrong_authority_a_missing_config_vault_and_a_foreign_settlement() {
        let (k, path) = key_file("dq-refuse");
        let me = key_pubkey(&k);
        let other = Pubkey::new_unique();
        let req = || init_req(path.clone(), ParamFlags::default());
        let chain = init_chain(other, 1);
        assert_eq!(
            name_of(init(&chain, req(), Mode::Confirm).await),
            "WrongChainAuthority"
        );
        let chain = FakeChain::default()
            .with(root_key(), root_bytes(CHAIN, me, 1, 0))
            .with(vault_key(), vault_bytes());
        assert_eq!(
            name_of(init(&chain, req(), Mode::Confirm).await),
            "BridgeConfigNotFound"
        );
        let chain = FakeChain::default()
            .with(config_key(), config_bytes(program()))
            .with(root_key(), root_bytes(CHAIN, me, 1, 0));
        assert_eq!(
            name_of(init(&chain, req(), Mode::Confirm).await),
            "VaultConfigNotFound"
        );
        let chain = init_chain(me, 1).with(config_key(), config_bytes(Pubkey::new_unique()));
        assert_eq!(
            name_of(init(&chain, req(), Mode::Confirm).await),
            "WrongSettlementProgram"
        );
        let chain = FakeChain::default()
            .with(config_key(), config_bytes(program()))
            .with(vault_key(), vault_bytes());
        assert_eq!(
            name_of(init(&chain, req(), Mode::Confirm).await),
            "ChainNotRegistered"
        );
        let chain = init_chain(me, 1).failing(queue_key(), "rpc down");
        assert_eq!(
            name_of(init(&chain, req(), Mode::Confirm).await),
            "DepositQueueLookupFailed"
        );
        remove(&path);
    }

    #[tokio::test]
    async fn init_on_an_existing_queue_reports_it_and_sends_nothing() {
        let (k, path) = key_file("dq-exists");
        let chain = init_chain(key_pubkey(&k), 1).with(
            queue_key(),
            queue_bytes(5, params(Pubkey::new_unique()), None),
        );
        let report = init(
            &chain,
            init_req(path.clone(), ParamFlags::default()),
            Mode::Confirm,
        )
        .await
        .unwrap();
        remove(&path);
        assert_eq!(chain.sent_count(), 0);
        let text = report.lines.join("\n");
        assert!(text.contains("already exists, nothing sent"), "{text}");
        assert!(text.contains("count=5"), "{text}");
    }

    #[tokio::test]
    async fn init_dry_run_prints_the_transaction_and_sends_nothing() {
        let (k, path) = key_file("dq-dry");
        let chain = init_chain(key_pubkey(&k), 1);
        let report = init(
            &chain,
            init_req(path.clone(), ParamFlags::default()),
            Mode::Dry,
        )
        .await
        .unwrap();
        remove(&path);
        assert_eq!(chain.sent_count(), 0);
        let text = report.lines.join("\n");
        assert!(text.contains("instruction: InitDepositQueue"), "{text}");
        assert!(text.contains("nothing sent to any cluster"), "{text}");
    }

    #[tokio::test]
    async fn init_confirm_sends_the_ruled_defaults_with_the_signer_as_fee_recipient() {
        let (k, path) = key_file("dq-send");
        let me = key_pubkey(&k);
        let chain = init_chain(me, 1);
        let report = init(
            &chain,
            init_req(path.clone(), ParamFlags::default()),
            Mode::Confirm,
        )
        .await
        .unwrap();
        remove(&path);
        assert!(report.sent());
        assert_eq!(chain.tx_count(), 1);
        let sent = chain.sent.lock().unwrap();
        let ix = &sent[0];
        assert_eq!(
            *ix,
            zk_bridge_client::init_deposit_queue_ix(
                &bridge(),
                &me,
                &me,
                &program(),
                CHAIN,
                DepositParamsArgs {
                    inclusion_deadline_secs: 43_200,
                    max_per_batch: 256,
                    max_per_block: 4,
                    min_amount: 1_000_000,
                    fee_lamports: 100_000,
                    fee_recipient: me,
                }
            )
        );
        // The program reads nine accounts, the fee recipient in the seventh place.
        assert_eq!(ix.accounts.len(), 9);
        assert_eq!(ix.accounts[6].pubkey, me);
        assert_eq!(ix.accounts[7].pubkey, queue_key());
    }

    // ---- propose ----

    fn propose_req(path: PathBuf) -> ProposeRequest {
        ProposeRequest {
            settlement: program(),
            bridge: bridge(),
            chain_id: CHAIN,
            authority_keypair: path,
            flags: ParamFlags::default(),
            activation_slot: None,
            activation_delay_slots: None,
        }
    }

    /// Slot 1000, challenge window 100: the allowed range is 1100 to 1200.
    fn propose_chain(authority: Pubkey, pending: Option<(u64, DepositParams)>) -> FakeChain {
        init_chain(authority, 1).with(queue_key(), queue_bytes(0, params(authority), pending))
    }

    #[tokio::test]
    async fn propose_refuses_an_activation_outside_one_to_two_windows_by_name() {
        let (k, path) = key_file("dq-window");
        let chain = propose_chain(key_pubkey(&k), None);
        for (slot, want) in [(1_099, "ActivationTooSoon"), (1_201, "ActivationTooLate")] {
            let req = ProposeRequest {
                activation_slot: Some(slot),
                ..propose_req(path.clone())
            };
            assert_eq!(name_of(propose(&chain, req, Mode::Confirm).await), want);
        }
        for ok in [1_100, 1_200] {
            let req = ProposeRequest {
                activation_slot: Some(ok),
                ..propose_req(path.clone())
            };
            propose(&chain, req, Mode::Dry).await.unwrap();
        }
        remove(&path);
        assert_eq!(chain.sent_count(), 0);
    }

    #[tokio::test]
    async fn propose_refuses_the_bounds_and_conflicting_flags_by_name() {
        let (k, path) = key_file("dq-pbounds");
        let chain = propose_chain(key_pubkey(&k), None);
        let req = ProposeRequest {
            flags: ParamFlags {
                deadline_secs: Some(100_000),
                ..Default::default()
            },
            ..propose_req(path.clone())
        };
        assert_eq!(
            name_of(propose(&chain, req, Mode::Confirm).await),
            "DeadlineAboveCeiling"
        );
        let req = ProposeRequest {
            flags: ParamFlags {
                blocks_per_batch: Some(100),
                ..Default::default()
            },
            ..propose_req(path.clone())
        };
        assert_eq!(
            name_of(propose(&chain, req, Mode::Confirm).await),
            "PerBlockExceedsBatch"
        );
        let req = ProposeRequest {
            activation_slot: Some(1_150),
            activation_delay_slots: Some(150),
            ..propose_req(path.clone())
        };
        assert_eq!(
            name_of(propose(&chain, req, Mode::Confirm).await),
            "ActivationFlagsConflict"
        );
        let empty = FakeChain::default();
        assert_eq!(
            name_of(propose(&empty, propose_req(path.clone()), Mode::Confirm).await),
            "DepositQueueNotFound"
        );
        remove(&path);
        assert_eq!(chain.sent_count(), 0);
    }

    #[tokio::test]
    async fn propose_defaults_to_one_window_plus_a_margin_and_keeps_the_live_values() {
        let (k, path) = key_file("dq-pdef");
        let me = key_pubkey(&k);
        let chain = propose_chain(me, None);
        let req = ProposeRequest {
            flags: ParamFlags {
                max_per_block: Some(2),
                ..Default::default()
            },
            ..propose_req(path.clone())
        };
        propose(&chain, req, Mode::Confirm).await.unwrap();
        remove(&path);
        let sent = chain.sent.lock().unwrap();
        assert_eq!(
            sent[0],
            zk_bridge_client::propose_deposit_params_ix(
                &bridge(),
                &me,
                &program(),
                CHAIN,
                1_000 + 100 + default_margin_slots(100),
                DepositParamsArgs {
                    inclusion_deadline_secs: 43_200,
                    max_per_batch: 256,
                    max_per_block: 2,
                    min_amount: 1_000_000,
                    fee_lamports: 100_000,
                    fee_recipient: me,
                }
            )
        );
        assert_eq!(sent[0].accounts.len(), 5);
    }

    #[tokio::test]
    async fn propose_says_so_when_it_replaces_a_pending_proposal() {
        let (k, path) = key_file("dq-replace");
        let me = key_pubkey(&k);
        let chain = propose_chain(me, Some((1_150, params(me))));
        let req = ProposeRequest {
            activation_delay_slots: Some(150),
            ..propose_req(path.clone())
        };
        let report = propose(&chain, req, Mode::Dry).await.unwrap();
        remove(&path);
        let text = report.lines.join("\n");
        assert!(
            text.contains("already pending (activation slot 1150); this one replaces it"),
            "{text}"
        );
        assert!(text.contains("activation_slot  1150"), "{text}");
    }

    // ---- activate ----

    fn activate_req(path: PathBuf) -> ActivateRequest {
        ActivateRequest {
            settlement: program(),
            bridge: bridge(),
            chain_id: CHAIN,
            payer_keypair: path,
        }
    }

    #[tokio::test]
    async fn activate_refuses_by_name_with_nothing_pending_or_too_early_or_no_queue() {
        let (k, path) = key_file("dq-act-no");
        let me = key_pubkey(&k);
        let none = FakeChain::default().with(queue_key(), queue_bytes(0, params(me), None));
        assert_eq!(
            name_of(activate(&none, activate_req(path.clone()), Mode::Confirm).await),
            "NoPendingParams"
        );
        let early = FakeChain::default().with(
            queue_key(),
            queue_bytes(0, params(me), Some((1_040, params(me)))),
        );
        let err = activate(&early, activate_req(path.clone()), Mode::Confirm)
            .await
            .unwrap_err();
        assert_eq!(err.name, "ActivationNotReached");
        assert!(err.detail.contains("40 slots to go"), "{err}");
        assert_eq!(
            name_of(
                activate(
                    &FakeChain::default(),
                    activate_req(path.clone()),
                    Mode::Confirm
                )
                .await
            ),
            "DepositQueueNotFound"
        );
        remove(&path);
        assert_eq!(none.sent_count() + early.sent_count(), 0);
    }

    #[tokio::test]
    async fn activate_sends_once_the_slot_has_passed_naming_the_pending_fee_recipient() {
        let (_, path) = key_file("dq-act-ok");
        let new_recipient = Pubkey::new_unique();
        let chain = FakeChain::default().with(
            queue_key(),
            queue_bytes(
                0,
                params(Pubkey::new_unique()),
                Some((900, params(new_recipient))),
            ),
        );
        let dry = activate(&chain, activate_req(path.clone()), Mode::Dry)
            .await
            .unwrap();
        assert!(dry
            .lines
            .join("\n")
            .contains("instruction: ActivateDepositParams"));
        assert_eq!(chain.sent_count(), 0);
        let report = activate(&chain, activate_req(path.clone()), Mode::Confirm)
            .await
            .unwrap();
        remove(&path);
        assert!(report.sent());
        let sent = chain.sent.lock().unwrap();
        assert_eq!(
            sent[0],
            zk_bridge_client::activate_deposit_params_ix(
                &bridge(),
                &program(),
                CHAIN,
                &new_recipient
            )
        );
        assert_eq!(sent[0].accounts.len(), 3);
        assert_eq!(sent[0].accounts[2].pubkey, new_recipient);
    }

    // ---- show ----

    fn text_of(r: &Report) -> String {
        r.lines.join("\n")
    }

    #[tokio::test]
    async fn show_says_when_there_is_no_queue_and_names_a_failed_read() {
        let chain = FakeChain::default();
        let r = show(&chain, &program(), &bridge(), CHAIN, NOW_UNIX)
            .await
            .unwrap();
        assert!(text_of(&r).contains("queue_exists=false"));
        let chain = FakeChain::default().failing(queue_key(), "rpc down");
        assert_eq!(
            name_of(show(&chain, &program(), &bridge(), CHAIN, NOW_UNIX).await),
            "DepositQueueLookupFailed"
        );
    }

    #[tokio::test]
    async fn show_prints_parameters_pending_cursor_backlog_and_the_oldest_waiting_age() {
        let me = Pubkey::new_unique();
        let sender = Pubkey::new_unique();
        let mut pending = params(me);
        pending.max_per_block = 2;
        let chain = FakeChain::default()
            .with(
                queue_key(),
                queue_bytes(7, params(me), Some((1_040, pending))),
            )
            .with(config_key(), config_bytes(program()))
            .with(cursor_key(), cursor_bytes(4, 2))
            .with(record_key(4), record_bytes(4, NOW_UNIX - 21_600, sender));
        let r = show(&chain, &program(), &bridge(), CHAIN, NOW_UNIX)
            .await
            .unwrap();
        let t = text_of(&r);
        for want in [
            "queue_exists=true",
            "count=7",
            "inclusion_deadline_secs=43200",
            "max_per_block=4",
            "pending=true",
            "pending_max_per_block=2",
            "activation_slot=1040",
            "activation_reached=false",
            "activation_slots_left=40",
            "cursor_version=2",
            "deposit_next=4",
            "deposit_final=2",
            "backlog=3",
            "oldest_waiting_index=4",
            "oldest_waiting_age_secs=21600",
            "oldest_waiting_deadline_secs=43200",
        ] {
            assert!(t.lines().any(|l| l == want), "missing `{want}` in:\n{t}");
        }
        assert_eq!(chain.sent_count(), 0);
    }

    #[tokio::test]
    async fn show_with_an_empty_backlog_a_v1_cursor_or_no_cursor() {
        let me = Pubkey::new_unique();
        let base = || {
            FakeChain::default()
                .with(queue_key(), queue_bytes(7, params(me), None))
                .with(config_key(), config_bytes(program()))
        };
        let empty = base().with(cursor_key(), cursor_bytes(7, 7));
        let t = text_of(
            &show(&empty, &program(), &bridge(), CHAIN, NOW_UNIX)
                .await
                .unwrap(),
        );
        assert!(
            t.contains("pending=false")
                && t.contains("backlog=0")
                && t.contains("oldest_waiting=none"),
            "{t}"
        );
        let v1 = base().with(cursor_key(), cursor_v1_bytes());
        let t = text_of(
            &show(&v1, &program(), &bridge(), CHAIN, NOW_UNIX)
                .await
                .unwrap(),
        );
        assert!(t.contains("cursor_version=1"), "{t}");
        let t = text_of(
            &show(&base(), &program(), &bridge(), CHAIN, NOW_UNIX)
                .await
                .unwrap(),
        );
        assert!(t.contains("cursor_version=none"), "{t}");
    }

    // ---- close-deposit ----

    fn close_req(path: PathBuf, index: u64) -> CloseDepositRequest {
        CloseDepositRequest {
            settlement: program(),
            bridge: bridge(),
            chain_id: CHAIN,
            index,
            payer_keypair: path,
        }
    }

    fn close_chain(cursor: Vec<u8>, sender: Pubkey) -> FakeChain {
        FakeChain::default()
            .with(config_key(), config_bytes(program()))
            .with(cursor_key(), cursor)
            .with(record_key(3), record_bytes(3, 10, sender))
    }

    #[tokio::test]
    async fn close_deposit_refuses_by_name_before_the_batch_is_final() {
        let (_, path) = key_file("dq-close-no");
        let sender = Pubkey::new_unique();
        let req = || close_req(path.clone(), 3);
        let not_credited = close_chain(cursor_bytes(3, 3), sender);
        assert_eq!(
            name_of(close_deposit(&not_credited, req(), Mode::Confirm).await),
            "DepositNotCredited"
        );
        let not_final = close_chain(cursor_bytes(9, 3), sender);
        assert_eq!(
            name_of(close_deposit(&not_final, req(), Mode::Confirm).await),
            "DepositNotFinal"
        );
        let v1 = close_chain(cursor_v1_bytes(), sender);
        assert_eq!(
            name_of(close_deposit(&v1, req(), Mode::Confirm).await),
            "CursorNotV2"
        );
        let closed = close_chain(cursor_bytes(9, 9), sender);
        assert_eq!(
            name_of(close_deposit(&closed, close_req(path.clone(), 4), Mode::Confirm).await),
            "DepositRecordNotFound"
        );
        let no_cursor = FakeChain::default()
            .with(config_key(), config_bytes(program()))
            .with(record_key(3), record_bytes(3, 10, sender));
        assert_eq!(
            name_of(close_deposit(&no_cursor, req(), Mode::Confirm).await),
            "CursorNotFound"
        );
        let no_config = FakeChain::default();
        assert_eq!(
            name_of(close_deposit(&no_config, req(), Mode::Confirm).await),
            "BridgeConfigNotFound"
        );
        remove(&path);
        assert_eq!(
            not_credited.sent_count() + not_final.sent_count() + v1.sent_count(),
            0
        );
    }

    #[tokio::test]
    async fn close_deposit_sends_one_close_that_refunds_the_records_sender() {
        let (_, path) = key_file("dq-close-ok");
        let sender = Pubkey::new_unique();
        let chain = close_chain(cursor_bytes(9, 9), sender);
        let dry = close_deposit(&chain, close_req(path.clone(), 3), Mode::Dry)
            .await
            .unwrap();
        assert_eq!(chain.sent_count(), 0);
        assert!(text_of(&dry).contains(&format!("rent goes to  {sender}")));
        let report = close_deposit(&chain, close_req(path.clone(), 3), Mode::Confirm)
            .await
            .unwrap();
        remove(&path);
        assert!(report.sent());
        let sent = chain.sent.lock().unwrap();
        assert_eq!(
            sent[0],
            zk_bridge_client::close_deposit_ix(&bridge(), &inbox(), &program(), CHAIN, 3, &sender)
        );
        assert_eq!(sent[0].accounts.len(), 5);
        assert_eq!(sent[0].accounts[3].pubkey, record_key(3));
        assert_eq!(sent[0].accounts[4].pubkey, sender);
    }
}
