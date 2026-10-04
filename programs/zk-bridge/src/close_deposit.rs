//! `CloseDeposit` — refunds a deposit record's rent to its sender once nothing can read the record again.
//!
//! The record has to outlive its crediting batch: the inbox reads `record(to - 1).hash_after` when a batch
//! finalizes, and derive and the prover read a batch's records until the batch is proved. So the record
//! closes only when the inbox's cursor says a finalized batch has credited it (`deposit_next > index`) and
//! that batch's root is final (`index < deposit_final`).
//!
//! Permissionless: the rent always goes to `record.sender`, whoever sends the transaction. Every account
//! is bound by address. The registry is the chain's, under the bridge config's settlement program, and
//! names the config's inbox. The cursor is that inbox's cursor for the chain, owned by it, at version 2.
//! The record holds no chain id, so only its address under this program ties it to the chain.

use crate::bridge_config;
use crate::deposit_queue::load_registry;
use crate::errors::BridgeError;
use crate::instruction::CloseDepositArgs;
use rome_zk_layouts::cursor;
use rome_zk_layouts::deposit_queue::deposit_record;
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    program_error::ProgramError,
    pubkey::Pubkey,
};
use solana_system_interface::program as system_program;

/// accounts: `[bridge_config (read-only), registry (read-only), cursor (read-only), deposit_record
/// (writable), rent_recipient (writable)]`.
pub fn close_deposit(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    args: CloseDepositArgs,
) -> ProgramResult {
    let config_acc = next_account_info(it)?;
    let registry_acc = next_account_info(it)?;
    let cursor_acc = next_account_info(it)?;
    let record_acc = next_account_info(it)?;
    let rent_recipient = next_account_info(it)?;

    let config = bridge_config::load(program_id, config_acc)?;
    let settlement_program = Pubkey::new_from_array(config.settlement_program);
    let inbox_program = Pubkey::new_from_array(config.inbox_program);

    let registry = load_registry(&settlement_program, args.chain_id, registry_acc)?;
    if registry.inbox_program != config.inbox_program {
        return Err(BridgeError::WrongInboxProgram.into());
    }

    // The cursor: the inbox's own account for this chain, at version 2.
    let (expect_cursor, _) = cursor::pda(&inbox_program, &settlement_program, args.chain_id);
    if expect_cursor != *cursor_acc.key || *cursor_acc.owner != inbox_program {
        return Err(BridgeError::WrongCursor.into());
    }
    let deposit_cursor = {
        let d = cursor_acc.try_borrow_data()?;
        let c = cursor::read(&d).map_err(|_| ProgramError::from(BridgeError::WrongCursor))?;
        if c.chain_id != args.chain_id {
            return Err(BridgeError::WrongCursor.into());
        }
        c.deposit
            .ok_or_else(|| ProgramError::from(BridgeError::WrongCursor))?
    };

    // The record: at its own address for this chain and index, owned by this program.
    let (expect_record, _) = deposit_record::pda(
        program_id,
        &config.settlement_program,
        args.chain_id,
        args.index,
    );
    if expect_record != *record_acc.key || record_acc.owner != program_id {
        return Err(BridgeError::WrongDepositRecord.into());
    }
    let record = {
        let d = record_acc.try_borrow_data()?;
        deposit_record::read(&d).map_err(|_| ProgramError::from(BridgeError::WrongDepositRecord))?
    };
    if record.index != args.index {
        return Err(BridgeError::WrongDepositRecord.into());
    }
    if rent_recipient.key.to_bytes() != record.sender {
        return Err(BridgeError::WrongRentRecipient.into());
    }

    if deposit_cursor.next <= args.index {
        return Err(BridgeError::DepositNotCredited.into());
    }
    if args.index >= deposit_cursor.final_ {
        return Err(BridgeError::DepositNotFinal.into());
    }

    // Close: the lamports to the sender, the data gone, the account back to the system program.
    let lamports = record_acc.lamports();
    **record_acc.try_borrow_mut_lamports()? = 0;
    let mut to = rent_recipient.try_borrow_mut_lamports()?;
    **to = to
        .checked_add(lamports)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    drop(to);
    record_acc.resize(0)?;
    record_acc.assign(&system_program::id());
    solana_program::msg!(
        "zk-bridge: chain {} deposit {} closed",
        args.chain_id,
        args.index
    );
    Ok(())
}
