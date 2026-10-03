//! Thin client for `programs/zk-settlement`: instruction builders, PDA derivation, and account decoding
//! for the root, pending and registry accounts. No async runtime opinions — this crate only builds
//! `Instruction`s and decodes bytes; sending transactions is the caller's job (the batch poster's own
//! async stack, or `examples/devnet_driver.rs` behind the `devnet-driver` feature here). The zk-inbox
//! chunk `Close` path uses [`root_pda`] to derive the settlement root account it needs to read.

use solana_program::{instruction::AccountMeta, pubkey::Pubkey};
// `system_program`/`bpf_loader_upgradeable` moved out of `solana_program`'s root re-export in the Agave 4.x
// line (API fallout).
use solana_system_interface::program as system_program;
use zk_settlement::{
    chain::{InitChainArgsV2, RegistryEntryArg},
    governance::InitGlobalConfigArgs,
};

pub use zk_settlement::exit::{ConsumeExitArgs, ExitMessageArg, ProveExitArgs};

pub use rome_zk_layouts::chainid::{is_reserved, permissionless_chain_id};
pub use zk_settlement::chain::RegistryEntryArg as RegistryEntry;
pub use zk_settlement::governance::InitGlobalConfigArgs as GlobalConfigFields;
pub use zk_settlement::governance::SetGlobalConfigArgs as GlobalConfigUpdate;
pub use zk_settlement::settle::{PostRootArgs as PostRootFields, RootViewData};
pub use zk_settlement::SettleIx;

pub mod ops_plan;

/// Decodes a raw top-level instruction's data back into a [`SettleIx`] — the inverse of every
/// instruction builder above (instruction decoding lives
/// here, next to the builders, never re-implemented by a consumer). Used by the settlement watcher and
/// any other reader of on-chain transaction history; never by anything that only *builds* transactions.
pub fn decode_instruction(data: &[u8]) -> Result<SettleIx, std::io::Error> {
    borsh::from_slice(data)
}

/// `["<program_id>"]` under `bpf_loader_upgradeable` — the program's own `ProgramData` account
/// (`InitGlobalConfig`'s upgrade-authority check).
pub fn program_data_pda(program_id: &Pubkey) -> Pubkey {
    solana_loader_v3_interface::get_program_data_address(program_id)
}

/// `["global_config"]` under the settlement program — the single definition is
/// `rome_zk_layouts::global_config::pda`.
pub fn global_config_pda(program_id: &Pubkey) -> (Pubkey, u8) {
    rome_zk_layouts::global_config::pda(program_id)
}
/// `["chain_config", chain_id]` under the settlement program.
pub fn chain_config_pda(program_id: &Pubkey, chain_id: u64) -> (Pubkey, u8) {
    rome_zk_layouts::chain_config::pda(program_id, chain_id)
}
/// `["perm_nonce", authority]` under the settlement program.
pub fn perm_nonce_pda(program_id: &Pubkey, authority: &Pubkey) -> (Pubkey, u8) {
    rome_zk_layouts::perm_nonce::pda(program_id, authority)
}
/// `["reserved_allow", chain_id]` under the settlement program.
pub fn reserved_allow_pda(program_id: &Pubkey, chain_id: u64) -> (Pubkey, u8) {
    rome_zk_layouts::reserved_allow::pda(program_id, chain_id)
}

/// A permissionless registration's expected chain id for `authority`'s current on-chain nonce
/// (`nonce` — read the `perm_nonce_pda` account and pass its `nonce` field, or `0` if that account does
/// not exist yet). Uses `rome_zk_merkle::keccak256` (this workspace's one keccak) so this is callable
/// off-chain; the on-chain program computes the identical value from the same function, dispatched to
/// the syscall by `target_os`.
pub fn derive_permissionless_chain_id(authority: &Pubkey, nonce: u64) -> u64 {
    permissionless_chain_id(&rome_zk_merkle::keccak256, &authority.to_bytes(), nonce)
}

/// What the `register_chain chain-id` command prints for an authority: the nonce the program will use for its
/// next permissionless registration and the chain id derived from it. One `key=value` per line, so a shell can
/// read it without a parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionlessChainId {
    pub authority: Pubkey,
    pub nonce: u64,
    pub chain_id: u64,
}

impl PermissionlessChainId {
    pub fn new(authority: Pubkey, nonce: u64) -> Self {
        Self {
            authority,
            nonce,
            chain_id: derive_permissionless_chain_id(&authority, nonce),
        }
    }
}

impl std::fmt::Display for PermissionlessChainId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "authority={}", self.authority)?;
        writeln!(f, "nonce={}", self.nonce)?;
        write!(f, "chain_id={}", self.chain_id)
    }
}

/// `["root", chain_id]` under the settlement program.
pub fn root_pda(program_id: &Pubkey, chain_id: u64) -> (Pubkey, u8) {
    rome_zk_layouts::root::pda(program_id, chain_id)
}
/// `["registry", chain_id]` under the settlement program.
pub fn registry_pda(program_id: &Pubkey, chain_id: u64) -> (Pubkey, u8) {
    rome_zk_layouts::registry::pda(program_id, chain_id)
}
/// `["pending", chain_id, batch]` under the settlement program.
pub fn pending_pda(program_id: &Pubkey, chain_id: u64, batch: u64) -> (Pubkey, u8) {
    rome_zk_layouts::pending::pda(program_id, chain_id, batch)
}
/// `["exit_config", chain_id]` under the settlement program.
pub fn exit_config_pda(program_id: &Pubkey, chain_id: u64) -> (Pubkey, u8) {
    rome_zk_layouts::exit::exit_config::pda(program_id, chain_id)
}
/// `["exit", chain_id, message_hash]` under the settlement program.
pub fn exit_record_pda(program_id: &Pubkey, chain_id: u64, message_hash: [u8; 32]) -> (Pubkey, u8) {
    rome_zk_layouts::exit::exit_record::pda(program_id, chain_id, message_hash)
}
/// `["exit_window", chain_id, window_index]` under the settlement program.
pub fn exit_window_pda(program_id: &Pubkey, chain_id: u64, window_index: u64) -> (Pubkey, u8) {
    rome_zk_layouts::exit::exit_window::pda(program_id, chain_id, window_index)
}
/// `["exit_nullifier", chain_id, page]` under the settlement program. `page` is
/// `rome_zk_layouts::exit::nullifier_page(nonce)`, never a caller-chosen value.
pub fn exit_nullifier_pda(program_id: &Pubkey, chain_id: u64, page: u64) -> (Pubkey, u8) {
    rome_zk_layouts::exit::exit_nullifier::pda(program_id, chain_id, page)
}
/// `["exit_consumer", chain_id]` under `bridge_program` — NOT under the settlement program;
/// this is the PDA `ConsumeExit`'s `bridge_signer` account must be, CPI-signed by whichever program
/// `exit_config.bridge_program` currently names. The single definition is
/// `rome_zk_layouts::exit::exit_consumer_pda`.
pub fn exit_consumer_pda(chain_id: u64, bridge_program: &Pubkey) -> (Pubkey, u8) {
    rome_zk_layouts::exit::exit_consumer_pda(chain_id, bridge_program)
}
/// `["batch", settlement_program, chain_id, batch]` under `inbox_program` — the inbox batch account
/// `PostRoot`/`PostRootProved` read; matches `zk_inbox_client::batch_pda`. The single definition is
/// `rome_zk_layouts::batch::pda`.
pub fn inbox_batch_pda(
    inbox_program: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
    batch: u64,
) -> Pubkey {
    rome_zk_layouts::batch::pda(inbox_program, settlement_program, chain_id, batch).0
}

fn ix(
    program_id: &Pubkey,
    accounts: Vec<AccountMeta>,
    data: SettleIx,
) -> solana_program::instruction::Instruction {
    solana_program::instruction::Instruction {
        program_id: *program_id,
        accounts,
        data: borsh::to_vec(&data).expect("SettleIx always serializes"),
    }
}

/// Every `InitChain` field except `chain_id`/`authority` (both threaded separately — `authority` doubles
/// as the required chain-authority signer, and `chain_id` is derived, not chosen, on the permissionless
/// path).
#[allow(clippy::too_many_arguments)]
pub struct InitChainFields {
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
    pub max_pending: u32,
    pub inbox_program: Pubkey,
    pub registry_entries: Vec<RegistryEntry>,
    /// The chain's drift bound — see `zk_settlement::chain::InitChainArgs`'s doc.
    /// `0` is refused (`DriftBoundZero`).
    pub max_drift_secs: u64,
}

fn init_chain_args(
    chain_id: u64,
    permissionless_nonce: u64,
    authority: Pubkey,
    f: InitChainFields,
) -> SettleIx {
    SettleIx::InitChainV2(InitChainArgsV2 {
        chain_id,
        permissionless_nonce,
        number: f.number,
        parent_hash: f.parent_hash,
        state_root: f.state_root,
        block_hash: f.block_hash,
        profile: f.profile,
        challenge_window_slots: f.challenge_window_slots,
        prove_window_slots: f.prove_window_slots,
        proving_policy: f.proving_policy,
        poster_bond: f.poster_bond,
        exit_cap_per_window: f.exit_cap_per_window,
        authority,
        max_pending: f.max_pending,
        inbox_program: f.inbox_program,
        max_drift_secs: f.max_drift_secs,
        registry_entries: f
            .registry_entries
            .into_iter()
            .map(|e| RegistryEntryArg {
                curve: e.curve,
                scheme: e.scheme,
                vkey_hash: e.vkey_hash,
                layout_id: e.layout_id,
            })
            .collect(),
    })
}

/// The genesis registry for a chain proving under the ZisK stateless-validator guest: the
/// layout-1 primary entry FIRST (`registry_entries[0]`, `layout_id`
/// [`rome_zk_layouts::registry::LAYOUT_ZISK_V1`]), then the two existing layout-2 fallback entries — the
/// ZisK PLONK header-RLP path and the zeroed Groth16 slot (`register_chain.rs`'s long-standing shape,
/// unchanged here). Factored out of `register_chain.rs` so it is unit-tested directly: a proof under the
/// layout-1 vkey must find `registry_entries[0]`, never a fallback slot that happens to share
/// `(curve, scheme)` (`rome_zk_layouts::registry::find`).
pub fn registry_entries_for_init(
    layout1_vk: [u8; 32],
    fallback_vk: [u8; 32],
) -> Vec<RegistryEntry> {
    vec![
        RegistryEntry {
            curve: rome_zk_layouts::registry::CURVE_BN254,
            scheme: rome_zk_layouts::registry::SCHEME_PLONK,
            vkey_hash: layout1_vk,
            layout_id: rome_zk_layouts::registry::LAYOUT_ZISK_V1,
        },
        RegistryEntry {
            curve: rome_zk_layouts::registry::CURVE_BN254,
            scheme: rome_zk_layouts::registry::SCHEME_PLONK,
            vkey_hash: fallback_vk,
            layout_id: rome_zk_layouts::registry::LAYOUT_HEADER_FALLBACK,
        },
        RegistryEntry {
            curve: rome_zk_layouts::registry::CURVE_BN254,
            scheme: rome_zk_layouts::registry::SCHEME_GROTH16,
            vkey_hash: [0u8; 32],
            layout_id: rome_zk_layouts::registry::LAYOUT_HEADER_FALLBACK,
        },
    ]
}

/// Why `register_chain` refuses a flag combination before it makes any RPC call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RegisterChainRefusal {
    /// `--permissionless` was given a verifier key file. A permissionless chain registers with an empty
    /// registry and the program refuses anything else (`RegistryEntriesNotAllowed`), so a
    /// key passed here would silently not be registered. Rome adds the key afterwards with
    /// `SetRegistryEntry`.
    #[error(
        "VkeysNotAllowedOnPermissionless: a permissionless chain registers with no verifier keys; \
         Rome registers its layout-1 key afterwards (SetRegistryEntry). Drop --layout1-vkey-json / \
         --zisk-vkey-json, or use --reserved"
    )]
    VkeysNotAllowedOnPermissionless,
    /// `--reserved` without `--layout1-vkey-json`: a reserved chain registered without a layout-1 entry
    /// can never finalize a proved root.
    #[error("--reserved requires --layout1-vkey-json (a chain without a layout-1 entry cannot finalize a proved root)")]
    MissingLayout1Vkey,
    /// `--reserved` without `--zisk-vkey-json` (the header-RLP fallback entry).
    #[error("--reserved requires --zisk-vkey-json")]
    MissingZiskVkey,
}

/// The flag check `register_chain` runs first, before it reads a keypair or touches an RPC: which
/// verifier-key files are allowed on which path. Kept in the library so it is unit-tested directly.
pub fn check_register_chain_flags(
    reserved: bool,
    layout1_vkey_json: Option<&str>,
    zisk_vkey_json: Option<&str>,
) -> Result<(), RegisterChainRefusal> {
    if !reserved {
        return if layout1_vkey_json.is_some() || zisk_vkey_json.is_some() {
            Err(RegisterChainRefusal::VkeysNotAllowedOnPermissionless)
        } else {
            Ok(())
        };
    }
    if layout1_vkey_json.is_none() {
        return Err(RegisterChainRefusal::MissingLayout1Vkey);
    }
    if zisk_vkey_json.is_none() {
        return Err(RegisterChainRefusal::MissingZiskVkey);
    }
    Ok(())
}

/// `InitChain` for a RESERVED id (`chain_id < 2^32`, registration-and-revenue proposal): requires the id
/// to already have a live `AllowReservedId` marker, and both `authority` (the chain authority) and
/// `registry_authority` to sign. No deposit.
pub fn init_chain_reserved_ix(
    program_id: &Pubkey,
    payer: &Pubkey,
    authority: &Pubkey,
    registry_authority: &Pubkey,
    chain_id: u64,
    fields: InitChainFields,
) -> solana_program::instruction::Instruction {
    let (root, _) = root_pda(program_id, chain_id);
    let (registry, _) = registry_pda(program_id, chain_id);
    let (chain_config, _) = chain_config_pda(program_id, chain_id);
    let (global_config, _) = global_config_pda(program_id);
    let (allow, _) = reserved_allow_pda(program_id, chain_id);
    ix(
        program_id,
        vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(*authority, true),
            AccountMeta::new_readonly(*registry_authority, true),
            AccountMeta::new(root, false),
            AccountMeta::new(registry, false),
            AccountMeta::new(chain_config, false),
            AccountMeta::new_readonly(global_config, false),
            AccountMeta::new_readonly(allow, false),
            AccountMeta::new_readonly(system_program::id(), false),
        ],
        init_chain_args(chain_id, 0, *authority, fields),
    )
}

/// `InitChain` for a PERMISSIONLESS id (`chain_id >= 2^32`): `chain_id` must be exactly
/// [`derive_permissionless_chain_id`] for the claimed `nonce` — the caller computes it to know which
/// accounts to derive, the program independently recomputes and checks it. `nonce` need not be exactly the
/// authority's current stored nonce — it must only be `>= ` it; read the `perm_nonce_pda` account for the
/// current value (`0` if it does not exist yet), and pass a later nonce to skip one an attacker
/// pre-claimed. Locks the configured deposit from `payer` into the new `chain_config` account.
/// `fields.registry_entries` must be empty: the program refuses a permissionless chain that brings its own
/// verifier keys (`RegistryEntriesNotAllowed`); the registry authority adds them afterwards with
/// [`set_registry_entry_ix`]. The registry-authority account slot is unchecked on this path — any account
/// (`payer` again is fine) satisfies the fixed account-list shape.
pub fn init_chain_permissionless_ix(
    program_id: &Pubkey,
    payer: &Pubkey,
    authority: &Pubkey,
    chain_id: u64,
    nonce: u64,
    fields: InitChainFields,
) -> solana_program::instruction::Instruction {
    let (root, _) = root_pda(program_id, chain_id);
    let (registry, _) = registry_pda(program_id, chain_id);
    let (chain_config, _) = chain_config_pda(program_id, chain_id);
    let (global_config, _) = global_config_pda(program_id);
    let (nonce_pda, _) = perm_nonce_pda(program_id, authority);
    ix(
        program_id,
        vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(*authority, true),
            AccountMeta::new_readonly(*payer, false),
            AccountMeta::new(root, false),
            AccountMeta::new(registry, false),
            AccountMeta::new(chain_config, false),
            AccountMeta::new_readonly(global_config, false),
            AccountMeta::new(nonce_pda, false),
            AccountMeta::new_readonly(system_program::id(), false),
        ],
        init_chain_args(chain_id, nonce, *authority, fields),
    )
}

/// `authority` must be the program's real upgrade authority — it signs, and this builds in the
/// `ProgramData` account read the program checks it against. `fields.treasury` must be an already
/// rent-exempt account — it is passed again here as an account, not just the pubkey embedded in `fields`.
pub fn init_global_config_ix(
    program_id: &Pubkey,
    payer: &Pubkey,
    authority: &Pubkey,
    fields: GlobalConfigFields,
) -> solana_program::instruction::Instruction {
    let (global_config, _) = global_config_pda(program_id);
    let program_data = program_data_pda(program_id);
    ix(
        program_id,
        vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(*authority, true),
            AccountMeta::new(global_config, false),
            AccountMeta::new_readonly(program_data, false),
            AccountMeta::new_readonly(fields.treasury, false),
            AccountMeta::new_readonly(system_program::id(), false),
        ],
        SettleIx::InitGlobalConfig(InitGlobalConfigArgs {
            registry_authority: fields.registry_authority,
            treasury: fields.treasury,
            permissionless_init_enabled: fields.permissionless_init_enabled,
            reclaim_window_slots: fields.reclaim_window_slots,
            deposit_lamports: fields.deposit_lamports,
            default_fee_base_lamports: fields.default_fee_base_lamports,
            default_fee_bps: fields.default_fee_bps,
        }),
    )
}

/// Registry-authority-only: sets every field of `global_config` together (including
/// `permissionless_init_enabled`, forced `false` at `InitGlobalConfig`).
pub fn set_global_config_ix(
    program_id: &Pubkey,
    registry_authority: &Pubkey,
    update: GlobalConfigUpdate,
) -> solana_program::instruction::Instruction {
    let (global_config, _) = global_config_pda(program_id);
    ix(
        program_id,
        vec![
            AccountMeta::new_readonly(*registry_authority, true),
            AccountMeta::new(global_config, false),
        ],
        SettleIx::SetGlobalConfig(update),
    )
}

/// Step one of the two-step rotation — the CURRENT registry authority proposes `new` (the program rejects
/// `Pubkey::default()`). Replaces the disabled single-step `SetRegistryAuthority`.
pub fn propose_registry_authority_ix(
    program_id: &Pubkey,
    registry_authority: &Pubkey,
    new: Pubkey,
) -> solana_program::instruction::Instruction {
    let (global_config, _) = global_config_pda(program_id);
    ix(
        program_id,
        vec![
            AccountMeta::new_readonly(*registry_authority, true),
            AccountMeta::new(global_config, false),
        ],
        SettleIx::ProposeRegistryAuthority { new },
    )
}

/// Step two: signed by the PENDING key (the argument to the last successful
/// [`propose_registry_authority_ix`]), not the outgoing authority.
pub fn accept_registry_authority_ix(
    program_id: &Pubkey,
    pending_authority: &Pubkey,
) -> solana_program::instruction::Instruction {
    let (global_config, _) = global_config_pda(program_id);
    ix(
        program_id,
        vec![
            AccountMeta::new_readonly(*pending_authority, true),
            AccountMeta::new(global_config, false),
        ],
        SettleIx::AcceptRegistryAuthority,
    )
}

pub fn allow_reserved_id_ix(
    program_id: &Pubkey,
    payer: &Pubkey,
    registry_authority: &Pubkey,
    chain_id: u64,
) -> solana_program::instruction::Instruction {
    let (global_config, _) = global_config_pda(program_id);
    let (allow, _) = reserved_allow_pda(program_id, chain_id);
    ix(
        program_id,
        vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(*registry_authority, true),
            AccountMeta::new_readonly(global_config, false),
            AccountMeta::new(allow, false),
            AccountMeta::new_readonly(system_program::id(), false),
        ],
        SettleIx::AllowReservedId { chain_id },
    )
}

pub fn revoke_reserved_id_ix(
    program_id: &Pubkey,
    registry_authority: &Pubkey,
    chain_id: u64,
) -> solana_program::instruction::Instruction {
    let (global_config, _) = global_config_pda(program_id);
    let (allow, _) = reserved_allow_pda(program_id, chain_id);
    ix(
        program_id,
        vec![
            AccountMeta::new(*registry_authority, true),
            AccountMeta::new_readonly(global_config, false),
            AccountMeta::new(allow, false),
        ],
        SettleIx::RevokeReservedId { chain_id },
    )
}

pub fn set_fee_ix(
    program_id: &Pubkey,
    registry_authority: &Pubkey,
    chain_id: u64,
    base: u64,
    bps: u32,
) -> solana_program::instruction::Instruction {
    let (global_config, _) = global_config_pda(program_id);
    let (chain_config, _) = chain_config_pda(program_id, chain_id);
    ix(
        program_id,
        vec![
            AccountMeta::new_readonly(*registry_authority, true),
            AccountMeta::new_readonly(global_config, false),
            AccountMeta::new(chain_config, false),
        ],
        SettleIx::SetFee {
            chain_id,
            base,
            bps,
        },
    )
}

/// `treasury` must already be an existing, rent-exempt account — passed again here so the program can check
/// its live lamport balance, not just accept the bare pubkey.
pub fn set_treasury_ix(
    program_id: &Pubkey,
    registry_authority: &Pubkey,
    treasury: Pubkey,
) -> solana_program::instruction::Instruction {
    let (global_config, _) = global_config_pda(program_id);
    ix(
        program_id,
        vec![
            AccountMeta::new_readonly(*registry_authority, true),
            AccountMeta::new(global_config, false),
            AccountMeta::new_readonly(treasury, false),
        ],
        SettleIx::SetTreasury { treasury },
    )
}

pub fn migrate_chain_ix(
    program_id: &Pubkey,
    registry_authority: &Pubkey,
    payer: &Pubkey,
    chain_id: u64,
    max_drift_secs: u64,
) -> solana_program::instruction::Instruction {
    let (global_config, _) = global_config_pda(program_id);
    let (root, _) = root_pda(program_id, chain_id);
    let (chain_config, _) = chain_config_pda(program_id, chain_id);
    ix(
        program_id,
        vec![
            AccountMeta::new_readonly(*registry_authority, true),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(global_config, false),
            AccountMeta::new_readonly(root, false),
            AccountMeta::new(chain_config, false),
            AccountMeta::new_readonly(system_program::id(), false),
        ],
        SettleIx::MigrateChainV2 {
            chain_id,
            max_drift_secs,
        },
    )
}

/// accounts: [registry_authority (signer), global_config (read-only), chain_config (writable)]. Sets a
/// chain's drift bound after it is already on `chain_config` v2 — same shape as `set_fee_ix`.
pub fn set_drift_bound_ix(
    program_id: &Pubkey,
    registry_authority: &Pubkey,
    chain_id: u64,
    max_drift_secs: u64,
) -> solana_program::instruction::Instruction {
    let (global_config, _) = global_config_pda(program_id);
    let (chain_config, _) = chain_config_pda(program_id, chain_id);
    ix(
        program_id,
        vec![
            AccountMeta::new_readonly(*registry_authority, true),
            AccountMeta::new_readonly(global_config, false),
            AccountMeta::new(chain_config, false),
        ],
        SettleIx::SetDriftBound {
            chain_id,
            max_drift_secs,
        },
    )
}

/// accounts: [registry_authority (signer), payer (signer, writable — funds the one-time v1->v2 realloc),
/// global_config (read-only), registry (writable), system_program]. Registers `entry` for `chain_id`,
/// usable by `PostRootProved` only once `Clock::slot >= activation_slot` —
/// `programs/zk-settlement`'s README has the full rotation procedure.
pub fn set_registry_entry_ix(
    program_id: &Pubkey,
    registry_authority: &Pubkey,
    payer: &Pubkey,
    chain_id: u64,
    entry: RegistryEntry,
    activation_slot: u64,
) -> solana_program::instruction::Instruction {
    let (global_config, _) = global_config_pda(program_id);
    let (registry, _) = registry_pda(program_id, chain_id);
    ix(
        program_id,
        vec![
            AccountMeta::new_readonly(*registry_authority, true),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(global_config, false),
            AccountMeta::new(registry, false),
            AccountMeta::new_readonly(system_program::id(), false),
        ],
        SettleIx::SetRegistryEntry {
            chain_id,
            entry: RegistryEntryArg {
                curve: entry.curve,
                scheme: entry.scheme,
                vkey_hash: entry.vkey_hash,
                layout_id: entry.layout_id,
            },
            activation_slot,
        },
    )
}

pub fn refund_deposit_ix(
    program_id: &Pubkey,
    chain_id: u64,
    chain_authority: &Pubkey,
) -> solana_program::instruction::Instruction {
    let (chain_config, _) = chain_config_pda(program_id, chain_id);
    let (root, _) = root_pda(program_id, chain_id);
    ix(
        program_id,
        vec![
            AccountMeta::new(chain_config, false),
            AccountMeta::new_readonly(root, false),
            AccountMeta::new(*chain_authority, false),
        ],
        SettleIx::RefundDeposit { chain_id },
    )
}

/// accounts: [chain_authority (signer, must equal `root.authority`), payer (signer, writable — funds
/// `exit_config`'s create-or-adopt), root (read-only), exit_config (writable, new-or-existing),
/// system_program]. `programs/zk-settlement`'s `governance::propose_exit_config` has the full contract.
#[allow(clippy::too_many_arguments)]
pub fn propose_exit_config_ix(
    program_id: &Pubkey,
    chain_authority: &Pubkey,
    payer: &Pubkey,
    chain_id: u64,
    exit_portal: Option<[u8; 20]>,
    bridge_program: Option<Pubkey>,
    exit_cap_per_window: Option<u64>,
    poster_bond: Option<u64>,
    activation_slot: u64,
) -> solana_program::instruction::Instruction {
    let (root, _) = root_pda(program_id, chain_id);
    let (exit_config, _) = exit_config_pda(program_id, chain_id);
    ix(
        program_id,
        vec![
            AccountMeta::new_readonly(*chain_authority, true),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(root, false),
            AccountMeta::new(exit_config, false),
            AccountMeta::new_readonly(system_program::id(), false),
        ],
        SettleIx::ProposeExitConfig {
            chain_id,
            exit_portal,
            bridge_program,
            exit_cap_per_window,
            poster_bond,
            activation_slot,
        },
    )
}

/// accounts: [root (writable), exit_config (writable)] — permissionless. `programs/zk-settlement`'s
/// `governance::activate_exit_config` has the full contract.
pub fn activate_exit_config_ix(
    program_id: &Pubkey,
    chain_id: u64,
) -> solana_program::instruction::Instruction {
    let (root, _) = root_pda(program_id, chain_id);
    let (exit_config, _) = exit_config_pda(program_id, chain_id);
    ix(
        program_id,
        vec![
            AccountMeta::new(root, false),
            AccountMeta::new(exit_config, false),
        ],
        SettleIx::ActivateExitConfig { chain_id },
    )
}

/// accounts: `[payer (signer, writable), root (read-only), pending(batch) (read-only), exit_config
/// (read-only), exit_record (writable, NEW), exit_window (writable, new-or-existing), exit_nullifier_page
/// (writable, new-or-existing), system_program]` — permissionless. `page` is
/// `rome_zk_layouts::exit::nullifier_page(message.nonce)` — the caller derives it the same way the program
/// does, to know which nullifier-page PDA to pass; `window_index` similarly must already be
/// `current_slot / challenge_window_slots` (`programs/zk-settlement`'s `exit::prove_exit` has the full
/// contract).
#[allow(clippy::too_many_arguments)]
pub fn prove_exit_ix(
    program_id: &Pubkey,
    payer: &Pubkey,
    chain_id: u64,
    batch: u64,
    message: ExitMessageArg,
    proof: rome_zk_mpt::ExitProof,
    window_index: u64,
    page: u64,
) -> solana_program::instruction::Instruction {
    let (root, _) = root_pda(program_id, chain_id);
    let (pending, _) = pending_pda(program_id, chain_id, batch);
    let (exit_config, _) = exit_config_pda(program_id, chain_id);
    let message_hash = message_fields(&message).message_hash();
    let (exit_record, _) = exit_record_pda(program_id, chain_id, message_hash);
    let (exit_window, _) = exit_window_pda(program_id, chain_id, window_index);
    let (exit_nullifier_page, _) = exit_nullifier_pda(program_id, chain_id, page);
    ix(
        program_id,
        vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(root, false),
            AccountMeta::new_readonly(pending, false),
            AccountMeta::new_readonly(exit_config, false),
            AccountMeta::new(exit_record, false),
            AccountMeta::new(exit_window, false),
            AccountMeta::new(exit_nullifier_page, false),
            AccountMeta::new_readonly(system_program::id(), false),
        ],
        SettleIx::ProveExit(ProveExitArgs {
            chain_id,
            batch,
            message,
            proof,
        }),
    )
}

/// accounts: `[bridge_signer (signer — the `exit_consumer` PDA, CPI-signed by whichever program
/// `exit_config.bridge_program` names; never a keypair — the caller only ever reaches this builder from
/// inside that program's own CPI, so this crate never signs it), exit_config (read-only), exit_record
/// (writable), payer_refund (writable, must equal `record.payer` — the caller reads the record's own
/// `payer` field, e.g. via [`decode_exit_record_account`], and passes that account, never a substitute)]` —
/// the ix the `zk-bridge` program CPIs. `programs/zk-settlement`'s `exit::consume_exit` has the full
/// contract.
pub fn consume_exit_ix(
    program_id: &Pubkey,
    chain_id: u64,
    message_hash: [u8; 32],
    bridge_signer: &Pubkey,
    exit_config: &Pubkey,
    exit_record: &Pubkey,
    payer_refund: &Pubkey,
) -> solana_program::instruction::Instruction {
    ix(
        program_id,
        vec![
            AccountMeta::new_readonly(*bridge_signer, true),
            AccountMeta::new_readonly(*exit_config, false),
            AccountMeta::new(*exit_record, false),
            AccountMeta::new(*payer_refund, false),
        ],
        SettleIx::ConsumeExit(ConsumeExitArgs {
            chain_id,
            message_hash,
        }),
    )
}

/// `ExitMessageArg` -> `rome_zk_layouts::exit::ExitMessage` field-for-field, matching `exit::ExitMessageArg`'s
/// own `From` impl (this client stays free to construct the layouts type directly, since it already depends
/// on `rome_zk_layouts`).
fn message_fields(m: &ExitMessageArg) -> rome_zk_layouts::exit::ExitMessage {
    rome_zk_layouts::exit::ExitMessage {
        nonce: m.nonce,
        l2_sender: m.l2_sender,
        sol_recipient: m.sol_recipient,
        asset: m.asset,
        amount: m.amount,
    }
}

/// Decoded view of the `exit_record` account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitRecordAccount {
    pub chain_id: u64,
    pub batch: u64,
    pub message_hash: [u8; 32],
    pub sol_recipient: [u8; 32],
    pub amount: u128,
    pub window_index: u64,
    pub proved_slot: u64,
    pub status: u8,
    pub payer: Pubkey,
    pub asset: [u8; 20],
}

pub fn decode_exit_record_account(d: &[u8]) -> Result<ExitRecordAccount, DecodeError> {
    let f = rome_zk_layouts::exit::exit_record::read(d)?;
    Ok(ExitRecordAccount {
        chain_id: f.chain_id,
        batch: f.batch,
        message_hash: f.message_hash,
        sol_recipient: f.sol_recipient,
        amount: f.amount,
        window_index: f.window_index,
        proved_slot: f.proved_slot,
        status: f.status,
        payer: Pubkey::new_from_array(f.payer),
        asset: f.asset,
    })
}

/// Decoded view of the `exit_window` account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitWindowAccount {
    pub chain_id: u64,
    pub window_index: u64,
    pub spent_cap_units: u64,
    pub exits: u32,
}

pub fn decode_exit_window_account(d: &[u8]) -> Result<ExitWindowAccount, DecodeError> {
    let f = rome_zk_layouts::exit::exit_window::read(d)?;
    Ok(ExitWindowAccount {
        chain_id: f.chain_id,
        window_index: f.window_index,
        spent_cap_units: f.spent_cap_units,
        exits: f.exits,
    })
}

/// The reserved-allow marker account: only actually live for a chain that is both RESERVED and still
/// allowlisted — for every other chain the account simply doesn't exist, and the program treats that as "no
/// marker to block reclaim on".
pub fn reclaim_chain_ix(
    program_id: &Pubkey,
    chain_id: u64,
    treasury: &Pubkey,
) -> solana_program::instruction::Instruction {
    let (global_config, _) = global_config_pda(program_id);
    let (chain_config, _) = chain_config_pda(program_id, chain_id);
    let (root, _) = root_pda(program_id, chain_id);
    let (registry, _) = registry_pda(program_id, chain_id);
    let (allow, _) = reserved_allow_pda(program_id, chain_id);
    ix(
        program_id,
        vec![
            AccountMeta::new_readonly(global_config, false),
            AccountMeta::new(chain_config, false),
            AccountMeta::new(root, false),
            AccountMeta::new(registry, false),
            AccountMeta::new(*treasury, false),
            AccountMeta::new_readonly(allow, false),
        ],
        SettleIx::ReclaimChain { chain_id },
    )
}

/// Shared account list for `PostRoot`/`PostRootProved` (registration-and-revenue proposal): [authority
/// (signer, **writable** — debited the protocol fee), root pda (writable), pending pda (writable, new),
/// predecessor pending pda (read-only), registry pda (read-only), inbox batch pda (read-only), chain_config
/// pda (writable), global_config pda (read-only), treasury (writable), system_program].
fn post_root_accounts(
    program_id: &Pubkey,
    authority: &Pubkey,
    inbox_program: &Pubkey,
    treasury: &Pubkey,
    args: &PostRootFields,
) -> Vec<AccountMeta> {
    let (root, _) = root_pda(program_id, args.chain_id);
    let (pending, _) = pending_pda(program_id, args.chain_id, args.batch);
    let (predecessor, _) = pending_pda(program_id, args.chain_id, args.prev_batch);
    let (registry, _) = registry_pda(program_id, args.chain_id);
    let inbox_batch = inbox_batch_pda(inbox_program, program_id, args.chain_id, args.batch);
    let (chain_config, _) = chain_config_pda(program_id, args.chain_id);
    let (global_config, _) = global_config_pda(program_id);
    vec![
        AccountMeta::new(*authority, true),
        AccountMeta::new(root, false),
        AccountMeta::new(pending, false),
        AccountMeta::new_readonly(predecessor, false),
        AccountMeta::new_readonly(registry, false),
        AccountMeta::new_readonly(inbox_batch, false),
        AccountMeta::new(chain_config, false),
        AccountMeta::new_readonly(global_config, false),
        AccountMeta::new(*treasury, false),
        AccountMeta::new_readonly(system_program::id(), false),
    ]
}

/// Why [`post_root_ix`] refused to build an instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PostRootRefusal {
    /// The unproved `PostRoot` was asked for a permissionless chain (`chain_id >= 2^32`). Such a chain is
    /// proved-only: the program refuses this instruction for it (`UnprovedRootNotAllowed`), so building it
    /// would only spend a fee on a refusal. Use [`post_root_proved_ix`].
    #[error(
        "UnprovedRootNotAllowed: chain {0} is permissionless, and a permissionless chain is proved-only; \
         post a proved root (post_root_proved_ix)"
    )]
    UnprovedRootNotAllowed(u64),
}

/// The unproved, challenge-window `PostRoot`. Only Rome's reserved-range chains (`chain_id < 2^32`) may
/// use it; for a permissionless chain this refuses by name instead of building an instruction the
/// program would refuse.
pub fn post_root_ix(
    program_id: &Pubkey,
    authority: &Pubkey,
    inbox_program: &Pubkey,
    treasury: &Pubkey,
    args: PostRootFields,
) -> Result<solana_program::instruction::Instruction, PostRootRefusal> {
    if !rome_zk_layouts::chainid::is_reserved(args.chain_id) {
        return Err(PostRootRefusal::UnprovedRootNotAllowed(args.chain_id));
    }
    Ok(post_root_ix_unchecked(
        program_id,
        authority,
        inbox_program,
        treasury,
        args,
    ))
}

/// [`post_root_ix`] without the client-side check. Not for posting: it exists so a test can send the
/// program an unproved `PostRoot` for a permissionless chain and show that the program itself refuses it.
#[doc(hidden)]
pub fn post_root_ix_unchecked(
    program_id: &Pubkey,
    authority: &Pubkey,
    inbox_program: &Pubkey,
    treasury: &Pubkey,
    args: PostRootFields,
) -> solana_program::instruction::Instruction {
    let accounts = post_root_accounts(program_id, authority, inbox_program, treasury, &args);
    ix(program_id, accounts, SettleIx::PostRoot(args))
}

/// Byte length of a layout-1 `PostRootProved` proof ABI: the 768-byte PLONK proof, the 32-byte
/// `programVK`, the 32-byte `rootCVadcopFinal`, then the prover's 512-byte packaging of the guest's
/// 64 public-output registers (`public_values::unpack_zisk_outputs` reads it back).
pub const LAYOUT1_PROOF_ABI_LEN: usize = 768 + 32 + 32 + 512;

/// A part handed to [`layout1_proof_abi`] had the wrong length — refused by name, never padded or
/// truncated, because the program reads every part at a fixed offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProofAbiError {
    #[error("proof bytes must be 768 bytes, got {0}")]
    ProofBytesLen(usize),
    #[error("public values must be the 512-byte ZisK packaging, got {0}")]
    PublicValuesLen(usize),
}

/// Assemble the layout-1 `proof_abi` exactly as `zk-settlement` reads it (`settle.rs`: `[768..800]`
/// programVK, `[800..832]` rootCVadcopFinal, `[832..]` publics). The four parts are what a decoded
/// ZisK PLONK proof file yields; the prover/poster and the gate's one-off sender both go through here so
/// the offsets live in one place.
pub fn layout1_proof_abi(
    proof_bytes: &[u8],
    program_vk: &[u8; 32],
    root_c_vadcop_final: &[u8; 32],
    public_values_512: &[u8],
) -> Result<Vec<u8>, ProofAbiError> {
    if proof_bytes.len() != 768 {
        return Err(ProofAbiError::ProofBytesLen(proof_bytes.len()));
    }
    if public_values_512.len() != 512 {
        return Err(ProofAbiError::PublicValuesLen(public_values_512.len()));
    }
    let mut d = Vec::with_capacity(LAYOUT1_PROOF_ABI_LEN);
    d.extend_from_slice(proof_bytes);
    d.extend_from_slice(program_vk);
    d.extend_from_slice(root_c_vadcop_final);
    d.extend_from_slice(public_values_512);
    debug_assert_eq!(d.len(), LAYOUT1_PROOF_ABI_LEN);
    Ok(d)
}

pub fn post_root_proved_ix(
    program_id: &Pubkey,
    authority: &Pubkey,
    inbox_program: &Pubkey,
    treasury: &Pubkey,
    args: PostRootFields,
    proof_abi: Vec<u8>,
    header_rlp: Vec<u8>,
) -> solana_program::instruction::Instruction {
    let accounts = post_root_accounts(program_id, authority, inbox_program, treasury, &args);
    ix(
        program_id,
        accounts,
        SettleIx::PostRootProved {
            args,
            proof_abi,
            header_rlp,
        },
    )
}

/// `walk_successors`: batch numbers immediately following `batch` (`batch+1`, `batch+2`, …, up to 8) that
/// the caller already knows are `Final` — passed as read-only trailing accounts so the program can advance
/// `head_final_batch` past them in the same call. Pass `&[]` when there is nothing to walk (the common
/// case).
pub fn finalize_batch_ix(
    program_id: &Pubkey,
    chain_id: u64,
    batch: u64,
    walk_successors: &[u64],
) -> solana_program::instruction::Instruction {
    let (root, _) = root_pda(program_id, chain_id);
    let (pending, _) = pending_pda(program_id, chain_id, batch);
    let mut accounts = vec![
        AccountMeta::new(root, false),
        AccountMeta::new(pending, false),
    ];
    for &b in walk_successors {
        let (p, _) = pending_pda(program_id, chain_id, b);
        accounts.push(AccountMeta::new_readonly(p, false));
    }
    ix(
        program_id,
        accounts,
        SettleIx::FinalizeBatch { chain_id, batch },
    )
}

pub fn close_pending_ix(
    program_id: &Pubkey,
    authority: &Pubkey,
    chain_id: u64,
    batch: u64,
) -> solana_program::instruction::Instruction {
    let (root, _) = root_pda(program_id, chain_id);
    let (pending, _) = pending_pda(program_id, chain_id, batch);
    ix(
        program_id,
        vec![
            AccountMeta::new(*authority, true),
            AccountMeta::new(root, false),
            AccountMeta::new(pending, false),
        ],
        SettleIx::ClosePending { chain_id, batch },
    )
}

pub fn root_view_ix(
    program_id: &Pubkey,
    chain_id: u64,
    batch: u64,
) -> solana_program::instruction::Instruction {
    let (root, _) = root_pda(program_id, chain_id);
    let (pending, _) = pending_pda(program_id, chain_id, batch);
    ix(
        program_id,
        vec![
            AccountMeta::new_readonly(root, false),
            AccountMeta::new_readonly(pending, false),
        ],
        SettleIx::RootView { chain_id, batch },
    )
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("account too short: {0} bytes")]
    TooShort(usize),
    #[error("bad magic")]
    BadMagic,
    #[error("bad version")]
    BadVersion,
    /// `public_values::unpack_zisk_outputs` refused a word (over `u32::MAX`, or a non-zero tail word).
    #[error("bad ZisK output packing at word {0}")]
    BadZiskPacking(usize),
}
impl From<rome_zk_layouts::LayoutError> for DecodeError {
    fn from(e: rome_zk_layouts::LayoutError) -> Self {
        match e {
            rome_zk_layouts::LayoutError::TooShort { got, .. } => DecodeError::TooShort(got),
            rome_zk_layouts::LayoutError::BadMagic => DecodeError::BadMagic,
            rome_zk_layouts::LayoutError::BadVersion => DecodeError::BadVersion,
            rome_zk_layouts::LayoutError::BadZiskWord { index }
            | rome_zk_layouts::LayoutError::BadZiskTail { index } => {
                DecodeError::BadZiskPacking(index)
            }
        }
    }
}

/// Decoded view of the root account, with pubkey fields as `solana_program::Pubkey`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootAccount {
    pub chain_id: u64,
    pub number: u64,
    pub parent_hash: [u8; 32],
    pub state_root: [u8; 32],
    pub block_hash: [u8; 32],
    pub updates: u32,
    pub profile: u8,
    pub challenge_window_slots: u32,
    pub prove_window_slots: u32,
    pub proving_policy: u8,
    pub poster_bond: u64,
    pub exit_cap_per_window: u64,
    pub authority: Pubkey,
    pub head_pending_batch: u64,
    pub head_final_batch: u64,
    pub pending_count: u32,
    pub max_pending: u32,
}

pub fn decode_root_account(d: &[u8]) -> Result<RootAccount, DecodeError> {
    let f = rome_zk_layouts::root::read(d)?;
    Ok(RootAccount {
        chain_id: f.chain_id,
        number: f.number,
        parent_hash: f.parent_hash,
        state_root: f.state_root,
        block_hash: f.block_hash,
        updates: f.updates,
        profile: f.profile,
        challenge_window_slots: f.challenge_window_slots,
        prove_window_slots: f.prove_window_slots,
        proving_policy: f.proving_policy,
        poster_bond: f.poster_bond,
        exit_cap_per_window: f.exit_cap_per_window,
        authority: Pubkey::new_from_array(f.authority),
        head_pending_batch: f.head_pending_batch,
        head_final_batch: f.head_final_batch,
        pending_count: f.pending_count,
        max_pending: f.max_pending,
    })
}

/// Decoded view of a pending-batch account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAccount {
    pub batch: u64,
    pub prev_batch: u64,
    pub pre_state_root: [u8; 32],
    pub first_block: u64,
    pub last_block: u64,
    pub state_root: [u8; 32],
    pub block_roots_merkle: [u8; 32],
    pub inbox_commitment: [u8; 32],
    pub forced_outcome_commitment: [u8; 32],
    pub posted_slot: u64,
    pub status: u8,
    pub disputes_open: u16,
    pub deadline_slot: u64,
    pub parent_hash: [u8; 32],
    pub last_block_hash: [u8; 32],
}

pub fn decode_pending_account(d: &[u8]) -> Result<PendingAccount, DecodeError> {
    let f = rome_zk_layouts::pending::read(d)?;
    Ok(PendingAccount {
        batch: f.batch,
        prev_batch: f.prev_batch,
        pre_state_root: f.pre_state_root,
        first_block: f.first_block,
        last_block: f.last_block,
        state_root: f.state_root,
        block_roots_merkle: f.block_roots_merkle,
        inbox_commitment: f.inbox_commitment,
        forced_outcome_commitment: f.forced_outcome_commitment,
        posted_slot: f.posted_slot,
        status: f.status,
        disputes_open: f.disputes_open,
        deadline_slot: f.deadline_slot,
        parent_hash: f.parent_hash,
        last_block_hash: f.last_block_hash,
    })
}

/// Decoded view of the `exit_config` account, with pubkey fields as `solana_program::Pubkey`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitConfigAccount {
    pub chain_id: u64,
    pub exit_portal: [u8; 20],
    pub bridge_program: Pubkey,
    pub pending_exit_portal: [u8; 20],
    pub pending_bridge_program: Pubkey,
    pub pending_exit_cap: u64,
    pub pending_poster_bond: u64,
    pub activation_slot: u64,
    pub pending_mask: u8,
}

pub fn decode_exit_config_account(d: &[u8]) -> Result<ExitConfigAccount, DecodeError> {
    let f = rome_zk_layouts::exit::exit_config::read(d)?;
    Ok(ExitConfigAccount {
        chain_id: f.chain_id,
        exit_portal: f.exit_portal,
        bridge_program: Pubkey::new_from_array(f.bridge_program),
        pending_exit_portal: f.pending_exit_portal,
        pending_bridge_program: Pubkey::new_from_array(f.pending_bridge_program),
        pending_exit_cap: f.pending_exit_cap,
        pending_poster_bond: f.pending_poster_bond,
        activation_slot: f.activation_slot,
        pending_mask: f.pending_mask,
    })
}

/// Decoded view of an `exit_nullifier` page account: the header (`chain_id`/`page`) plus the full `bits`
/// bitmap, exactly as `programs/zk-settlement::exit::prove_exit` itself reads it (`exit.rs:234-246`: owner
/// and length check, `read_header`, then a plain byte-slice copy of `OFF_BITS..LEN` — no separate "bits"
/// layout struct exists, so this decoder mirrors that on-chain read rather than wrapping a nonexistent
/// one). Callers pass `bits` to `rome_zk_layouts::exit::bit_is_set` themselves; this crate does not
/// interpret the bitmap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NullifierPageAccount {
    pub chain_id: u64,
    pub page: u64,
    pub bits: [u8; rome_zk_layouts::exit::exit_nullifier::BITS_LEN],
}

pub fn decode_exit_nullifier_account(d: &[u8]) -> Result<NullifierPageAccount, DecodeError> {
    let hdr = rome_zk_layouts::exit::exit_nullifier::read_header(d)?;
    let mut bits = [0u8; rome_zk_layouts::exit::exit_nullifier::BITS_LEN];
    bits.copy_from_slice(
        &d[rome_zk_layouts::exit::exit_nullifier::OFF_BITS
            ..rome_zk_layouts::exit::exit_nullifier::LEN],
    );
    Ok(NullifierPageAccount {
        chain_id: hdr.chain_id,
        page: hdr.page,
        bits,
    })
}

/// One decoded registry entry, WITH its activation slot — `0` on a v1-length account (active since genesis;
/// every entry `InitChainV2` writes), the real stored value once `SetRegistryEntry` has grown the account
/// to v2. `retired` is `activation_slot == rome_zk_layouts::registry::RETIRED_SLOT` (`u64::MAX`) — the same
/// tombstone `registry::find` already skips for every real slot on-chain; derived here, not stored
/// separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistryEntryView {
    pub curve: u8,
    pub scheme: u8,
    pub vkey_hash: [u8; 32],
    pub layout_id: u8,
    pub activation_slot: u64,
    pub retired: bool,
}

/// Decoded view of the verifier registry account: the header plus every populated
/// (`0..count`) entry, each with its activation slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryAccount {
    pub chain_id: u64,
    pub inbox_program: Pubkey,
    pub count: u8,
    pub entries: Vec<RegistryEntryView>,
}

pub fn decode_registry_account(d: &[u8]) -> Result<RegistryAccount, DecodeError> {
    let hdr = rome_zk_layouts::registry::read_header(d)?;
    let mut entries = Vec::with_capacity(hdr.count as usize);
    for i in 0..(hdr.count as usize).min(rome_zk_layouts::registry::MAX_ENTRIES) {
        let (e, activation_slot) = rome_zk_layouts::registry::entry_at(d, i)?;
        entries.push(RegistryEntryView {
            curve: e.curve,
            scheme: e.scheme,
            vkey_hash: e.vkey_hash,
            layout_id: e.layout_id,
            activation_slot,
            retired: activation_slot == rome_zk_layouts::registry::RETIRED_SLOT,
        });
    }
    Ok(RegistryAccount {
        chain_id: hdr.chain_id,
        inbox_program: Pubkey::new_from_array(hdr.inbox_program),
        count: hdr.count,
        entries,
    })
}

/// Decoded view of the `chain_config` account (registration-and-revenue proposal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainConfigAccount {
    pub chain_id: u64,
    pub reserved: bool,
    pub deposit_lamports: u64,
    pub deposit_refunded: bool,
    pub registered_slot: u64,
    pub posted_batches: u32,
    pub fee_base_lamports: u64,
    pub fee_bps: u32,
    /// `None` for a v1 account (not yet brought forward by `MigrateChainV2`); `Some` for v2.
    pub max_drift_secs: Option<u64>,
}

pub fn decode_chain_config_account(d: &[u8]) -> Result<ChainConfigAccount, DecodeError> {
    let f = rome_zk_layouts::chain_config::read(d)?;
    Ok(ChainConfigAccount {
        chain_id: f.chain_id,
        reserved: f.reserved,
        deposit_lamports: f.deposit_lamports,
        deposit_refunded: f.deposit_refunded,
        registered_slot: f.registered_slot,
        posted_batches: f.posted_batches,
        fee_base_lamports: f.fee_base_lamports,
        fee_bps: f.fee_bps,
        max_drift_secs: f.max_drift_secs,
    })
}

/// Decoded view of the `global_config` account (registration-and-revenue proposal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlobalConfigAccount {
    pub registry_authority: Pubkey,
    pub treasury: Pubkey,
    pub permissionless_init_enabled: bool,
    pub reclaim_window_slots: u64,
    pub deposit_lamports: u64,
    pub default_fee_base_lamports: u64,
    pub default_fee_bps: u32,
    /// `Pubkey::default()` when no rotation is in flight.
    pub pending_registry_authority: Pubkey,
}

pub fn decode_global_config_account(d: &[u8]) -> Result<GlobalConfigAccount, DecodeError> {
    let f = rome_zk_layouts::global_config::read(d)?;
    Ok(GlobalConfigAccount {
        registry_authority: Pubkey::new_from_array(f.registry_authority),
        treasury: Pubkey::new_from_array(f.treasury),
        permissionless_init_enabled: f.permissionless_init_enabled,
        reclaim_window_slots: f.reclaim_window_slots,
        deposit_lamports: f.deposit_lamports,
        default_fee_base_lamports: f.default_fee_base_lamports,
        default_fee_bps: f.default_fee_bps,
        pending_registry_authority: Pubkey::new_from_array(f.pending_registry_authority),
    })
}

#[cfg(test)]
mod tests {

    /// The layout-1 `proof_abi` the program reads at fixed offsets (`settle.rs`: `[768..800]` programVK,
    /// `[800..832]` rootCVadcopFinal, `[832..]` the 512-byte publics) is assembled by ONE function so
    /// no caller re-derives the layout; wrong part lengths are refused by name, never padded.
    #[test]
    fn layout1_proof_abi_is_assembled_at_the_program_offsets() {
        let proof = vec![0xABu8; 768];
        let vk = [0x11u8; 32];
        let rc = [0x22u8; 32];
        let pv = vec![0x33u8; 512];
        let abi = layout1_proof_abi(&proof, &vk, &rc, &pv).unwrap();
        assert_eq!(abi.len(), LAYOUT1_PROOF_ABI_LEN);
        assert_eq!(&abi[..768], &proof[..]);
        assert_eq!(&abi[768..800], &vk);
        assert_eq!(&abi[800..832], &rc);
        assert_eq!(&abi[832..], &pv[..]);
        assert_eq!(
            layout1_proof_abi(&proof[..767], &vk, &rc, &pv),
            Err(ProofAbiError::ProofBytesLen(767))
        );
        assert_eq!(
            layout1_proof_abi(&proof, &vk, &rc, &pv[..256]),
            Err(ProofAbiError::PublicValuesLen(256))
        );
    }
    /// A shipped instruction body is immutable per discriminant. These are the exact 289 bytes of the
    /// `InitChain` (3) call recorded on-chain at slot 2554 (settlement program HXT199RA…) — they must
    /// decode by name forever, whatever `InitChain`'s successor carries.
    #[test]
    fn the_recorded_init_chain_body_decodes_by_name_forever() {
        let data: &[u8] =
            include_bytes!("../../../fixtures/settlement-program/txv1-dev-initchain-slot2554.bin");
        assert_eq!(data.len(), 289);
        assert_eq!(data[0], 3);
        match super::decode_instruction(data).expect("recorded InitChain(3) must decode") {
            zk_settlement::SettleIx::InitChain(args) => {
                assert_eq!(args.chain_id, 200_101);
                assert_eq!(args.registry_entries.len(), 2);
            }
            other => panic!("expected InitChain, got {other:?}"),
        }
    }

    use super::*;

    /// `decode_instruction` is the exact inverse of `root_view_ix` (and every other builder above) —
    /// round-trips through the same borsh bytes a real transaction carries.
    #[test]
    fn decode_instruction_round_trips_root_view() {
        let ix = root_view_ix(&Pubkey::new_unique(), 200_101, 4003);
        let decoded = decode_instruction(&ix.data).unwrap();
        assert!(matches!(
            decoded,
            SettleIx::RootView {
                chain_id: 200_101,
                batch: 4003,
            }
        ));
    }

    #[test]
    fn decode_instruction_rejects_garbage() {
        assert!(decode_instruction(&[0xffu8; 3]).is_err());
    }

    #[test]
    fn pda_derivations_are_deterministic_and_distinct() {
        let program_id = Pubkey::new_unique();
        let (root_a, _) = root_pda(&program_id, 7);
        let (root_b, _) = root_pda(&program_id, 7);
        assert_eq!(root_a, root_b);
        let (registry, _) = registry_pda(&program_id, 7);
        assert_ne!(root_a, registry);
        let (pending_1, _) = pending_pda(&program_id, 7, 1);
        let (pending_2, _) = pending_pda(&program_id, 7, 2);
        assert_ne!(pending_1, pending_2);
        assert_ne!(root_a, pending_1);
    }

    #[test]
    fn decode_root_account_round_trips_a_hand_built_account() {
        let mut d = vec![0u8; rome_zk_layouts::root::MIN_LEN];
        d[0..4].copy_from_slice(&rome_zk_layouts::root::MAGIC.to_le_bytes());
        d[4..12].copy_from_slice(&11u64.to_le_bytes());
        let authority = [9u8; 32];
        d[rome_zk_layouts::root::OFF_AUTHORITY..rome_zk_layouts::root::OFF_AUTHORITY + 32]
            .copy_from_slice(&authority);
        let acct = decode_root_account(&d).unwrap();
        assert_eq!(acct.chain_id, 11);
        assert_eq!(acct.authority.to_bytes(), authority);
    }

    #[test]
    fn decode_pending_account_round_trips() {
        let mut d = vec![0u8; rome_zk_layouts::pending::PENDING_LEN];
        d[0..8].copy_from_slice(&5u64.to_le_bytes());
        d[rome_zk_layouts::pending::OFF_STATUS] = rome_zk_layouts::pending::STATUS_FINAL;
        let acct = decode_pending_account(&d).unwrap();
        assert_eq!(acct.batch, 5);
        assert_eq!(acct.status, rome_zk_layouts::pending::STATUS_FINAL);
    }

    /// Every PDA function this client exposes is a zero-logic delegate to `rome_zk_layouts`'s own
    /// derivation, and the settlement program's own seed functions delegate to the same place — these
    /// tests prove that wrapper delegation, not that the derivation itself is correct. A seed change in
    /// `rome_zk_layouts` moves the program, this client and these tests together, so they stay green
    /// under a seed typo that would strand every already-deployed account; the real check on the
    /// deployed addresses is `rome-zk-layouts`'s own `tests/pda_pins.rs`.
    #[test]
    fn root_pda_matches_program_and_layouts() {
        let program = Pubkey::new_unique();
        assert_eq!(
            root_pda(&program, 7),
            zk_settlement::chain::root_pda(&program, 7)
        );
        assert_eq!(
            root_pda(&program, 7),
            rome_zk_layouts::root::pda(&program, 7)
        );
    }

    #[test]
    fn registry_pda_matches_program_and_layouts() {
        let program = Pubkey::new_unique();
        assert_eq!(
            registry_pda(&program, 7),
            zk_settlement::chain::registry_pda(&program, 7)
        );
        assert_eq!(
            registry_pda(&program, 7),
            rome_zk_layouts::registry::pda(&program, 7)
        );
    }

    #[test]
    fn pending_pda_matches_program_and_layouts() {
        let program = Pubkey::new_unique();
        assert_eq!(
            pending_pda(&program, 7, 3),
            zk_settlement::settle::pending_pda(&program, 7, 3)
        );
        assert_eq!(
            pending_pda(&program, 7, 3),
            rome_zk_layouts::pending::pda(&program, 7, 3)
        );
    }

    #[test]
    fn global_config_pda_matches_program_and_layouts() {
        let program = Pubkey::new_unique();
        assert_eq!(
            global_config_pda(&program),
            zk_settlement::governance::global_config_pda(&program)
        );
        assert_eq!(
            global_config_pda(&program),
            rome_zk_layouts::global_config::pda(&program)
        );
    }

    #[test]
    fn chain_config_pda_matches_program_and_layouts() {
        let program = Pubkey::new_unique();
        assert_eq!(
            chain_config_pda(&program, 7),
            zk_settlement::governance::chain_config_pda(&program, 7)
        );
        assert_eq!(
            chain_config_pda(&program, 7),
            rome_zk_layouts::chain_config::pda(&program, 7)
        );
    }

    #[test]
    fn perm_nonce_pda_matches_program_and_layouts() {
        let program = Pubkey::new_unique();
        let authority = Pubkey::new_unique();
        assert_eq!(
            perm_nonce_pda(&program, &authority),
            zk_settlement::governance::perm_nonce_pda(&program, &authority)
        );
        assert_eq!(
            perm_nonce_pda(&program, &authority),
            rome_zk_layouts::perm_nonce::pda(&program, &authority)
        );
    }

    #[test]
    fn reserved_allow_pda_matches_program_and_layouts() {
        let program = Pubkey::new_unique();
        assert_eq!(
            reserved_allow_pda(&program, 7),
            zk_settlement::governance::reserved_allow_pda(&program, 7)
        );
        assert_eq!(
            reserved_allow_pda(&program, 7),
            rome_zk_layouts::reserved_allow::pda(&program, 7)
        );
    }

    #[test]
    fn inbox_batch_pda_matches_program_and_layouts() {
        let inbox_program = Pubkey::new_unique();
        let settlement_program = Pubkey::new_unique();
        assert_eq!(
            inbox_batch_pda(&inbox_program, &settlement_program, 7, 3),
            zk_settlement::settle::inbox_batch_pda(&inbox_program, &settlement_program, 7, 3)
        );
        assert_eq!(
            inbox_batch_pda(&inbox_program, &settlement_program, 7, 3),
            rome_zk_layouts::batch::pda(&inbox_program, &settlement_program, 7, 3).0
        );
    }

    // --- SetRegistryEntry builder + registry decode with activation slots ---

    #[test]
    fn set_registry_entry_ix_round_trips_through_decode_instruction() {
        let program = Pubkey::new_unique();
        let authority = Pubkey::new_unique();
        let payer = Pubkey::new_unique();
        let entry = RegistryEntry {
            curve: rome_zk_layouts::registry::CURVE_BN254,
            scheme: rome_zk_layouts::registry::SCHEME_PLONK,
            vkey_hash: [0x44u8; 32],
            layout_id: rome_zk_layouts::registry::LAYOUT_ZISK_V1,
        };
        let ix = set_registry_entry_ix(&program, &authority, &payer, 200_101, entry, 500_000);
        assert_eq!(ix.accounts.len(), 5);
        assert!(ix.accounts[0].is_signer && !ix.accounts[0].is_writable); // registry_authority
        assert!(ix.accounts[1].is_signer && ix.accounts[1].is_writable); // payer
        assert!(ix.accounts[3].is_writable); // registry
        match decode_instruction(&ix.data).unwrap() {
            SettleIx::SetRegistryEntry {
                chain_id,
                entry: decoded,
                activation_slot,
            } => {
                assert_eq!(chain_id, 200_101);
                assert_eq!(decoded.curve, entry.curve);
                assert_eq!(decoded.scheme, entry.scheme);
                assert_eq!(decoded.vkey_hash, entry.vkey_hash);
                assert_eq!(decoded.layout_id, entry.layout_id);
                assert_eq!(activation_slot, 500_000);
            }
            other => panic!("expected SetRegistryEntry, got {other:?}"),
        }
    }

    #[test]
    fn decode_registry_account_reports_activation_slots() {
        // v1-length account: one entry, no tail — activation_slot must read back 0.
        let mut d = vec![0u8; rome_zk_layouts::registry::REGISTRY_LEN];
        d[0..4].copy_from_slice(&rome_zk_layouts::registry::MAGIC.to_le_bytes());
        d[4..12].copy_from_slice(&200_101u64.to_le_bytes());
        d[44] = 1; // count
        let base = rome_zk_layouts::registry::OFF_ENTRIES;
        d[base] = rome_zk_layouts::registry::CURVE_BN254;
        d[base + 1] = rome_zk_layouts::registry::SCHEME_PLONK;
        d[base + 2..base + 34].copy_from_slice(&[0x22u8; 32]);
        d[base + 34] = rome_zk_layouts::registry::LAYOUT_HEADER_FALLBACK;

        let acct = decode_registry_account(&d).unwrap();
        assert_eq!(acct.chain_id, 200_101);
        assert_eq!(acct.count, 1);
        assert_eq!(acct.entries.len(), 1);
        assert_eq!(acct.entries[0].vkey_hash, [0x22u8; 32]);
        assert_eq!(acct.entries[0].activation_slot, 0);
        assert!(!acct.entries[0].retired);

        // grow to v2 and write a real activation slot for that same entry — decode must report it.
        d.resize(rome_zk_layouts::registry::REGISTRY_LEN_V2, 0);
        let aoff = rome_zk_layouts::registry::OFF_ACTIVATION;
        d[aoff..aoff + 8].copy_from_slice(&777_000u64.to_le_bytes());
        let acct2 = decode_registry_account(&d).unwrap();
        assert_eq!(acct2.entries[0].activation_slot, 777_000);
        assert!(!acct2.entries[0].retired);
    }

    /// `retired` is true exactly when `activation_slot == RETIRED_SLOT` (`u64::MAX`), never for any other
    /// value, however large.
    #[test]
    fn decode_registry_account_reports_retired_entries() {
        let mut d = vec![0u8; rome_zk_layouts::registry::REGISTRY_LEN_V2];
        d[0..4].copy_from_slice(&rome_zk_layouts::registry::MAGIC.to_le_bytes());
        d[4..12].copy_from_slice(&200_101u64.to_le_bytes());
        d[44] = 1; // count
        let base = rome_zk_layouts::registry::OFF_ENTRIES;
        d[base] = rome_zk_layouts::registry::CURVE_BN254;
        d[base + 1] = rome_zk_layouts::registry::SCHEME_PLONK;
        d[base + 2..base + 34].copy_from_slice(&[0x33u8; 32]);
        d[base + 34] = rome_zk_layouts::registry::LAYOUT_ZISK_V1;
        let aoff = rome_zk_layouts::registry::OFF_ACTIVATION;
        d[aoff..aoff + 8].copy_from_slice(&rome_zk_layouts::registry::RETIRED_SLOT.to_le_bytes());

        let acct = decode_registry_account(&d).unwrap();
        assert_eq!(acct.entries[0].activation_slot, u64::MAX);
        assert!(acct.entries[0].retired);

        // one below RETIRED_SLOT is a real, very-far-future activation slot, not retired.
        d[aoff..aoff + 8].copy_from_slice(&(u64::MAX - 1).to_le_bytes());
        let acct2 = decode_registry_account(&d).unwrap();
        assert!(!acct2.entries[0].retired);
    }

    /// `register_chain --permissionless` given a verifier key file refuses by name, so nobody believes a
    /// key was registered; `--reserved` still needs both.
    #[test]
    fn register_chain_permissionless_refuses_vkey_files_by_name() {
        assert_eq!(
            check_register_chain_flags(false, Some("l1.json"), None),
            Err(RegisterChainRefusal::VkeysNotAllowedOnPermissionless)
        );
        assert_eq!(
            check_register_chain_flags(false, None, Some("zisk.json")),
            Err(RegisterChainRefusal::VkeysNotAllowedOnPermissionless)
        );
        assert_eq!(
            check_register_chain_flags(false, Some("l1.json"), Some("zisk.json")),
            Err(RegisterChainRefusal::VkeysNotAllowedOnPermissionless)
        );
        assert!(check_register_chain_flags(false, None, None).is_ok());
        assert!(RegisterChainRefusal::VkeysNotAllowedOnPermissionless
            .to_string()
            .starts_with("VkeysNotAllowedOnPermissionless"));
        assert_eq!(
            check_register_chain_flags(true, None, Some("zisk.json")),
            Err(RegisterChainRefusal::MissingLayout1Vkey)
        );
        assert_eq!(
            check_register_chain_flags(true, Some("l1.json"), None),
            Err(RegisterChainRefusal::MissingZiskVkey)
        );
        assert!(check_register_chain_flags(true, Some("l1.json"), Some("zisk.json")).is_ok());
    }

    fn post_root_fields_for(chain_id: u64) -> PostRootFields {
        PostRootFields {
            chain_id,
            batch: 1,
            prev_batch: 0,
            pre_state_root: [0u8; 32],
            first_block: 1,
            last_block: 1,
            state_root: [1u8; 32],
            block_roots_merkle: [0u8; 32],
            inbox_commitment: [2u8; 32],
            forced_outcome_commitment: [3u8; 32],
            parent_hash: [0u8; 32],
            last_block_hash: [0u8; 32],
            gas_in_batch: 0,
        }
    }

    /// The unproved `PostRoot` builder refuses a permissionless chain by name, and still
    /// builds for Rome's reserved range (including the last reserved id, `2^32 - 1`).
    #[test]
    fn unproved_post_root_is_refused_for_a_permissionless_chain_and_built_for_a_reserved_one() {
        let key = Pubkey::new_unique();
        let build =
            |chain_id: u64| post_root_ix(&key, &key, &key, &key, post_root_fields_for(chain_id));
        let permissionless = rome_zk_layouts::chainid::PERMISSIONLESS_BASE;
        assert_eq!(
            build(permissionless).unwrap_err(),
            PostRootRefusal::UnprovedRootNotAllowed(permissionless)
        );
        assert!(build(permissionless)
            .unwrap_err()
            .to_string()
            .starts_with("UnprovedRootNotAllowed"));
        assert!(build(permissionless - 1).is_ok());
        assert!(build(200_101).is_ok());
    }

    /// `registry_entries_for_init` builds the genesis registry for a chain proving under the ZisK
    /// stateless-validator guest — the layout-1 primary entry FIRST (`registry_entries[0]`), so a proof
    /// under the layout-1 vkey finds it and not a fallback slot that happens to share `(curve, scheme)`
    /// (`rome_zk_layouts::registry::find`).
    #[test]
    fn registry_entries_for_init_puts_layout1_first() {
        let layout1 = [0x11u8; 32];
        let fallback = [0x22u8; 32];
        let entries = registry_entries_for_init(layout1, fallback);
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries[0].layout_id,
            rome_zk_layouts::registry::LAYOUT_ZISK_V1
        );
        assert_eq!(entries[0].vkey_hash, layout1);
        assert_eq!(entries[0].curve, rome_zk_layouts::registry::CURVE_BN254);
        assert_eq!(entries[0].scheme, rome_zk_layouts::registry::SCHEME_PLONK);
        assert_eq!(
            entries[1].layout_id,
            rome_zk_layouts::registry::LAYOUT_HEADER_FALLBACK
        );
        assert_eq!(entries[1].vkey_hash, fallback);
        assert_eq!(
            entries[2].layout_id,
            rome_zk_layouts::registry::LAYOUT_HEADER_FALLBACK
        );
        assert_eq!(entries[2].scheme, rome_zk_layouts::registry::SCHEME_GROTH16);
        assert_eq!(entries[2].vkey_hash, [0u8; 32]);
    }

    /// `decode_exit_nullifier_account` round-trips the exact bytes
    /// `programs/zk-settlement::exit::prove_exit` itself writes (`write_header` + a raw bits copy) — the
    /// exit-prover's pre-send read depends on this decoding the same header + bitmap the program wrote,
    /// never a re-derived layout.
    #[test]
    fn exit_nullifier_account_round_trips() {
        let mut d = [0u8; rome_zk_layouts::exit::exit_nullifier::LEN];
        rome_zk_layouts::exit::exit_nullifier::write_header(&mut d, 200_101, 7);
        // Set nonce 42's bit within page 7 (page = nonce >> 13, so nonce = 7*8192 + 42 lands on page 7).
        let nonce = 7 * rome_zk_layouts::exit::NULLIFIER_EXITS_PER_PAGE + 42;
        rome_zk_layouts::exit::set_bit(
            &mut d[rome_zk_layouts::exit::exit_nullifier::OFF_BITS..],
            7,
            nonce,
        )
        .unwrap();

        let decoded = decode_exit_nullifier_account(&d).unwrap();
        assert_eq!(decoded.chain_id, 200_101);
        assert_eq!(decoded.page, 7);
        assert!(rome_zk_layouts::exit::bit_is_set(&decoded.bits, 7, nonce).unwrap());
        assert!(!rome_zk_layouts::exit::bit_is_set(&decoded.bits, 7, nonce + 1).unwrap());
    }

    #[test]
    fn exit_nullifier_account_too_short_is_refused() {
        let d = [0u8; 10];
        assert!(matches!(
            decode_exit_nullifier_account(&d).unwrap_err(),
            DecodeError::TooShort(10)
        ));
    }

    /// The id the `chain-id` command prints must be the layouts crate's own derivation, not a second copy:
    /// its golden vector (authority = [0x42; 32], nonce 0 -> 7_066_855_974_458_915, see
    /// `rome_zk_layouts::chainid` tests) and a sweep against `permissionless_chain_id` called directly.
    #[test]
    fn chain_id_command_matches_the_layouts_crate_vectors() {
        let authority = Pubkey::new_from_array([0x42u8; 32]);
        assert_eq!(
            PermissionlessChainId::new(authority, 0).chain_id,
            7_066_855_974_458_915u64
        );
        for nonce in 0..20u64 {
            let got = PermissionlessChainId::new(authority, nonce);
            assert_eq!(
                got.chain_id,
                permissionless_chain_id(&rome_zk_merkle::keccak256, &[0x42u8; 32], nonce)
            );
            assert!(!is_reserved(got.chain_id));
        }
        assert_ne!(
            PermissionlessChainId::new(authority, 0).chain_id,
            PermissionlessChainId::new(authority, 1).chain_id
        );
    }

    #[test]
    fn chain_id_command_prints_authority_nonce_and_id_one_per_line() {
        let authority = Pubkey::new_from_array([0x42u8; 32]);
        let text = PermissionlessChainId::new(authority, 0).to_string();
        assert_eq!(
            text,
            format!("authority={authority}\nnonce=0\nchain_id=7066855974458915")
        );
    }
}
