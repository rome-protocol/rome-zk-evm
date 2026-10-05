//! `Deposit` — locks the vault's mint in the vault, pays the queue's fee, and appends one record to the
//! chain's deposit queue. The chain credits the record's amount, in gwei, to the L2 recipient once a batch
//! takes the record in.
//!
//! Every account is bound by address, so a caller cannot swap in a look-alike:
//! - `vault_config` sits at `vault_config_pda(program, sp, chain_id)`, where `sp` is the settlement program
//!   the vault config itself names;
//! - the queue sits at `["deposit_queue", sp, chain_id]` under this program. `InitDepositQueue` creates
//!   queues only under the bridge config's settlement program, so a queue at that address proves `sp` is
//!   the canonical one; a vault initialised under any other settlement program finds no queue there;
//! - the chain's `root`, `registry` and `exit_config` sit at their PDAs under `sp` and are owned by it. A
//!   chain whose root or registry is gone takes no deposits, since nothing could credit or refund them,
//!   and neither does a chain whose root has never taken a posted batch (such a chain can be reclaimed by
//!   anyone; one that has posted never can). The registry must name the bridge config's inbox, the same
//!   check `CloseDeposit` makes;
//! - the record sits at `["deposit", sp, chain_id, count]` and is created, or adopted if its address was
//!   pre-funded.
//!
//! The record's sender, and the leaf the hash chain commits to, is the depositor's wallet key, never the
//! token account's address.

use crate::bridge_config;
use crate::deposit_queue::{load_queue, load_registry, require_root, syscall_keccak};
use crate::errors::BridgeError;
use crate::instruction::DepositArgs;
use crate::state::{vault_config, vault_config_pda, vault_token_pda};
use rome_zk_layouts::deposit;
use rome_zk_layouts::deposit_queue::{deposit_queue, deposit_record};
use rome_zk_layouts::exit::exit_config;
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    program::invoke,
    program_error::ProgramError,
    pubkey::Pubkey,
    sysvar::Sysvar,
};
use solana_system_interface::{instruction as system_instruction, program as system_program};

/// accounts: `[depositor (signer, writable), depositor_token (writable), vault_config (read-only),
/// vault_token (writable), deposit_queue (writable), deposit_record (writable, NEW), exit_config
/// (read-only), fee_recipient (writable), token_program, system_program, root (read-only), registry
/// (read-only), bridge_config (read-only)]`.
pub fn deposit(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    args: DepositArgs,
) -> ProgramResult {
    let depositor = next_account_info(it)?;
    let depositor_token = next_account_info(it)?;
    let vault_config_acc = next_account_info(it)?;
    let vault_token_acc = next_account_info(it)?;
    let queue_acc = next_account_info(it)?;
    let record_acc = next_account_info(it)?;
    let exit_config_acc = next_account_info(it)?;
    let fee_recipient_acc = next_account_info(it)?;
    let token_program_acc = next_account_info(it)?;
    let sys = next_account_info(it)?;
    let root_acc = next_account_info(it)?;
    let registry_acc = next_account_info(it)?;
    let config_acc = next_account_info(it)?;

    if !depositor.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *token_program_acc.key != crate::token::TOKEN_PROGRAM_ID || *sys.key != system_program::id()
    {
        return Err(ProgramError::IncorrectProgramId);
    }

    // The vault: at its own address under this program, for this chain.
    if vault_config_acc.owner != program_id {
        return Err(BridgeError::WrongVaultConfig.into());
    }
    let vault = {
        let d = vault_config_acc.try_borrow_data()?;
        vault_config::read(&d).map_err(|_| ProgramError::from(BridgeError::WrongVaultConfig))?
    };
    let settlement_program = vault.settlement_program;
    let (expect_vault_config, _) = vault_config_pda(program_id, &settlement_program, args.chain_id);
    if vault.chain_id != args.chain_id || expect_vault_config != *vault_config_acc.key {
        return Err(BridgeError::WrongVaultConfig.into());
    }
    // The queue, and with it the proof that `settlement_program` is the canonical one.
    let mut queue = load_queue(program_id, &settlement_program, args.chain_id, queue_acc)?;

    // The vault's token account for this chain and mint.
    let (expect_vault_token, _) =
        vault_token_pda(program_id, &settlement_program, args.chain_id, &vault.mint);
    if expect_vault_token != *vault_token_acc.key {
        return Err(BridgeError::WrongVaultToken.into());
    }

    // The chain is live under that settlement program: root and registry exist and belong to it.
    require_root(&settlement_program, args.chain_id, root_acc)?;
    let registry = load_registry(&settlement_program, args.chain_id, registry_acc)?;
    let config = bridge_config::load(program_id, config_acc)?;
    if registry.inbox_program != config.inbox_program {
        return Err(BridgeError::WrongInboxProgram.into());
    }

    // The chain's exit config names this program as its bridge, and the recipient is not the portal.
    let (expect_exit_config, _) = exit_config::pda(&settlement_program, args.chain_id);
    if expect_exit_config != *exit_config_acc.key || exit_config_acc.owner != &settlement_program {
        return Err(BridgeError::ExitConfigNotCanonical.into());
    }
    let exit = {
        let d = exit_config_acc.try_borrow_data()?;
        exit_config::read(&d)
            .map_err(|_| ProgramError::from(BridgeError::ExitConfigNotCanonical))?
    };
    if exit.chain_id != args.chain_id {
        return Err(BridgeError::ExitConfigNotCanonical.into());
    }
    if exit.bridge_program != program_id.to_bytes() {
        return Err(BridgeError::NotChainsBridge.into());
    }
    if args.l2_recipient == [0u8; 20] || args.l2_recipient == exit.exit_portal {
        return Err(BridgeError::DepositRecipientInvalid.into());
    }

    // The queue's active parameters: the minimum and the fee.
    let params = queue.params;
    if fee_recipient_acc.key.to_bytes() != params.fee_recipient {
        return Err(BridgeError::WrongFeeRecipient.into());
    }
    if args.amount < params.min_amount {
        return Err(BridgeError::DepositBelowMinimum.into());
    }
    let amount_gwei = rome_zk_layouts::deposit_queue::amount_gwei(args.amount, vault.mint_decimals)
        .map_err(|e| match e {
            rome_zk_layouts::deposit_queue::AmountError::DecimalsAbove9 => {
                ProgramError::from(BridgeError::MintTooManyDecimals)
            }
            rome_zk_layouts::deposit_queue::AmountError::Overflow => {
                ProgramError::from(BridgeError::AmountConversionOverflow)
            }
        })?;

    // The record for the next index, at its own address.
    let index = queue.count;
    let next_count = index
        .checked_add(1)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let (expect_record, bump) = deposit_record::pda(
        program_id,
        &settlement_program.to_bytes(),
        args.chain_id,
        index,
    );
    if expect_record != *record_acc.key {
        return Err(BridgeError::WrongDepositRecord.into());
    }
    // Checked before create-or-adopt, which cannot tell "already written" from "pre-funded".
    if record_acc.owner == program_id && record_acc.data_len() != 0 {
        return Err(BridgeError::DepositRecordInUse.into());
    }

    // The tokens, exactly as `Fund` moves them.
    invoke(
        &crate::token::transfer_ix(
            depositor_token.key,
            vault_token_acc.key,
            depositor.key,
            args.amount,
        ),
        &[
            depositor_token.clone(),
            vault_token_acc.clone(),
            depositor.clone(),
        ],
    )?;

    // The fee.
    if params.fee_lamports > 0 {
        invoke(
            &system_instruction::transfer(
                depositor.key,
                fee_recipient_acc.key,
                params.fee_lamports,
            ),
            &[depositor.clone(), fee_recipient_acc.clone(), sys.clone()],
        )?;
    }

    // The record, and the hash chain's next value.
    let sender = depositor.key.to_bytes();
    let leaf = deposit::leaf(
        &syscall_keccak,
        &settlement_program.to_bytes(),
        args.chain_id,
        index,
        &sender,
        &args.l2_recipient,
        amount_gwei,
    );
    let hash_after = deposit::chain_next(&syscall_keccak, &queue.head_hash, &leaf);

    let seeds = deposit_record::seeds(&settlement_program.to_bytes(), args.chain_id, index);
    rome_zk_pda::create_or_adopt_pda(
        depositor,
        record_acc,
        sys,
        program_id,
        deposit_record::LEN,
        &[&seeds[0], &seeds[1], &seeds[2], &seeds[3], &[bump]],
    )?;
    {
        let mut d = record_acc.try_borrow_mut_data()?;
        deposit_record::write(
            &mut d,
            &deposit_record::DepositRecordFields {
                index,
                enqueue_unix_ts: Clock::get()?.unix_timestamp,
                sender,
                recipient: args.l2_recipient,
                amount_gwei,
                hash_after,
            },
        );
    }

    queue.count = next_count;
    queue.head_hash = hash_after;
    {
        let mut d = queue_acc.try_borrow_mut_data()?;
        deposit_queue::write(&mut d, &queue);
    }

    solana_program::msg!(
        "zk-bridge: chain {} deposit {} of {} gwei queued",
        args.chain_id,
        index,
        amount_gwei
    );
    Ok(())
}
