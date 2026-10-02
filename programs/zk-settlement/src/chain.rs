//! `InitChainV2` (24; the original `InitChain` (3) is retired): creates the root account (`rome_zk_layouts::root`,
//! exactly `MIN_LEN` = 202 bytes, every offset used), the verifier registry account
//! (`rome_zk_layouts::registry`), and the `chain_config` account (`rome_zk_layouts::chain_config`) for one
//! chain, in one instruction. `head_pending_batch = head_final_batch = 0` (the "none/genesis" sentinel —
//! `PostRoot`'s first call treats batch 1's predecessor as the values written here) and `pending_count =
//! 0`.
//!
//! Two chain-id namespaces:
//! - RESERVED (`chain_id < 2^32`): requires both the chain authority and the Rome registry authority
//!   (`global_config.registry_authority`) as signers, and the id must have a live `AllowReservedId`
//!   marker (`governance::reserved_allow_pda`). No deposit.
//! - PERMISSIONLESS (`chain_id >= 2^32`): requires `global_config.permissionless_init_enabled`; the
//!   caller names a `permissionless_nonce` claimed to be `>= ` its authority's currently-stored nonce
//!   (`rome_zk_layouts::perm_nonce`), the program independently recomputes
//!   `rome_zk_layouts::chainid::permissionless_chain_id(authority, permissionless_nonce)` and rejects any
//!   `chain_id` argument that disagrees — the client must compute the same id off-chain first (to know
//!   which `root`/`registry`/`chain_config` addresses to pass), but cannot choose or front-run it. The
//!   nonce need not be exactly the stored value: an authority whose next id
//!   was pre-claimed by someone else (see `require_permissionless_id`'s doc) can skip past it by naming a
//!   later nonce; a rejected call leaves the stored nonce untouched (a failed instruction has no partial
//!   commit) — only a call that succeeds ever advances it, to `permissionless_nonce + 1`. Locks
//!   `global_config.deposit_lamports` into `chain_config`'s own balance as escrow.
//!   A permissionless call must carry an EMPTY `registry_entries` (`RegistryEntriesNotAllowed` otherwise,
//!   checked before anything is created, locked or advanced): the chain starts with an empty
//!   registry and cannot finalize a proved root until the registry authority adds its layout-1 key with
//!   `SetRegistryEntry`. The reserved path keeps accepting `registry_entries`, since the registry
//!   authority co-signs it.

use crate::errors::SettleError;
use crate::governance::{self, chain_config_pda, perm_nonce_pda, reserved_allow_pda};
use borsh::{BorshDeserialize, BorshSerialize};
use rome_zk_layouts::{chain_config, chainid, perm_nonce, registry, root};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    program::invoke,
    program_error::ProgramError,
    pubkey::Pubkey,
    sysvar::Sysvar,
};
// `system_instruction`/`system_program` moved out of `solana_program`'s root re-export in the Agave 4.x
// line (API fallout).
use solana_system_interface::{instruction as system_instruction, program as system_program};

/// The single definitions are `rome_zk_layouts::root`/`registry`; kept under
/// these names so every existing call site in this program is unchanged.
#[inline]
pub fn root_seeds(chain_id: u64) -> [Vec<u8>; 2] {
    root::seeds(chain_id)
}
#[inline]
pub fn registry_seeds(chain_id: u64) -> [Vec<u8>; 2] {
    registry::seeds(chain_id)
}
#[inline]
pub fn root_pda(program_id: &Pubkey, chain_id: u64) -> (Pubkey, u8) {
    root::pda(program_id, chain_id)
}
#[inline]
pub fn registry_pda(program_id: &Pubkey, chain_id: u64) -> (Pubkey, u8) {
    registry::pda(program_id, chain_id)
}

/// One verifier registration, as an instruction argument.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, Copy)]
pub struct RegistryEntryArg {
    pub curve: u8,
    pub scheme: u8,
    pub vkey_hash: [u8; 32],
    pub layout_id: u8,
}

/// RETIRED body of `InitChain` (3): kept byte-for-byte as recorded on chain (at slot 2554,
/// `fixtures/settlement-program/txv1-dev-initchain-slot2554.bin`) so recorded history decodes forever;
/// the program refuses it by name. New chains use [`InitChainArgsV2`] / `InitChainV2` (24).
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone)]
pub struct InitChainArgs {
    pub chain_id: u64,
    pub permissionless_nonce: u64,
    pub number: u64,
    pub parent_hash: [u8; 32],
    pub state_root: [u8; 32],
    pub block_hash: [u8; 32],
    pub profile: u8,
    pub challenge_window_slots: u32,
    pub prove_window_slots: u32,
    pub proving_policy: u8,
    pub poster_bond: u64,
    pub exit_cap_per_window: u64,
    pub authority: Pubkey,
    pub max_pending: u32,
    pub inbox_program: Pubkey,
    pub registry_entries: Vec<RegistryEntryArg>,
}

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone)]
pub struct InitChainArgsV2 {
    pub chain_id: u64,
    /// The permissionless nonce the caller claims produces `chain_id` (`chainid::permissionless_chain_id
    /// (authority, permissionless_nonce) == chain_id`). Ignored on the reserved path — same "ignored on
    /// the other path" shape `registry_authority_acc` already has (module doc). Must be
    /// `>= ` the authority's currently-stored nonce, not necessarily equal to it — see
    /// `require_permissionless_id`'s doc for why a caller ever needs to skip ahead.
    pub permissionless_nonce: u64,
    pub number: u64,
    pub parent_hash: [u8; 32],
    pub state_root: [u8; 32],
    pub block_hash: [u8; 32],
    pub profile: u8,
    pub challenge_window_slots: u32,
    pub prove_window_slots: u32,
    pub proving_policy: u8,
    pub poster_bond: u64,
    pub exit_cap_per_window: u64,
    pub authority: Pubkey,
    pub max_pending: u32,
    pub inbox_program: Pubkey,
    pub registry_entries: Vec<RegistryEntryArg>,
    /// The chain's drift bound: `PostRootProved`'s layout-1 path checks a proof's
    /// committed `max_drift_secs` against this value. An explicit argument, never a program constant
    /// (ops values are runtime parameters) — `0` is refused (`DriftBoundZero`).
    pub max_drift_secs: u64,
}

/// `InitChainV2` (24). accounts: [payer (signer, writable), chain_authority (signer), registry_authority (signer only when
/// `chain_id` is reserved — ignored, need not even be a real co-signer, on the permissionless path), root
/// pda (writable, new), registry pda (writable, new), chain_config pda (writable, new), global_config
/// (read-only), reserved_allow marker OR perm_nonce pda (reserved: read-only, must already exist;
/// permissionless: writable, created on first use), system_program].
///
/// On the permissionless path `args.registry_entries` must be empty (`RegistryEntriesNotAllowed`):
/// the refusal comes first, before the nonce account or any chain account is created and before the deposit is locked.
/// Verifier keys for such a chain are added afterwards by the registry authority.
pub fn init_chain(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    args: InitChainArgsV2,
) -> ProgramResult {
    if args.registry_entries.len() > registry::MAX_ENTRIES {
        return Err(ProgramError::InvalidArgument);
    }
    if find_duplicate_registry_entry(&args.registry_entries).is_some() {
        return Err(SettleError::DuplicateRegistryEntry.into());
    }
    if args.max_drift_secs == 0 {
        return Err(SettleError::DriftBoundZero.into());
    }
    let payer = next_account_info(it)?;
    let chain_authority = next_account_info(it)?;
    let registry_authority_acc = next_account_info(it)?;
    let root_acc = next_account_info(it)?;
    let registry_acc = next_account_info(it)?;
    let chain_config_acc = next_account_info(it)?;
    let global_acc = next_account_info(it)?;
    let namespace_acc = next_account_info(it)?;
    let sys = next_account_info(it)?;
    if !payer.is_signer || *sys.key != system_program::id() {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if !chain_authority.is_signer || *chain_authority.key != args.authority {
        return Err(SettleError::NotChainAuthority.into());
    }

    let global = governance::read_global_config(program_id, global_acc)?;
    let reserved = chainid::is_reserved(args.chain_id);
    let deposit_lamports = if reserved {
        require_reserved_id_allowed(
            program_id,
            &global,
            registry_authority_acc,
            namespace_acc,
            args.chain_id,
        )?;
        0
    } else {
        // A permissionless chain carries no verifier keys of its own. Refused here,
        // ahead of `require_permissionless_id` (which creates the nonce account and derives the id) and
        // ahead of every `create_account` below, so a refused call costs a few CU and touches nothing.
        if !args.registry_entries.is_empty() {
            return Err(SettleError::RegistryEntriesNotAllowed.into());
        }
        require_permissionless_id(
            program_id,
            &global,
            payer,
            &chain_authority.key.to_bytes(),
            namespace_acc,
            sys,
            args.chain_id,
            args.permissionless_nonce,
        )?;
        global.deposit_lamports
    };

    let (expect_root, root_bump) = root_pda(program_id, args.chain_id);
    if expect_root != *root_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }
    let (expect_registry, registry_bump) = registry_pda(program_id, args.chain_id);
    if expect_registry != *registry_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }
    let (expect_chain_config, chain_config_bump) = chain_config_pda(program_id, args.chain_id);
    if expect_chain_config != *chain_config_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }

    // Both PDAs' addresses are public and predictable ((chain_id)-derived) — an attacker can pre-fund
    // either before this transaction lands, and a bare `create_account` would then fail
    // `AccountAlreadyInUse` forever (chain bring-up has no other id to fall back to). `create_or_adopt_pda`
    // adopts a pre-funded PDA instead.
    let root_seeds = root_seeds(args.chain_id);
    rome_zk_pda::create_or_adopt_pda(
        payer,
        root_acc,
        sys,
        program_id,
        root::MIN_LEN,
        &[&root_seeds[0], &root_seeds[1], &[root_bump]],
    )?;

    let registry_seeds = registry_seeds(args.chain_id);
    rome_zk_pda::create_or_adopt_pda(
        payer,
        registry_acc,
        sys,
        program_id,
        registry::REGISTRY_LEN,
        &[&registry_seeds[0], &registry_seeds[1], &[registry_bump]],
    )?;

    let cc_seeds = governance::chain_config_seeds(args.chain_id);
    rome_zk_pda::create_or_adopt_pda(
        payer,
        chain_config_acc,
        sys,
        program_id,
        chain_config::LEN_V2,
        &[&cc_seeds[0], &cc_seeds[1], &[chain_config_bump]],
    )?;
    // The deposit escrow is the chain_config account's own lamport balance above its rent-exempt
    // minimum — a plain transfer on top of the create/adopt above (`payer` is a real signer, no PDA
    // signature needed).
    if deposit_lamports > 0 {
        invoke(
            &system_instruction::transfer(payer.key, chain_config_acc.key, deposit_lamports),
            &[payer.clone(), chain_config_acc.clone(), sys.clone()],
        )?;
    }
    let registered_slot = Clock::get()?.slot;
    {
        let mut d = chain_config_acc.try_borrow_mut_data()?;
        chain_config::write(
            &mut d,
            &chain_config::ChainConfigFields {
                chain_id: args.chain_id,
                reserved,
                deposit_lamports,
                deposit_refunded: deposit_lamports == 0,
                registered_slot,
                posted_batches: 0,
                fee_base_lamports: global.default_fee_base_lamports,
                fee_bps: global.default_fee_bps,
                max_drift_secs: Some(args.max_drift_secs),
            },
        );
    }

    {
        let mut d = root_acc.try_borrow_mut_data()?;
        d[root::OFF_MAGIC..root::OFF_MAGIC + 4].copy_from_slice(&root::MAGIC.to_le_bytes());
        d[root::OFF_CHAIN_ID..root::OFF_CHAIN_ID + 8].copy_from_slice(&args.chain_id.to_le_bytes());
        d[root::OFF_NUMBER..root::OFF_NUMBER + 8].copy_from_slice(&args.number.to_le_bytes());
        d[root::OFF_PARENT_HASH..root::OFF_PARENT_HASH + 32].copy_from_slice(&args.parent_hash);
        d[root::OFF_STATE_ROOT..root::OFF_STATE_ROOT + 32].copy_from_slice(&args.state_root);
        d[root::OFF_BLOCK_HASH..root::OFF_BLOCK_HASH + 32].copy_from_slice(&args.block_hash);
        // OFF_UPDATES left at 0
        d[root::OFF_PROFILE] = args.profile;
        d[root::OFF_CHALLENGE_WINDOW_SLOTS..root::OFF_CHALLENGE_WINDOW_SLOTS + 4]
            .copy_from_slice(&args.challenge_window_slots.to_le_bytes());
        d[root::OFF_PROVE_WINDOW_SLOTS..root::OFF_PROVE_WINDOW_SLOTS + 4]
            .copy_from_slice(&args.prove_window_slots.to_le_bytes());
        d[root::OFF_PROVING_POLICY] = args.proving_policy;
        d[root::OFF_POSTER_BOND..root::OFF_POSTER_BOND + 8]
            .copy_from_slice(&args.poster_bond.to_le_bytes());
        d[root::OFF_EXIT_CAP_PER_WINDOW..root::OFF_EXIT_CAP_PER_WINDOW + 8]
            .copy_from_slice(&args.exit_cap_per_window.to_le_bytes());
        d[root::OFF_AUTHORITY..root::OFF_AUTHORITY + 32].copy_from_slice(args.authority.as_ref());
        // OFF_HEAD_PENDING_BATCH / OFF_HEAD_FINAL_BATCH / OFF_PENDING_COUNT left at 0 (genesis)
        d[root::OFF_MAX_PENDING..root::OFF_MAX_PENDING + 4]
            .copy_from_slice(&args.max_pending.to_le_bytes());
    }
    {
        let mut d = registry_acc.try_borrow_mut_data()?;
        d[registry::OFF_MAGIC..registry::OFF_MAGIC + 4]
            .copy_from_slice(&registry::MAGIC.to_le_bytes());
        d[registry::OFF_CHAIN_ID..registry::OFF_CHAIN_ID + 8]
            .copy_from_slice(&args.chain_id.to_le_bytes());
        d[registry::OFF_INBOX_PROGRAM..registry::OFF_INBOX_PROGRAM + 32]
            .copy_from_slice(args.inbox_program.as_ref());
        d[registry::OFF_COUNT] = args.registry_entries.len() as u8;
        for (i, e) in args.registry_entries.iter().enumerate() {
            let base = registry::OFF_ENTRIES + i * registry::ENTRY_LEN;
            d[base] = e.curve;
            d[base + 1] = e.scheme;
            d[base + 2..base + 34].copy_from_slice(&e.vkey_hash);
            d[base + 34] = e.layout_id;
        }
    }
    solana_program::msg!(
        "chain {} initialized ({}): root {}, registry {}, chain_config {}, {} verifier entries",
        args.chain_id,
        if reserved {
            "reserved"
        } else {
            "permissionless"
        },
        root_acc.key,
        registry_acc.key,
        chain_config_acc.key,
        args.registry_entries.len()
    );
    Ok(())
}

/// Reserved path: both the chain authority (checked by the caller, above) and the registry
/// authority must sign, and `chain_id` must carry a live `AllowReservedId` marker.
fn require_reserved_id_allowed(
    program_id: &Pubkey,
    global: &rome_zk_layouts::global_config::GlobalConfigFields,
    registry_authority_acc: &AccountInfo,
    allow_acc: &AccountInfo,
    chain_id: u64,
) -> ProgramResult {
    governance::require_registry_authority(global, registry_authority_acc)?;
    let (expect_allow, _) = reserved_allow_pda(program_id, chain_id);
    if expect_allow != *allow_acc.key || allow_acc.owner != program_id {
        return Err(SettleError::ReservedIdNotAllowed.into());
    }
    let d = allow_acc.try_borrow_data()?;
    let allowed_id = rome_zk_layouts::reserved_allow::read_chain_id(&d)
        .map_err(|_| SettleError::ReservedIdNotAllowed)?;
    if allowed_id != chain_id {
        return Err(SettleError::ReservedIdNotAllowed.into());
    }
    Ok(())
}

/// Permissionless path: reads (or bootstraps) the authority's nonce PDA, checks
/// `global.permissionless_init_enabled`, recomputes the expected id at the CALLER-CLAIMED `nonce`, and
/// rejects a mismatch — then stores `nonce + 1`.
///
/// `nonce` need not equal the stored `current_nonce` — only `>= current_nonce` — because the id space is
/// not the caller's private property to walk one-by-one. `permissionless_chain_id` is a public function
/// of `(authority, nonce)`; anyone can precompute the id a given authority's *next* call would produce
/// and grind a colliding `(their_own_authority, some_nonce)` pair that lands on that exact same id first
/// (birthday-bound work against a ~2^53 modulus, not infeasible the way finding an SHA-256 preimage is) —
/// `create_or_adopt_pda`'s very purpose (adopting pre-funded PDAs) means that pre-creation doesn't even
/// fail loudly, it just means the victim's own later `InitChain` at their true next nonce silently adopts
/// an already-claimed-by- someone-else chain (wrong `authority` written into `root`). A victim who
/// discovers their next id was burned this way must be able to skip it without waiting on a rescue
/// instruction: they simply try the next nonce (or the one after that), same as this function already
/// recomputes from whatever `nonce` the caller passes. The stored nonce afterward is `nonce + 1`, not
/// `current_nonce + 1` — so a skip is recorded as a skip, never replayed.
///
/// This function is only ever reached mid-transaction — a rejection here means the whole transaction
/// reverts and NO account write survives, `perm_nonce` included (this replaces the old docstring's false
/// "advances unconditionally": Solana instructions have no partial-commit).
#[allow(clippy::too_many_arguments)]
fn require_permissionless_id<'a>(
    program_id: &Pubkey,
    global: &rome_zk_layouts::global_config::GlobalConfigFields,
    payer: &AccountInfo<'a>,
    authority: &[u8; 32],
    nonce_acc: &AccountInfo<'a>,
    sys: &AccountInfo<'a>,
    chain_id: u64,
    nonce: u64,
) -> ProgramResult {
    if !global.permissionless_init_enabled {
        return Err(SettleError::PermissionlessInitDisabled.into());
    }
    let (expect_nonce, bump) = perm_nonce_pda(program_id, &Pubkey::new_from_array(*authority));
    if expect_nonce != *nonce_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }
    let already_initialized = nonce_acc.owner == program_id && nonce_acc.data_len() != 0;
    let current_nonce = if already_initialized {
        let d = nonce_acc.try_borrow_data()?;
        perm_nonce::read(&d)
            .map_err(|_| ProgramError::InvalidAccountData)?
            .nonce
    } else {
        0
    };
    if nonce < current_nonce {
        return Err(SettleError::BadPermissionlessChainId.into());
    }

    let expected_id =
        chainid::permissionless_chain_id(&rome_zk_merkle::keccak256, authority, nonce);
    if expected_id != chain_id {
        return Err(SettleError::BadPermissionlessChainId.into());
    }

    if !already_initialized {
        let seeds = governance::perm_nonce_seeds(&Pubkey::new_from_array(*authority));
        rome_zk_pda::create_or_adopt_pda(
            payer,
            nonce_acc,
            sys,
            program_id,
            perm_nonce::LEN,
            &[&seeds[0], &seeds[1], &[bump]],
        )?;
    }
    let mut d = nonce_acc.try_borrow_mut_data()?;
    perm_nonce::write(
        &mut d,
        &perm_nonce::NonceFields {
            authority: *authority,
            nonce: next_nonce(nonce)?,
        },
    );
    Ok(())
}

/// `nonce + 1` at `u64::MAX` would silently wrap to `0` — an authority's stored nonce cycling back into
/// an already-consumed id range instead of failing closed. Split out as its own function so the
/// overflow edge is unit-testable directly, without driving a `perm_nonce` account to `u64::MAX - 1`
/// through `solana-program-test` (not constructible — that many successful `InitChain` calls is not a
/// real test).
fn next_nonce(nonce: u64) -> Result<u64, ProgramError> {
    nonce.checked_add(1).ok_or(ProgramError::InvalidArgument)
}

/// Two `registry_entries` sharing `(curve, scheme, vkey_hash)` are never both meaningful —
/// one vkey is one ELF is one layout, so this is a duplicate even when the two name DIFFERENT `layout_id`
/// values. Returns the first colliding pair's indices, if any, so `init_chain` can refuse before writing
/// anything. Split out as its own function so the O(n^2) genesis check (n <= `MAX_ENTRIES` = 4, so at
/// most 6 comparisons) is unit-testable directly, without driving a real `InitChainV2` through
/// `solana-program-test`.
fn find_duplicate_registry_entry(entries: &[RegistryEntryArg]) -> Option<(usize, usize)> {
    for i in 0..entries.len() {
        for j in (i + 1)..entries.len() {
            let a = &entries[i];
            let b = &entries[j];
            if a.curve == b.curve && a.scheme == b.scheme && a.vkey_hash == b.vkey_hash {
                return Some((i, j));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_nonce_rejects_overflow_at_u64_max() {
        assert_eq!(
            next_nonce(u64::MAX).unwrap_err(),
            ProgramError::InvalidArgument
        );
    }

    #[test]
    fn next_nonce_increments_normally() {
        assert_eq!(next_nonce(41).unwrap(), 42);
    }

    fn arg(curve: u8, scheme: u8, vkey_hash: [u8; 32], layout_id: u8) -> RegistryEntryArg {
        RegistryEntryArg {
            curve,
            scheme,
            vkey_hash,
            layout_id,
        }
    }

    /// Four distinct vkeys (the ordinary genesis shape) have no duplicate.
    #[test]
    fn find_duplicate_registry_entry_none_for_distinct_vkeys() {
        let entries = [
            arg(0, 1, [1u8; 32], 2),
            arg(0, 1, [2u8; 32], 2),
            arg(0, 0, [3u8; 32], 2),
            arg(1, 0, [4u8; 32], 1),
        ];
        assert_eq!(find_duplicate_registry_entry(&entries), None);
    }

    /// The `[L2, L2, A, A]` shape reduced to its essential pair: an exact duplicate (same
    /// curve/scheme/vkey_hash, same layout) is caught.
    #[test]
    fn find_duplicate_registry_entry_catches_an_exact_duplicate() {
        let entries = [
            arg(0, 1, [9u8; 32], 2),
            arg(0, 1, [9u8; 32], 2),
            arg(0, 0, [3u8; 32], 2),
        ];
        assert_eq!(find_duplicate_registry_entry(&entries), Some((0, 1)));
    }

    /// The `[A/L1, A/L2]` shape: the SAME vkey under two DIFFERENT layouts is still a
    /// duplicate — one vkey is one ELF is one layout, so this is never meaningful either.
    #[test]
    fn find_duplicate_registry_entry_catches_same_vkey_different_layout() {
        let entries = [arg(0, 1, [7u8; 32], 1), arg(0, 1, [7u8; 32], 2)];
        assert_eq!(find_duplicate_registry_entry(&entries), Some((0, 1)));
    }

    /// PDA parity: this program's own `root_seeds`/`registry_seeds` must equal
    /// `rome_zk_layouts::{root, registry}::seeds` — the single definitions every consumer (this program,
    /// `zk-settlement-client`) is required to share.
    #[test]
    fn root_seeds_matches_rome_zk_layouts_root_seeds() {
        assert_eq!(root_seeds(7), root::seeds(7));
    }

    #[test]
    fn registry_seeds_matches_rome_zk_layouts_registry_seeds() {
        assert_eq!(registry_seeds(7), registry::seeds(7));
    }
}
