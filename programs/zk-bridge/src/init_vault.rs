//! `InitVault` — creates the vault's SPL Token account and its config PDA for one `(settlement_program,
//! chain_id, mint)` triple. Gated by the settlement chain authority: `chain_authority` must sign AND its key
//! must equal `root.authority` — the settlement `["root", chain_id]` PDA's own authority field, the same
//! account `zk-settlement`'s `ProposeExitConfig` gates exits by (`require_chain_authority`). This ties the
//! vault's root of trust to the same authority that governs the chain's exits: `chain_authority` must equal
//! the `root.authority` of whatever `settlement_program` the caller names, so a config naming the REAL
//! settlement program can only ever be created by the real chain authority.
//!
//! **The address itself is also un-frontrunnable:** the three vault PDAs are keyed by
//! `[settlement_program, chain_id]` (see `state.rs`'s module doc), not `chain_id` alone. A hostile
//! `settlement_program` argument does not let an attacker occupy the real chain's vault slot — it can
//! only ever create a config at `pda(hostile_settlement, chain_id)`, a DIFFERENT address than
//! `pda(real_settlement, chain_id)`, which requires signing as `real_settlement`'s own `root.authority`
//! (something only the real chain authority can do). So the real authority is never locked out by a
//! front-runner, regardless of ordering. `vault_config.authority` is recorded as the chain authority
//! (previously the caller/payer — forward-looking bookkeeping that was never enforced by anything). See
//! the crate README's "Fund-safety invariants".

use crate::bridge_config;
use crate::errors::BridgeError;
use crate::instruction::InitVaultArgs;
use crate::state::{vault_authority_pda, vault_config, vault_config_pda, vault_token_pda};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    program::invoke,
    program_error::ProgramError,
    pubkey::Pubkey,
};
// `system_program` moved out of `solana_program`'s root re-export in the Agave
// 4.x line (API fallout).
use solana_system_interface::program as system_program;

/// accounts: `[payer (signer, writable), vault_config (writable, NEW), mint (read-only), vault_token
/// (writable, NEW), vault_authority (read-only, PDA), chain_authority (signer — must equal the settlement
/// `root.authority` for `args.chain_id`), root (read-only — the settlement `["root", chain_id]` PDA, owned
/// by `args.settlement_program`), token_program, system_program, bridge_config (read-only)]`.
pub fn init_vault(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    args: InitVaultArgs,
) -> ProgramResult {
    let payer = next_account_info(it)?;
    let vault_config_acc = next_account_info(it)?;
    let mint_acc = next_account_info(it)?;
    let vault_token_acc = next_account_info(it)?;
    let vault_authority_acc = next_account_info(it)?;
    let chain_authority_acc = next_account_info(it)?;
    let root_acc = next_account_info(it)?;
    let token_program_acc = next_account_info(it)?;
    let sys = next_account_info(it)?;
    let config_acc = next_account_info(it)?;

    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *sys.key != system_program::id() {
        return Err(ProgramError::IncorrectProgramId);
    }
    if *token_program_acc.key != crate::token::TOKEN_PROGRAM_ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    // Only the bridge config's settlement program gets a vault.
    let config = bridge_config::load(program_id, config_acc)?;
    if args.settlement_program.to_bytes() != config.settlement_program {
        return Err(BridgeError::WrongSettlementProgram.into());
    }
    if args.mint_decimals > 18 {
        return Err(BridgeError::InvalidMintDecimals.into());
    }
    if *mint_acc.key != args.mint {
        return Err(ProgramError::InvalidArgument);
    }

    // --- mint_decimals must match the mint's own real decimals byte ---
    // offset 44 of the standard 82-byte SPL Token `Mint` layout (`crate::token::MINT_LEN`); bounds-checked
    // here (rather than assumed) since a malformed/foreign account could otherwise be shorter than that.
    {
        let d = mint_acc.try_borrow_data()?;
        if d.len() < crate::token::MINT_LEN {
            return Err(ProgramError::InvalidAccountData);
        }
        if d[44] != args.mint_decimals {
            return Err(BridgeError::MintDecimalsMismatch.into());
        }
    }

    // --- the settlement chain-authority gate:
    // the `root` account must be owned by `args.settlement_program` and live at that program's own
    // `["root", chain_id]` PDA (cheapest, ownership-bound refusal first — an attacker cannot forge a root
    // account under a program they do not control and have it accepted here), and `chain_authority` must
    // be a signer whose key equals the decoded `root.authority` — the exact gate
    // `zk-settlement::governance::require_chain_authority` applies to `ProposeExitConfig`. Only then may
    // the vault + config be created. ---
    let (expect_root, _) = rome_zk_layouts::root::pda(&args.settlement_program, args.chain_id);
    if expect_root != *root_acc.key || root_acc.owner != &args.settlement_program {
        return Err(ProgramError::IncorrectProgramId);
    }
    let root = {
        let d = root_acc.try_borrow_data()?;
        rome_zk_layouts::root::read(&d).map_err(|_| ProgramError::InvalidAccountData)?
    };
    if root.chain_id != args.chain_id {
        return Err(ProgramError::InvalidAccountData);
    }
    if !chain_authority_acc.is_signer
        || Pubkey::new_from_array(root.authority) != *chain_authority_acc.key
    {
        return Err(BridgeError::NotChainAuthority.into());
    }

    let (expect_config, _config_bump) =
        vault_config_pda(program_id, &args.settlement_program, args.chain_id);
    if expect_config != *vault_config_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }
    // Refuse a second InitVault for the same (settlement_program, chain_id) — checked BEFORE
    // `create_or_adopt_pda`, which itself only distinguishes "no lamports yet" from "pre-funded", never
    // "already truly initialised" (its own doc, `rome_zk_pda::create_or_adopt_pda`).
    if vault_config_acc.owner == program_id && vault_config_acc.data_len() == vault_config::LEN {
        return Err(BridgeError::VaultAlreadyInitialized.into());
    }

    let (expect_vault, vault_bump) = vault_token_pda(
        program_id,
        &args.settlement_program,
        args.chain_id,
        &args.mint,
    );
    if expect_vault != *vault_token_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }
    let (expect_authority, _authority_bump) =
        vault_authority_pda(program_id, &args.settlement_program, args.chain_id);
    if expect_authority != *vault_authority_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }

    // --- create the vault_config PDA (owned by this program) ---
    let config_seeds = [
        crate::state::VAULT_CONFIG_SEED,
        args.settlement_program.as_ref(),
        &args.chain_id.to_le_bytes()[..],
    ];
    let (_, config_bump) = Pubkey::find_program_address(&config_seeds, program_id);
    rome_zk_pda::create_or_adopt_pda(
        payer,
        vault_config_acc,
        sys,
        program_id,
        vault_config::LEN,
        &[
            config_seeds[0],
            config_seeds[1],
            config_seeds[2],
            &[config_bump],
        ],
    )?;
    {
        let mut d = vault_config_acc.try_borrow_mut_data()?;
        d.copy_from_slice(&vault_config::write(&vault_config::VaultConfigFields {
            chain_id: args.chain_id,
            settlement_program: args.settlement_program,
            mint: args.mint,
            mint_decimals: args.mint_decimals,
            authority: *chain_authority_acc.key,
        }));
    }

    // --- create the vault's own SPL Token account, owned by the SPL Token program, at OUR PDA address ---
    let vault_seeds = [
        crate::state::VAULT_SEED,
        args.settlement_program.as_ref(),
        &args.chain_id.to_le_bytes()[..],
        args.mint.as_ref(),
    ];
    rome_zk_pda::create_or_adopt_pda(
        payer,
        vault_token_acc,
        sys,
        &crate::token::TOKEN_PROGRAM_ID,
        crate::token::TOKEN_ACCOUNT_LEN,
        &[
            vault_seeds[0],
            vault_seeds[1],
            vault_seeds[2],
            vault_seeds[3],
            &[vault_bump],
        ],
    )?;
    // `InitializeAccount3` requires no signer at all (the vault's own SPL account was just created by the
    // `create_or_adopt_pda` call above, under our seeds — that CPI is what needed `invoke_signed`; this
    // one is a plain data-initializing call).
    invoke(
        &crate::token::initialize_account3_ix(vault_token_acc.key, &args.mint, &expect_authority),
        &[vault_token_acc.clone(), mint_acc.clone()],
    )?;

    solana_program::msg!(
        "zk-bridge: vault initialised for chain {} mint {}",
        args.chain_id,
        args.mint
    );
    Ok(())
}
