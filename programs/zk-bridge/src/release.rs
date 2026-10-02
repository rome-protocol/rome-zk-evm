//! `ReleaseExit` — the vault's core instruction: reads a settlement `exit_record`, CPIs `zk-settlement`'s
//! `ConsumeExit` (closing the record and authorising the release, atomically, in the SAME transaction),
//! then transfers the decimal-scaled amount to `record.sol_recipient`'s ATA. Permissionless — the caller
//! chooses nothing about where funds go; that is read entirely off the record.
//!
//! **Fund-safety invariant (the whole point of this instruction): the recipient is never an instruction
//! argument.** `recipient_ata` is a *caller-supplied account*, but its correctness is checked, not
//! trusted: this handler independently derives `get_associated_token_address(record.sol_recipient,
//! vault_config.mint)` from the record it just read off-chain state and refuses
//! [`BridgeError::WrongRecipientAta`] before the settlement CPI ever runs if the supplied account is
//! anything else.
//!
//! **Atomicity (the double-payout guard).** The `ConsumeExit` CPI runs BEFORE the SPL transfer. That CPI
//! closes the `exit_record` (assigns it to the system program, zeroes its data) and only succeeds once —
//! `zk-settlement::exit::consume_exit`'s own seeds/owner check refuses a second call against the same
//! `message_hash` outright (`ProgramError::IncorrectProgramId`, the record is no longer owned by the
//! settlement program). Because the CPI runs first and a failed CPI aborts the whole transaction, there is
//! no reachable state where the transfer happens without the record having just been consumed, and no
//! reachable state where the record is consumed twice.
//!
//! **Decimal scaling rounds down.** `record.amount` is u128 wei of an 18-decimal EVM asset;
//! `mint_amount = amount / 10^(18 - mint_decimals)` — integer division truncates, so any sub-unit
//! remainder (dust) is never transferred and simply stays in the vault's SPL balance.

use crate::errors::BridgeError;
use crate::instruction::ReleaseExitArgs;
use crate::state::{vault_authority_pda, vault_config, vault_token_pda};
use rome_zk_layouts::exit::{exit_config, exit_consumer_seeds, exit_record};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    program::invoke_signed,
    program_error::ProgramError,
    pubkey::Pubkey,
};

/// The one-time exponent table for `10^(18 - mint_decimals)`, `mint_decimals` in `0..=18` —
/// `AmountConversionOverflow` for anything else (checked by the caller before this is reached; kept as a
/// safety net here too).
fn decimal_scale(mint_decimals: u8) -> Result<u128, ProgramError> {
    if mint_decimals > 18 {
        return Err(BridgeError::InvalidMintDecimals.into());
    }
    Ok(10u128.pow((18 - mint_decimals) as u32))
}

/// accounts: `[vault_config (read-only), exit_config (read-only), exit_record (writable), exit_consumer
/// (read-only — this program's own `["exit_consumer", chain_id]` PDA), settlement_program (read-only,
/// executable), payer_refund (writable), vault_token (writable), vault_authority (read-only),
/// recipient_ata (writable), token_program]`.
pub fn release_exit(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    args: ReleaseExitArgs,
) -> ProgramResult {
    let vault_config_acc = next_account_info(it)?;
    let exit_config_acc = next_account_info(it)?;
    let exit_record_acc = next_account_info(it)?;
    let exit_consumer_acc = next_account_info(it)?;
    let settlement_program_acc = next_account_info(it)?;
    let payer_refund_acc = next_account_info(it)?;
    let vault_token_acc = next_account_info(it)?;
    let vault_authority_acc = next_account_info(it)?;
    let recipient_ata_acc = next_account_info(it)?;
    let token_program_acc = next_account_info(it)?;

    // --- (1) our own accounts: owner first (cheapest refusal), THEN read the data (the
    // vault_config PDA is keyed by [settlement_program, chain_id] — settlement_program is only known
    // once the account is decoded, so the seeds check below runs on the DATA it just read, verifying the
    // account is self-consistent: its own address must equal the PDA its own recorded settlement_program +
    // chain_id derive, under this program's ownership) ---
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
    let (expect_vault_config, _) =
        crate::state::vault_config_pda(program_id, &cfg.settlement_program, args.chain_id);
    if expect_vault_config != *vault_config_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }
    if *token_program_acc.key != crate::token::TOKEN_PROGRAM_ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if *settlement_program_acc.key != cfg.settlement_program {
        return Err(ProgramError::InvalidArgument);
    }

    // --- (2) exit_config: must be the settlement program's own account for this chain ---
    let (expect_exit_config, _) = exit_config::pda(&cfg.settlement_program, args.chain_id);
    if expect_exit_config != *exit_config_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }
    if exit_config_acc.owner != &cfg.settlement_program {
        return Err(ProgramError::IncorrectProgramId);
    }

    // --- (3) exit_record: seeds + settlement ownership (BridgeError::WrongSettlementOwner, named —
    // this is what refuses a forged record AND what refuses a second ReleaseExit of an already-closed one,
    // since ConsumeExit reassigns a closed record to the system program) ---
    let (expect_record, _) =
        exit_record::pda(&cfg.settlement_program, args.chain_id, args.message_hash);
    if expect_record != *exit_record_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }
    if exit_record_acc.owner != &cfg.settlement_program {
        return Err(BridgeError::WrongSettlementOwner.into());
    }
    let record = {
        let d = exit_record_acc.try_borrow_data()?;
        exit_record::read(&d).map_err(|_| ProgramError::InvalidAccountData)?
    };
    if record.chain_id != args.chain_id || record.message_hash != args.message_hash {
        return Err(ProgramError::InvalidAccountData);
    }

    // --- (4) native asset only (v1) ---
    if record.asset != [0u8; 20] {
        return Err(BridgeError::UnsupportedAsset.into());
    }

    // --- (5) the refund destination is the record's own payer (defense in depth; ConsumeExit itself
    // enforces the same thing on the CPI about to run) ---
    if payer_refund_acc.key.to_bytes() != record.payer {
        return Err(BridgeError::WrongPayerRefund.into());
    }

    // --- (6) the recipient ATA is DERIVED from record.sol_recipient — never taken as a bare argument.
    // This is the whole fund-safety property this instruction exists to enforce. ---
    let recipient = Pubkey::new_from_array(record.sol_recipient);
    let expect_ata = crate::token::get_associated_token_address(&recipient, &cfg.mint);
    if expect_ata != *recipient_ata_acc.key {
        return Err(BridgeError::WrongRecipientAta.into());
    }

    // --- (7) our own vault accounts: seeds ---
    let (expect_vault_token, _) = vault_token_pda(
        program_id,
        &cfg.settlement_program,
        args.chain_id,
        &cfg.mint,
    );
    if expect_vault_token != *vault_token_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }
    let (expect_vault_authority, authority_bump) =
        vault_authority_pda(program_id, &cfg.settlement_program, args.chain_id);
    if expect_vault_authority != *vault_authority_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }

    // --- (8) our own exit_consumer PDA — the identity we are about to CPI-sign as ---
    let seeds = exit_consumer_seeds(args.chain_id);
    let (expect_consumer, consumer_bump) =
        Pubkey::find_program_address(&[&seeds[0], &seeds[1]], program_id);
    if expect_consumer != *exit_consumer_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }

    // --- (9) every check above passed: CPI ConsumeExit BEFORE any transfer (atomicity: a failed CPI
    // aborts the whole transaction, so there is no reachable state with a transfer but no consume). ---
    let consume_ix = zk_settlement_client::consume_exit_ix(
        &cfg.settlement_program,
        args.chain_id,
        args.message_hash,
        exit_consumer_acc.key,
        exit_config_acc.key,
        exit_record_acc.key,
        payer_refund_acc.key,
    );
    invoke_signed(
        &consume_ix,
        &[
            exit_consumer_acc.clone(),
            exit_config_acc.clone(),
            exit_record_acc.clone(),
            payer_refund_acc.clone(),
            settlement_program_acc.clone(),
        ],
        &[&[&seeds[0], &seeds[1], &[consumer_bump]]],
    )?;

    // --- (10) decimal-scale amount, rounding DOWN (dust stays in the vault) ---
    let scale = decimal_scale(cfg.mint_decimals)?;
    let mint_amount_u128 = record.amount / scale;
    let mint_amount: u64 = mint_amount_u128
        .try_into()
        .map_err(|_| ProgramError::from(BridgeError::AmountConversionOverflow))?;

    // --- (11) the SPL transfer, signed by our OWN vault_authority PDA (invoke_signed — a keypair can
    // never sign as this PDA, only our own program's CPI, under our own seeds, can) ---
    invoke_signed(
        &crate::token::transfer_ix(
            vault_token_acc.key,
            recipient_ata_acc.key,
            vault_authority_acc.key,
            mint_amount,
        ),
        &[
            vault_token_acc.clone(),
            recipient_ata_acc.clone(),
            vault_authority_acc.clone(),
        ],
        &[&[
            crate::state::VAULT_AUTHORITY_SEED,
            cfg.settlement_program.as_ref(),
            &args.chain_id.to_le_bytes(),
            &[authority_bump],
        ]],
    )?;

    solana_program::msg!(
        "zk-bridge: chain {} exit released: {} mint units to {}",
        args.chain_id,
        mint_amount,
        recipient
    );
    Ok(())
}
