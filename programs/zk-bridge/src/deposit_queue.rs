//! A chain's deposit queue: `InitDepositQueue`, `ProposeDepositParams` and `ActivateDepositParams`.
//!
//! The queue lives at `["deposit_queue", settlement_program, chain_id]` under this program, where
//! `settlement_program` is always the one in the bridge config. A queue exists only for a chain whose
//! `root` and `registry` sit under that settlement program (owned by it) and whose registry names the
//! config's inbox, so a queue at that address proves the chain settles through the canonical programs.
//!
//! Parameter bounds are fixed in this program and change only with an upgrade: a deadline between 1 and 24
//! hours, `1 <= max_per_block <= max_per_batch <= 256`, a minimum amount of at least 1 base unit, a fee of
//! at most 0.01 SOL, and a fee recipient that holds the rent-exempt minimum for an empty account and is
//! neither executable nor a sysvar (checked again when a proposal activates). The
//! chain authority changes the parameters the way it changes its exit config: a proposal with an
//! activation slot at least one challenge window away, then a permissionless activation.

use crate::bridge_config;
use crate::errors::BridgeError;
use crate::instruction::{
    ActivateDepositParamsArgs, DepositParamsArgs, InitDepositQueueArgs, ProposeDepositParamsArgs,
};
use crate::state::{vault_config, vault_config_pda};
use rome_zk_layouts::deposit_queue::deposit_queue::{self, DepositParams, DepositQueueFields};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    keccak,
    program_error::ProgramError,
    pubkey::Pubkey,
    rent::Rent,
    sysvar::Sysvar,
};
use solana_system_interface::program as system_program;

/// The shortest inclusion deadline: 1 hour.
pub const MIN_DEADLINE_SECS: u32 = 3_600;
/// The longest inclusion deadline: 24 hours.
pub const MAX_DEADLINE_SECS: u32 = 86_400;
/// The most deposits one batch may take.
pub const MAX_PER_BATCH_CEILING: u16 = 256;
/// The smallest minimum amount, in the vault mint's base units.
pub const MIN_AMOUNT_FLOOR: u64 = 1;
/// The highest fee: 0.01 SOL.
pub const MAX_FEE_LAMPORTS: u64 = 10_000_000;
/// The most decimals the vault mint may have.
pub const MAX_MINT_DECIMALS: u8 = 9;

/// The keccak syscall as a `rome_zk_layouts::HashV`, so the queue's hash chain is computed on chain by
/// the one definition in `rome_zk_layouts::deposit`.
pub fn syscall_keccak(parts: &[&[u8]]) -> [u8; 32] {
    keccak::hashv(parts).to_bytes()
}

fn to_layout(p: &DepositParamsArgs) -> DepositParams {
    DepositParams {
        inclusion_deadline_secs: p.inclusion_deadline_secs,
        max_per_batch: p.max_per_batch,
        max_per_block: p.max_per_block,
        min_amount: p.min_amount,
        fee_lamports: p.fee_lamports,
        fee_recipient: p.fee_recipient.to_bytes(),
    }
}

/// The program's fixed bounds on one parameter set, plus the fee recipient's rent exemption.
pub fn check_params(p: &DepositParamsArgs, fee_recipient: &AccountInfo) -> ProgramResult {
    if p.inclusion_deadline_secs < MIN_DEADLINE_SECS {
        return Err(BridgeError::DeadlineBelowFloor.into());
    }
    if p.inclusion_deadline_secs > MAX_DEADLINE_SECS {
        return Err(BridgeError::DeadlineAboveCeiling.into());
    }
    if p.max_per_batch > MAX_PER_BATCH_CEILING {
        return Err(BridgeError::MaxPerBatchTooLarge.into());
    }
    if p.max_per_block == 0 || p.max_per_block > p.max_per_batch {
        return Err(BridgeError::MaxPerBlockOutOfRange.into());
    }
    if (p.min_amount) < MIN_AMOUNT_FLOOR {
        return Err(BridgeError::MinAmountZero.into());
    }
    if p.fee_lamports > MAX_FEE_LAMPORTS {
        return Err(BridgeError::FeeTooHigh.into());
    }
    check_fee_recipient(fee_recipient, &p.fee_recipient)
}

/// The fee recipient account is the key the parameters name, holds the rent-exempt minimum for an empty
/// account, and can take lamports: not executable and not a sysvar. Run when parameters are set and again
/// when a proposal activates, because the account can change between the two.
pub fn check_fee_recipient(fee_recipient: &AccountInfo, expected: &Pubkey) -> ProgramResult {
    if fee_recipient.key != expected || fee_recipient.lamports() < Rent::get()?.minimum_balance(0) {
        return Err(BridgeError::FeeRecipientNotRentExempt.into());
    }
    if fee_recipient.executable || *fee_recipient.owner == solana_sdk_ids::sysvar::id() {
        return Err(BridgeError::FeeRecipientNotPlain.into());
    }
    Ok(())
}

/// Checks `root_acc` is the chain's `["root", chain_id]` account under `settlement_program` and is owned by
/// it. A reclaimed chain has no root, and nothing can credit or refund a deposit made there.
pub fn require_root(
    settlement_program: &Pubkey,
    chain_id: u64,
    root_acc: &AccountInfo,
) -> ProgramResult {
    let (expect_root, _) = rome_zk_layouts::root::pda(settlement_program, chain_id);
    if expect_root != *root_acc.key || root_acc.owner != settlement_program {
        return Err(BridgeError::RootNotCanonical.into());
    }
    let d = root_acc.try_borrow_data()?;
    match rome_zk_layouts::root::read(&d) {
        Ok(root) if root.chain_id == chain_id => Ok(()),
        _ => Err(BridgeError::RootNotCanonical.into()),
    }
}

/// Checks `registry_acc` is the chain's registry under `settlement_program`, owned by it and holding this
/// chain's registry, and returns its header.
pub fn load_registry(
    settlement_program: &Pubkey,
    chain_id: u64,
    registry_acc: &AccountInfo,
) -> Result<rome_zk_layouts::registry::RegistryHeader, ProgramError> {
    let (expect_registry, _) = rome_zk_layouts::registry::pda(settlement_program, chain_id);
    if expect_registry != *registry_acc.key || registry_acc.owner != settlement_program {
        return Err(BridgeError::RegistryNotCanonical.into());
    }
    let registry = {
        let d = registry_acc.try_borrow_data()?;
        rome_zk_layouts::registry::read_header(&d)
            .map_err(|_| ProgramError::from(BridgeError::RegistryNotCanonical))?
    };
    if registry.chain_id != chain_id {
        return Err(BridgeError::RegistryNotCanonical.into());
    }
    Ok(registry)
}

/// Checks `root` is at the config's settlement program's `["root", chain_id]` PDA, owned by that program,
/// and that `chain_authority` signed and is its authority. Returns the root's challenge window.
fn require_chain_authority(
    settlement_program: &Pubkey,
    chain_id: u64,
    root_acc: &AccountInfo,
    chain_authority: &AccountInfo,
) -> Result<u32, ProgramError> {
    let (expect_root, _) = rome_zk_layouts::root::pda(settlement_program, chain_id);
    if expect_root != *root_acc.key || root_acc.owner != settlement_program {
        return Err(BridgeError::RootNotCanonical.into());
    }
    let root = {
        let d = root_acc.try_borrow_data()?;
        rome_zk_layouts::root::read(&d).map_err(|_| ProgramError::InvalidAccountData)?
    };
    if root.chain_id != chain_id {
        return Err(ProgramError::InvalidAccountData);
    }
    if !chain_authority.is_signer || Pubkey::new_from_array(root.authority) != *chain_authority.key
    {
        return Err(BridgeError::NotChainAuthority.into());
    }
    Ok(root.challenge_window_slots)
}

/// Checks `queue_acc` is the chain's queue under this program, owned by it, and decodes it.
pub fn load_queue(
    program_id: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
    queue_acc: &AccountInfo,
) -> Result<DepositQueueFields, ProgramError> {
    let (expect, _) = deposit_queue::pda(program_id, &settlement_program.to_bytes(), chain_id);
    if expect != *queue_acc.key || queue_acc.owner != program_id {
        return Err(BridgeError::WrongDepositQueue.into());
    }
    let d = queue_acc.try_borrow_data()?;
    deposit_queue::read(&d).map_err(|_| BridgeError::WrongDepositQueue.into())
}

/// accounts: `[payer (signer, writable), chain_authority (signer), bridge_config (read-only), root
/// (read-only), registry (read-only), vault_config (read-only), fee_recipient (read-only), deposit_queue
/// (writable, NEW), system_program]`.
pub fn init_deposit_queue(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    args: InitDepositQueueArgs,
) -> ProgramResult {
    let payer = next_account_info(it)?;
    let chain_authority = next_account_info(it)?;
    let config_acc = next_account_info(it)?;
    let root_acc = next_account_info(it)?;
    let registry_acc = next_account_info(it)?;
    let vault_config_acc = next_account_info(it)?;
    let fee_recipient_acc = next_account_info(it)?;
    let queue_acc = next_account_info(it)?;
    let sys = next_account_info(it)?;

    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *sys.key != system_program::id() {
        return Err(ProgramError::IncorrectProgramId);
    }
    let config = bridge_config::load(program_id, config_acc)?;
    let settlement_program = Pubkey::new_from_array(config.settlement_program);
    if args.settlement_program != settlement_program {
        return Err(BridgeError::WrongSettlementProgram.into());
    }
    if rome_zk_layouts::chainid::is_reserved(args.chain_id) {
        return Err(BridgeError::ReservedChainId.into());
    }
    require_chain_authority(
        &settlement_program,
        args.chain_id,
        root_acc,
        chain_authority,
    )?;

    // The registry belongs to the canonical settlement program, and it must name the canonical inbox.
    let registry = load_registry(&settlement_program, args.chain_id, registry_acc)?;
    if registry.inbox_program != config.inbox_program {
        return Err(BridgeError::WrongInboxProgram.into());
    }

    // The chain's vault must exist, and its mint must fit a gwei amount.
    let (expect_vault_config, _) = vault_config_pda(program_id, &settlement_program, args.chain_id);
    if expect_vault_config != *vault_config_acc.key || vault_config_acc.owner != program_id {
        return Err(BridgeError::WrongVaultConfig.into());
    }
    let vault = {
        let d = vault_config_acc.try_borrow_data()?;
        vault_config::read(&d).map_err(|_| ProgramError::from(BridgeError::WrongVaultConfig))?
    };
    if vault.mint_decimals > MAX_MINT_DECIMALS {
        return Err(BridgeError::MintTooManyDecimals.into());
    }

    check_params(&args.params, fee_recipient_acc)?;

    let (expect_queue, bump) =
        deposit_queue::pda(program_id, &config.settlement_program, args.chain_id);
    if expect_queue != *queue_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }
    // Checked before create-or-adopt, which cannot tell "already written" from "pre-funded".
    if queue_acc.owner == program_id && queue_acc.data_len() != 0 {
        return Err(BridgeError::QueueAlreadyInitialized.into());
    }
    let seeds = deposit_queue::seeds(&config.settlement_program, args.chain_id);
    rome_zk_pda::create_or_adopt_pda(
        payer,
        queue_acc,
        sys,
        program_id,
        deposit_queue::LEN,
        &[&seeds[0], &seeds[1], &seeds[2], &[bump]],
    )?;
    let fields = DepositQueueFields {
        count: 0,
        head_hash: rome_zk_layouts::deposit::queue_seed_hash(
            &syscall_keccak,
            &config.settlement_program,
            args.chain_id,
        ),
        params: to_layout(&args.params),
        pending: DepositParams::default(),
        activation_slot: 0,
    };
    let mut d = queue_acc.try_borrow_mut_data()?;
    deposit_queue::write(&mut d, &fields);
    solana_program::msg!(
        "zk-bridge: deposit queue created for chain {}",
        args.chain_id
    );
    Ok(())
}

/// accounts: `[chain_authority (signer), bridge_config (read-only), root (read-only), deposit_queue
/// (writable), fee_recipient (read-only)]`.
pub fn propose_deposit_params(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    args: ProposeDepositParamsArgs,
) -> ProgramResult {
    let chain_authority = next_account_info(it)?;
    let config_acc = next_account_info(it)?;
    let root_acc = next_account_info(it)?;
    let queue_acc = next_account_info(it)?;
    let fee_recipient_acc = next_account_info(it)?;

    let config = bridge_config::load(program_id, config_acc)?;
    let settlement_program = Pubkey::new_from_array(config.settlement_program);
    let challenge_window = require_chain_authority(
        &settlement_program,
        args.chain_id,
        root_acc,
        chain_authority,
    )?;
    let mut fields = load_queue(program_id, &settlement_program, args.chain_id, queue_acc)?;

    if challenge_window == 0 {
        return Err(BridgeError::ChallengeWindowZero.into());
    }
    let now = Clock::get()?.slot;
    if args.activation_slot < now.saturating_add(challenge_window as u64) {
        return Err(BridgeError::ActivationTooSoon.into());
    }
    // No cancel path: a pending proposal is applied or superseded only after it activates.
    if fields.activation_slot != 0 {
        return Err(BridgeError::PendingParamsExist.into());
    }
    check_params(&args.params, fee_recipient_acc)?;

    fields.pending = to_layout(&args.params);
    fields.activation_slot = args.activation_slot;
    let mut d = queue_acc.try_borrow_mut_data()?;
    deposit_queue::write(&mut d, &fields);
    solana_program::msg!(
        "zk-bridge: chain {} deposit parameters proposed, activation slot {}",
        args.chain_id,
        args.activation_slot
    );
    Ok(())
}

/// accounts: `[bridge_config (read-only), deposit_queue (writable), fee_recipient (read-only)]`.
/// Permissionless: only the clock, the pending state a proposal wrote and the fee recipient's own account
/// are trusted. The fee recipient is checked again here, since it can have changed since the proposal.
pub fn activate_deposit_params(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    args: ActivateDepositParamsArgs,
) -> ProgramResult {
    let config_acc = next_account_info(it)?;
    let queue_acc = next_account_info(it)?;
    let fee_recipient_acc = next_account_info(it)?;

    let config = bridge_config::load(program_id, config_acc)?;
    let settlement_program = Pubkey::new_from_array(config.settlement_program);
    let mut fields = load_queue(program_id, &settlement_program, args.chain_id, queue_acc)?;

    if fields.activation_slot == 0 {
        return Err(BridgeError::NoPendingParams.into());
    }
    if Clock::get()?.slot < fields.activation_slot {
        return Err(BridgeError::ActivationNotReached.into());
    }
    check_fee_recipient(
        fee_recipient_acc,
        &Pubkey::new_from_array(fields.pending.fee_recipient),
    )?;
    fields.params = fields.pending;
    fields.pending = DepositParams::default();
    fields.activation_slot = 0;
    let mut d = queue_acc.try_borrow_mut_data()?;
    deposit_queue::write(&mut d, &fields);
    solana_program::msg!(
        "zk-bridge: chain {} deposit parameters activated",
        args.chain_id
    );
    Ok(())
}
