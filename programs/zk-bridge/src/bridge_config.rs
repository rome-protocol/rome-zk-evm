//! `InitBridgeConfig` — writes the bridge's one-time config at `["bridge_config"]`: the canonical
//! settlement program and the canonical inbox. Every deposit queue is bound to these two programs
//! (`deposit_queue.rs`), and a chain can only take deposits if its registry, which its settlement program
//! owns, names this inbox.
//!
//! The config is written once, by the bridge program's own upgrade authority, the same gate settlement's
//! `InitGlobalConfig` uses: the `program_data` account is this program's real `ProgramData` account and
//! its stored authority must sign. There is nothing else to authenticate against at that point.

use crate::errors::BridgeError;
use crate::instruction::InitBridgeConfigArgs;
use rome_zk_layouts::deposit_queue::bridge_config;
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    program_error::ProgramError,
    pubkey::Pubkey,
};
use solana_system_interface::program as system_program;

// The upgradeable loader's id and the `ProgramData` address moved out of `solana_program`'s root
// re-export in the Agave 4.x line; this local module keeps the call sites short.
mod bpf_loader_upgradeable {
    pub use solana_loader_v3_interface::get_program_data_address;
    pub use solana_sdk_ids::bpf_loader_upgradeable::id;
}

/// The signer must be the upgrade authority stored in this program's own `ProgramData` account. The loader
/// stores `UpgradeableLoaderState::ProgramData { slot: u64, upgrade_authority_address: Option<Pubkey> }`:
/// a `u32` enum tag (3), the `u64` slot, then a 1-byte option tag and 32 authority bytes. It fails closed
/// on a wrong address or owner, short data, a wrong tag, an immutable program, or a signer that is not the
/// stored authority.
fn require_upgrade_authority(
    program_id: &Pubkey,
    program_data_acc: &AccountInfo,
    authority: &AccountInfo,
) -> ProgramResult {
    let bad = || -> ProgramError { BridgeError::NotUpgradeAuthority.into() };
    let expect = bpf_loader_upgradeable::get_program_data_address(program_id);
    if expect != *program_data_acc.key || *program_data_acc.owner != bpf_loader_upgradeable::id() {
        return Err(bad());
    }
    let d = program_data_acc.try_borrow_data()?;
    const PROGRAM_DATA_TAG: u32 = 3;
    if d.len() < 45 {
        return Err(bad());
    }
    if u32::from_le_bytes(d[0..4].try_into().unwrap()) != PROGRAM_DATA_TAG {
        return Err(bad());
    }
    if d[12] != 1 {
        return Err(bad());
    }
    let stored = Pubkey::new_from_array(d[13..45].try_into().unwrap());
    if !authority.is_signer || stored != *authority.key {
        return Err(bad());
    }
    Ok(())
}

/// Reads the bridge config, after checking that the account is the `["bridge_config"]` PDA under this
/// program and is owned by it. Every instruction that needs the canonical programs goes through here.
pub fn load(
    program_id: &Pubkey,
    bridge_config_acc: &AccountInfo,
) -> Result<bridge_config::BridgeConfigFields, ProgramError> {
    let (expect, _) = bridge_config::pda(program_id);
    if expect != *bridge_config_acc.key || bridge_config_acc.owner != program_id {
        return Err(BridgeError::WrongBridgeConfig.into());
    }
    let d = bridge_config_acc.try_borrow_data()?;
    bridge_config::read(&d).map_err(|_| BridgeError::WrongBridgeConfig.into())
}

/// accounts: `[payer (signer, writable), authority (signer — this program's upgrade authority),
/// bridge_config (writable, NEW), program_data (read-only), system_program]`.
pub fn init_bridge_config(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    args: InitBridgeConfigArgs,
) -> ProgramResult {
    let payer = next_account_info(it)?;
    let authority = next_account_info(it)?;
    let config_acc = next_account_info(it)?;
    let program_data_acc = next_account_info(it)?;
    let sys = next_account_info(it)?;
    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *sys.key != system_program::id() {
        return Err(ProgramError::IncorrectProgramId);
    }
    require_upgrade_authority(program_id, program_data_acc, authority)?;
    if args.settlement_program == Pubkey::default() || args.inbox_program == Pubkey::default() {
        return Err(BridgeError::BridgeConfigProgramZero.into());
    }
    let (expect, bump) = bridge_config::pda(program_id);
    if expect != *config_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }
    // Checked before create-or-adopt, which cannot tell "already written" from "pre-funded".
    if config_acc.owner == program_id && config_acc.data_len() != 0 {
        return Err(BridgeError::BridgeConfigAlreadyInitialized.into());
    }
    let seeds = bridge_config::seeds();
    rome_zk_pda::create_or_adopt_pda(
        payer,
        config_acc,
        sys,
        program_id,
        bridge_config::LEN,
        &[seeds[0], &[bump]],
    )?;
    let mut d = config_acc.try_borrow_mut_data()?;
    bridge_config::write(
        &mut d,
        &bridge_config::BridgeConfigFields {
            settlement_program: args.settlement_program.to_bytes(),
            inbox_program: args.inbox_program.to_bytes(),
        },
    );
    solana_program::msg!(
        "zk-bridge: bridge config written (settlement {}, inbox {})",
        args.settlement_program,
        args.inbox_program
    );
    Ok(())
}
