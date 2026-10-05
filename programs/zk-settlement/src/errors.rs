//! Settlement-specific error codes (mapped to `ProgramError::Custom`).

use solana_program::program_error::ProgramError;

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleError {
    /// `PostRoot`/`PostRootProved`'s `batch` argument is not `head_pending_batch + 1`.
    BadBatchSequence = 1,
    /// `prev_batch` argument does not equal `head_pending_batch`.
    BadPrevBatch = 2,
    /// `pre_state_root` argument does not equal the predecessor's posted `state_root`.
    BadPreStateRoot = 3,
    /// `first_block` argument does not equal `predecessor.last_block + 1`.
    BadFirstBlock = 4,
    /// The inbox batch account is not `finalized`.
    InboxNotFinalized = 5,
    /// The inbox batch account's `acc` does not equal the supplied `inbox_commitment`.
    AccMismatch = 6,
    /// The inbox batch account is not owned by the chain's registered inbox program, or is not at the
    /// expected `["batch", chain_id, batch]` PDA.
    WrongInboxAccount = 7,
    /// `pending_count >= max_pending` (backpressure).
    MaxPendingReached = 8,
    /// Signer is not `root.authority`.
    NotChainAuthority = 9,
    /// `FinalizeBatch` called before `Clock.slot >= deadline_slot`.
    BeforeDeadline = 10,
    /// `FinalizeBatch` called while `disputes_open != 0`.
    DisputesOpen = 11,
    /// `FinalizeBatch`/`PostRootProved`'s finality-advance is not the next in order
    /// (`batch != head_final_batch + 1`) — the batch itself may still become `Final`, but the root
    /// account's head pointer does not move until its predecessor already has.
    OutOfOrderFinality = 12,
    /// The pending account is not in `Pending` status where that is required.
    NotPending = 13,
    /// `RootView`/`ClosePending` requires a batch that is not yet final (`head_final_batch < batch`).
    NotFinal = 14,
    /// `ClosePending` refuses to recycle the head pending batch (the next `PostRoot` may
    /// still need to read it as its predecessor).
    IsHeadPendingBatch = 15,
    /// No registry entry matches the requested `(curve, scheme)`.
    RegistryEntryNotFound = 16,
    /// The proof's own committed verifying-key material does not match the registry entry's
    /// `vkey_hash` — the core binding that makes the registry meaningful.
    VkeyMismatch = 17,
    /// A registry entry's `layout_id` is not one this instruction knows how to interpret.
    UnsupportedLayout = 18,
    /// The proved header's committed hash does not match `keccak(header_rlp)`.
    HeaderHashMismatch = 19,
    /// The registry index the caller named does not exist in the account (`index >= count`).
    RegistryIndexOutOfRange = 20,
    /// `RejectBatch` — the challenge flow is not built yet; the discriminant and status code are
    /// reserved but the instruction is not implemented.
    NotImplemented = 21,
    /// The registered inbox program id does not match what the caller supplied for account derivation.
    WrongInboxProgram = 22,
    /// The proved header's `state_root` does not match the poster's claimed `state_root` for the batch.
    StateRootMismatch = 23,
    /// The proved header's `parent_hash` does not match the poster's claimed `PostRootArgs::parent_hash`
    /// (every posted batch claims its own block-hash pair, checked against the verified header on the
    /// proved path).
    ParentHashMismatch = 24,

    // --- registration and revenue ---
    /// A reserved (`chain_id < 2^32`) `InitChain` without the registry authority's signature, or whose
    /// signature does not match `global_config.registry_authority`.
    NotRegistryAuthority = 25,
    /// A reserved `InitChain` for a `chain_id` with no live `AllowReservedId` marker.
    ReservedIdNotAllowed = 26,
    /// A permissionless (`chain_id >= 2^32`) `InitChain` while `global_config.permissionless_init_enabled
    /// == false` (the mainnet gate — default off).
    PermissionlessInitDisabled = 27,
    /// A permissionless `InitChain`'s `chain_id` argument does not equal
    /// `chainid::permissionless_chain_id(authority, nonce)` for that authority's current nonce.
    BadPermissionlessChainId = 28,
    /// The `global_config` account is not at `["global_config"]` or fails to decode.
    WrongGlobalConfig = 29,
    /// The `chain_config` account is not at `["chain_config", chain_id]`, not owned by this program, or
    /// its stored `chain_id` disagrees with the instruction argument.
    WrongChainConfig = 30,
    /// The supplied treasury account does not equal `global_config.treasury`.
    WrongTreasury = 31,
    /// `InitGlobalConfig` called a second time (the account already holds real data).
    GlobalConfigAlreadyInitialized = 32,
    /// `RefundDeposit` on a chain with nothing locked (reserved id, or already refunded).
    NoDepositToRefund = 33,
    /// `RefundDeposit` before either trigger (1 final root or 10 posted batches).
    RefundNotYetEligible = 34,
    /// `ReclaimChain` before `registered_slot + reclaim_window_slots`, or after a root has been posted.
    ChainNotReclaimable = 35,
    /// The account supplied as the chain authority's refund/reclaim recipient does not match
    /// `root.authority`.
    WrongChainAuthority = 36,
    /// A fee computation overflowed (`base + bps * gas_in_batch / 10_000`).
    FeeOverflow = 37,
    /// `MigrateChainV2` for a chain whose `chain_config` is already v2 — the one-time
    /// v1→v2 bring-forward has nothing left to do. A v1 account is realloc'd and migrated instead of
    /// hitting this; an absent account is created fresh at v2.
    ChainAlreadyMigrated = 38,
    /// `InitGlobalConfig`'s `authority` account is not the program's real, live upgrade authority:
    /// the `ProgramData` account is missing/wrong/not owned by `bpf_loader_upgradeable`, its header does
    /// not parse, the program is immutable (`None` authority), or the signer does not match the stored
    /// authority pubkey.
    NotUpgradeAuthority = 39,
    /// `InitGlobalConfig`/`SetTreasury`'s treasury account argument does not match the treasury account
    /// supplied, or that account holds fewer lamports than the 0-byte rent-exempt minimum —
    /// an unfunded treasury would otherwise make every chain's first `PostRoot` fail.
    TreasuryNotRentExempt = 40,
    /// `PostRootProved`'s `args.gas_in_batch` does not equal the proved header's own `gasUsed` field:
    /// the bps fee component binds to the proof, not the poster's self-declared claim.
    GasInBatchMismatch = 41,

    // --- registry authority and global config limits ---
    /// `ProposeRegistryAuthority`'s `new` is `Pubkey::default()` — a typo'd all-zero key would otherwise
    /// lock every registry-authority instruction out forever once accepted, and `InitGlobalConfig` runs
    /// only once so there is no do-over.
    InvalidRegistryAuthority = 42,
    /// `AcceptRegistryAuthority`'s signer is not `global_config.pending_registry_authority` — including
    /// "nothing proposed", where that field is `Pubkey::default()` and no real key can ever sign as the
    /// all-zero pubkey.
    NotPendingRegistryAuthority = 43,
    /// `SetGlobalConfig`/`InitGlobalConfig`'s `reclaim_window_slots` argument is below the 1-day-of-slots
    /// floor: a shorter window would make a never-posted permissionless chain reclaimable in (or near)
    /// its own registration slot.
    ReclaimWindowTooShort = 44,
    /// `SetGlobalConfig`'s `deposit_lamports == 0` while `permissionless_init_enabled` is true:
    /// permissionless chain ids would be free to mint while the gate is on.
    DepositRequiredForPermissionless = 45,

    // --- public values v2: chain_config v2 / MigrateChainV2 / SetDriftBound / PostRootProved layout 1 ---
    /// `InitChainV2`/`MigrateChainV2`/`SetDriftBound`'s `max_drift_secs` argument is `0` — a program constant
    /// this is not; the drift bound must be a real, nonzero window.
    DriftBoundZero = 46,
    /// `PostRootProved` layout 1: the guest's committed `pv.chain_id` does not equal `args.chain_id`.
    PublicValuesChainMismatch = 47,
    /// `PostRootProved` layout 1: the guest's committed `pv.last_number` does not equal `args.last_block`
    /// (paired with the existing `BadFirstBlock` for `pv.first_number`).
    BadLastBlock = 48,
    /// `PostRootProved` layout 1: the guest's committed `pv.inbox_commitment` or
    /// `pv.forced_outcome_commitment` does not equal the corresponding `args` field.
    CommitmentMismatch = 49,
    /// `PostRootProved` layout 1: the guest's committed `pv.open_unix_ts` does not equal the inbox batch
    /// account's own `open_unix_ts` (the same committed Solana clock reading `OpenBatch` wrote).
    OpenTsMismatch = 50,
    /// `PostRootProved` layout 1: the guest's committed `pv.max_drift_secs` does not equal
    /// `chain_config.max_drift_secs` (only reachable when the chain_config is v2 — see
    /// `DriftBoundUnset` for the v1 case).
    DriftBoundMismatch = 51,
    /// `PostRootProved` layout 1 against a chain whose `chain_config` is still v1 (`max_drift_secs:
    /// None`) — there is no drift bound on record yet to check the proof against; `MigrateChainV2` first.
    DriftBoundUnset = 52,
    /// A retired instruction body: the discriminant still decodes so recorded history stays readable,
    /// but the program refuses it — `MigrateChain` (15) is superseded by `MigrateChainV2` (23),
    /// `InitChain` (3) by `InitChainV2` (24).
    RetiredInstruction = 53,
    /// `PostRootProved` layout 1: the 512-byte ZisK publics are not a v2 packing (a word over `u32::MAX`
    /// or a non-zero tail word) — no guest produces this; refused before any binding or pairing.
    BadPublicValuesPacking = 54,
    /// `PostRootProved` layout 1: the unpacked 208 bytes do not decode as the v2 struct.
    BadPublicValues = 55,

    // --- verifier-key rotation with an activation delay (matching is keyed on the vkey) ---
    /// `SetRegistryEntry` would append a new vkey, but the registry already holds `MAX_ENTRIES` and none
    /// of them is retired (`activation_slot == RETIRED_SLOT`) to reuse — nothing left to rotate into.
    RegistryFull = 56,
    /// `SetRegistryEntry`'s `activation_slot` argument is before the current `Clock::slot` — a rotation
    /// cannot be backdated; equal (immediate activation) is allowed.
    ActivationInPast = 57,
    /// `SetRegistryEntry`'s `entry.layout_id` argument is neither `LAYOUT_ZISK_V1` nor
    /// `LAYOUT_HEADER_FALLBACK` — this instruction only ever registers a layout `PostRootProved` already
    /// knows how to interpret.
    UnknownLayout = 58,
    /// `SetRegistryEntry`'s `entry.curve`/`entry.scheme` argument is not one of the known constants.
    UnknownCurveOrScheme = 59,
    /// `SetRegistryEntry` names a vkey that is already registered under a DIFFERENT `layout_id`. A rotation
    /// keys on `(curve, scheme, vkey_hash)` — one vkey is one ELF is one layout — so it can only ever
    /// update that entry's `activation_slot`, never its layout.
    LayoutMismatch = 60,

    // --- duplicate vkeys unconstructable, writer mirrors reader, retirement terminal ---
    /// `InitChainV2`'s `registry_entries` argument names the same `(curve, scheme, vkey_hash)` twice —
    /// one vkey is one ELF is one layout, so two entries sharing a vkey can never both be meaningful, and
    /// `set_registry_entry`'s later scan for that same key would have no single well-defined entry to act
    /// on. Refused before any account write.
    DuplicateRegistryEntry = 61,
    /// `SetRegistryEntry` named a vkey whose stored `activation_slot` is already `RETIRED_SLOT`, with a
    /// requested `activation_slot` that is not itself `RETIRED_SLOT` — retirement is terminal while the
    /// entry stands: a retired vkey cannot be re-activated by naming it again, only a NEW vkey can take a
    /// retired slot (after which the registry no longer remembers the retired vkey — a bound).
    /// Retiring an already-retired entry (`activation_slot == RETIRED_SLOT` both before and after) is
    /// unaffected by this check and stays a no-op success.
    EntryRetired = 62,

    // --- exit-config governance ---
    // Codes 63-71, 74 and 81 are the ProveExit/ConsumeExit errors further down; 80 is unused (its planned
    // `NotChainAuthority` was dropped).
    /// `ProposeExitConfig`'s `exit_portal` argument is `Some([0; 20])` — a proposal may set a subset of
    /// fields (`None` leaves a field out of this proposal entirely), but a *supplied* portal may never be
    /// the zero sentinel.
    ExitPortalZero = 72,
    /// `ProposeExitConfig`'s `bridge_program` argument is `Some(Pubkey::default())` — same rule as
    /// `ExitPortalZero`, for the bridge-program field.
    BridgeProgramZero = 73,
    /// `ProposeExitConfig`'s `activation_slot` argument is less than `Clock::slot +
    /// root.challenge_window_slots` — the delay must be at least one full challenge window, with no
    /// bootstrap exception.
    ActivationTooSoon = 75,
    /// `ActivateExitConfig` called before `Clock::slot >= exit_config.activation_slot`.
    ActivationNotReached = 76,
    /// `ActivateExitConfig` called against an `exit_config` whose `pending_mask == 0` — nothing proposed.
    NoPendingExitConfig = 77,
    /// `ProposeExitConfig` called against an `exit_config` that already has a proposal in flight
    /// (`pending_mask != 0`) — one proposal in flight at a time; activate it or wait, then propose again.
    PendingExitConfigExists = 78,
    /// `ProposeExitConfig` called against a chain whose `root.challenge_window_slots == 0` — fail-closed:
    /// a chain with no challenge window can never configure exits.
    ChallengeWindowZero = 79,

    // --- `ProveExit` ---
    /// `ProveExit`: `exit_config.exit_portal == [0; 20]` — exits are disabled until governed
    /// (`ProposeExitConfig`/`ActivateExitConfig`).
    ExitConfigUnset = 63,
    /// `ProveExit`: `root.exit_cap_per_window == 0` — `0` disables exits, fail-closed.
    ExitCapUnset = 64,
    /// `ProveExit`: the nullifier page's bit for `message.nonce` is already set — this exit was already
    /// proved. The bit persists past `ConsumeExit`'s release, so a re-prove after release
    /// lands here too.
    ExitAlreadyProved = 65,
    /// `ProveExit`: the MPT proof does not verify — a hash/path mismatch, a malformed node, or a proven
    /// storage value that is not exactly `1` (including a proof of the wrong account — the portal address
    /// is bound from `exit_config`, never the message or an instruction argument).
    ExitProofInvalid = 66,
    /// `ProveExit`: `verify_storage` returned a proven ABSENCE — `sentMessages[message_hash]` was never
    /// set at the proved batch's `state_root` (a valid exclusion proof, not a malformed one).
    ExitNotSent = 67,
    /// `ProveExit`: this window's `spent_cap_units + cap_units(message.amount)` would exceed
    /// `root.exit_cap_per_window` — refused before any account is written, so the client may re-queue the
    /// same message into a later window.
    ExitCapExceeded = 68,
    /// `ProveExit`: `message.amount`'s `cap_units` conversion (`ceil(amount / 1 gwei)`) does not fit a
    /// `u64`.
    ExitAmountOverflow = 69,
    /// `ProveExit`: `proof.account_nodes`/`proof.storage_nodes` exceeded `rome_zk_mpt`'s bounds
    /// (`MAX_NODES`/`MAX_NODE_BYTES`) — refused before the walk ever hashes a node.
    ExitProofTooLarge = 74,
    /// `ProveExit`: `message.asset != [0; 20]` — v1 supports the native asset only; a per-asset cap is a
    /// later seam.
    UnsupportedAsset = 81,

    // --- `ConsumeExit` ---
    /// `ConsumeExit`: `exit_record.status != STATUS_PROVED` — either the record was never proved (a
    /// seeds/owner mismatch is refused earlier by name; this is the "exists but already released, or its
    /// stored status is otherwise not PROVED" case) or `ConsumeExit` already recycled it once.
    ExitNotProved = 70,
    /// `ConsumeExit`: `bridge_signer.key != find_program_address(["exit_consumer", chain_id],
    /// exit_config.bridge_program)` — the whole authorisation for releasing a proved exit. Only the
    /// registered bridge program's own PDA, CPI-signed, can ever satisfy this; a keypair can never sign
    /// as a PDA, and a different program's `exit_consumer` PDA derives to a different address.
    NotBridgeProgram = 71,

    // --- permissionless chains: registry entries ---
    /// `InitChainV2` on the PERMISSIONLESS path (`chain_id >= 2^32`) carried a non-empty
    /// `registry_entries`. A permissionless chain starts with an empty registry; the verifier keys it may
    /// finalize under are added afterwards by the registry authority (`SetRegistryEntry`). Refused before
    /// any account is created, any deposit is locked, or the authority's nonce moves. The reserved path
    /// is unaffected: the registry authority co-signs there.
    RegistryEntriesNotAllowed = 82,

    // --- permissionless chains are proved-only ---
    /// `PostRoot` (the unproved, challenge-window path) named a PERMISSIONLESS chain (`chain_id >= 2^32`).
    /// A permissionless chain may only ever finalize a root that a ZisK proof backs, checked against a
    /// verifier key Rome registered for it, so the unproved path is closed to it. Refused before any
    /// account is read or written and before any lamport moves. Rome's reserved-range chains keep the
    /// unproved path.
    UnprovedRootNotAllowed = 83,
    /// `PostRootProved`: the parent hash the proof is bound to is not the last block hash of the batch
    /// before it (the genesis block hash held in the root account when no batch has been posted, otherwise
    /// the previous batch's own pending account). The proof chains onto some other history, so no root
    /// may be written from it. Refused before the pairing and before any account is written.
    PredecessorHashMismatch = 84,
    /// `SetRegistryEntry` named `LAYOUT_HEADER_FALLBACK` (layout 2) for a PERMISSIONLESS chain
    /// (`chain_id >= 2^32`). Layout 2 binds neither the chain id nor the inbox commitment, so a key
    /// registered under it would let a proof of some other chain's blocks finalize a root here. A
    /// permissionless chain registers layout-1 keys only. Refused before the registry is read or
    /// written. Rome's reserved-range chains still accept layout 2.
    HeaderFallbackNotAllowed = 85,

    // --- the bridge program is set once ---
    /// `ProposeExitConfig` named a bridge program for a chain whose exit config already holds one. A
    /// chain's bridge program is set once and never replaced, so deposit credits and exits keep
    /// reading the same bridge for the life of the chain. Refused before any account is created or
    /// written. Portal, cap and bond proposals are unaffected.
    BridgeProgramSetOnce = 86,

    // --- ZisK releases ---
    /// `PostRootProved`: the registry entry the proof's programVK matches names a ZisK release that is
    /// withdrawn (or one this build has no row for). No proof is verified under it; the chain registers a key
    /// under an open release instead. Refused before the layout checks and before the pairing.
    ZiskVersionWithdrawn = 87,
    /// `PostRootProved`: the proof's `rootCVadcopFinal` (`proof_abi[800..832]`) is not the value pinned for the
    /// release the registry entry names. A proof can carry any recursion root it likes, so the pinned one is
    /// the only one accepted. Refused before the layout checks and before the pairing.
    RootCNotOfVersion = 88,
    /// `SetRegistryEntry` or a genesis registry names a ZisK release that is closing. A closing release
    /// takes no new entries and no moved activation slot; its existing entries still verify and can be
    /// retired. Refused before anything is written.
    ZiskVersionClosing = 89,
    /// `SetRegistryEntry` or a genesis registry names a programVK that a registry entry which is not
    /// retired already holds under a different ZisK release. One programVK picks one entry, so the
    /// release a proof is checked under can never be a choice. Retire the other release's entry first.
    VkeyUnderOtherZiskVersion = 90,
}

impl From<SettleError> for ProgramError {
    fn from(e: SettleError) -> Self {
        ProgramError::Custom(e as u32)
    }
}
