//! `ProveExit` (28): proves that `sentMessages[message_hash]` was set in the L2 exit portal's storage,
//! against a FINAL batch's `state_root` — the same finality gate `RootView` uses
//! (`settle::final_root_tuple`) — binds the portal address from the on-chain `exit_config` (never the
//! message or an instruction argument), enforces the per-window cap, burns a persistent per-nonce replay
//! nullifier, and writes an `exit_record` a later `ConsumeExit` recycles.
//!
//! **Write ordering (the cap-refusal-must-not-burn-the-nullifier invariant).** Every check below —
//! finality, config/cap/window-slot gates, the asset restriction, the MPT proof, replay, and the
//! per-window cap — runs to completion, and every account write happens only after ALL of them pass. In
//! particular the replay check (`bit_is_set`) reads the nullifier page's bits without yet setting the bit,
//! and the cap check computes `spent + units` without yet writing the window — so a call refused
//! `ExitCapExceeded` leaves the nullifier bit clear: the exact same message can be re-proved once the
//! client re-queues it into a later window, exactly as `ProveExit` (28) requires. Reordering the
//! nullifier `set_bit` ahead of the cap check would silently break this — see the mutation test
//! `dropping_the_cap_compare_lets_the_over_cap_exit_through` and
//! `prove_exit_over_cap_refused_and_next_window_admits`.
//!
//! **The nullifier's own page-mismatch guard is dead code on this call path, by construction.** The exit
//! nullifier PDA is derived from `nullifier_page(message.nonce)` — never taken as a separate instruction
//! argument — so a caller who tries to point at the wrong page's account fails the seeds check (step 1)
//! before `bit_is_set`/`set_bit` (which each carry their own `NullifierPageMismatch` two-layer guard,
//! `rome_zk_layouts::exit`) ever run against a page argument that could disagree with the derived one. The
//! layouts crate's own guard stays as defense in depth for any other future caller of those functions; this
//! module's real "wrong page" refusal is the seeds check, and its mutation test proves that.

use crate::errors::SettleError;
use crate::settle::final_root_tuple;
use borsh::{BorshDeserialize, BorshSerialize};
use rome_zk_layouts::exit::{
    exit_config, exit_consumer_pda, exit_nullifier, exit_record, exit_window,
};
use rome_zk_mpt::{MptError, StorageValue};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    msg,
    program_error::ProgramError,
    pubkey::Pubkey,
    sysvar::Sysvar,
};
// `system_program` moved out of `solana_program`'s root re-export in the Agave 4.x line (API fallout).
use solana_system_interface::program as system_program;

/// Borsh-serializable mirror of `rome_zk_layouts::exit::ExitMessage`. That crate stays borsh-free by
/// design (it must build for a ZisK guest target, `rome-zk-layouts/Cargo.toml`'s module doc) — the
/// wire-carrying shape `ProveExit` actually decodes lives here instead, next to the instruction, and
/// converts into the layouts crate's own type (which owns `message_hash`/`storage_slot`/field order) via
/// [`From`], never re-implementing that arithmetic.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitMessageArg {
    pub nonce: u64,
    pub l2_sender: [u8; 20],
    pub sol_recipient: [u8; 32],
    pub asset: [u8; 20],
    pub amount: u128,
}

impl From<ExitMessageArg> for rome_zk_layouts::exit::ExitMessage {
    fn from(m: ExitMessageArg) -> Self {
        rome_zk_layouts::exit::ExitMessage {
            nonce: m.nonce,
            l2_sender: m.l2_sender,
            sol_recipient: m.sol_recipient,
            asset: m.asset,
            amount: m.amount,
        }
    }
}

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone)]
pub struct ProveExitArgs {
    pub chain_id: u64,
    pub batch: u64,
    pub message: ExitMessageArg,
    pub proof: rome_zk_mpt::ExitProof,
}

/// Maps a `rome-zk-mpt` refusal onto its named `SettleError`: a bound blown before any hashing
/// (`TooManyNodes`/`NodeTooLarge`) is `ExitProofTooLarge`; every other refusal (a hash/path mismatch, a
/// malformed node, a trailing node, a mis-shaped account/storage value) is the general `ExitProofInvalid`.
fn map_mpt_error(e: MptError) -> ProgramError {
    match e {
        MptError::TooManyNodes { .. } | MptError::NodeTooLarge { .. } => {
            SettleError::ExitProofTooLarge.into()
        }
        _ => SettleError::ExitProofInvalid.into(),
    }
}

/// `true` iff `v` is the big-endian, left-padded-to-32-bytes encoding of `1` — the only value
/// `sentMessages[message_hash]` (a Solidity `mapping(bytes32 => bool)`) is ever written as when set.
fn is_value_one(v: &[u8; 32]) -> bool {
    let mut one = [0u8; 32];
    one[31] = 1;
    *v == one
}

/// accounts: `[payer (signer, writable), root (read-only), pending(batch) (read-only), exit_config
/// (read-only), exit_record (writable, NEW), exit_window (writable, new-or-existing), exit_nullifier_page
/// (writable, new-or-existing), system_program]`. Permissionless. Check order (cheapest refusal first,
/// module doc's write-ordering invariant last):
///
/// 1. Signer + every account's PDA seeds (the nullifier page's seed is derived on-chain from
///    `nullifier_page(message.nonce)` — never a caller-supplied page).
/// 2. `final_root_tuple` — the SAME finality predicate `RootView` uses (`NotFinal` otherwise).
/// 3. `exit_config.exit_portal == [0; 20]` → `ExitConfigUnset`; `root.exit_cap_per_window == 0` →
///    `ExitCapUnset`; `root.challenge_window_slots == 0` → `ChallengeWindowZero`; `message.asset != [0;
///    20]` → `UnsupportedAsset` (v1 native-asset only).
/// 4. The MPT proof: `verify_account(state_root, exit_config.exit_portal, proof.account_nodes)` — the
///    portal address comes from the on-chain config, never the message or an instruction argument — then
///    `verify_storage(account.storage_root, message.storage_slot(), proof.storage_nodes)`;
///    `Absent` → `ExitNotSent`; `Present(v)` with `v != 1` → `ExitProofInvalid`; a verifier error →
///    `ExitProofInvalid`/`ExitProofTooLarge` by name (`map_mpt_error`).
/// 5. Nullifier: if the page account already exists, `bit_is_set` → `ExitAlreadyProved` when set; the bit
///    itself is NOT set yet (module doc's write-ordering invariant).
/// 6. Window: `units = cap_units(message.amount)` (`ExitAmountOverflow`); `spent + units > cap` →
///    `ExitCapExceeded` — computed, not yet written.
/// 7. Every check above passed: commit — set the nullifier bit, write the window's new spent/exits, write
///    the `exit_record` (`STATUS_PROVED`).
pub fn prove_exit(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    args: ProveExitArgs,
) -> ProgramResult {
    let payer = next_account_info(it)?;
    let root_acc = next_account_info(it)?;
    let pending_acc = next_account_info(it)?;
    let exit_config_acc = next_account_info(it)?;
    let exit_record_acc = next_account_info(it)?;
    let exit_window_acc = next_account_info(it)?;
    let exit_nullifier_acc = next_account_info(it)?;
    let sys = next_account_info(it)?;

    if !payer.is_signer || *sys.key != system_program::id() {
        return Err(ProgramError::MissingRequiredSignature);
    }

    // --- (1) seeds + owner, cheapest refusal first ---
    let (expect_root, _) = rome_zk_layouts::root::pda(program_id, args.chain_id);
    if expect_root != *root_acc.key || root_acc.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    let (expect_pending, _) = rome_zk_layouts::pending::pda(program_id, args.chain_id, args.batch);
    if expect_pending != *pending_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }
    let (expect_exit_config, _) = exit_config::pda(program_id, args.chain_id);
    if expect_exit_config != *exit_config_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }

    let message: rome_zk_layouts::exit::ExitMessage = args.message.into();
    let message_hash = message.message_hash();
    let (expect_record, record_bump) = exit_record::pda(program_id, args.chain_id, message_hash);
    if expect_record != *exit_record_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }
    // The page seed is DERIVED from the nonce, never caller-chosen — a wrong page account fails here,
    // before any nullifier bit is ever read (see the module doc's "dead code by construction" note).
    let page = rome_zk_layouts::exit::nullifier_page(message.nonce);
    let (expect_nullifier, nullifier_bump) = exit_nullifier::pda(program_id, args.chain_id, page);
    if expect_nullifier != *exit_nullifier_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }

    // --- (2) finality: the SAME predicate RootView uses ---
    let root = {
        let d = root_acc.try_borrow_data()?;
        rome_zk_layouts::root::read(&d).map_err(|_| ProgramError::InvalidAccountData)?
    };
    if root.chain_id != args.chain_id {
        return Err(ProgramError::InvalidAccountData);
    }
    let final_tuple = final_root_tuple(program_id, &root, pending_acc, args.batch)?;

    // --- (3) config / cap / window-slot / asset gates ---
    let exit_cfg_fields = if exit_config_acc.owner == program_id {
        let d = exit_config_acc.try_borrow_data()?;
        exit_config::read(&d).map_err(|_| ProgramError::InvalidAccountData)?
    } else {
        // Absent exit_config = exits disabled for this chain (module doc, `rome_zk_layouts::exit::exit_config`)
        // — never read data from an account this program does not own.
        exit_config::ExitConfigFields {
            chain_id: args.chain_id,
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
    if exit_cfg_fields.exit_portal == [0u8; 20] {
        return Err(SettleError::ExitConfigUnset.into());
    }
    if root.exit_cap_per_window == 0 {
        return Err(SettleError::ExitCapUnset.into());
    }
    if root.challenge_window_slots == 0 {
        return Err(SettleError::ChallengeWindowZero.into());
    }
    if message.asset != [0u8; 20] {
        return Err(SettleError::UnsupportedAsset.into());
    }

    // --- (4) MPT proof: the portal address comes from exit_config, NEVER the message/an argument ---
    let account = rome_zk_mpt::verify_account(
        &final_tuple.state_root,
        &exit_cfg_fields.exit_portal,
        &args.proof.account_nodes,
    )
    .map_err(map_mpt_error)?;
    let storage_slot = message.storage_slot();
    let value = rome_zk_mpt::verify_storage(
        &account.storage_root,
        &storage_slot,
        &args.proof.storage_nodes,
    )
    .map_err(map_mpt_error)?;
    match value {
        StorageValue::Absent => return Err(SettleError::ExitNotSent.into()),
        StorageValue::Present(v) if !is_value_one(&v) => {
            return Err(SettleError::ExitProofInvalid.into())
        }
        StorageValue::Present(_) => {}
    }

    // --- (5) nullifier: replay refused by name. The bit is NOT set yet — see the module doc's
    // write-ordering invariant (a cap-exceeded refusal below must leave this exit re-provable). ---
    let nullifier_already_exists = exit_nullifier_acc.owner == program_id
        && exit_nullifier_acc.data_len() == exit_nullifier::LEN;
    let mut bits = if nullifier_already_exists {
        let d = exit_nullifier_acc.try_borrow_data()?;
        let hdr = exit_nullifier::read_header(&d).map_err(|_| ProgramError::InvalidAccountData)?;
        if hdr.chain_id != args.chain_id || hdr.page != page {
            return Err(ProgramError::InvalidAccountData);
        }
        let mut b = [0u8; exit_nullifier::BITS_LEN];
        b.copy_from_slice(&d[exit_nullifier::OFF_BITS..exit_nullifier::LEN]);
        b
    } else {
        [0u8; exit_nullifier::BITS_LEN]
    };
    if rome_zk_layouts::exit::bit_is_set(&bits, page, message.nonce)
        .map_err(|_| ProgramError::InvalidAccountData)?
    {
        return Err(SettleError::ExitAlreadyProved.into());
    }

    // --- (6) window cap: computed, not yet written — same reason as (5) ---
    let now = Clock::get()?.slot;
    let window_index = now / root.challenge_window_slots as u64;
    let (expect_window, window_bump) = exit_window::pda(program_id, args.chain_id, window_index);
    if expect_window != *exit_window_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }
    let window_already_exists =
        exit_window_acc.owner == program_id && exit_window_acc.data_len() == exit_window::LEN;
    let existing_window = if window_already_exists {
        let d = exit_window_acc.try_borrow_data()?;
        exit_window::read(&d).map_err(|_| ProgramError::InvalidAccountData)?
    } else {
        exit_window::ExitWindowFields {
            chain_id: args.chain_id,
            window_index,
            spent_cap_units: 0,
            exits: 0,
        }
    };
    let units = rome_zk_layouts::exit::cap_units(message.amount)
        .map_err(|_| ProgramError::from(SettleError::ExitAmountOverflow))?;
    let new_spent = existing_window
        .spent_cap_units
        .checked_add(units)
        .ok_or(ProgramError::from(SettleError::ExitCapExceeded))?;
    if new_spent > root.exit_cap_per_window {
        return Err(SettleError::ExitCapExceeded.into());
    }

    // --- (7) every refusal above left zero writes; commit now ---
    if !nullifier_already_exists {
        let seeds = exit_nullifier::seeds(args.chain_id, page);
        rome_zk_pda::create_or_adopt_pda(
            payer,
            exit_nullifier_acc,
            sys,
            program_id,
            exit_nullifier::LEN,
            &[&seeds[0], &seeds[1], &seeds[2], &[nullifier_bump]],
        )?;
        let mut d = exit_nullifier_acc.try_borrow_mut_data()?;
        exit_nullifier::write_header(&mut d, args.chain_id, page);
    }
    rome_zk_layouts::exit::set_bit(&mut bits, page, message.nonce)
        .map_err(|_| ProgramError::InvalidAccountData)?;
    {
        let mut d = exit_nullifier_acc.try_borrow_mut_data()?;
        d[exit_nullifier::OFF_BITS..exit_nullifier::LEN].copy_from_slice(&bits);
    }

    if !window_already_exists {
        let seeds = exit_window::seeds(args.chain_id, window_index);
        rome_zk_pda::create_or_adopt_pda(
            payer,
            exit_window_acc,
            sys,
            program_id,
            exit_window::LEN,
            &[&seeds[0], &seeds[1], &seeds[2], &[window_bump]],
        )?;
    }
    {
        let mut d = exit_window_acc.try_borrow_mut_data()?;
        d.copy_from_slice(&exit_window::write(&exit_window::ExitWindowFields {
            chain_id: args.chain_id,
            window_index,
            spent_cap_units: new_spent,
            exits: existing_window.exits + 1,
        }));
    }

    // Always NEW in practice (a re-prove of the same message hits the nullifier at (5) first) — guarded
    // with `if !already_exists` anyway, as a defensive measure.
    let record_already_exists =
        exit_record_acc.owner == program_id && exit_record_acc.data_len() == exit_record::LEN;
    if !record_already_exists {
        let seeds = exit_record::seeds(args.chain_id, message_hash);
        rome_zk_pda::create_or_adopt_pda(
            payer,
            exit_record_acc,
            sys,
            program_id,
            exit_record::LEN,
            &[&seeds[0], &seeds[1], &seeds[2], &[record_bump]],
        )?;
    }
    {
        let mut d = exit_record_acc.try_borrow_mut_data()?;
        d.copy_from_slice(&exit_record::write(&exit_record::ExitRecordFields {
            chain_id: args.chain_id,
            batch: args.batch,
            message_hash,
            sol_recipient: message.sol_recipient,
            amount: message.amount,
            window_index,
            proved_slot: now,
            status: exit_record::STATUS_PROVED,
            payer: payer.key.to_bytes(),
            asset: message.asset,
        }));
    }

    msg!(
        "chain {} exit proved: batch {} window {} nullifier page {}",
        args.chain_id,
        args.batch,
        window_index,
        page
    );
    Ok(())
}

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsumeExitArgs {
    pub chain_id: u64,
    pub message_hash: [u8; 32],
}

/// accounts: `[bridge_signer (signer — the `exit_consumer` PDA `["exit_consumer", chain_id]` under
/// `exit_config.bridge_program`, CPI-signed by the registered bridge program; never a keypair — a PDA has
/// no private key), exit_config (read-only), exit_record (writable), payer_refund (writable, must equal
/// `record.payer`)]`. No `system_program` account (the close path below never calls `create_account`,
/// only drains lamports + reallocs to zero + reassigns — the exact same three-call shape
/// `settle::close_pending` already uses). Check order (cheapest refusal first):
///
/// 1. Seeds + owner of `exit_config` and `exit_record` (record seeds `["exit", chain_id, message_hash]`).
/// 2. `exit_record.status == STATUS_PROVED` else [`SettleError::ExitNotProved`] — a record only ever
///    exists as PROVED (written that way by `prove_exit`) or is already gone (recycled by a prior
///    `ConsumeExit`, in which case step 1's seeds/owner check already refused it as "no such account" —
///    this named refusal is reached only for a record that DOES exist but is not, or is no longer,
///    PROVED; today that is unreachable in practice since `STATUS_RELEASED` never lands back on a live
///    account, but the check stays as the named guard the design's state machine documents, not an
///    "impossible, so skip it" shortcut).
/// 3. `bridge_signer.key == exit_consumer_pda(chain_id, exit_config.bridge_program)` else
///    [`SettleError::NotBridgeProgram`] — checked BEFORE `is_signer` so a caller who is not even trying to
///    be the registered bridge gets the more specific refusal; `bridge_signer.is_signer` (a keypair can
///    never satisfy this — only an `invoke_signed` CPI from the program that owns the derivation can) is
///    then required too. This pair is the WHOLE authorisation for releasing a proved exit: no other check on
///    this instruction gates who may call it.
/// 4. `payer_refund.key == record.payer` else [`ProgramError::InvalidArgument`] — the refund destination
///    is read from the record the prover itself paid to create, NEVER an instruction-argument-chosen
///    account: a caller cannot redirect somebody else's rent refund by naming a
///    different `payer_refund`.
/// 5. Every check above passed: drain `exit_record`'s lamports to `payer_refund`, zero its data, realloc
///    to 0 bytes and reassign it to the system program (`settle::close_pending`'s own three-call close
///    shape) — the account is gone, its rent recycled. **The `exit_nullifier` bit for this message is NOT
///    touched**: it was set once, permanently, by `prove_exit`, and stays set forever — a re-`ProveExit`
///    of the same message after this call still hits `ExitAlreadyProved` at prove_exit's own nullifier
///    check, because that check reads the persistent bit, not this now-recycled record (the
///    "nothing is paid forever" rule applies to the ONE-BIT nullifier, not to the record, which is
///    exactly the rent this instruction gives back).
pub fn consume_exit(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    args: ConsumeExitArgs,
) -> ProgramResult {
    let bridge_signer = next_account_info(it)?;
    let exit_config_acc = next_account_info(it)?;
    let exit_record_acc = next_account_info(it)?;
    let payer_refund = next_account_info(it)?;

    // --- (1) seeds + owner, cheapest refusal first ---
    let (expect_exit_config, _) = exit_config::pda(program_id, args.chain_id);
    if expect_exit_config != *exit_config_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }
    if exit_config_acc.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    let (expect_record, _) = exit_record::pda(program_id, args.chain_id, args.message_hash);
    if expect_record != *exit_record_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }
    if exit_record_acc.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }

    // --- (2) the record must be PROVED ---
    let record = {
        let d = exit_record_acc.try_borrow_data()?;
        exit_record::read(&d).map_err(|_| ProgramError::InvalidAccountData)?
    };
    if record.chain_id != args.chain_id || record.message_hash != args.message_hash {
        return Err(ProgramError::InvalidAccountData);
    }
    if record.status != exit_record::STATUS_PROVED {
        return Err(SettleError::ExitNotProved.into());
    }

    // --- (3) the registered bridge program's exit_consumer PDA, CPI-signed ---
    let exit_cfg_fields = {
        let d = exit_config_acc.try_borrow_data()?;
        exit_config::read(&d).map_err(|_| ProgramError::InvalidAccountData)?
    };
    let bridge_program = Pubkey::new_from_array(exit_cfg_fields.bridge_program);
    let (expect_signer, _) = exit_consumer_pda(args.chain_id, &bridge_program);
    if expect_signer != *bridge_signer.key {
        return Err(SettleError::NotBridgeProgram.into());
    }
    if !bridge_signer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    // --- (4) the refund destination is the record's own payer, never an ix-arg account ---
    if payer_refund.key.to_bytes() != record.payer {
        return Err(ProgramError::InvalidArgument);
    }

    // --- (5) release: drain lamports to payer_refund, zero + close the record. The exit_nullifier bit
    // is never touched here — it is the persistent replay guard, untouched by this call. ---
    let lamports = exit_record_acc.lamports();
    **exit_record_acc.try_borrow_mut_lamports()? = 0;
    **payer_refund.try_borrow_mut_lamports()? += lamports;
    exit_record_acc.resize(0)?; // resize(len) replaces the old realloc(len, zero_init) (no-op for a shrink)
    exit_record_acc.assign(&system_program::id());

    msg!(
        "chain {} exit released: message_hash {:?}, {} lamports refunded to payer",
        args.chain_id,
        args.message_hash,
        lamports
    );
    Ok(())
}

/// Pure-function guard tests (no `ProgramTest`/BPF needed — these run on the host): `is_value_one` is the
/// exact `sentMessages[message_hash] == 1` gate `prove_exit`'s MPT step relies on, and `map_mpt_error` is
/// the exact `rome_zk_mpt::MptError` → `SettleError` mapping. Mutating either function's body (e.g.
/// `is_value_one` returning `true` unconditionally) turns these tests red directly — the real-BPF
/// `prove_exit_valid_writes_record_window_page`/`prove_exit_exclusion_proof_is_refused` etc.
/// (`tests/exit_prove.rs`) exercise the same guards end to end, through the real MPT proof.
#[cfg(test)]
mod pure_guard_tests {
    use super::*;

    #[test]
    fn is_value_one_only_true_for_exactly_one() {
        let mut one = [0u8; 32];
        one[31] = 1;
        assert!(is_value_one(&one));

        let mut two = [0u8; 32];
        two[31] = 2;
        assert!(!is_value_one(&two));

        assert!(!is_value_one(&[0u8; 32]));

        let mut high_byte_set = [0u8; 32];
        high_byte_set[0] = 1; // 1 << 248, not the small integer 1
        assert!(!is_value_one(&high_byte_set));
    }

    #[test]
    fn map_mpt_error_names_bounds_as_too_large_and_everything_else_as_invalid() {
        assert_eq!(
            map_mpt_error(MptError::TooManyNodes { got: 65 }),
            SettleError::ExitProofTooLarge.into()
        );
        assert_eq!(
            map_mpt_error(MptError::NodeTooLarge { index: 0, len: 533 }),
            SettleError::ExitProofTooLarge.into()
        );
        for e in [
            MptError::RootMismatch,
            MptError::HashMismatch { index: 0 },
            MptError::PathMismatch,
            MptError::BadRlp { index: 0 },
            MptError::BadNodeShape { index: 0, items: 3 },
            MptError::TrailingNodes,
            MptError::BadAccountRlp,
            MptError::ValueTooLong,
        ] {
            assert_eq!(map_mpt_error(e), SettleError::ExitProofInvalid.into());
        }
    }
}
