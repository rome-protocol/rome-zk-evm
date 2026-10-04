//! The global config PDA, the reserved-id allowlist, the per-chain deposit/fee lifecycle, and
//! `MigrateChainV2` for a chain that predates the registration-and-revenue changes (Tiber).
//! `chain::init_chain` (the namespace split) and `settle::post_root`/`post_root_proved` (the protocol fee)
//! are the two call sites in the other modules that consume the PDAs and fee-charging
//! helper defined here.

use crate::chain::RegistryEntryArg;
use crate::errors::SettleError;
use borsh::{BorshDeserialize, BorshSerialize};
use rome_zk_layouts::{
    chain_config, chainid, exit::exit_config, global_config, perm_nonce, registry, reserved_allow,
    root,
};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    msg,
    program::invoke,
    program_error::ProgramError,
    pubkey::Pubkey,
    rent::Rent,
    sysvar::Sysvar,
};
// `bpf_loader_upgradeable`/`system_instruction`/`system_program` moved out of `solana_program`'s root
// re-export in the Agave 4.x line (API fallout) — `id`/`check_id` now live in `solana_sdk_ids`,
// `get_program_data_address` in `solana_loader_v3_interface`; this local module keeps every existing
// `bpf_loader_upgradeable::` call site below unchanged.
mod bpf_loader_upgradeable {
    pub use solana_loader_v3_interface::get_program_data_address;
    pub use solana_sdk_ids::bpf_loader_upgradeable::id;
}
use solana_system_interface::{instruction as system_instruction, program as system_program};

/// The single definitions are `rome_zk_layouts::{global_config, chain_config, perm_nonce,
/// reserved_allow}`; kept under these names so every existing call site in this program is unchanged.
#[inline]
pub fn global_config_seeds() -> [&'static [u8]; 1] {
    global_config::seeds()
}
#[inline]
pub fn global_config_pda(program_id: &Pubkey) -> (Pubkey, u8) {
    global_config::pda(program_id)
}

#[inline]
pub fn chain_config_seeds(chain_id: u64) -> [Vec<u8>; 2] {
    chain_config::seeds(chain_id)
}
#[inline]
pub fn chain_config_pda(program_id: &Pubkey, chain_id: u64) -> (Pubkey, u8) {
    chain_config::pda(program_id, chain_id)
}

#[inline]
pub fn perm_nonce_seeds(authority: &Pubkey) -> [Vec<u8>; 2] {
    perm_nonce::seeds(authority)
}
#[inline]
pub fn perm_nonce_pda(program_id: &Pubkey, authority: &Pubkey) -> (Pubkey, u8) {
    perm_nonce::pda(program_id, authority)
}

#[inline]
pub fn reserved_allow_seeds(chain_id: u64) -> [Vec<u8>; 2] {
    reserved_allow::seeds(chain_id)
}
#[inline]
pub fn reserved_allow_pda(program_id: &Pubkey, chain_id: u64) -> (Pubkey, u8) {
    reserved_allow::pda(program_id, chain_id)
}

/// Reads and validates the global config account at `acc` (must be the canonical `["global_config"]`
/// PDA, owned by this program).
pub fn read_global_config(
    program_id: &Pubkey,
    acc: &AccountInfo,
) -> Result<global_config::GlobalConfigFields, ProgramError> {
    let (expect, _) = global_config_pda(program_id);
    if expect != *acc.key || acc.owner != program_id {
        return Err(SettleError::WrongGlobalConfig.into());
    }
    let d = acc.try_borrow_data()?;
    global_config::read(&d).map_err(|_| SettleError::WrongGlobalConfig.into())
}

/// Reads and validates a chain's `chain_config` account (must be `["chain_config", chain_id]`, owned by
/// this program, and its own stored `chain_id` must agree).
pub fn read_chain_config(
    program_id: &Pubkey,
    chain_id: u64,
    acc: &AccountInfo,
) -> Result<chain_config::ChainConfigFields, ProgramError> {
    let (expect, _) = chain_config_pda(program_id, chain_id);
    if expect != *acc.key || acc.owner != program_id {
        return Err(SettleError::WrongChainConfig.into());
    }
    let d = acc.try_borrow_data()?;
    let f = chain_config::read(&d).map_err(|_| SettleError::WrongChainConfig)?;
    if f.chain_id != chain_id {
        return Err(SettleError::WrongChainConfig.into());
    }
    Ok(f)
}

/// `fee = base_lamports_per_batch + bps * gas_in_batch / 10_000`. `bps` is basis points
/// (1 bp = 1/10_000). Checked arithmetic throughout — an operator-set `fee_bps`/`gas_in_batch` combination
/// large enough to overflow `u64` fails closed (`FeeOverflow`), never wraps.
pub fn compute_fee(
    fee_base_lamports: u64,
    fee_bps: u32,
    gas_in_batch: u64,
) -> Result<u64, ProgramError> {
    let variable = (fee_bps as u128)
        .checked_mul(gas_in_batch as u128)
        .ok_or(SettleError::FeeOverflow)?
        / 10_000u128;
    let total = (fee_base_lamports as u128)
        .checked_add(variable)
        .ok_or(SettleError::FeeOverflow)?;
    u64::try_from(total).map_err(|_| SettleError::FeeOverflow.into())
}

/// Transfers `fee` lamports from `payer` (a real transaction signer — a plain `transfer`, no PDA
/// signature needed) to `treasury`, after checking `treasury` is the account `global_config` currently
/// names. Called by `settle::post_root`/`post_root_proved` — "without it the instruction fails" is
/// just `system_instruction::transfer`'s own insufficient-funds failure; there is no separate check here.
pub fn charge_fee<'a>(
    payer: &AccountInfo<'a>,
    treasury: &AccountInfo<'a>,
    sys: &AccountInfo<'a>,
    global: &global_config::GlobalConfigFields,
    fee: u64,
) -> ProgramResult {
    if Pubkey::new_from_array(global.treasury) != *treasury.key {
        return Err(SettleError::WrongTreasury.into());
    }
    invoke(
        &system_instruction::transfer(payer.key, treasury.key, fee),
        &[payer.clone(), treasury.clone(), sys.clone()],
    )
}

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone)]
pub struct InitGlobalConfigArgs {
    pub registry_authority: Pubkey,
    pub treasury: Pubkey,
    pub permissionless_init_enabled: bool,
    pub reclaim_window_slots: u64,
    pub deposit_lamports: u64,
    pub default_fee_base_lamports: u64,
    pub default_fee_bps: u32,
}

/// `InitGlobalConfig` is the one governance instruction with no existing `global_config` to authenticate
/// its caller against, so it must instead authenticate against something that already exists at that
/// point — the program's own upgrade authority. `program_data_acc` must be the program's real
/// `["<program_id>"]` PDA under `bpf_loader_upgradeable` (`get_program_data_address`), owned by that
/// loader; its account data is the loader's bincode-encoded
/// `UpgradeableLoaderState::ProgramData { slot: u64, upgrade_authority_address: Option<Pubkey> }`, whose
/// exact byte layout (45-byte header: `u32` enum discriminant, `u64` slot, then a 1-byte `Option` tag and
/// 32 authority bytes) is hand-parsed here rather than pulling in a `bincode` dependency for one read.
/// Fails closed — wrong PDA/owner, too-short data, wrong discriminant, an immutable program (`None`
/// authority, tag byte 0), or a signer that does not match the stored authority — all reject with
/// `NotUpgradeAuthority` before any account is created.
fn require_upgrade_authority(
    program_id: &Pubkey,
    program_data_acc: &AccountInfo,
    authority: &AccountInfo,
) -> ProgramResult {
    let bad = || -> ProgramError { SettleError::NotUpgradeAuthority.into() };
    let expect = bpf_loader_upgradeable::get_program_data_address(program_id);
    if expect != *program_data_acc.key || *program_data_acc.owner != bpf_loader_upgradeable::id() {
        return Err(bad());
    }
    let d = program_data_acc.try_borrow_data()?;
    // UpgradeableLoaderState discriminant: Uninitialized=0, Buffer=1, Program=2, ProgramData=3.
    const PROGRAM_DATA_DISCRIMINANT: u32 = 3;
    if d.len() < 45 {
        return Err(bad());
    }
    if u32::from_le_bytes(d[0..4].try_into().unwrap()) != PROGRAM_DATA_DISCRIMINANT {
        return Err(bad());
    }
    // Offset 4..12 is the `slot: u64` field — not needed here. Offset 12 is the `Option<Pubkey>` tag
    // (0 = None = immutable program, 1 = Some), offset 13..45 the authority pubkey when Some.
    if d[12] != 1 {
        return Err(bad());
    }
    let stored = Pubkey::new_from_array(d[13..45].try_into().unwrap());
    if !authority.is_signer || stored != *authority.key {
        return Err(bad());
    }
    Ok(())
}

/// An unfunded treasury account makes every chain's first `PostRoot` fail
/// (`InsufficientFundsForRent` on the fee transfer) — caught here, at config-write time, instead.
fn require_treasury_rent_exempt(treasury_acc: &AccountInfo, treasury: &Pubkey) -> ProgramResult {
    if treasury_acc.key != treasury {
        return Err(SettleError::TreasuryNotRentExempt.into());
    }
    if treasury_acc.lamports() < Rent::default().minimum_balance(0) {
        return Err(SettleError::TreasuryNotRentExempt.into());
    }
    Ok(())
}

/// A `reclaim_window_slots` below one day of slots would make a never-posted
/// permissionless chain reclaimable in (or near) its own registration slot. Checked at both
/// `InitGlobalConfig` and `SetGlobalConfig` — the two instructions that ever write this field.
pub const MIN_RECLAIM_WINDOW_SLOTS: u64 = 216_000;

fn require_reclaim_window_floor(reclaim_window_slots: u64) -> ProgramResult {
    if reclaim_window_slots < MIN_RECLAIM_WINDOW_SLOTS {
        return Err(SettleError::ReclaimWindowTooShort.into());
    }
    Ok(())
}

/// accounts: [payer (signer, writable), authority (signer, the program's real upgrade authority),
/// global_config pda (writable, new), program_data (read-only, the program's own `bpf_loader_upgradeable`
/// `ProgramData` account), treasury (read-only, must equal `args.treasury` and be rent-exempt),
/// system_program]. Runs exactly once per deployment (the second call sees a real, program-owned
/// `global_config` and is rejected — same "already initialised" shape `InitBatchCursor` uses, checked
/// before `create_or_adopt_pda` so a pre-funding attempt is still adopted rather than treated as "already
/// initialised"). `permissionless_init_enabled` is always forced `false` here regardless of `args` — the
/// mainnet gate only ever turns on via `SetGlobalConfig`, signed by the registry authority this call
/// installs.
pub fn init_global_config(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    args: InitGlobalConfigArgs,
) -> ProgramResult {
    let payer = next_account_info(it)?;
    let authority = next_account_info(it)?;
    let global_acc = next_account_info(it)?;
    let program_data_acc = next_account_info(it)?;
    let treasury_acc = next_account_info(it)?;
    let sys = next_account_info(it)?;
    if !payer.is_signer || *sys.key != system_program::id() {
        return Err(ProgramError::MissingRequiredSignature);
    }
    require_upgrade_authority(program_id, program_data_acc, authority)?;
    require_treasury_rent_exempt(treasury_acc, &args.treasury)?;
    require_reclaim_window_floor(args.reclaim_window_slots)?;
    let (expect, bump) = global_config_pda(program_id);
    if expect != *global_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }
    if global_acc.owner == program_id && global_acc.data_len() != 0 {
        return Err(SettleError::GlobalConfigAlreadyInitialized.into());
    }
    let seeds = global_config_seeds();
    rome_zk_pda::create_or_adopt_pda(
        payer,
        global_acc,
        sys,
        program_id,
        global_config::LEN,
        &[seeds[0], &[bump]],
    )?;
    let mut d = global_acc.try_borrow_mut_data()?;
    global_config::write(
        &mut d,
        &global_config::GlobalConfigFields {
            registry_authority: args.registry_authority.to_bytes(),
            treasury: args.treasury.to_bytes(),
            permissionless_init_enabled: false,
            reclaim_window_slots: args.reclaim_window_slots,
            deposit_lamports: args.deposit_lamports,
            default_fee_base_lamports: args.default_fee_base_lamports,
            default_fee_bps: args.default_fee_bps,
            pending_registry_authority: [0u8; 32],
        },
    );
    msg!(
        "global config initialized: registry_authority {}, treasury {}, permissionless_init_enabled false (forced)",
        args.registry_authority,
        args.treasury,
    );
    Ok(())
}

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone)]
pub struct SetGlobalConfigArgs {
    pub permissionless_init_enabled: bool,
    pub reclaim_window_slots: u64,
    pub deposit_lamports: u64,
    pub default_fee_base_lamports: u64,
    pub default_fee_bps: u32,
}

/// accounts: [registry_authority (signer), global_config (writable)]. The one instruction that
/// can flip `permissionless_init_enabled` (forced `false` at `InitGlobalConfig`) and the other
/// registry-wide knobs after the fact — registry-authority-only, every field replaced together (not a
/// partial patch) so there is exactly one call site to reason about for "what changed".
pub fn set_global_config(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    args: SetGlobalConfigArgs,
) -> ProgramResult {
    let authority = next_account_info(it)?;
    let global_acc = next_account_info(it)?;
    let mut global = read_global_config(program_id, global_acc)?;
    require_registry_authority(&global, authority)?;
    require_reclaim_window_floor(args.reclaim_window_slots)?;
    // Permissionless ids must never be free while the gate that mints them is on.
    if args.permissionless_init_enabled && args.deposit_lamports == 0 {
        return Err(SettleError::DepositRequiredForPermissionless.into());
    }
    global.permissionless_init_enabled = args.permissionless_init_enabled;
    global.reclaim_window_slots = args.reclaim_window_slots;
    global.deposit_lamports = args.deposit_lamports;
    global.default_fee_base_lamports = args.default_fee_base_lamports;
    global.default_fee_bps = args.default_fee_bps;
    let mut d = global_acc.try_borrow_mut_data()?;
    global_config::write(&mut d, &global);
    msg!(
        "global config updated: permissionless_init_enabled {}, reclaim_window_slots {}",
        args.permissionless_init_enabled,
        args.reclaim_window_slots
    );
    Ok(())
}

/// accounts: [registry_authority (signer), global_config (writable)]. The single-step
/// `SetRegistryAuthority` this replaces accepted any pubkey, including `Pubkey::default()` or a typo — since
/// `InitGlobalConfig` runs only once, that would lock every registry-authority instruction out forever with no do-over.
/// Two-step rotation instead, mirroring the loader's own checked set-authority: the CURRENT authority proposes (`new`
/// stored as `pending_registry_authority`, rejecting `Pubkey::default()`); [`accept_registry_authority`] — signed by
/// the PROPOSED key, not this one — is what actually seats it. The old authority keeps working for
/// every registry-authority-gated instruction right up until that accept lands.
pub fn propose_registry_authority(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    new: Pubkey,
) -> ProgramResult {
    let authority = next_account_info(it)?;
    let global_acc = next_account_info(it)?;
    let mut global = read_global_config(program_id, global_acc)?;
    require_registry_authority(&global, authority)?;
    if new == Pubkey::default() {
        return Err(SettleError::InvalidRegistryAuthority.into());
    }
    global.pending_registry_authority = new.to_bytes();
    let mut d = global_acc.try_borrow_mut_data()?;
    global_config::write(&mut d, &global);
    msg!("registry authority rotation proposed: {}", new);
    Ok(())
}

/// accounts: [pending_authority (signer), global_config (writable)]. Step two of
/// [`propose_registry_authority`]: only the pubkey currently stored as `pending_registry_authority` may
/// accept — not the outgoing authority, not any other signer. A `global_config` with nothing proposed
/// stores `Pubkey::default()` there, which no real keypair can ever sign as, so "nothing pending" and "a
/// third key tried to accept" both fail the same check.
pub fn accept_registry_authority(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
) -> ProgramResult {
    let signer = next_account_info(it)?;
    let global_acc = next_account_info(it)?;
    let mut global = read_global_config(program_id, global_acc)?;
    if !signer.is_signer || Pubkey::new_from_array(global.pending_registry_authority) != *signer.key
    {
        return Err(SettleError::NotPendingRegistryAuthority.into());
    }
    global.registry_authority = global.pending_registry_authority;
    global.pending_registry_authority = Pubkey::default().to_bytes();
    let mut d = global_acc.try_borrow_mut_data()?;
    global_config::write(&mut d, &global);
    msg!("registry authority accepted: {}", signer.key);
    Ok(())
}

pub(crate) fn require_registry_authority(
    global: &global_config::GlobalConfigFields,
    signer: &AccountInfo,
) -> ProgramResult {
    if !signer.is_signer || Pubkey::new_from_array(global.registry_authority) != *signer.key {
        return Err(SettleError::NotRegistryAuthority.into());
    }
    Ok(())
}

/// accounts: [payer (signer, writable — funds rent only), registry_authority (signer), global_config
/// (read-only), allow pda (writable, new), system_program]. `payer` and `registry_authority` may be the
/// same account, but need not be — the registry authority (a multisig, typically) authorizes the
/// call without itself needing a lamport balance.
pub fn allow_reserved_id(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    chain_id: u64,
) -> ProgramResult {
    let payer = next_account_info(it)?;
    let authority = next_account_info(it)?;
    let global_acc = next_account_info(it)?;
    let allow_acc = next_account_info(it)?;
    let sys = next_account_info(it)?;
    let global = read_global_config(program_id, global_acc)?;
    require_registry_authority(&global, authority)?;
    if !payer.is_signer || *sys.key != system_program::id() {
        return Err(ProgramError::MissingRequiredSignature);
    }
    // Only a RESERVED (`chain_id < 2^32`) id can be allowlisted — a permissionless id needs
    // no marker (its namespace is enforced by derivation, not an allowlist) and allowing one would be a
    // meaningless, never-consumed account.
    if !chainid::is_reserved(chain_id) {
        return Err(ProgramError::InvalidArgument);
    }
    let (expect, bump) = reserved_allow_pda(program_id, chain_id);
    if expect != *allow_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }
    let seeds = reserved_allow_seeds(chain_id);
    rome_zk_pda::create_or_adopt_pda(
        payer,
        allow_acc,
        sys,
        program_id,
        reserved_allow::LEN,
        &[&seeds[0], &seeds[1], &[bump]],
    )?;
    let mut d = allow_acc.try_borrow_mut_data()?;
    reserved_allow::write(&mut d, chain_id);
    msg!("reserved chain id {} allowed", chain_id);
    Ok(())
}

/// accounts: [registry_authority (signer, writable), global_config (read-only), allow pda (writable)].
/// Closes the marker, rent to the registry authority who paid to revoke it.
pub fn revoke_reserved_id(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    chain_id: u64,
) -> ProgramResult {
    let authority = next_account_info(it)?;
    let global_acc = next_account_info(it)?;
    let allow_acc = next_account_info(it)?;
    let global = read_global_config(program_id, global_acc)?;
    require_registry_authority(&global, authority)?;
    let (expect, _) = reserved_allow_pda(program_id, chain_id);
    if expect != *allow_acc.key || allow_acc.owner != program_id {
        return Err(ProgramError::InvalidSeeds);
    }
    close_to(allow_acc, authority)?;
    msg!("reserved chain id {} revoked", chain_id);
    Ok(())
}

/// accounts: [registry_authority (signer), global_config (read-only), chain_config (writable)].
pub fn set_fee(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    chain_id: u64,
    base: u64,
    bps: u32,
) -> ProgramResult {
    let authority = next_account_info(it)?;
    let global_acc = next_account_info(it)?;
    let chain_config_acc = next_account_info(it)?;
    let global = read_global_config(program_id, global_acc)?;
    require_registry_authority(&global, authority)?;
    let mut cfg = read_chain_config(program_id, chain_id, chain_config_acc)?;
    cfg.fee_base_lamports = base;
    cfg.fee_bps = bps;
    let mut d = chain_config_acc.try_borrow_mut_data()?;
    chain_config::write(&mut d, &cfg);
    msg!("chain {} fee set: base {} bps {}", chain_id, base, bps);
    Ok(())
}

/// accounts: [registry_authority (signer), global_config (writable), treasury (read-only, must equal
/// `treasury` and be rent-exempt)].
pub fn set_treasury(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    treasury: Pubkey,
) -> ProgramResult {
    let authority = next_account_info(it)?;
    let global_acc = next_account_info(it)?;
    let treasury_acc = next_account_info(it)?;
    let mut global = read_global_config(program_id, global_acc)?;
    require_registry_authority(&global, authority)?;
    require_treasury_rent_exempt(treasury_acc, &treasury)?;
    global.treasury = treasury.to_bytes();
    let mut d = global_acc.try_borrow_mut_data()?;
    global_config::write(&mut d, &global);
    msg!("treasury set to {}", treasury);
    Ok(())
}

/// accounts: [registry_authority (signer), payer (signer, writable), global_config (read-only), root
/// (read-only), chain_config (writable, new-or-existing), system_program]. Three shapes (this extends the
/// one-time bring-forward this instruction already was):
/// - **absent** `chain_config`: created fresh at v2 (`LEN_V2`), same as before.
/// - **v1** `chain_config` (a chain migrated before v2 existed, or Tiber's original bring-forward): realloc'd
///   47→55 bytes via `create_or_adopt_pda`'s own resize path and rewritten as v2, preserving every
///   existing field (fee schedule, `posted_batches`, deposit bookkeeping) — only `max_drift_secs` is new.
/// - **v2** `chain_config`: refused (`ChainAlreadyMigrated`) — nothing left to migrate.
///
/// `max_drift_secs` is an explicit argument (`0` refused, `DriftBoundZero`) — never a program constant.
/// Reserved/permissionless is read off `chain_id` itself (the namespace split), not asserted by the
/// caller.
pub fn migrate_chain(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    chain_id: u64,
    max_drift_secs: u64,
) -> ProgramResult {
    let authority = next_account_info(it)?;
    let payer = next_account_info(it)?;
    let global_acc = next_account_info(it)?;
    let root_acc = next_account_info(it)?;
    let chain_config_acc = next_account_info(it)?;
    let sys = next_account_info(it)?;
    if !payer.is_signer || *sys.key != system_program::id() {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if max_drift_secs == 0 {
        return Err(SettleError::DriftBoundZero.into());
    }
    let global = read_global_config(program_id, global_acc)?;
    require_registry_authority(&global, authority)?;

    let (expect_root, _) = crate::chain::root_pda(program_id, chain_id);
    if expect_root != *root_acc.key || root_acc.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    {
        let d = root_acc.try_borrow_data()?;
        let f = root::read(&d).map_err(|_| ProgramError::InvalidAccountData)?;
        if f.chain_id != chain_id {
            return Err(ProgramError::InvalidAccountData);
        }
    }
    let (expect_cfg, bump) = chain_config_pda(program_id, chain_id);
    if expect_cfg != *chain_config_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }

    let already_exists = chain_config_acc.owner == program_id && chain_config_acc.data_len() != 0;
    let existing_v1 = if already_exists {
        let d = chain_config_acc.try_borrow_data()?;
        let f = chain_config::read(&d).map_err(|_| ProgramError::InvalidAccountData)?;
        if f.max_drift_secs.is_some() {
            return Err(SettleError::ChainAlreadyMigrated.into());
        }
        Some(f)
    } else {
        None
    };

    let seeds = chain_config_seeds(chain_id);
    if let Some(v1) = existing_v1 {
        // Realloc the existing v1 account 47→55 bytes in place (same address, same rent-exempt payer
        // topping up the delta) rather than close-and-recreate — every existing field survives untouched.
        let new_len = chain_config::LEN_V2;
        let new_min_rent = Rent::get()?.minimum_balance(new_len);
        // Top up against the NON-deposit balance: a permissionless chain's deposit escrow sits above rent
        // in this same account and must stay movable by `RefundDeposit` — if it covered the 8-byte delta
        // here, the refund would later leave the account rent-paying and be refused by the runtime.
        let escrow = if v1.deposit_refunded {
            0
        } else {
            v1.deposit_lamports
        };
        let free = chain_config_acc.lamports().saturating_sub(escrow);
        if new_min_rent > free {
            invoke(
                &system_instruction::transfer(payer.key, chain_config_acc.key, new_min_rent - free),
                &[payer.clone(), chain_config_acc.clone(), sys.clone()],
            )?;
        }
        // resize(len) replaces the old realloc(len, zero_init) and always zeroes growth; the write() right
        // below overwrites the whole buffer anyway
        chain_config_acc.resize(new_len)?;
        let mut d = chain_config_acc.try_borrow_mut_data()?;
        chain_config::write(
            &mut d,
            &chain_config::ChainConfigFields {
                max_drift_secs: Some(max_drift_secs),
                ..v1
            },
        );
        msg!(
            "chain {} migrated: chain_config v1 -> v2 (realloc)",
            chain_id
        );
    } else {
        rome_zk_pda::create_or_adopt_pda(
            payer,
            chain_config_acc,
            sys,
            program_id,
            chain_config::LEN_V2,
            &[&seeds[0], &seeds[1], &[bump]],
        )?;
        let now = Clock::get()?.slot;
        let mut d = chain_config_acc.try_borrow_mut_data()?;
        chain_config::write(
            &mut d,
            &chain_config::ChainConfigFields {
                chain_id,
                reserved: rome_zk_layouts::chainid::is_reserved(chain_id),
                deposit_lamports: 0,
                deposit_refunded: true, // no deposit was ever locked for a migrated (pre-existing) chain
                registered_slot: now,
                posted_batches: 0,
                fee_base_lamports: global.default_fee_base_lamports,
                fee_bps: global.default_fee_bps,
                max_drift_secs: Some(max_drift_secs),
            },
        );
        msg!("chain {} migrated: chain_config created (v2)", chain_id);
    }
    Ok(())
}

/// accounts: [registry_authority (signer), global_config (read-only), chain_config (writable)]. Same
/// authority gate as `set_fee`; refuses `0` (`DriftBoundZero`) — the drift bound is a chain parameter,
/// changeable after `MigrateChainV2`/`InitChainV2` has already put the chain on v2 (a v1
/// `chain_config` reads back `max_drift_secs: None`, and this instruction has nothing to compare a new
/// value against there — `WrongChainConfig` if the read comes back short for any other reason).
pub fn set_drift_bound(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    chain_id: u64,
    max_drift_secs: u64,
) -> ProgramResult {
    let authority = next_account_info(it)?;
    let global_acc = next_account_info(it)?;
    let chain_config_acc = next_account_info(it)?;
    let global = read_global_config(program_id, global_acc)?;
    require_registry_authority(&global, authority)?;
    if max_drift_secs == 0 {
        return Err(SettleError::DriftBoundZero.into());
    }
    let mut cfg = read_chain_config(program_id, chain_id, chain_config_acc)?;
    cfg.max_drift_secs = Some(max_drift_secs);
    let mut d = chain_config_acc.try_borrow_mut_data()?;
    chain_config::write(&mut d, &cfg);
    msg!("chain {} drift bound set: {} s", chain_id, max_drift_secs);
    Ok(())
}

/// The chain-authority gate (established by `refund_deposit` below; `ProposeExitConfig` reuses it):
/// true whenever `signer`'s key equals `root.authority`. Checks only key equality —
/// `refund_deposit` calls this against a `chain_authority` account that is NOT required to sign (the
/// refund is permissionless; only the crediting is restricted to the real authority), while
/// `ProposeExitConfig` additionally requires its `chain_authority` account to be a transaction signer —
/// that is the caller's own concern, checked separately, before or after this call.
pub fn require_chain_authority(root: &root::RootFields, signer: &AccountInfo) -> ProgramResult {
    if Pubkey::new_from_array(root.authority) != *signer.key {
        return Err(SettleError::WrongChainAuthority.into());
    }
    Ok(())
}

/// accounts: [chain_config (writable), root (read-only), chain_authority (writable)] — permissionless;
/// anyone may trigger the refund once it is due, but only `root.authority` is ever credited.
pub fn refund_deposit(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    chain_id: u64,
) -> ProgramResult {
    let chain_config_acc = next_account_info(it)?;
    let root_acc = next_account_info(it)?;
    let chain_authority = next_account_info(it)?;

    let cfg = read_chain_config(program_id, chain_id, chain_config_acc)?;
    let (expect_root, _) = crate::chain::root_pda(program_id, chain_id);
    if expect_root != *root_acc.key || root_acc.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    let root = {
        let d = root_acc.try_borrow_data()?;
        root::read(&d).map_err(|_| ProgramError::InvalidAccountData)?
    };
    if root.chain_id != chain_id {
        return Err(ProgramError::InvalidAccountData);
    }
    require_chain_authority(&root, chain_authority)?;
    if cfg.deposit_lamports == 0 || cfg.deposit_refunded {
        return Err(SettleError::NoDepositToRefund.into());
    }
    let eligible = root.head_final_batch >= 1 || cfg.posted_batches >= 10;
    if !eligible {
        return Err(SettleError::RefundNotYetEligible.into());
    }

    let amount = cfg.deposit_lamports;
    **chain_config_acc.try_borrow_mut_lamports()? -= amount;
    **chain_authority.try_borrow_mut_lamports()? += amount;

    let mut cfg = cfg;
    cfg.deposit_lamports = 0;
    cfg.deposit_refunded = true;
    let mut d = chain_config_acc.try_borrow_mut_data()?;
    chain_config::write(&mut d, &cfg);
    msg!(
        "chain {} deposit refunded: {} lamports to {}",
        chain_id,
        amount,
        chain_authority.key
    );
    Ok(())
}

/// accounts: [global_config (read-only), chain_config (writable), root (writable), registry (writable),
/// treasury (writable), reserved_allow marker (read-only — only ever actually live for a reserved chain)]
/// — permissionless. Requires no root ever posted (`root.head_pending_batch == 0`, true for
/// both a reserved and a permissionless chain that never posted) and the reclaim window elapsed since
/// `registered_slot`. No pending PDAs exist to sweep in that state — the first `PostRoot`/`PostRootProved`
/// is what creates the first one, and this path requires none has ever succeeded.
///
/// A RESERVED chain (e.g. Tiber, brought forward via `MigrateChainV2` and never posted) is NOT reclaimable
/// while its `AllowReservedId` marker is still live — the id is not free to hand to anyone else while the
/// registry authority is still vouching for it. The registry authority must `RevokeReservedId` first; only
/// then does the normal window-elapsed reclaim apply. A permissionless chain never has a live marker
/// (`AllowReservedId` only ever targets a reserved id), so this changes nothing for it.
pub fn reclaim_chain(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    chain_id: u64,
) -> ProgramResult {
    let global_acc = next_account_info(it)?;
    let chain_config_acc = next_account_info(it)?;
    let root_acc = next_account_info(it)?;
    let registry_acc = next_account_info(it)?;
    let treasury = next_account_info(it)?;
    let allow_acc = next_account_info(it)?;

    let global = read_global_config(program_id, global_acc)?;
    if Pubkey::new_from_array(global.treasury) != *treasury.key {
        return Err(SettleError::WrongTreasury.into());
    }
    let cfg = read_chain_config(program_id, chain_id, chain_config_acc)?;

    let (expect_root, _) = crate::chain::root_pda(program_id, chain_id);
    if expect_root != *root_acc.key || root_acc.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    let (expect_registry, _) = crate::chain::registry_pda(program_id, chain_id);
    if expect_registry != *registry_acc.key || registry_acc.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    let (expect_allow, _) = reserved_allow_pda(program_id, chain_id);
    if expect_allow != *allow_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }
    if cfg.reserved && allow_acc.owner == program_id && allow_acc.data_len() != 0 {
        return Err(SettleError::ChainNotReclaimable.into());
    }
    let root = {
        let d = root_acc.try_borrow_data()?;
        root::read(&d).map_err(|_| ProgramError::InvalidAccountData)?
    };
    if root.chain_id != chain_id || root.head_pending_batch != 0 {
        return Err(SettleError::ChainNotReclaimable.into());
    }
    let now = Clock::get()?.slot;
    if now
        < cfg
            .registered_slot
            .saturating_add(global.reclaim_window_slots)
    {
        return Err(SettleError::ChainNotReclaimable.into());
    }

    close_to(chain_config_acc, treasury)?;
    close_to(root_acc, treasury)?;
    close_to(registry_acc, treasury)?;
    msg!(
        "chain {} reclaimed: rent + deposit swept to treasury, id free",
        chain_id
    );
    Ok(())
}

/// Zeroes `acc`'s lamports into `dest`, deallocates its data, and hands it back to the system program —
/// the same recycle shape `settle::close_pending` uses.
fn close_to(acc: &AccountInfo, dest: &AccountInfo) -> ProgramResult {
    let lamports = acc.lamports();
    **acc.try_borrow_mut_lamports()? = 0;
    **dest.try_borrow_mut_lamports()? += lamports;
    acc.resize(0)?; // resize(len) replaces the old realloc(len, zero_init) (no-op for a shrink)
    acc.assign(&system_program::id());
    Ok(())
}

/// `SetRegistryEntry` (25): registers a verifier key with an explicit activation delay — vkey rotation is
/// an explicit instruction with a delay. accounts:
/// [registry_authority (signer), payer (signer, writable — funds the one-time v1->v2 realloc), global_config
/// (read-only), registry (writable), system_program]. Same authority gate as `set_fee`/`set_drift_bound`;
/// same realloc-in-place shape as `migrate_chain`'s v1->v2 bring-forward, applied to the registry account
/// the first time this instruction touches it.
///
/// `activation_slot` must be `>= Clock::slot` (`ActivationInPast` otherwise — equal is allowed, immediate
/// activation, the devnet case; `RETIRED_SLOT` = `u64::MAX` always passes this, by construction — it is
/// never in the past). `entry.layout_id` must be `LAYOUT_ZISK_V1` or `LAYOUT_HEADER_FALLBACK`
/// (`UnknownLayout`), and `LAYOUT_HEADER_FALLBACK` only on a reserved chain (`HeaderFallbackNotAllowed`
/// for a permissionless one); `entry.curve`/`entry.scheme` must be one of the known constants
/// (`UnknownCurveOrScheme`).
///
/// **Rotation never touches another entry; retirement is explicit.** The key is
/// `(curve, scheme, vkey_hash)` — the same key `registry::find` uses, NOT `(curve, scheme, layout_id)`
/// (matching on the layout let a delayed rotation of a different vkey under the same layout silently
/// clobber the vkey already registered there). If that vkey is already present:
/// its stored `layout_id` must equal `entry.layout_id` (`LayoutMismatch` otherwise — one vkey is one ELF
/// is one layout), and only its `activation_slot` is updated (same index, same entry bytes, `count`
/// unchanged) — this is also how a vkey is **retired**: call this with the SAME vkey and
/// `activation_slot = RETIRED_SLOT`, and `registry::find`'s own `activation_slot > at_slot` skip excludes
/// it from every real slot from then on. If the vkey is absent: it is appended at `count` when the
/// registry has room, or — once `count == MAX_ENTRIES` — written into the FIRST slot whose
/// `activation_slot == RETIRED_SLOT` (`count` unchanged; `RegistryFull` if none is retired).
pub fn set_registry_entry(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    chain_id: u64,
    entry: RegistryEntryArg,
    activation_slot: u64,
) -> ProgramResult {
    let authority = next_account_info(it)?;
    let payer = next_account_info(it)?;
    let global_acc = next_account_info(it)?;
    let registry_acc = next_account_info(it)?;
    let sys = next_account_info(it)?;
    if !payer.is_signer || *sys.key != system_program::id() {
        return Err(ProgramError::MissingRequiredSignature);
    }
    let global = read_global_config(program_id, global_acc)?;
    require_registry_authority(&global, authority)?;

    if entry.layout_id != registry::LAYOUT_ZISK_V1
        && entry.layout_id != registry::LAYOUT_HEADER_FALLBACK
    {
        return Err(SettleError::UnknownLayout.into());
    }
    // Layout 2 binds neither the chain id nor the inbox commitment, so a permissionless
    // chain's registry may only ever hold layout-1 keys. Reserved chains keep layout 2.
    if entry.layout_id == registry::LAYOUT_HEADER_FALLBACK && !chainid::is_reserved(chain_id) {
        return Err(SettleError::HeaderFallbackNotAllowed.into());
    }
    let curve_known =
        entry.curve == registry::CURVE_BN254 || entry.curve == registry::CURVE_BLS12_381;
    let scheme_known =
        entry.scheme == registry::SCHEME_GROTH16 || entry.scheme == registry::SCHEME_PLONK;
    if !curve_known || !scheme_known {
        return Err(SettleError::UnknownCurveOrScheme.into());
    }

    let (expect_registry, _) = crate::chain::registry_pda(program_id, chain_id);
    if expect_registry != *registry_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }
    if registry_acc.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }

    let now = Clock::get()?.slot;
    if activation_slot < now {
        return Err(SettleError::ActivationInPast.into());
    }

    // One-time v1 -> v2 realloc: top up rent against the account's OWN current balance (the registry
    // holds no escrow, unlike `chain_config` — the whole balance is fair game), then zero the entire new
    // activation tail explicitly (never trust `realloc`'s own zeroing for bytes this instruction does not
    // immediately overwrite — every existing entry's activation slot must read back `0`, not garbage).
    if registry_acc.data_len() < registry::REGISTRY_LEN_V2 {
        let new_len = registry::REGISTRY_LEN_V2;
        let new_min_rent = Rent::get()?.minimum_balance(new_len);
        let have = registry_acc.lamports();
        if new_min_rent > have {
            invoke(
                &system_instruction::transfer(payer.key, registry_acc.key, new_min_rent - have),
                &[payer.clone(), registry_acc.clone(), sys.clone()],
            )?;
        }
        // resize(len) replaces the old realloc(len, zero_init) and always zeroes growth; the manual fill(0) right
        // below stays (it zeroes the tail regardless of which realloc variant ran)
        registry_acc.resize(new_len)?;
        let mut d = registry_acc.try_borrow_mut_data()?;
        d[registry::OFF_ACTIVATION..registry::REGISTRY_LEN_V2].fill(0);
    }

    // Key = (curve, scheme, vkey_hash) — the same key `registry::find` uses, and the writer must mirror
    // the reader's own match order: the FIRST populated entry (by index) matching this key is the
    // canonical one — never a later duplicate, so a retire/rotate call always lands on the entry `find`
    // would still return. A SECOND populated entry matching the same key is a corrupt registry by
    // definition (unreachable once `InitChainV2` refuses duplicate vkeys at genesis) — refused outright
    // rather than silently acting on either copy. Also records the first RETIRED slot seen
    // (`activation_slot == RETIRED_SLOT`), scanned in the same pass, for the full-registry reuse path
    // below — cheaper than a second scan, and every slot is already being read.
    let (count, existing, first_retired_idx) = {
        let d = registry_acc.try_borrow_data()?;
        let hdr = registry::read_header(&d).map_err(|_| ProgramError::InvalidAccountData)?;
        if hdr.chain_id != chain_id {
            return Err(ProgramError::InvalidAccountData);
        }
        let mut existing: Option<(usize, u8, u64)> = None;
        let mut first_retired_idx = None;
        for i in 0..(hdr.count as usize).min(registry::MAX_ENTRIES) {
            let (e, activation_slot) =
                registry::entry_at(&d, i).map_err(|_| ProgramError::InvalidAccountData)?;
            if e.curve == entry.curve && e.scheme == entry.scheme && e.vkey_hash == entry.vkey_hash
            {
                if existing.is_some() {
                    return Err(ProgramError::InvalidAccountData);
                }
                existing = Some((i, e.layout_id, activation_slot));
            }
            if first_retired_idx.is_none() && activation_slot == registry::RETIRED_SLOT {
                first_retired_idx = Some(i);
            }
        }
        (hdr.count, existing, first_retired_idx)
    };

    let (target_idx, is_new_slot) = match existing {
        // Present: only the activation slot changes — never a different layout for the same vkey.
        Some((i, stored_layout_id, stored_activation)) => {
            if stored_layout_id != entry.layout_id {
                return Err(SettleError::LayoutMismatch.into());
            }
            // Retirement is terminal: once this vkey's stored activation slot is the
            // tombstone, no other activation slot may ever be written to it again — only a NEW vkey can
            // take this slot (the `None` arm below, once it is reused). Retiring an already-retired entry
            // (RETIRED_SLOT -> RETIRED_SLOT) is unaffected and stays a no-op success.
            if stored_activation == registry::RETIRED_SLOT
                && activation_slot != registry::RETIRED_SLOT
            {
                return Err(SettleError::EntryRetired.into());
            }
            (i, false)
        }
        // Absent: append if there's room, else reuse the first retired slot, else out of room entirely.
        None => {
            if (count as usize) < registry::MAX_ENTRIES {
                (count as usize, true)
            } else {
                match first_retired_idx {
                    Some(i) => (i, false),
                    None => return Err(SettleError::RegistryFull.into()),
                }
            }
        }
    };

    let new_entry = registry::RegistryEntry {
        curve: entry.curve,
        scheme: entry.scheme,
        vkey_hash: entry.vkey_hash,
        layout_id: entry.layout_id,
    };
    {
        let mut d = registry_acc.try_borrow_mut_data()?;
        registry::write_entry(&mut d, target_idx, &new_entry, activation_slot)
            .map_err(|_| ProgramError::InvalidAccountData)?;
        if is_new_slot {
            d[registry::OFF_COUNT] = count + 1;
        }
    }

    msg!(
        "chain {} registry entry {} {}: curve {} scheme {} layout {} activation_slot {}",
        chain_id,
        target_idx,
        if is_new_slot {
            "appended"
        } else if existing.is_some() {
            "updated"
        } else {
            "reused a retired slot for"
        },
        entry.curve,
        entry.scheme,
        entry.layout_id,
        activation_slot
    );
    Ok(())
}

/// `ProposeExitConfig` (26): the chain authority proposes a change to the chain's exit
/// machinery — the `exit_config` PDA's portal/bridge-program and, indirectly, the root's exit cap and
/// poster bond — with an activation delay that must be at least one full challenge window (no bootstrap
/// exception: the portal is a fund-moving lever). accounts: [chain_authority (signer, must equal
/// `root.authority`), payer (signer, writable — funds `exit_config`'s create-or-adopt), root (read-only),
/// exit_config (writable, new-or-existing), system_program].
///
/// A proposal may set any subset of the four optional fields; an omitted (`None`) field is simply not
/// part of this proposal (its `pending_mask` bit stays clear) — but a *supplied* value may never be the
/// zero sentinel (`ExitPortalZero`/`BridgeProgramZero`), so a later `ActivateExitConfig` can never install
/// an all-zero portal or bridge program. Refuses a second proposal while one is still pending
/// (`PendingExitConfigExists`) — `ActivateExitConfig` or waiting past the current `activation_slot` is the
/// only way to clear that state; there is no cancel path. Writes only the PENDING slots and
/// `activation_slot`; the CURRENT `exit_portal`/`bridge_program` fields are untouched — a proposal is
/// invisible to `ProveExit` until `ActivateExitConfig` copies it across.
#[allow(clippy::too_many_arguments)]
pub fn propose_exit_config(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    chain_id: u64,
    exit_portal: Option<[u8; 20]>,
    bridge_program: Option<Pubkey>,
    exit_cap_per_window: Option<u64>,
    poster_bond: Option<u64>,
    activation_slot: u64,
) -> ProgramResult {
    let chain_authority = next_account_info(it)?;
    let payer = next_account_info(it)?;
    let root_acc = next_account_info(it)?;
    let exit_config_acc = next_account_info(it)?;
    let sys = next_account_info(it)?;
    if !chain_authority.is_signer || !payer.is_signer || *sys.key != system_program::id() {
        return Err(ProgramError::MissingRequiredSignature);
    }

    let (expect_root, _) = root::pda(program_id, chain_id);
    if expect_root != *root_acc.key || root_acc.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    let (expect_exit_config, bump) = exit_config::pda(program_id, chain_id);
    if expect_exit_config != *exit_config_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }

    let root = {
        let d = root_acc.try_borrow_data()?;
        root::read(&d).map_err(|_| ProgramError::InvalidAccountData)?
    };
    if root.chain_id != chain_id {
        return Err(ProgramError::InvalidAccountData);
    }
    require_chain_authority(&root, chain_authority)?;

    if root.challenge_window_slots == 0 {
        return Err(SettleError::ChallengeWindowZero.into());
    }
    let now = Clock::get()?.slot;
    if activation_slot < now + root.challenge_window_slots as u64 {
        return Err(SettleError::ActivationTooSoon.into());
    }

    // A proposal that touches nothing (every optional field None) writes an inert exit_config
    // (pending_mask == 0) that ActivateExitConfig can only ever refuse (NoPendingExitConfig) —
    // refuse it by name here instead of spending the chain authority's rent on a no-op.
    if exit_portal.is_none()
        && bridge_program.is_none()
        && exit_cap_per_window.is_none()
        && poster_bond.is_none()
    {
        return Err(ProgramError::InvalidArgument);
    }

    // Only an already-created, program-owned, full-length exit_config can carry a real pending proposal
    // to collide with — a fresh (system-owned or absent) account has never had one.
    let already_exists =
        exit_config_acc.owner == program_id && exit_config_acc.data_len() == exit_config::LEN;
    let mut fields = if already_exists {
        let d = exit_config_acc.try_borrow_data()?;
        exit_config::read(&d).map_err(|_| ProgramError::InvalidAccountData)?
    } else {
        exit_config::ExitConfigFields {
            chain_id,
            exit_portal: [0u8; 20],
            bridge_program: [0u8; 32],
            pending_exit_portal: [0u8; 20],
            pending_bridge_program: [0u8; 32],
            pending_exit_cap: 0,
            pending_poster_bond: 0,
            activation_slot: 0,
            pending_mask: 0,
        }
    };
    if fields.pending_mask != 0 {
        return Err(SettleError::PendingExitConfigExists.into());
    }

    if let Some(p) = exit_portal {
        if p == [0u8; 20] {
            return Err(SettleError::ExitPortalZero.into());
        }
    }
    if let Some(b) = bridge_program {
        if b == Pubkey::default() {
            return Err(SettleError::BridgeProgramZero.into());
        }
        // The bridge is set once: a chain whose exit config already holds a bridge cannot name
        // another one, nor the same one again.
        if fields.bridge_program != [0u8; 32] {
            return Err(SettleError::BridgeProgramSetOnce.into());
        }
    }

    // Governance is repeatable: on a repeat cycle the account is already program-owned and
    // LEN-sized (a prior proposal created it, and ActivateExitConfig never closes it) — only the
    // very first cycle needs `create_or_adopt_pda` at all. Calling it unconditionally here would
    // walk `already_exists == true` straight into `adopt_funded_pda`'s `*pda.owner !=
    // system_program::id()` guard and revert every proposal after the chain's first, forever
    // freezing the exit portal/bridge/cap/bond at whatever the first cycle set.
    if !already_exists {
        let seeds = exit_config::seeds(chain_id);
        rome_zk_pda::create_or_adopt_pda(
            payer,
            exit_config_acc,
            sys,
            program_id,
            exit_config::LEN,
            &[&seeds[0], &seeds[1], &[bump]],
        )?;
    }

    let mut mask = 0u8;
    if let Some(p) = exit_portal {
        fields.pending_exit_portal = p;
        mask |= exit_config::PENDING_MASK_PORTAL;
    }
    if let Some(b) = bridge_program {
        fields.pending_bridge_program = b.to_bytes();
        mask |= exit_config::PENDING_MASK_BRIDGE;
    }
    if let Some(c) = exit_cap_per_window {
        fields.pending_exit_cap = c;
        mask |= exit_config::PENDING_MASK_CAP;
    }
    if let Some(b) = poster_bond {
        fields.pending_poster_bond = b;
        mask |= exit_config::PENDING_MASK_BOND;
    }
    fields.chain_id = chain_id;
    fields.activation_slot = activation_slot;
    fields.pending_mask = mask;

    let mut d = exit_config_acc.try_borrow_mut_data()?;
    d.copy_from_slice(&exit_config::write(&fields));
    msg!(
        "chain {} exit config proposed: mask {} activation_slot {}",
        chain_id,
        mask,
        activation_slot
    );
    Ok(())
}

/// `ActivateExitConfig` (27): permissionless — anyone may activate a proposal once its delay
/// has elapsed; nothing about the caller is trusted, only the clock and the pending state already written
/// by `ProposeExitConfig`. accounts: [root (writable), exit_config (writable)]. Copies
/// `pending_exit_portal`/`pending_bridge_program` into `exit_config`'s current fields, and
/// `pending_exit_cap`/`pending_poster_bond` into the ROOT's `exit_cap_per_window`/`poster_bond` (the root
/// stays the source of truth for both numbers; its layout is unchanged — nothing here grows it), per the
/// bits actually set in `pending_mask`; then clears every pending slot, `activation_slot` and the mask.
pub fn activate_exit_config(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    chain_id: u64,
) -> ProgramResult {
    let root_acc = next_account_info(it)?;
    let exit_config_acc = next_account_info(it)?;

    let (expect_root, _) = root::pda(program_id, chain_id);
    if expect_root != *root_acc.key || root_acc.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    let (expect_exit_config, _) = exit_config::pda(program_id, chain_id);
    if expect_exit_config != *exit_config_acc.key || exit_config_acc.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }

    let mut root_fields = {
        let d = root_acc.try_borrow_data()?;
        root::read(&d).map_err(|_| ProgramError::InvalidAccountData)?
    };
    if root_fields.chain_id != chain_id {
        return Err(ProgramError::InvalidAccountData);
    }
    let mut fields = {
        let d = exit_config_acc.try_borrow_data()?;
        exit_config::read(&d).map_err(|_| ProgramError::InvalidAccountData)?
    };
    if fields.chain_id != chain_id {
        return Err(ProgramError::InvalidAccountData);
    }

    if fields.pending_mask == 0 {
        return Err(SettleError::NoPendingExitConfig.into());
    }
    let now = Clock::get()?.slot;
    if now < fields.activation_slot {
        return Err(SettleError::ActivationNotReached.into());
    }

    if fields.pending_mask & exit_config::PENDING_MASK_PORTAL != 0 {
        fields.exit_portal = fields.pending_exit_portal;
    }
    if fields.pending_mask & exit_config::PENDING_MASK_BRIDGE != 0 {
        if fields.bridge_program == [0u8; 32] {
            fields.bridge_program = fields.pending_bridge_program;
        } else {
            // A bridge proposal made before the bridge became set-once. It may not replace a set
            // bridge, and a pending proposal can never be cancelled, so refusing here would freeze
            // the chain's exit config for good. Drop the bridge part, apply the rest, clear the slot.
            msg!(
                "chain {} exit config: dropped pending bridge program {}, the bridge is already set",
                chain_id,
                Pubkey::new_from_array(fields.pending_bridge_program)
            );
        }
    }
    if fields.pending_mask & exit_config::PENDING_MASK_CAP != 0 {
        root_fields.exit_cap_per_window = fields.pending_exit_cap;
    }
    if fields.pending_mask & exit_config::PENDING_MASK_BOND != 0 {
        root_fields.poster_bond = fields.pending_poster_bond;
    }

    fields.pending_exit_portal = [0u8; 20];
    fields.pending_bridge_program = [0u8; 32];
    fields.pending_exit_cap = 0;
    fields.pending_poster_bond = 0;
    fields.activation_slot = 0;
    fields.pending_mask = 0;

    {
        let mut d = exit_config_acc.try_borrow_mut_data()?;
        d.copy_from_slice(&exit_config::write(&fields));
    }
    {
        let mut d = root_acc.try_borrow_mut_data()?;
        d.copy_from_slice(&root::write(&root_fields));
    }
    msg!("chain {} exit config activated", chain_id);
    Ok(())
}

#[cfg(test)]
mod pda_parity_tests {
    use super::*;

    /// PDA parity: this program's own `global_config_seeds`/
    /// `chain_config_seeds`/`perm_nonce_seeds`/`reserved_allow_seeds` must equal
    /// `rome_zk_layouts::{global_config, chain_config, perm_nonce, reserved_allow}::seeds` — the single
    /// definitions every consumer (this program, `zk-settlement-client`) is required to share.
    #[test]
    fn global_config_seeds_matches_rome_zk_layouts() {
        assert_eq!(global_config_seeds(), global_config::seeds());
    }

    #[test]
    fn chain_config_seeds_matches_rome_zk_layouts() {
        assert_eq!(chain_config_seeds(7), chain_config::seeds(7));
    }

    #[test]
    fn perm_nonce_seeds_matches_rome_zk_layouts() {
        let authority = Pubkey::new_unique();
        assert_eq!(perm_nonce_seeds(&authority), perm_nonce::seeds(&authority));
    }

    #[test]
    fn reserved_allow_seeds_matches_rome_zk_layouts() {
        assert_eq!(reserved_allow_seeds(7), reserved_allow::seeds(7));
    }
}
