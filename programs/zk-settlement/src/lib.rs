//! zk-settlement: the settlement program.
//!
//! Discriminants 0-2 were the original single-block measurement path (`legacy.rs`, removed:
//! `legacy::init` shared `InitChain`'s `["root", chain_id]` seeds and was permissionless, and Tiber's
//! programs are fresh deployments with no already-deployed 88-byte legacy root anywhere for it to stay
//! backward-compatible with, so it had no live consumer and only griefing potential — anyone could
//! pre-create a legacy root for any future chain id and brick that chain's `InitChain` forever). All
//! three now decode but are rejected — `InvalidInstructionData` — before touching any account; their
//! discriminant values stay exactly where they were (pinned, tested below) so `InitChain`/`PostRoot`/…
//! at discriminants 3+ never shift. Discriminant 19 (`SetRegistryAuthority`) is disabled the same way:
//! a single-step rotation that accepted any pubkey, `Pubkey::default()` included, with
//! `InitGlobalConfig` a once-only call and so no do-over. Replaced by the two-step
//! `ProposeRegistryAuthority`/`AcceptRegistryAuthority` at 20/21.
//! From discriminant 3 on (3 and 15 are retired, 19 is disabled, see the enum):
//! `InitChainV2` (24) creates the 202-byte root account plus a verifier registry account and
//! `chain_config` v2;
//! `PostRoot`/`PostRootProved` create per-batch pending PDAs with predecessor + inbox-accumulator binding
//! (`settle.rs`); `FinalizeBatch`/`ClosePending`/`RootView` are the window-elapsed finality, rent-recycle
//! and CPI-read paths; `RejectBatch` reserves the challenge-flow discriminant (not implemented).
//! `SetRegistryEntry` (25) is the verifier-key rotation instruction: it registers a registry entry with
//! an explicit activation delay (`governance.rs`).

pub mod chain;
pub mod errors;
pub mod exit;
pub mod governance;
pub mod header;
pub mod settle;

pub use chain::{registry_pda, root_pda, InitChainArgs, InitChainArgsV2, RegistryEntryArg};
pub use governance::InitGlobalConfigArgs;
pub use settle::{PostRootArgs, RootViewData};

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{
    account_info::AccountInfo, entrypoint::ProgramResult, program_error::ProgramError,
    pubkey::Pubkey,
};

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone)]
pub enum SettleIx {
    /// Disabled — decodes, then `process_instruction` rejects it with `InvalidInstructionData` before
    /// touching any account. Kept as a variant, not removed, purely to hold this discriminant's slot so
    /// `InitChain`/`PostRoot`/… below never shift.
    Init {
        chain_id: u64,
        number: u64,
        state_root: [u8; 32],
    },
    /// Disabled, same as `Init` above.
    UpdateRoot {
        chain_id: u64,
        proof: Vec<u8>,
        public_values: Vec<u8>,
    },
    /// Disabled, same as `Init` above.
    UpdateRootZisk {
        chain_id: u64,
        proof_abi: Vec<u8>,
        header_rlp: Vec<u8>,
    },
    // --- appended so the three disabled discriminants above never move ---
    /// RETIRED: the original chain registration, body kept exactly as recorded on chain so
    /// history decodes forever; refused by name (`RetiredInstruction`). Superseded by `InitChainV2` (24).
    InitChain(chain::InitChainArgs),
    /// accounts: [authority (signer), root pda (writable), pending pda (writable, new), predecessor
    /// pending pda (read-only; ignored when `head_pending_batch == 0`), registry pda (read-only), inbox
    /// batch pda (read-only), system_program].
    PostRoot(settle::PostRootArgs),
    /// Same accounts as `PostRoot`. `proof_abi` = the veritas ABI blob (768 + 32 + 32 + 512 B);
    /// `header_rlp` = the RLP header the header-fallback layout binds the proof to (registry
    /// `layout_id == LAYOUT_HEADER_FALLBACK`, single-block batches) — and must be EMPTY under layout 1
    /// (`LAYOUT_ZISK_V1`), where the guest's committed 208-byte public values are bound instead
    /// (`settle::post_root_proved`).
    PostRootProved {
        args: settle::PostRootArgs,
        proof_abi: Vec<u8>,
        header_rlp: Vec<u8>,
    },
    /// accounts: [root pda (writable), pending pda (writable), then up to 8 optional trailing
    /// already-Final successor pending PDAs (read-only, `[pending(batch+1), pending(batch+2), …]`) the
    /// caller wants walked past in the same call — permissionless.
    FinalizeBatch { chain_id: u64, batch: u64 },
    /// accounts: [authority (signer, writable), root pda (writable), pending pda (writable)].
    ClosePending { chain_id: u64, batch: u64 },
    /// accounts: [root pda (read-only), pending pda for `batch` (read-only)] — CPI-callable; fails unless
    /// `head_final_batch >= batch`. Returns `batch`'s own tuple, not necessarily the current head's.
    RootView { chain_id: u64, batch: u64 },
    /// Reserved: the challenge state machine is not implemented.
    RejectBatch { chain_id: u64, batch: u64 },

    // --- registration and revenue, appended so 0-9 above never move ---
    /// accounts: [payer (signer, writable), authority (signer, the program's real upgrade authority),
    /// global_config pda (writable, new), program_data (read-only, the program's own
    /// `bpf_loader_upgradeable` `ProgramData` account), treasury (read-only, must equal `args.treasury`
    /// and be rent-exempt), system_program]. Runs once per deployment.
    InitGlobalConfig(governance::InitGlobalConfigArgs),
    /// accounts: [payer (signer, writable), registry_authority (signer), global_config (read-only),
    /// allow pda (writable, new), system_program]. `chain_id` must be RESERVED.
    AllowReservedId { chain_id: u64 },
    /// accounts: [registry_authority (signer, writable), global_config (read-only), allow pda
    /// (writable)].
    RevokeReservedId { chain_id: u64 },
    /// accounts: [registry_authority (signer), global_config (read-only), chain_config (writable)].
    SetFee { chain_id: u64, base: u64, bps: u32 },
    /// accounts: [registry_authority (signer), global_config (writable), treasury (read-only, must equal
    /// `treasury` and be rent-exempt)].
    SetTreasury { treasury: Pubkey },
    /// RETIRED: the original bring-forward (`chain_id` only). Its body stays exactly as
    /// Tiber's recorded call carries it so the settlement watcher decodes that history forever; the
    /// program refuses it by name (`RetiredInstruction`). Superseded by `MigrateChainV2` (23).
    MigrateChain { chain_id: u64 },
    /// accounts: [chain_config (writable), root (read-only), chain_authority (writable)] —
    /// permissionless.
    RefundDeposit { chain_id: u64 },
    /// accounts: [global_config (read-only), chain_config (writable), root (writable), registry
    /// (writable), treasury (writable), reserved_allow marker (read-only)] — permissionless.
    ReclaimChain { chain_id: u64 },
    /// accounts: [registry_authority (signer), global_config (writable)]. The one instruction
    /// that can change `permissionless_init_enabled` (forced `false` at `InitGlobalConfig`) and the other
    /// registry-wide knobs after the fact.
    SetGlobalConfig(governance::SetGlobalConfigArgs),
    /// Disabled — decodes, then `process_instruction` rejects it with `InvalidInstructionData` before
    /// touching any account. Kept as a variant, not removed, purely to hold discriminant 19's slot.
    /// Replaced by `ProposeRegistryAuthority` / `AcceptRegistryAuthority` below.
    SetRegistryAuthority { new: Pubkey },
    /// accounts: [registry_authority (signer), global_config (writable)]. Step one of
    /// the two-step rotation — the CURRENT authority proposes `new` (rejects `Pubkey::default()`).
    ProposeRegistryAuthority { new: Pubkey },
    /// accounts: [pending_authority (signer), global_config (writable)]. Step two: the key named by a
    /// live `ProposeRegistryAuthority` accepts and becomes `registry_authority`.
    AcceptRegistryAuthority,

    // --- public values v2, appended so 0-21 above never move ---
    /// accounts: [registry_authority (signer), global_config (read-only), chain_config (writable)]. Sets
    /// a chain's drift bound after it is already on `chain_config` v2 (same authority-gated, whole-value-
    /// replace pattern as `SetFee`); `0` refused (`DriftBoundZero`).
    SetDriftBound { chain_id: u64, max_drift_secs: u64 },
    /// accounts: [registry_authority (signer), payer (signer, writable), global_config (read-only), root
    /// (read-only), chain_config (writable, new-or-existing), system_program]. Bring-forward for a chain
    /// that predates the registration scheme or `chain_config` v2 (Tiber): create v2 if absent, realloc
    /// v1→v2 in place, refuse if already v2 (`governance::migrate_chain`'s doc has the three
    /// shapes). Replaces `MigrateChain` (15) — a shipped body never changes at its discriminant.
    MigrateChainV2 { chain_id: u64, max_drift_secs: u64 },
    /// accounts: as `chain::init_chain` documents (payer, chain_authority, registry_authority, root,
    /// registry, chain_config, global_config, reserved_allow OR perm_nonce, system_program). Creates the
    /// root account, its verifier registry and `chain_config` v2 with an explicit `max_drift_secs`.
    /// Replaces `InitChain` (3) — a shipped body never changes at its discriminant.
    InitChainV2(chain::InitChainArgsV2),

    // --- verifier-key rotation with an activation delay, appended so 0-24 above never move;
    // rotation never touches another entry, retirement is explicit ---
    /// accounts: [registry_authority (signer), payer (signer, writable — funds the one-time v1->v2
    /// realloc), global_config (read-only), registry (writable), system_program]. Registers `entry` for
    /// `chain_id`'s registry, usable by `PostRootProved` only once `Clock::slot >= activation_slot`
    /// (`ActivationInPast` if the argument is already behind the clock; the reserved tombstone slot
    /// `RETIRED_SLOT` always passes this). Keyed on `(curve, scheme, vkey_hash)`: a vkey already present
    /// only has its `activation_slot` updated (`LayoutMismatch` if its stored `layout_id` differs); an
    /// absent vkey is appended, or — once `count == MAX_ENTRIES` — written into the first retired slot
    /// (`RegistryFull` if none is retired). Retiring a vkey is this same instruction with `activation_slot
    /// = RETIRED_SLOT`; `governance::set_registry_entry`'s doc has the full contract.
    SetRegistryEntry {
        chain_id: u64,
        entry: chain::RegistryEntryArg,
        activation_slot: u64,
    },

    // --- exit-config governance, appended so 0-25 above never move ---
    /// accounts: [chain_authority (signer, must equal `root.authority`), payer (signer, writable — funds
    /// `exit_config`'s create-or-adopt), root (read-only), exit_config (writable, new-or-existing),
    /// system_program]. Proposes a change to the chain's exit portal/bridge-program/cap/bond, gated
    /// by an activation delay of at least one full challenge window (no bootstrap exception).
    /// A supplied field may not be the zero sentinel (`ExitPortalZero`/`BridgeProgramZero`); an omitted
    /// (`None`) field is simply not part of this proposal. Refuses a second proposal while one is pending
    /// (`PendingExitConfigExists`) and a chain with `challenge_window_slots == 0` (`ChallengeWindowZero`,
    /// fail-closed). `governance::propose_exit_config`'s doc has the full contract.
    ProposeExitConfig {
        chain_id: u64,
        exit_portal: Option<[u8; 20]>,
        bridge_program: Option<Pubkey>,
        exit_cap_per_window: Option<u64>,
        poster_bond: Option<u64>,
        activation_slot: u64,
    },
    /// accounts: [root (writable), exit_config (writable)] — permissionless. Copies a pending proposal
    /// into `exit_config`'s current portal/bridge-program and the ROOT's `exit_cap_per_window`/
    /// `poster_bond` (the root stays the source of truth for both numbers, unchanged layout) once
    /// `Clock::slot >= exit_config.activation_slot`; refuses `NoPendingExitConfig`/`ActivationNotReached`
    /// otherwise. `governance::activate_exit_config`'s doc has the full contract.
    ActivateExitConfig { chain_id: u64 },

    // --- `ProveExit`, appended so 0-27 above never move ---
    /// accounts: [payer (signer, writable), root (read-only), pending(batch) (read-only), exit_config
    /// (read-only), exit_record (writable, NEW), exit_window (writable, new-or-existing),
    /// exit_nullifier_page (writable, new-or-existing), system_program]. Permissionless. Proves
    /// `sentMessages[message.message_hash()]` in the exit portal's storage (the portal bound from
    /// `exit_config`, never `message`/an argument) against the FINAL `state_root` of `batch` (the same
    /// finality gate `RootView` uses), enforces the per-window cap and burns the message's replay
    /// nullifier. `exit::prove_exit`'s doc has the full check order and the write-ordering invariant.
    ProveExit(exit::ProveExitArgs),

    // --- `ConsumeExit`, appended so 0-28 above never move ---
    /// accounts: [bridge_signer (signer — the `exit_consumer` PDA `["exit_consumer", chain_id]` under
    /// `exit_config.bridge_program`, CPI-signed by the registered bridge program; never a keypair),
    /// exit_config (read-only), exit_record (writable), payer_refund (writable, must equal
    /// `record.payer`)]. The registered bridge's PDA signer is the whole authorisation for releasing a
    /// proved exit; the refund always goes to the record's own payer, never an instruction argument.
    /// Recycles the `exit_record` account (rent to `payer_refund`) but never touches the `exit_nullifier`
    /// bit — that stays set forever, so a re-`ProveExit` of the same message still hits
    /// `ExitAlreadyProved`. `exit::consume_exit`'s doc has the full check order.
    ConsumeExit(exit::ConsumeExitArgs),
}

pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let ix = SettleIx::try_from_slice(data).map_err(|_| ProgramError::InvalidInstructionData)?;
    let it = &mut accounts.iter();
    match ix {
        // legacy.rs removed, these three discriminants disabled. Rejected before touching `it`/any account.
        SettleIx::Init { .. } | SettleIx::UpdateRoot { .. } | SettleIx::UpdateRootZisk { .. } => {
            Err(ProgramError::InvalidInstructionData)
        }
        // Decodes (history stays readable), never executes.
        SettleIx::InitChain(_) => Err(errors::SettleError::RetiredInstruction.into()),
        SettleIx::PostRoot(args) => settle::post_root(program_id, it, args),
        SettleIx::PostRootProved {
            args,
            proof_abi,
            header_rlp,
        } => settle::post_root_proved(program_id, it, args, proof_abi, header_rlp),
        SettleIx::FinalizeBatch { chain_id, batch } => {
            settle::finalize_batch(program_id, it, chain_id, batch)
        }
        SettleIx::ClosePending { chain_id, batch } => {
            settle::close_pending(program_id, it, chain_id, batch)
        }
        SettleIx::RootView { chain_id, batch } => {
            settle::root_view(program_id, it, chain_id, batch)
        }
        SettleIx::RejectBatch { chain_id, batch } => settle::reject_batch(chain_id, batch),
        SettleIx::InitGlobalConfig(args) => governance::init_global_config(program_id, it, args),
        SettleIx::AllowReservedId { chain_id } => {
            governance::allow_reserved_id(program_id, it, chain_id)
        }
        SettleIx::RevokeReservedId { chain_id } => {
            governance::revoke_reserved_id(program_id, it, chain_id)
        }
        SettleIx::SetFee {
            chain_id,
            base,
            bps,
        } => governance::set_fee(program_id, it, chain_id, base, bps),
        SettleIx::SetTreasury { treasury } => governance::set_treasury(program_id, it, treasury),
        // Decodes (history stays readable), never executes.
        SettleIx::MigrateChain { .. } => Err(errors::SettleError::RetiredInstruction.into()),
        SettleIx::MigrateChainV2 {
            chain_id,
            max_drift_secs,
        } => governance::migrate_chain(program_id, it, chain_id, max_drift_secs),
        SettleIx::InitChainV2(args) => chain::init_chain(program_id, it, args),
        SettleIx::RefundDeposit { chain_id } => {
            governance::refund_deposit(program_id, it, chain_id)
        }
        SettleIx::ReclaimChain { chain_id } => governance::reclaim_chain(program_id, it, chain_id),
        SettleIx::SetGlobalConfig(args) => governance::set_global_config(program_id, it, args),
        // Disabled — see the enum variant's doc and the module doc above.
        SettleIx::SetRegistryAuthority { .. } => Err(ProgramError::InvalidInstructionData),
        SettleIx::ProposeRegistryAuthority { new } => {
            governance::propose_registry_authority(program_id, it, new)
        }
        SettleIx::AcceptRegistryAuthority => governance::accept_registry_authority(program_id, it),
        SettleIx::SetDriftBound {
            chain_id,
            max_drift_secs,
        } => governance::set_drift_bound(program_id, it, chain_id, max_drift_secs),
        SettleIx::SetRegistryEntry {
            chain_id,
            entry,
            activation_slot,
        } => governance::set_registry_entry(program_id, it, chain_id, entry, activation_slot),
        SettleIx::ProposeExitConfig {
            chain_id,
            exit_portal,
            bridge_program,
            exit_cap_per_window,
            poster_bond,
            activation_slot,
        } => governance::propose_exit_config(
            program_id,
            it,
            chain_id,
            exit_portal,
            bridge_program,
            exit_cap_per_window,
            poster_bond,
            activation_slot,
        ),
        SettleIx::ActivateExitConfig { chain_id } => {
            governance::activate_exit_config(program_id, it, chain_id)
        }
        SettleIx::ProveExit(args) => exit::prove_exit(program_id, it, args),
        SettleIx::ConsumeExit(args) => exit::consume_exit(program_id, it, args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The deployed program is upgraded in place from the CI artifact — an existing
    /// instruction's borsh discriminant (its variant index) must never move, or an already-signed or
    /// already-queued `Init`/`UpdateRoot`/`UpdateRootZisk` would decode as a different variant after the
    /// upgrade. Pins every variant's first byte, 0 through 29.
    #[test]
    fn instruction_discriminants_are_pinned() {
        let cases: Vec<(u8, SettleIx)> = vec![
            (
                0,
                SettleIx::Init {
                    chain_id: 0,
                    number: 0,
                    state_root: [0u8; 32],
                },
            ),
            (
                1,
                SettleIx::UpdateRoot {
                    chain_id: 0,
                    proof: vec![],
                    public_values: vec![],
                },
            ),
            (
                2,
                SettleIx::UpdateRootZisk {
                    chain_id: 0,
                    proof_abi: vec![],
                    header_rlp: vec![],
                },
            ),
            (
                3,
                SettleIx::InitChain(chain::InitChainArgs {
                    chain_id: 0,
                    permissionless_nonce: 0,
                    number: 0,
                    parent_hash: [0u8; 32],
                    state_root: [0u8; 32],
                    block_hash: [0u8; 32],
                    profile: 0,
                    challenge_window_slots: 0,
                    prove_window_slots: 0,
                    proving_policy: 0,
                    poster_bond: 0,
                    exit_cap_per_window: 0,
                    authority: Pubkey::default(),
                    max_pending: 0,
                    inbox_program: Pubkey::default(),
                    registry_entries: vec![],
                }),
            ),
            (4, SettleIx::PostRoot(empty_post_root_args())),
            (
                5,
                SettleIx::PostRootProved {
                    args: empty_post_root_args(),
                    proof_abi: vec![],
                    header_rlp: vec![],
                },
            ),
            (
                6,
                SettleIx::FinalizeBatch {
                    chain_id: 0,
                    batch: 0,
                },
            ),
            (
                7,
                SettleIx::ClosePending {
                    chain_id: 0,
                    batch: 0,
                },
            ),
            (
                8,
                SettleIx::RootView {
                    chain_id: 0,
                    batch: 0,
                },
            ),
            (
                9,
                SettleIx::RejectBatch {
                    chain_id: 0,
                    batch: 0,
                },
            ),
            (
                10,
                SettleIx::InitGlobalConfig(governance::InitGlobalConfigArgs {
                    registry_authority: Pubkey::default(),
                    treasury: Pubkey::default(),
                    permissionless_init_enabled: false,
                    reclaim_window_slots: 0,
                    deposit_lamports: 0,
                    default_fee_base_lamports: 0,
                    default_fee_bps: 0,
                }),
            ),
            (11, SettleIx::AllowReservedId { chain_id: 0 }),
            (12, SettleIx::RevokeReservedId { chain_id: 0 }),
            (
                13,
                SettleIx::SetFee {
                    chain_id: 0,
                    base: 0,
                    bps: 0,
                },
            ),
            (
                14,
                SettleIx::SetTreasury {
                    treasury: Pubkey::default(),
                },
            ),
            (15, SettleIx::MigrateChain { chain_id: 0 }),
            (16, SettleIx::RefundDeposit { chain_id: 0 }),
            (17, SettleIx::ReclaimChain { chain_id: 0 }),
            (
                18,
                SettleIx::SetGlobalConfig(governance::SetGlobalConfigArgs {
                    permissionless_init_enabled: false,
                    reclaim_window_slots: 0,
                    deposit_lamports: 0,
                    default_fee_base_lamports: 0,
                    default_fee_bps: 0,
                }),
            ),
            (
                19,
                SettleIx::SetRegistryAuthority {
                    new: Pubkey::default(),
                },
            ),
            (
                20,
                SettleIx::ProposeRegistryAuthority {
                    new: Pubkey::default(),
                },
            ),
            (21, SettleIx::AcceptRegistryAuthority),
            (
                22,
                SettleIx::SetDriftBound {
                    chain_id: 0,
                    max_drift_secs: 0,
                },
            ),
            (
                23,
                SettleIx::MigrateChainV2 {
                    chain_id: 0,
                    max_drift_secs: 0,
                },
            ),
            (
                24,
                SettleIx::InitChainV2(chain::InitChainArgsV2 {
                    chain_id: 0,
                    permissionless_nonce: 0,
                    number: 0,
                    parent_hash: [0u8; 32],
                    state_root: [0u8; 32],
                    block_hash: [0u8; 32],
                    profile: 0,
                    challenge_window_slots: 0,
                    prove_window_slots: 0,
                    proving_policy: 0,
                    poster_bond: 0,
                    exit_cap_per_window: 0,
                    authority: Pubkey::default(),
                    max_pending: 0,
                    inbox_program: Pubkey::default(),
                    registry_entries: vec![],
                    max_drift_secs: 0,
                }),
            ),
            (
                25,
                SettleIx::SetRegistryEntry {
                    chain_id: 0,
                    entry: chain::RegistryEntryArg {
                        curve: 0,
                        scheme: 0,
                        vkey_hash: [0u8; 32],
                        layout_id: 0,
                    },
                    activation_slot: 0,
                },
            ),
            (
                26,
                SettleIx::ProposeExitConfig {
                    chain_id: 0,
                    exit_portal: None,
                    bridge_program: None,
                    exit_cap_per_window: None,
                    poster_bond: None,
                    activation_slot: 0,
                },
            ),
            (27, SettleIx::ActivateExitConfig { chain_id: 0 }),
            (
                28,
                SettleIx::ProveExit(exit::ProveExitArgs {
                    chain_id: 0,
                    batch: 0,
                    message: exit::ExitMessageArg {
                        nonce: 0,
                        l2_sender: [0u8; 20],
                        sol_recipient: [0u8; 32],
                        asset: [0u8; 20],
                        amount: 0,
                    },
                    proof: rome_zk_mpt::ExitProof {
                        account_nodes: vec![],
                        storage_nodes: vec![],
                    },
                }),
            ),
            (
                29,
                SettleIx::ConsumeExit(exit::ConsumeExitArgs {
                    chain_id: 0,
                    message_hash: [0u8; 32],
                }),
            ),
        ];
        for (expected, ix) in cases {
            let bytes = borsh::to_vec(&ix).unwrap();
            assert_eq!(
                bytes[0], expected,
                "{ix:?} must serialize with discriminant {expected}"
            );
        }
    }

    fn empty_post_root_args() -> settle::PostRootArgs {
        settle::PostRootArgs {
            chain_id: 0,
            batch: 0,
            prev_batch: 0,
            pre_state_root: [0u8; 32],
            first_block: 0,
            last_block: 0,
            state_root: [0u8; 32],
            block_roots_merkle: [0u8; 32],
            inbox_commitment: [0u8; 32],
            forced_outcome_commitment: [0u8; 32],
            parent_hash: [0u8; 32],
            last_block_hash: [0u8; 32],
            gas_in_batch: 0,
        }
    }

    #[test]
    fn root_and_registry_pda_derivations_are_deterministic() {
        let program_id = Pubkey::new_unique();
        let (a, _) = root_pda(&program_id, 7);
        let (b, _) = root_pda(&program_id, 7);
        assert_eq!(a, b);
        let (r, _) = registry_pda(&program_id, 7);
        assert_ne!(a, r, "root and registry PDAs must not collide");
    }
}
