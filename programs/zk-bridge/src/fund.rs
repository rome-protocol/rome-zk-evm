//! `Fund` — a permissionless SPL transfer into the vault. Anyone may top the vault up; nothing
//! about who funds it is trusted, only where the tokens land.

use crate::instruction::FundArgs;
use crate::state::{vault_config, vault_token_pda};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    program::invoke,
    program_error::ProgramError,
    pubkey::Pubkey,
};

/// accounts: `[funder (signer), funder_token_account (writable), vault_config (read-only), vault_token
/// (writable), token_program]`.
pub fn fund(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    args: FundArgs,
) -> ProgramResult {
    let funder = next_account_info(it)?;
    let funder_token_acc = next_account_info(it)?;
    let vault_config_acc = next_account_info(it)?;
    let vault_token_acc = next_account_info(it)?;
    let token_program_acc = next_account_info(it)?;

    if !funder.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *token_program_acc.key != crate::token::TOKEN_PROGRAM_ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if vault_config_acc.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    let cfg = {
        let d = vault_config_acc.try_borrow_data()?;
        vault_config::read(&d).map_err(|_| ProgramError::InvalidAccountData)?
    };
    if cfg.chain_id != args.chain_id {
        return Err(ProgramError::InvalidAccountData);
    }
    let (expect_vault, _) = vault_token_pda(
        program_id,
        &cfg.settlement_program,
        args.chain_id,
        &cfg.mint,
    );
    if expect_vault != *vault_token_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }

    invoke(
        &crate::token::transfer_ix(
            funder_token_acc.key,
            vault_token_acc.key,
            funder.key,
            args.amount,
        ),
        &[
            funder_token_acc.clone(),
            vault_token_acc.clone(),
            funder.clone(),
        ],
    )?;

    solana_program::msg!(
        "zk-bridge: chain {} vault funded with {} units",
        args.chain_id,
        args.amount
    );
    Ok(())
}
