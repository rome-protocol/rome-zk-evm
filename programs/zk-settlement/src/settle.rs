//! The batch-level settlement instructions: `PostRoot`, `PostRootProved`, `FinalizeBatch`, `ClosePending`,
//! `RootView`, `RejectBatch` (reserved).

use crate::chain::{registry_pda as chain_registry_pda, root_pda as chain_root_pda};
use crate::errors::SettleError;
use crate::governance;
use crate::header;
use borsh::{BorshDeserialize, BorshSerialize};
use rome_zk_layouts::{chain_config, pending, registry, root};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    msg,
    program::set_return_data,
    program_error::ProgramError,
    pubkey::Pubkey,
    sysvar::Sysvar,
};
// `system_program` moved out of `solana_program`'s root re-export in the Agave 4.x line (API fallout).
use solana_system_interface::program as system_program;

/// The single definition is `rome_zk_layouts::pending::seeds`; kept under this name
/// so every existing call site in this program is unchanged.
#[inline]
pub fn pending_seeds(chain_id: u64, batch: u64) -> [Vec<u8>; 3] {
    pending::seeds(chain_id, batch)
}
#[inline]
pub fn pending_pda(program_id: &Pubkey, chain_id: u64, batch: u64) -> (Pubkey, u8) {
    pending::pda(program_id, chain_id, batch)
}
/// Matches `zk_inbox::batch::seeds`/`rome_zk_layouts::batch::pda` — this program has no build-time
/// dependency on `zk-inbox` (only its shared `rome_zk_layouts::batch` decode does the work here); the
/// inbox program id itself comes from the registry account, checked at the call site.
///
/// The inbox batch account is keyed by the settlement program the chain was registered under; this program
/// passes its own `program_id` there, so the only batch account it can ever read for a chain is one that was
/// opened through this very deployment.
#[inline]
pub fn inbox_batch_pda(
    inbox_program: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
    batch: u64,
) -> Pubkey {
    rome_zk_layouts::batch::pda(inbox_program, settlement_program, chain_id, batch).0
}

/// The batch-identifying fields common to `PostRoot` and `PostRootProved`.
///
/// `parent_hash`/`last_block_hash`: every posted batch now carries its own block-hash pair into the
/// pending PDA, so `FinalizeBatch` can write a complete, per-batch-consistent
/// `{number, parent_hash, block_hash, state_root}` tuple into the root account regardless of which path
/// finalizes it — window-elapsed or immediately proved. For `PostRoot` (the unproved, window-based path)
/// these are a poster claim like `block_roots_merkle`, checked only at dispute time; for
/// `PostRootProved` they are checked against the verified header (`settle::post_root_proved`).
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone)]
pub struct PostRootArgs {
    pub chain_id: u64,
    pub batch: u64,
    pub prev_batch: u64,
    pub pre_state_root: [u8; 32],
    pub first_block: u64,
    pub last_block: u64,
    pub state_root: [u8; 32],
    pub block_roots_merkle: [u8; 32],
    pub inbox_commitment: [u8; 32],
    pub forced_outcome_commitment: [u8; 32],
    pub parent_hash: [u8; 32],
    pub last_block_hash: [u8; 32],
    /// The poster's declared EVM gas consumed by this batch — the variable input to the protocol fee (`fee
    /// = base_lamports_per_batch + bps * gas_in_batch / 10_000`). On `PostRootProved` this is checked equal
    /// to the proved header's own `gasUsed` before the bps term is charged; on the unproved `PostRoot` path
    /// there is nothing to check it against, so the bps term is simply never charged there (base fee only).
    pub gas_in_batch: u64,
}

/// Common account list for `PostRoot`/`PostRootProved`: [authority (signer, writable), root pda
/// (writable), pending pda (writable, new), predecessor pending pda (read-only; ignored when
/// `head_pending_batch == 0`), registry pda (read-only), inbox batch pda (read-only), chain_config pda
/// (writable — fee schedule + `posted_batches` counter), global_config pda (read-only — treasury),
/// treasury (writable), system_program].
struct PostRootAccounts<'a, 'b> {
    authority: &'a AccountInfo<'b>,
    root: &'a AccountInfo<'b>,
    pending: &'a AccountInfo<'b>,
    predecessor: &'a AccountInfo<'b>,
    registry: &'a AccountInfo<'b>,
    inbox_batch: &'a AccountInfo<'b>,
    chain_config: &'a AccountInfo<'b>,
    global_config: &'a AccountInfo<'b>,
    treasury: &'a AccountInfo<'b>,
    sys: &'a AccountInfo<'b>,
}

fn take_accounts<'a, 'b>(
    it: &mut std::slice::Iter<'a, AccountInfo<'b>>,
) -> Result<PostRootAccounts<'a, 'b>, ProgramError> {
    Ok(PostRootAccounts {
        authority: next_account_info(it)?,
        root: next_account_info(it)?,
        pending: next_account_info(it)?,
        predecessor: next_account_info(it)?,
        registry: next_account_info(it)?,
        inbox_batch: next_account_info(it)?,
        chain_config: next_account_info(it)?,
        global_config: next_account_info(it)?,
        treasury: next_account_info(it)?,
        sys: next_account_info(it)?,
    })
}

/// Charges the protocol fee from `a.authority` to `a.treasury` and bumps `chain_config`'s
/// `posted_batches` counter — called by both `post_root` and `post_root_proved` once every other check
/// has passed, so a call that would otherwise fail never touches the poster's balance.
///
/// The bps (variable) component only ever applies on the PROVED path, where `gas_in_batch` has already
/// been checked equal to the proof-bound header's own `gasUsed` (`post_root_proved`, before this is called)
/// — `gas_for_bps` is that same value there. `post_root` (the unproved, window-based path) has no such
/// binding available, so it passes `0`: `fee = base + bps * 0 / 10_000 == base`, the bps component silently
/// drops out rather than trusting the poster's unchecked claim.
fn charge_fee_and_count_post(
    program_id: &Pubkey,
    a: &PostRootAccounts,
    chain_id: u64,
    gas_for_bps: u64,
) -> ProgramResult {
    let mut cfg = governance::read_chain_config(program_id, chain_id, a.chain_config)?;
    let global = governance::read_global_config(program_id, a.global_config)?;
    let fee = governance::compute_fee(cfg.fee_base_lamports, cfg.fee_bps, gas_for_bps)?;
    governance::charge_fee(a.authority, a.treasury, a.sys, &global, fee)?;
    cfg.posted_batches = cfg.posted_batches.saturating_add(1);
    let mut d = a.chain_config.try_borrow_mut_data()?;
    chain_config::write(&mut d, &cfg);
    Ok(())
}

/// Predecessor's claimed `(last_block, state_root, last_block_hash)`: the root account's own genesis
/// fields when `head_pending_batch == 0` (no batch posted yet), otherwise the predecessor's own pending
/// PDA (continuity binds to the immediate predecessor batch, final or not — "so a dispute on
/// a descendant binds to that predecessor, not to the last final root"). The block hash is the one
/// `PostRootProved` binds a proof's parent hash to.
fn predecessor_state(
    program_id: &Pubkey,
    root: &root::RootFields,
    predecessor_acc: &AccountInfo,
) -> Result<(u64, [u8; 32], [u8; 32]), ProgramError> {
    if root.head_pending_batch == 0 {
        return Ok((root.number, root.state_root, root.block_hash));
    }
    let (expect, _) = pending_pda(program_id, root.chain_id, root.head_pending_batch);
    if expect != *predecessor_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }
    if predecessor_acc.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    let d = predecessor_acc.try_borrow_data()?;
    let f = pending::read(&d).map_err(|_| ProgramError::InvalidAccountData)?;
    Ok((f.last_block, f.state_root, f.last_block_hash))
}

/// `PostRootProved`'s layout-1 binding: every field the guest's public values commits
/// must agree with the poster's claimed `args` (and, for the two fields the guest cannot see directly,
/// with the inbox batch account's own `open_unix_ts` and the chain's `chain_config.max_drift_secs`) —
/// pure and account-free so every refusal is unit-testable directly, one field wrong at a time. Checked in
/// this exact order (cheapest structural fields first, the pairing-adjacent `gas_in_batch` last), matching
/// `header::parse_header`'s binding style on the layout-2 path.
fn bind_layout1_public_values(
    pv: &rome_zk_layouts::public_values::PublicValues,
    args: &PostRootArgs,
    inbox_open_unix_ts: i64,
    chain_config_max_drift_secs: Option<u64>,
) -> Result<(), SettleError> {
    if pv.chain_id != args.chain_id {
        return Err(SettleError::PublicValuesChainMismatch);
    }
    if pv.first_number != args.first_block {
        return Err(SettleError::BadFirstBlock);
    }
    if pv.last_number != args.last_block {
        return Err(SettleError::BadLastBlock);
    }
    if pv.parent_hash != args.parent_hash {
        return Err(SettleError::ParentHashMismatch);
    }
    if pv.last_block_hash != args.last_block_hash {
        return Err(SettleError::HeaderHashMismatch);
    }
    if pv.state_root != args.state_root {
        return Err(SettleError::StateRootMismatch);
    }
    if pv.inbox_commitment != args.inbox_commitment {
        return Err(SettleError::CommitmentMismatch);
    }
    if pv.forced_outcome_commitment != args.forced_outcome_commitment {
        return Err(SettleError::CommitmentMismatch);
    }
    // `inbox_open_unix_ts` is the batch account's own committed Solana clock reading; a
    // negative value is unconstructable at `OpenBatch`, but if one were ever seen, casting it
    // to `u64` here never matches a real (non-negative) guest-committed value, so it is refused by this
    // same name rather than needing a separate check.
    if pv.open_unix_ts != inbox_open_unix_ts as u64 {
        return Err(SettleError::OpenTsMismatch);
    }
    match chain_config_max_drift_secs {
        None => return Err(SettleError::DriftBoundUnset),
        Some(bound) if bound != pv.max_drift_secs => return Err(SettleError::DriftBoundMismatch),
        Some(_) => {}
    }
    if args.gas_in_batch != pv.gas_used {
        return Err(SettleError::GasInBatchMismatch);
    }
    Ok(())
}

/// The shared `PostRoot`/`PostRootProved` core: validates continuity against the
/// predecessor, the cap, and the inbox batch account, and returns the decoded root (for the caller to
/// mutate), the `posted_slot`/`deadline_slot` to write into the new pending PDA, and the inbox batch
/// account's own `open_unix_ts` (`PostRootProved`'s layout-1 path binds a proof's committed
/// `open_unix_ts` to this same value — exposed here rather than re-reading the inbox account a second
/// time) and the predecessor's last block hash (the value `PostRootProved` binds a
/// proof's parent hash to). Does not create or write any account — callers do that after this
/// returns `Ok`.
fn validate_post_root(
    program_id: &Pubkey,
    a: &PostRootAccounts,
    args: &PostRootArgs,
) -> Result<(root::RootFields, u64, u64, i64, [u8; 32]), ProgramError> {
    if !a.authority.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *a.sys.key != system_program::id() {
        return Err(ProgramError::InvalidAccountData);
    }
    let (expect_root, _) = chain_root_pda(program_id, args.chain_id);
    if expect_root != *a.root.key {
        return Err(ProgramError::InvalidSeeds);
    }
    if a.root.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    let root = {
        let d = a.root.try_borrow_data()?;
        root::read(&d).map_err(|_| ProgramError::InvalidAccountData)?
    };
    if root.authority != a.authority.key.to_bytes() {
        return Err(SettleError::NotChainAuthority.into());
    }

    let expected_batch = root.head_pending_batch + 1;
    if args.batch != expected_batch {
        return Err(SettleError::BadBatchSequence.into());
    }
    if args.prev_batch != root.head_pending_batch {
        return Err(SettleError::BadPrevBatch.into());
    }
    let (pred_last_block, pred_state_root, pred_block_hash) =
        predecessor_state(program_id, &root, a.predecessor)?;
    if args.pre_state_root != pred_state_root {
        return Err(SettleError::BadPreStateRoot.into());
    }
    if args.first_block != pred_last_block + 1 {
        return Err(SettleError::BadFirstBlock.into());
    }
    if root.pending_count >= root.max_pending {
        return Err(SettleError::MaxPendingReached.into());
    }

    // Registry: read the chain's registered inbox program id.
    let (expect_registry, _) = chain_registry_pda(program_id, args.chain_id);
    if expect_registry != *a.registry.key {
        return Err(ProgramError::InvalidSeeds);
    }
    if a.registry.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    let inbox_program = {
        let d = a.registry.try_borrow_data()?;
        let hdr = registry::read_header(&d).map_err(|_| ProgramError::InvalidAccountData)?;
        if hdr.chain_id != args.chain_id {
            return Err(ProgramError::InvalidAccountData);
        }
        Pubkey::new_from_array(hdr.inbox_program)
    };

    // Inbox batch account: owned by the registered inbox program, at the expected PDA, finalized, and
    // its `acc` equals the caller's claimed `inbox_commitment`.
    let expect_inbox = inbox_batch_pda(&inbox_program, program_id, args.chain_id, args.batch);
    if expect_inbox != *a.inbox_batch.key {
        return Err(SettleError::WrongInboxAccount.into());
    }
    if *a.inbox_batch.owner != inbox_program {
        return Err(SettleError::WrongInboxAccount.into());
    }
    let inbox_open_unix_ts = {
        let d = a.inbox_batch.try_borrow_data()?;
        let f = rome_zk_layouts::batch::read(&d).map_err(|_| SettleError::WrongInboxAccount)?;
        if f.chain_id != args.chain_id || f.batch != args.batch {
            return Err(SettleError::WrongInboxAccount.into());
        }
        // The address above already ties the batch to this program; the batch's own recorded settlement
        // program must agree (a second, independent statement of the same fact).
        if f.settlement_program != program_id.to_bytes() {
            return Err(SettleError::WrongInboxAccount.into());
        }
        if !f.finalized {
            return Err(SettleError::InboxNotFinalized.into());
        }
        if f.acc != args.inbox_commitment {
            return Err(SettleError::AccMismatch.into());
        }
        f.open_unix_ts
    };

    let posted_slot = Clock::get()?.slot;
    let deadline_slot = posted_slot.saturating_add(root.challenge_window_slots as u64);
    Ok((
        root,
        posted_slot,
        deadline_slot,
        inbox_open_unix_ts,
        pred_block_hash,
    ))
}

/// `pending_acc`'s address is public and predictable ((chain_id, batch)-derived), and in-order finality
/// (`finalize_batch` requires `batch == head_final_batch + 1`) means a batch id that can never be posted
/// halts settlement for the whole chain from that point on. `create_or_adopt_pda` adopts a pre-funded PDA
/// instead of leaving a bare `create_account` to fail `AccountAlreadyInUse` forever.
fn create_pending_account<'a>(
    program_id: &Pubkey,
    payer: &AccountInfo<'a>,
    pending_acc: &AccountInfo<'a>,
    sys: &AccountInfo<'a>,
    chain_id: u64,
    batch: u64,
) -> ProgramResult {
    let (expect, bump) = pending_pda(program_id, chain_id, batch);
    if expect != *pending_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }
    let seeds = pending_seeds(chain_id, batch);
    rome_zk_pda::create_or_adopt_pda(
        payer,
        pending_acc,
        sys,
        program_id,
        pending::PENDING_LEN,
        &[&seeds[0], &seeds[1], &seeds[2], &[bump]],
    )
}

fn write_pending_fields(
    d: &mut [u8],
    args: &PostRootArgs,
    posted_slot: u64,
    deadline_slot: u64,
    status: u8,
) {
    d[pending::OFF_BATCH..pending::OFF_BATCH + 8].copy_from_slice(&args.batch.to_le_bytes());
    d[pending::OFF_PREV_BATCH..pending::OFF_PREV_BATCH + 8]
        .copy_from_slice(&args.prev_batch.to_le_bytes());
    d[pending::OFF_PRE_STATE_ROOT..pending::OFF_PRE_STATE_ROOT + 32]
        .copy_from_slice(&args.pre_state_root);
    d[pending::OFF_FIRST_BLOCK..pending::OFF_FIRST_BLOCK + 8]
        .copy_from_slice(&args.first_block.to_le_bytes());
    d[pending::OFF_LAST_BLOCK..pending::OFF_LAST_BLOCK + 8]
        .copy_from_slice(&args.last_block.to_le_bytes());
    d[pending::OFF_STATE_ROOT..pending::OFF_STATE_ROOT + 32].copy_from_slice(&args.state_root);
    d[pending::OFF_BLOCK_ROOTS_MERKLE..pending::OFF_BLOCK_ROOTS_MERKLE + 32]
        .copy_from_slice(&args.block_roots_merkle);
    d[pending::OFF_INBOX_COMMITMENT..pending::OFF_INBOX_COMMITMENT + 32]
        .copy_from_slice(&args.inbox_commitment);
    d[pending::OFF_FORCED_OUTCOME_COMMITMENT..pending::OFF_FORCED_OUTCOME_COMMITMENT + 32]
        .copy_from_slice(&args.forced_outcome_commitment);
    d[pending::OFF_POSTED_SLOT..pending::OFF_POSTED_SLOT + 8]
        .copy_from_slice(&posted_slot.to_le_bytes());
    d[pending::OFF_STATUS] = status;
    d[pending::OFF_DISPUTES_OPEN..pending::OFF_DISPUTES_OPEN + 2]
        .copy_from_slice(&0u16.to_le_bytes());
    d[pending::OFF_DEADLINE_SLOT..pending::OFF_DEADLINE_SLOT + 8]
        .copy_from_slice(&deadline_slot.to_le_bytes());
    d[pending::OFF_PARENT_HASH..pending::OFF_PARENT_HASH + 32].copy_from_slice(&args.parent_hash);
    d[pending::OFF_LAST_BLOCK_HASH..pending::OFF_LAST_BLOCK_HASH + 32]
        .copy_from_slice(&args.last_block_hash);
}

/// Advances `head_pending_batch` to `batch` (every posted batch, `PostRoot` or `PostRootProved`, moves
/// this pointer). `pending_count` is the number of batches currently in `Pending` status
/// — it is incremented here only when `created_pending` is true (i.e. `PostRoot`'s window-based
/// path); `PostRootProved` creates its pending PDA already `Final` and must never count towards
/// `pending_count`, or an `always`-policy chain (which never has a truly pending batch) would exhaust
/// `max_pending` on bookkeeping alone. The corresponding decrement happens once, at the PENDING → FINAL
/// transition in `finalize_batch` — never in `ClosePending`, which only recycles rent for an account this
/// counter no longer tracks.
fn bump_root_pending_head(d: &mut [u8], batch: u64, created_pending: bool) {
    d[root::OFF_HEAD_PENDING_BATCH..root::OFF_HEAD_PENDING_BATCH + 8]
        .copy_from_slice(&batch.to_le_bytes());
    if created_pending {
        let count = u32::from_le_bytes(
            d[root::OFF_PENDING_COUNT..root::OFF_PENDING_COUNT + 4]
                .try_into()
                .unwrap(),
        );
        d[root::OFF_PENDING_COUNT..root::OFF_PENDING_COUNT + 4]
            .copy_from_slice(&(count + 1).to_le_bytes());
    }
}

/// Decrements `pending_count` by one (saturating) — called exactly once per batch, at its own
/// PENDING → FINAL transition in `finalize_batch`. Never called for a batch that was born `Final`
/// (`PostRootProved`, never counted) and never called again for the same batch (`ClosePending` no longer
/// touches this counter — see `bump_root_pending_head`'s doc).
fn decrement_root_pending_count(d: &mut [u8]) {
    let count = u32::from_le_bytes(
        d[root::OFF_PENDING_COUNT..root::OFF_PENDING_COUNT + 4]
            .try_into()
            .unwrap(),
    );
    d[root::OFF_PENDING_COUNT..root::OFF_PENDING_COUNT + 4]
        .copy_from_slice(&count.saturating_sub(1).to_le_bytes());
}

/// Advances the root account's finality head to `batch`, writing the full `{number, parent_hash,
/// block_hash, state_root}` tuple from that batch's own pending PDA — every pending PDA now carries a
/// real `parent_hash`/`last_block_hash` pair (`PostRootArgs` carries these fields),
/// so there is no more "the window path has no block hash to write" special case.
fn advance_root_final(
    d: &mut [u8],
    batch: u64,
    last_block: u64,
    state_root: [u8; 32],
    parent_hash: [u8; 32],
    block_hash: [u8; 32],
) {
    d[root::OFF_HEAD_FINAL_BATCH..root::OFF_HEAD_FINAL_BATCH + 8]
        .copy_from_slice(&batch.to_le_bytes());
    d[root::OFF_NUMBER..root::OFF_NUMBER + 8].copy_from_slice(&last_block.to_le_bytes());
    d[root::OFF_STATE_ROOT..root::OFF_STATE_ROOT + 32].copy_from_slice(&state_root);
    d[root::OFF_PARENT_HASH..root::OFF_PARENT_HASH + 32].copy_from_slice(&parent_hash);
    d[root::OFF_BLOCK_HASH..root::OFF_BLOCK_HASH + 32].copy_from_slice(&block_hash);
    let updates = u32::from_le_bytes(
        d[root::OFF_UPDATES..root::OFF_UPDATES + 4]
            .try_into()
            .unwrap(),
    );
    d[root::OFF_UPDATES..root::OFF_UPDATES + 4].copy_from_slice(&(updates + 1).to_le_bytes());
}

pub fn post_root(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    args: PostRootArgs,
) -> ProgramResult {
    // Proved-only: a permissionless chain (`chain_id >= 2^32`, the same predicate `InitChainV2` uses) may
    // only finalize a root a ZisK proof backs, against a verifier key Rome registered. This unproved,
    // challenge-window path is closed to it. First thing in the handler, ahead of every account read,
    // write and fee: a refusal touches nothing and costs next to nothing.
    if !rome_zk_layouts::chainid::is_reserved(args.chain_id) {
        return Err(SettleError::UnprovedRootNotAllowed.into());
    }
    let a = take_accounts(it)?;
    let (_root, posted_slot, deadline_slot, _inbox_open_unix_ts, _pred_block_hash) =
        validate_post_root(program_id, &a, &args)?;
    // The unproved path has no proof-bound value to check `gas_in_batch` against, so it
    // charges the base fee only — `0` here means the bps term drops out of `compute_fee` entirely.
    charge_fee_and_count_post(program_id, &a, args.chain_id, 0)?;

    create_pending_account(
        program_id,
        a.authority,
        a.pending,
        a.sys,
        args.chain_id,
        args.batch,
    )?;
    {
        let mut d = a.pending.try_borrow_mut_data()?;
        write_pending_fields(
            &mut d,
            &args,
            posted_slot,
            deadline_slot,
            pending::STATUS_PENDING,
        );
    }
    {
        let mut d = a.root.try_borrow_mut_data()?;
        bump_root_pending_head(&mut d, args.batch, true);
    }
    msg!(
        "chain {} batch {} posted (pending), deadline slot {}",
        args.chain_id,
        args.batch,
        deadline_slot
    );
    Ok(())
}

pub fn post_root_proved(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    args: PostRootArgs,
    proof_abi: Vec<u8>,
    header_rlp: Vec<u8>,
) -> ProgramResult {
    let a = take_accounts(it)?;
    let (_root, posted_slot, deadline_slot, inbox_open_unix_ts, pred_block_hash) =
        validate_post_root(program_id, &a, &args)?;

    // Cheap checks before the pairing (same discipline as the legacy path), in cheapest-first order: the
    // proof blob's length is checked, then the registry lookup and vkey binding (a handful of account-byte
    // compares) — only once the registry has actually vouched for this exact vkey do we pay for decoding
    // the layout's own public values, and the pairing itself is always last. The
    // `first_block == last_block` (single-block) restriction is layout-2-specific (it moved under that
    // branch) — layout 1's public values commit a whole batch range, so a multi- block batch is allowed
    // there.
    if proof_abi.len() != 768 + 32 + 32 + 512 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let program_vk: [u8; 32] = proof_abi[768..800]
        .try_into()
        .map_err(|_| ProgramError::InvalidInstructionData)?;

    // Registry: the (BN254, PLONK, vkey) triple must exist as one entry (matching
    // only (curve, scheme) let a proof under a *different* registered vkey satisfy this check whenever
    // two entries shared a curve and scheme; the vkey is part of the lookup key, not a value to
    // double-check after the fact).
    let entry = {
        let d = a.registry.try_borrow_data()?;
        registry::find(
            &d,
            registry::CURVE_BN254,
            registry::SCHEME_PLONK,
            &program_vk,
            Clock::get()?.slot,
        )
        .map_err(|_| ProgramError::InvalidAccountData)?
        .ok_or(SettleError::RegistryEntryNotFound)?
        .1
    };

    // `parent_hash`/`block_hash` to advance the root with once the pairing verifies — either the layout's
    // own decoded pair (layout 1) or the parsed header's (layout 2), so the shared tail below is
    // layout-agnostic.
    let (parent_hash, block_hash) = match entry.layout_id {
        registry::LAYOUT_ZISK_V1 => {
            // Layout 1: the guest's public values are the batch-level accumulator- and
            // drift-bound struct, packed into the ZisK ABI's 512-byte publics — no header to parse, so
            // `header_rlp` must be empty (no second decoder path for this layout).
            if !header_rlp.is_empty() {
                return Err(ProgramError::InvalidInstructionData);
            }
            // Both refusals carry their own name: the pairing itself fails with a bare
            // `InvalidInstructionData`, and a refusal log must tell the two apart.
            let pv_bytes = rome_zk_layouts::public_values::unpack_zisk_outputs(&proof_abi[832..])
                .map_err(|_| SettleError::BadPublicValuesPacking)?;
            let pv = rome_zk_layouts::public_values::read(&pv_bytes)
                .map_err(|_| SettleError::BadPublicValues)?;
            let cfg = governance::read_chain_config(program_id, args.chain_id, a.chain_config)?;
            bind_layout1_public_values(&pv, &args, inbox_open_unix_ts, cfg.max_drift_secs)?;
            (pv.parent_hash, pv.last_block_hash)
        }
        registry::LAYOUT_HEADER_FALLBACK => {
            // Single-block batches only: the header-only layout commits one block's header, unlike layout 1,
            // which commits a whole batch range. The restriction is specific to this layout, not to
            // `PostRootProved` in general.
            if args.first_block != args.last_block {
                return Err(SettleError::UnsupportedLayout.into());
            }
            // Now the header: parse, and bind every claim the poster made in `args` to what the guest
            // actually committed to (keccak(header) == the guest's committed hash) — a header that decodes
            // but doesn't match the proof's own public signal is exactly as invalid as one that doesn't
            // parse.
            let hdr = header::parse_header(&header_rlp)?;
            if hdr.number != args.first_block {
                return Err(SettleError::BadFirstBlock.into());
            }
            if hdr.state_root != args.state_root {
                return Err(SettleError::StateRootMismatch.into());
            }
            if hdr.parent_hash != args.parent_hash {
                return Err(SettleError::ParentHashMismatch.into());
            }
            // The bps fee component binds to the proof — `gasUsed` is the RLP field at index 10 (zero-based) of
            // the same header whose hash the guest committed to, so a poster cannot claim a `gas_in_batch` the
            // proof doesn't back.
            if args.gas_in_batch != hdr.gas_used {
                return Err(SettleError::GasInBatchMismatch.into());
            }
            let committed = header::zisk_committed_hash(&proof_abi[832..])?;
            let header_hash = solana_program::keccak::hash(&header_rlp).to_bytes();
            if committed != header_hash {
                return Err(SettleError::HeaderHashMismatch.into());
            }
            if header_hash != args.last_block_hash {
                return Err(SettleError::HeaderHashMismatch.into());
            }
            (hdr.parent_hash, header_hash)
        }
        _ => return Err(SettleError::UnsupportedLayout.into()),
    };

    // Continuity: the proof's own parent hash — decoded from the proof's public values
    // (layout 1) or from the header the proof commits to (layout 2), never a poster-supplied value that
    // was not itself checked against the proof above — must be the last block hash of the batch before
    // this one. Without it a valid proof of a chain that merely shares a state root could be posted on
    // top of any predecessor. Still ahead of the pairing, so a wrong predecessor costs next to nothing.
    if parent_hash != pred_block_hash {
        return Err(SettleError::PredecessorHashMismatch.into());
    }

    // The expensive part last.
    if !veritas::verify_zisk(&proof_abi)? {
        return Err(ProgramError::InvalidInstructionData);
    }

    // `args.gas_in_batch` is proof-bound as of the checks above (== hdr.gas_used, or == pv.gas_used) — the
    // bps term is safe to charge on the proved path.
    charge_fee_and_count_post(program_id, &a, args.chain_id, args.gas_in_batch)?;

    create_pending_account(
        program_id,
        a.authority,
        a.pending,
        a.sys,
        args.chain_id,
        args.batch,
    )?;
    {
        let mut d = a.pending.try_borrow_mut_data()?;
        write_pending_fields(
            &mut d,
            &args,
            posted_slot,
            deadline_slot,
            pending::STATUS_FINAL,
        );
    }
    let advances_head = {
        let d = a.root.try_borrow_data()?;
        let root = root::read(&d).map_err(|_| ProgramError::InvalidAccountData)?;
        args.batch == root.head_final_batch + 1
    };
    {
        let mut d = a.root.try_borrow_mut_data()?;
        // `created_pending = false`: this pending PDA is born `Final`, never `Pending` — it must never
        // count towards `pending_count`.
        bump_root_pending_head(&mut d, args.batch, false);
        if advances_head {
            advance_root_final(
                &mut d,
                args.batch,
                args.last_block,
                args.state_root,
                parent_hash,
                block_hash,
            );
        }
    }
    msg!(
        "chain {} batch {} posted+proved (final immediately), head_final advanced: {}",
        args.chain_id,
        args.batch,
        advances_head
    );
    Ok(())
}

/// Trailing accounts, beyond the mandatory [root, pending(batch)], that let one `FinalizeBatch` call
/// advance `head_final_batch` past batches already `Final` out of order — capped per call (without this,
/// a batch proved `Final` while its predecessor was still `Pending` could get stuck forever behind that
/// predecessor, since `finalize_batch` on the successor itself fails `NotPending`). The poster passes
/// `[pending(batch+1), pending(batch+2), …]`, read-only, in PDA order; the walk stops at the first
/// account that is not that exact next PDA or is not yet `Final` — it never errors, since these accounts
/// are an optional optimization, not a required argument.
const MAX_FINALITY_WALK: u8 = 8;

pub fn finalize_batch(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    chain_id: u64,
    batch: u64,
) -> ProgramResult {
    let root_acc = next_account_info(it)?;
    let pending_acc = next_account_info(it)?;

    let (expect_root, _) = chain_root_pda(program_id, chain_id);
    if expect_root != *root_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }
    if root_acc.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    let (expect_pending, _) = pending_pda(program_id, chain_id, batch);
    if expect_pending != *pending_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }
    if pending_acc.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }

    let root = {
        let d = root_acc.try_borrow_data()?;
        root::read(&d).map_err(|_| ProgramError::InvalidAccountData)?
    };
    if batch != root.head_final_batch + 1 {
        return Err(SettleError::OutOfOrderFinality.into());
    }
    // Liveness: the in-order batch may ALREADY be Final — a proved batch posted
    // out of order that no walk reached. Then this call is a pure head advance: no status
    // transition, no pending_count decrement (it was never counted). Only a Pending batch goes
    // through the window/dispute checks.
    let (already_final, last_block, state_root, parent_hash, last_block_hash) = {
        let d = pending_acc.try_borrow_data()?;
        let p = pending::read(&d).map_err(|_| ProgramError::InvalidAccountData)?;
        match p.status {
            pending::STATUS_FINAL => (
                true,
                p.last_block,
                p.state_root,
                p.parent_hash,
                p.last_block_hash,
            ),
            pending::STATUS_PENDING => {
                if p.disputes_open != 0 {
                    return Err(SettleError::DisputesOpen.into());
                }
                let now = Clock::get()?.slot;
                if now < p.deadline_slot {
                    return Err(SettleError::BeforeDeadline.into());
                }
                (
                    false,
                    p.last_block,
                    p.state_root,
                    p.parent_hash,
                    p.last_block_hash,
                )
            }
            _ => return Err(SettleError::NotPending.into()),
        }
    };
    if !already_final {
        let mut d = pending_acc.try_borrow_mut_data()?;
        d[pending::OFF_STATUS] = pending::STATUS_FINAL;
    }
    {
        let mut d = root_acc.try_borrow_mut_data()?;
        advance_root_final(
            &mut d,
            batch,
            last_block,
            state_root,
            parent_hash,
            last_block_hash,
        );
        // This is the one PENDING → FINAL transition this call performs directly — decrement once, here,
        // never in ClosePending. An already-final batch was never counted.
        if !already_final {
            decrement_root_pending_count(&mut d);
        }
    }
    msg!(
        "chain {} batch {} {}",
        chain_id,
        batch,
        if already_final {
            "head advanced over an already-final batch"
        } else {
            "finalized (window elapsed)"
        }
    );

    // Walk forward over any already-Final successors the caller passed as trailing accounts, advancing
    // head_final_batch past each in turn. Best-effort: stop silently at the first account that isn't
    // the expected next pending PDA, isn't owned by this program, or isn't Final yet — none of that is
    // an error, it just means there is nothing more to walk this call.
    let mut next_batch = batch;
    for _ in 0..MAX_FINALITY_WALK {
        let Some(next_acc) = it.next() else {
            break;
        };
        let candidate = next_batch + 1;
        let (expect_next_pending, _) = pending_pda(program_id, chain_id, candidate);
        if expect_next_pending != *next_acc.key || next_acc.owner != program_id {
            break;
        }
        let (n_last_block, n_state_root, n_parent_hash, n_last_block_hash) = {
            let d = next_acc
                .try_borrow_data()
                .map_err(|_| ProgramError::AccountBorrowFailed)?;
            let Ok(p) = pending::read(&d) else {
                break;
            };
            if p.status != pending::STATUS_FINAL {
                break;
            }
            (p.last_block, p.state_root, p.parent_hash, p.last_block_hash)
        };
        {
            let mut d = root_acc.try_borrow_mut_data()?;
            advance_root_final(
                &mut d,
                candidate,
                n_last_block,
                n_state_root,
                n_parent_hash,
                n_last_block_hash,
            );
        }
        msg!(
            "chain {} batch {} finality walk: advanced past already-final batch",
            chain_id,
            candidate
        );
        next_batch = candidate;
    }
    Ok(())
}

pub fn close_pending(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    chain_id: u64,
    batch: u64,
) -> ProgramResult {
    let authority = next_account_info(it)?;
    let root_acc = next_account_info(it)?;
    let pending_acc = next_account_info(it)?;

    if !authority.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    let (expect_root, _) = chain_root_pda(program_id, chain_id);
    if expect_root != *root_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }
    if root_acc.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    let (expect_pending, _) = pending_pda(program_id, chain_id, batch);
    if expect_pending != *pending_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }
    if pending_acc.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }

    let root = {
        let d = root_acc.try_borrow_data()?;
        root::read(&d).map_err(|_| ProgramError::InvalidAccountData)?
    };
    if root.authority != authority.key.to_bytes() {
        return Err(SettleError::NotChainAuthority.into());
    }
    if batch == root.head_pending_batch {
        return Err(SettleError::IsHeadPendingBatch.into());
    }
    {
        let d = pending_acc.try_borrow_data()?;
        let p = pending::read(&d).map_err(|_| ProgramError::InvalidAccountData)?;
        if p.status != pending::STATUS_FINAL {
            return Err(SettleError::NotFinal.into());
        }
    }

    let lamports = pending_acc.lamports();
    **pending_acc.try_borrow_mut_lamports()? = 0;
    **authority.try_borrow_mut_lamports()? += lamports;
    pending_acc.resize(0)?; // resize(len) replaces the old realloc(len, zero_init) (no-op for a shrink)
    pending_acc.assign(&system_program::id());

    // `pending_count` is not touched here: it counts batches currently in `Pending` status, and
    // `ClosePending` only ever recycles a batch that is already `Final` (checked above) — its PENDING
    // → FINAL transition, if it had one, already decremented this counter exactly once in
    // `finalize_batch`; a batch born `Final` via `PostRootProved` was never counted at all.
    msg!(
        "chain {} pending batch {} closed, {} lamports reclaimed",
        chain_id,
        batch,
        lamports
    );
    Ok(())
}

/// Return-data payload for `RootView` — the final root fields a CPI caller needs, borsh-serialized.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct RootViewData {
    pub chain_id: u64,
    pub number: u64,
    pub parent_hash: [u8; 32],
    pub state_root: [u8; 32],
    pub block_hash: [u8; 32],
}

/// The exact finality predicate `RootView` has always implemented, factored out so
/// `ProveExit` reads finality through the identical gate — one predicate, not two
/// independently-maintained copies. Given `root`'s own already-decoded fields and `pending_acc` (the
/// caller has already checked its PDA address matches `["pending", chain_id, batch]` — this function does
/// not re-derive or re-check that key, matching `root_view`'s own pre-existing call shape), returns the
/// FINAL `{number, parent_hash, state_root, block_hash}` tuple for `batch`: read directly off `root` when
/// `batch` is the current head, or off `batch`'s own pending PDA (checked `owner == program_id` only on
/// this branch, exactly as before this refactor) when it is an older, still-`Final` batch. `NotFinal`
/// otherwise — `batch` ahead of the head, the pending PDA not `Final`, or its own `batch` field
/// disagreeing (a recycled/reused PDA slot) — and always for a permissionless chain that has no proved
/// batch yet (`head_final_batch == 0`): its genesis was never proved.
pub fn final_root_tuple(
    program_id: &Pubkey,
    root: &root::RootFields,
    pending_acc: &AccountInfo,
    batch: u64,
) -> Result<RootViewData, ProgramError> {
    if root.head_final_batch < batch {
        return Err(SettleError::NotFinal.into());
    }
    // A permissionless chain is proved-only: its genesis (batch 0) is a root the chain
    // authority itself wrote at registration, never proved, so it is not final. The chain has no final
    // root until its first proved batch advances `head_final_batch`. Rome's reserved chains keep their
    // genesis as the final root.
    if root.head_final_batch == 0 && !rome_zk_layouts::chainid::is_reserved(root.chain_id) {
        return Err(SettleError::NotFinal.into());
    }
    if batch == root.head_final_batch {
        return Ok(RootViewData {
            chain_id: root.chain_id,
            number: root.number,
            parent_hash: root.parent_hash,
            state_root: root.state_root,
            block_hash: root.block_hash,
        });
    }
    if pending_acc.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    let d = pending_acc.try_borrow_data()?;
    let p = pending::read(&d).map_err(|_| ProgramError::InvalidAccountData)?;
    if p.status != pending::STATUS_FINAL || p.batch != batch {
        return Err(SettleError::NotFinal.into());
    }
    Ok(RootViewData {
        chain_id: root.chain_id,
        number: p.last_block,
        parent_hash: p.parent_hash,
        state_root: p.state_root,
        block_hash: p.last_block_hash,
    })
}

/// accounts: [root pda (read-only), pending pda for `batch` (read-only)]. The pending account is always
/// required — its seeds are checked regardless of branch — but its *contents* are only read when `batch`
/// is strictly behind `head_final_batch`: the root account's own fields hold only the current head's
/// tuple, which after a finality walk (`finalize_batch`) may belong to a later batch than the one
/// requested. A batch's own pending PDA — while it hasn't been recycled by `ClosePending` — is the exact
/// per-batch record; once recycled, `RootView` for that batch simply has nothing left to read
/// (`IncorrectProgramId`/`InvalidAccountData`), by design (losing history for reclaimed rent on non-head
/// batches).
pub fn root_view(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<AccountInfo>,
    chain_id: u64,
    batch: u64,
) -> ProgramResult {
    let root_acc = next_account_info(it)?;
    let pending_acc = next_account_info(it)?;
    let (expect_root, _) = chain_root_pda(program_id, chain_id);
    if expect_root != *root_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }
    if root_acc.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    let (expect_pending, _) = pending_pda(program_id, chain_id, batch);
    if expect_pending != *pending_acc.key {
        return Err(ProgramError::InvalidSeeds);
    }
    let root = {
        let d = root_acc.try_borrow_data()?;
        root::read(&d).map_err(|_| ProgramError::InvalidAccountData)?
    };
    let out = final_root_tuple(program_id, &root, pending_acc, batch)?;
    set_return_data(&borsh::to_vec(&out).expect("RootViewData always serializes"));
    Ok(())
}

/// Reserved: the challenge state machine is not implemented in this program. The
/// discriminant and `pending::STATUS_REJECTED` are reserved so a future implementation does not need a
/// wire-breaking change.
pub fn reject_batch(_chain_id: u64, _batch: u64) -> ProgramResult {
    Err(SettleError::NotImplemented.into())
}

/// `bind_layout1_public_values` unit tests: a matched, consistent
/// `(pv, args, inbox_open_unix_ts, chain_config_max_drift_secs)` passes; each test below then perturbs
/// exactly one field and asserts the specific named refusal — exhaustive over every check
/// `bind_layout1_public_values` makes, in the order it makes them.
#[cfg(test)]
mod bind_layout1_public_values_tests {
    use super::*;
    use rome_zk_layouts::public_values::PublicValues;

    fn matched_pv_and_args() -> (PublicValues, PostRootArgs, i64, Option<u64>) {
        let pv = PublicValues {
            chain_id: 200100,
            first_number: 11,
            last_number: 20,
            open_unix_ts: 1_700_000_000,
            max_drift_secs: 60,
            gas_used: 21_000_777,
            parent_hash: [0x11u8; 32],
            last_block_hash: [0x22u8; 32],
            state_root: [0x33u8; 32],
            inbox_commitment: [0x44u8; 32],
            forced_outcome_commitment: [0x55u8; 32],
        };
        let args = PostRootArgs {
            chain_id: pv.chain_id,
            batch: 5,
            prev_batch: 4,
            pre_state_root: [0u8; 32],
            first_block: pv.first_number,
            last_block: pv.last_number,
            state_root: pv.state_root,
            block_roots_merkle: [0u8; 32],
            inbox_commitment: pv.inbox_commitment,
            forced_outcome_commitment: pv.forced_outcome_commitment,
            parent_hash: pv.parent_hash,
            last_block_hash: pv.last_block_hash,
            gas_in_batch: pv.gas_used,
        };
        let ts = pv.open_unix_ts as i64;
        let drift = Some(pv.max_drift_secs);
        (pv, args, ts, drift)
    }

    #[test]
    fn matched_values_pass_every_binding() {
        let (pv, args, ts, drift) = matched_pv_and_args();
        assert!(bind_layout1_public_values(&pv, &args, ts, drift).is_ok());
    }

    #[test]
    fn chain_id_mismatch_is_named() {
        let (pv, mut args, ts, drift) = matched_pv_and_args();
        args.chain_id += 1;
        assert_eq!(
            bind_layout1_public_values(&pv, &args, ts, drift).unwrap_err(),
            SettleError::PublicValuesChainMismatch
        );
    }

    #[test]
    fn first_block_mismatch_is_named() {
        let (pv, mut args, ts, drift) = matched_pv_and_args();
        args.first_block += 1;
        assert_eq!(
            bind_layout1_public_values(&pv, &args, ts, drift).unwrap_err(),
            SettleError::BadFirstBlock
        );
    }

    #[test]
    fn last_block_mismatch_is_named() {
        let (pv, mut args, ts, drift) = matched_pv_and_args();
        args.last_block += 1;
        assert_eq!(
            bind_layout1_public_values(&pv, &args, ts, drift).unwrap_err(),
            SettleError::BadLastBlock
        );
    }

    #[test]
    fn parent_hash_mismatch_is_named() {
        let (pv, mut args, ts, drift) = matched_pv_and_args();
        args.parent_hash[0] ^= 0xff;
        assert_eq!(
            bind_layout1_public_values(&pv, &args, ts, drift).unwrap_err(),
            SettleError::ParentHashMismatch
        );
    }

    #[test]
    fn last_block_hash_mismatch_is_named() {
        let (pv, mut args, ts, drift) = matched_pv_and_args();
        args.last_block_hash[0] ^= 0xff;
        assert_eq!(
            bind_layout1_public_values(&pv, &args, ts, drift).unwrap_err(),
            SettleError::HeaderHashMismatch
        );
    }

    #[test]
    fn state_root_mismatch_is_named() {
        let (pv, mut args, ts, drift) = matched_pv_and_args();
        args.state_root[0] ^= 0xff;
        assert_eq!(
            bind_layout1_public_values(&pv, &args, ts, drift).unwrap_err(),
            SettleError::StateRootMismatch
        );
    }

    #[test]
    fn inbox_commitment_mismatch_is_named() {
        let (pv, mut args, ts, drift) = matched_pv_and_args();
        args.inbox_commitment[0] ^= 0xff;
        assert_eq!(
            bind_layout1_public_values(&pv, &args, ts, drift).unwrap_err(),
            SettleError::CommitmentMismatch
        );
    }

    #[test]
    fn forced_outcome_commitment_mismatch_is_named() {
        let (pv, mut args, ts, drift) = matched_pv_and_args();
        args.forced_outcome_commitment[0] ^= 0xff;
        assert_eq!(
            bind_layout1_public_values(&pv, &args, ts, drift).unwrap_err(),
            SettleError::CommitmentMismatch
        );
    }

    #[test]
    fn open_unix_ts_mismatch_is_named() {
        let (pv, args, ts, drift) = matched_pv_and_args();
        assert_eq!(
            bind_layout1_public_values(&pv, &args, ts + 1, drift).unwrap_err(),
            SettleError::OpenTsMismatch
        );
    }

    /// A negative inbox `open_unix_ts` is unconstructable at `OpenBatch`, but if one were
    /// ever seen here, it must be refused by the SAME name as any other mismatch — never treated as a
    /// wrap-around match.
    #[test]
    fn negative_open_unix_ts_never_matches_and_is_named_the_same() {
        let (pv, args, _ts, drift) = matched_pv_and_args();
        assert_eq!(
            bind_layout1_public_values(&pv, &args, -1, drift).unwrap_err(),
            SettleError::OpenTsMismatch
        );
    }

    #[test]
    fn drift_bound_unset_on_a_v1_chain_config() {
        let (pv, args, ts, _drift) = matched_pv_and_args();
        assert_eq!(
            bind_layout1_public_values(&pv, &args, ts, None).unwrap_err(),
            SettleError::DriftBoundUnset
        );
    }

    #[test]
    fn drift_bound_mismatch_on_a_v2_chain_config_with_a_different_value() {
        let (pv, args, ts, _drift) = matched_pv_and_args();
        assert_eq!(
            bind_layout1_public_values(&pv, &args, ts, Some(61)).unwrap_err(),
            SettleError::DriftBoundMismatch
        );
    }

    #[test]
    fn gas_in_batch_mismatch_is_named() {
        let (pv, mut args, ts, drift) = matched_pv_and_args();
        args.gas_in_batch += 1;
        assert_eq!(
            bind_layout1_public_values(&pv, &args, ts, drift).unwrap_err(),
            SettleError::GasInBatchMismatch
        );
    }

    /// Mutation: removing the `open_unix_ts` binding — i.e. calling with any `inbox_open_unix_ts`
    /// at all — must no longer distinguish a wrong timestamp from a right one. This test pins the CORRECT
    /// (post-fix) behaviour so that if the check is ever deleted, `open_unix_ts_mismatch_is_named` above
    /// goes red (a wrong timestamp would then wrongly pass).
    #[test]
    fn removing_the_open_unix_ts_binding_would_turn_a_wrong_timestamp_green() {
        let (pv, args, ts, drift) = matched_pv_and_args();
        // Sanity: the correct binding rejects a wrong timestamp (this is what would go green if the
        // check were removed — see `open_unix_ts_mismatch_is_named`, which is this same assertion).
        assert!(bind_layout1_public_values(&pv, &args, ts + 1, drift).is_err());
    }

    /// Mutation: same pin for `gas_in_batch`/`gas_used` — see `gas_in_batch_mismatch_is_named`.
    #[test]
    fn removing_the_gas_used_binding_would_turn_a_wrong_gas_value_green() {
        let (pv, mut args, ts, drift) = matched_pv_and_args();
        args.gas_in_batch += 1;
        assert!(bind_layout1_public_values(&pv, &args, ts, drift).is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank_root() -> Vec<u8> {
        vec![0u8; root::MIN_LEN]
    }

    fn pending_count(d: &[u8]) -> u32 {
        u32::from_le_bytes(
            d[root::OFF_PENDING_COUNT..root::OFF_PENDING_COUNT + 4]
                .try_into()
                .unwrap(),
        )
    }

    /// Isolated: a batch created `Pending` (`created_pending = true`, `PostRoot`)
    /// increments `pending_count`; a batch created `Final` (`created_pending = false`, `PostRootProved`)
    /// must not. Before this fix, `bump_root_pending_head` took no such flag and always incremented —
    /// this test fails against that shape (there would be nothing to pass `false` for).
    #[test]
    fn bump_root_pending_head_only_counts_a_batch_created_pending() {
        let mut d = blank_root();
        bump_root_pending_head(&mut d, 1, true);
        assert_eq!(pending_count(&d), 1, "PostRoot's batch must be counted");
        assert_eq!(
            u64::from_le_bytes(
                d[root::OFF_HEAD_PENDING_BATCH..root::OFF_HEAD_PENDING_BATCH + 8]
                    .try_into()
                    .unwrap()
            ),
            1
        );

        bump_root_pending_head(&mut d, 2, false);
        assert_eq!(
            pending_count(&d),
            1,
            "PostRootProved's batch is born Final — it must never be counted as pending"
        );
        assert_eq!(
            u64::from_le_bytes(
                d[root::OFF_HEAD_PENDING_BATCH..root::OFF_HEAD_PENDING_BATCH + 8]
                    .try_into()
                    .unwrap()
            ),
            2,
            "head_pending_batch still advances for every posted batch, counted or not"
        );
    }

    /// `decrement_root_pending_count` is the one place `pending_count` goes down — called once, at a
    /// batch's own PENDING -> FINAL transition — and it saturates at 0 rather than underflowing
    /// (defensive; the call sites never call it more than once per counted batch).
    #[test]
    fn decrement_root_pending_count_decrements_once_and_saturates() {
        let mut d = blank_root();
        bump_root_pending_head(&mut d, 1, true);
        bump_root_pending_head(&mut d, 2, true);
        assert_eq!(pending_count(&d), 2);
        decrement_root_pending_count(&mut d);
        assert_eq!(pending_count(&d), 1);
        decrement_root_pending_count(&mut d);
        assert_eq!(pending_count(&d), 0);
        decrement_root_pending_count(&mut d);
        assert_eq!(pending_count(&d), 0, "must saturate, not underflow");
    }

    /// PDA parity: this program's own `pending_seeds` and `inbox_batch_pda` must equal
    /// `rome_zk_layouts::{pending, batch}` — the single definitions every consumer (this program,
    /// `zk-settlement-client`) is required to share.
    #[test]
    fn pending_seeds_matches_rome_zk_layouts_pending_seeds() {
        assert_eq!(pending_seeds(7, 3), pending::seeds(7, 3));
    }

    #[test]
    fn inbox_batch_pda_matches_rome_zk_layouts_batch_pda() {
        let inbox_program = Pubkey::new_unique();
        let settlement_program = Pubkey::new_unique();
        assert_eq!(
            inbox_batch_pda(&inbox_program, &settlement_program, 7, 3),
            rome_zk_layouts::batch::pda(&inbox_program, &settlement_program, 7, 3).0
        );
    }
}
