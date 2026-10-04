//! The per-`(chain, batch)` accumulator: `OpenBatch` / `SealLeaf` / `FinalizeBatch` /
//! `CloseBatch` / `AbandonBatch`. A batch account holds one `leaf_hashes[idx] = keccak(chunk body)` per
//! chunk plus a presence bitmap, so `PostRoot` (zk-settlement, not this program) never has to touch the
//! chunk PDAs themselves — Solana's 64-account-per-transaction limit rules that out directly.
//!
//! ## Account layout (fixed, byte-exact — not borsh; see the module-level rationale in `lib.rs`)
//!
//! Defined once in `rome_zk_layouts::batch` (re-exported here as `MAGIC`/`VERSION`/`HEADER_LEN_V2`/the
//! `OFF_*` offsets/`bitmap_len`; the lengths and offsets that depend on the header version are
//! `header_len`/`leaves_offset_for`/`account_len_for`, which take the account's version byte) so this program and `zk-inbox-client`
//! read and write the identical bytes:
//! ```text
//! magic 'ZKBT' u32 | version u8 | chain_id u64 | batch u64 | open_slot u64 | expected_count u32
//! | leaves_present u32 | finalized u8 | settlement_program [32] | authority [32] | root [32]
//! | forced_root [32] | acc [32] | finalize_cursor u32 | open_unix_ts i64
//! | leaf_present_bitmap [ceil(expected_count/8)] | leaf_hashes [32 × expected_count]
//! ```
//! All integers little-endian. `leaf_present_bitmap` (not a zero-hash sentinel) is the source of truth
//! for "is leaf `idx` present" — a chunk whose keccak happens to be all-zero is astronomically unlikely
//! but the design must not lean on that. **Header v2:** `open_unix_ts` is the committed
//! `Clock::unix_timestamp` `OpenBatch` writes alongside `open_slot` (one `Clock::get()` call for both);
//! it is not part of `acc`. No migration — a v1 account is refused (`BadVersion`) by every reader.
//!
//! ## Merkle leaves and the forced lane
//! Each merkle leaf is `keccak(idx_le[4] ‖ chunk_hash[32])`, binding position into the leaf — see
//! [`rome_zk_merkle::indexed_leaf`]. This program's forced lane is always empty, so `forced_root` is the
//! fixed constant `rome_zk_layouts::forced_empty_root` = `keccak(b"rome-zk/forced/empty/v1")` (documented,
//! not computed from any forced-lane state — there isn't one yet).
//!
//! ## The `acc` formula, byte-exact
//! `acc = keccak(chain_id_le[8] ‖ batch_le[8] ‖ open_slot_le[8] ‖ expected_count_le[4] ‖ root[32] ‖
//! forced_root[32])`, computed by `rome_zk_layouts::acc` — the single definition both this program
//! (syscall keccak) and `zk-inbox-client` (software keccak, off-chain verification) call, so the two
//! sides can never drift apart on field order or endianness.
//!
//! ## Finalize is resumable over the leaf-transform pass, not the tree-combine pass
//! `FinalizeBatch { step }` advances `finalize_cursor` by at most `step` leaves per call (0 = all
//! remaining), turning `leaf_hashes[i]` from a bare chunk hash into the indexed leaf value, in place —
//! this is the O(n) pass the design's "resumable across transactions with a cursor above ~2,000 leaves"
//! refers to. Once every leaf is transformed (`finalize_cursor == expected_count`), the *same* call goes
//! on to combine the whole tree in one pass (`rome_zk_merkle::root_in_place`, in place over the same
//! buffer) and sets `finalized = 1` — the combine pass is O(n) keccaks total (not resumable further),
//! which the CU budget covers for the sizes this program targets (measured: 900 leaves in one call;
//! 2,500 leaves need ≥2 calls only because of the *transform* pass's own cost, not because combine
//! itself needs splitting).
//!
//! ## Authority model
//! - `OpenBatch`'s signer must be the chain's `authority`, read from the zk-settlement root PDA:
//!   otherwise anyone could pre-create a batch id with a wrong `expected_count` /
//!   `settlement_program` and lock the real batcher out of it forever. The signer becomes the batch's
//!   stored `authority`.
//! - Chunk `Open` requires the batch pda to already exist and its signer to be that same batch
//!   `authority`: otherwise anyone could pre-create a chunk PDA for someone else's batch id.
//! - `SealLeaf` is permissionless — deterministic given the chunk bytes already on chain, so there is
//!   nothing to gate. **`FinalizeBatch` requires the batch's `authority` signer:** with a
//!   bounded posting window a third party could otherwise finalize batch N+1 while the
//!   real poster's N is still open, stranding N's blocks — the batcher's startup sweep can no longer
//!   safely `AbandonBatch` N once that happens (it would skip past N's blocks forever). Gating the
//!   signer at the core makes that unconstructable rather than merely detected (the sweep's own
//!   `FinalizedAboveOpenBatch` refusal stays as defense-in-depth for any batch opened before this fix
//!   reached a chain).
//! - `Close` (a chunk) and `CloseBatch` need a **final** covering root: closing early would
//!   delete DA the settlement layer might still need to verify against.
//! - `AbandonBatch` needs the opposite guarantee — **no post ever happened** (`finalized == 0`) — and
//!   returns rent for a batch id that was opened but never used; a finalized batch can only leave via
//!   `CloseBatch`. Once a batch pda is gone (abandoned, or never created), its chunks may close for the
//!   chunk authority alone: there is no DA left to protect.

use solana_program::{
    account_info::{next_account_info, AccountInfo, MAX_PERMITTED_DATA_INCREASE},
    clock::Clock,
    entrypoint::ProgramResult,
    msg,
    program::invoke,
    program_error::ProgramError,
    pubkey::Pubkey,
    rent::Rent,
    sysvar::Sysvar,
};
// `system_instruction`/`system_program` moved out of `solana_program`'s root
// re-export in the Agave 4.x line (API fallout).
use solana_system_interface::{instruction as system_instruction, program as system_program};

pub use rome_zk_layouts::batch::{
    account_len_for, bitmap_len, header_len, leaves_offset_for, MAGIC, OFF_ACC, OFF_AUTHORITY,
    OFF_BATCH, OFF_CHAIN_ID, OFF_EXPECTED_COUNT, OFF_FINALIZED, OFF_FINALIZE_CURSOR,
    OFF_FORCED_ROOT, OFF_LEAVES_PRESENT, OFF_MAGIC, OFF_OPEN_SLOT, OFF_OPEN_UNIX_TS, OFF_ROOT,
    OFF_SETTLEMENT_PROGRAM, OFF_VERSION, VERSION,
};

/// `["batch", settlement_program, chain_id, batch]`. The single definition is
/// `rome_zk_layouts::batch::seeds`; kept under this name so every call site in this program stays short.
#[inline]
pub fn seeds(settlement_program: &Pubkey, chain_id: u64, batch: u64) -> [Vec<u8>; 4] {
    rome_zk_layouts::batch::seeds(&settlement_program.to_bytes(), chain_id, batch)
}

/// `["batch_cursor", settlement_program, chain_id]` — one per (settlement program, chain), inbox-owned. The
/// single definition is `rome_zk_layouts::cursor::seeds`.
#[inline]
pub fn cursor_seeds(settlement_program: &Pubkey, chain_id: u64) -> [Vec<u8>; 3] {
    rome_zk_layouts::cursor::seeds(&settlement_program.to_bytes(), chain_id)
}

/// Errors specific to the accumulator (mapped to `ProgramError::Custom`).
#[repr(u32)]
pub enum BatchError {
    IdxOutOfRange = 1,
    ChunkNotSealed = 2,
    LeafHashMismatch = 3,
    AlreadyFinalized = 4,
    NotAllLeavesSealed = 5,
    RootNotFinal = 6,
    NotFinalized = 7,
    WrongBatchAccount = 8,
    /// `OpenBatch`'s signer is not the root account's `authority`.
    NotChainAuthority = 9,
    /// Chunk `Open`'s signer is not the batch account's `authority`.
    NotBatchAuthority = 10,
    /// Chunk `Open`, `SealLeaf` or `FinalizeBatch` was attempted before the batch account reached
    /// `account_len(expected_count)` — `OpenBatch` alone only reaches that size when
    /// `account_len(expected_count) <= MAX_PERMITTED_DATA_INCREASE`; above that, `GrowBatch` must finish
    /// the job first.
    BatchNotGrown = 11,
    /// `OpenBatch`'s `batch` does not equal the chain's `batch_cursor.next_batch` — ids are sequential and
    /// never reused, so a stale or out-of-order id is rejected here at the core rather than left to the
    /// batcher's own resume scan.
    CursorMismatch = 12,
    /// `InitBatchCursor` was called for a chain whose cursor PDA already holds real cursor data (`owner ==
    /// program_id && data_len != 0`) — never inferred from `create_account`'s own error any more, since
    /// `create_or_adopt_pda` also succeeds against a merely-griefed (pre-funded, still system-owned) PDA,
    /// which is not "already initialised".
    CursorAlreadyInitialized = 13,
    /// `OpenBatch`'s `Clock::get()` returned a negative `unix_timestamp` — not a real
    /// reading any validator clock produces, but unconstructable-at-the-source is cheap here and turns
    /// an impossible value into a named refusal instead of a silently-negative anchor a downstream
    /// reader (`rome-zk-derive`) would otherwise have to make sense of.
    NegativeUnixTimestamp = 14,
}
impl From<BatchError> for ProgramError {
    fn from(e: BatchError) -> Self {
        ProgramError::Custom(e as u32)
    }
}

fn u32_at(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(d[o..o + 4].try_into().unwrap())
}
fn u64_at(d: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(d[o..o + 8].try_into().unwrap())
}
fn pubkey_at(d: &[u8], o: usize) -> Pubkey {
    Pubkey::new_from_array(d[o..o + 32].try_into().unwrap())
}

/// Validates the header magic/version (2 or 3) and returns `(version, chain_id, batch, expected_count)` — the 3 fields
/// `SealLeaf`/`FinalizeBatch` need on every call. Reads only those 3 fields directly by offset rather
/// than through `rome_zk_layouts::batch::read` (which also decodes 5 unused `[u8; 32]` fields): measured
/// at +23.5k CU on the 900-leaf `FinalizeBatch` path when this used the full decode instead (369,406 vs
/// 345,915 CU), so the hot path stays on the lightweight read; `open_chunk_check`,
/// `close_chunk_check` and `abandon_batch_inner` (each called at most once per instruction, never in a
/// per-leaf loop) use the full decode for the extra fields they need. Does not check the account's
/// owner — callers must do that themselves (they already have `program_id` in scope).
fn read_header(d: &[u8]) -> Result<(u8, u64, u64, u32), ProgramError> {
    if d.len() <= OFF_VERSION || u32_at(d, OFF_MAGIC) != MAGIC {
        return Err(ProgramError::InvalidAccountData);
    }
    // Accepts header v2 and v3 and returns the account's own version byte; every length and offset the
    // callers need comes from the version-taking forms in `rome_zk_layouts::batch`.
    let version = d[OFF_VERSION];
    let need = header_len(version).map_err(|_| ProgramError::InvalidAccountData)?;
    if d.len() < need {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok((
        version,
        u64_at(d, OFF_CHAIN_ID),
        u64_at(d, OFF_BATCH),
        u32_at(d, OFF_EXPECTED_COUNT),
    ))
}

/// Loads and validates a root account: owned by `settlement_program`, at the right `["root",
/// chain_id]` PDA (so an attacker cannot substitute an unrelated account merely owned by that program),
/// decodable per `rome_zk_layouts::root`, and with a matching `chain_id`. Shared by `OpenBatch`'s
/// authority check and `check_final_root`'s finality check. Only `rome_zk_layouts::root::MIN_LEN` bytes
/// are required — `zk-settlement`'s `InitChain` (`chain.rs`) creates the root account at
/// exactly that size.
fn load_root(
    root_pda: &AccountInfo,
    settlement_program: &Pubkey,
    chain_id: u64,
) -> Result<rome_zk_layouts::root::RootFields, ProgramError> {
    if root_pda.owner != settlement_program {
        return Err(ProgramError::IncorrectProgramId);
    }
    if *root_pda.key != rome_zk_layouts::root::pda(settlement_program, chain_id).0 {
        return Err(ProgramError::InvalidSeeds);
    }
    let d = root_pda.try_borrow_data()?;
    let fields = rome_zk_layouts::root::read(&d).map_err(|_| ProgramError::InvalidAccountData)?;
    if fields.chain_id != chain_id {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(fields)
}

/// Derives the `["batch_cursor", settlement_program, chain_id]` PDA and loads it, checking owner + address + `chain_id`
/// match (mirrors [`load_root`]'s shape). Does not check the cursor account exists in the sense of
/// having a helpful error for "never initialised" beyond the generic owner mismatch a system-owned
/// (never-created) account already produces — `InitBatchCursor` is the only way to create one.
fn load_cursor(
    program_id: &Pubkey,
    cursor_pda: &AccountInfo,
    settlement_program: &Pubkey,
    chain_id: u64,
) -> Result<rome_zk_layouts::cursor::CursorFields, ProgramError> {
    if cursor_pda.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    let sd = cursor_seeds(settlement_program, chain_id);
    let expected = Pubkey::find_program_address(&[&sd[0], &sd[1], &sd[2]], program_id).0;
    if expected != *cursor_pda.key {
        return Err(ProgramError::InvalidSeeds);
    }
    let d = cursor_pda.try_borrow_data()?;
    let f = rome_zk_layouts::cursor::read(&d).map_err(|_| ProgramError::InvalidAccountData)?;
    if f.chain_id != chain_id {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(f)
}

/// Checks that `root_pda` is the covering root (per [`load_root`]) and that its `head_final_batch >=
/// batch`.
fn check_final_root(
    root_pda: &AccountInfo,
    settlement_program: &Pubkey,
    chain_id: u64,
    batch: u64,
) -> ProgramResult {
    let fields = load_root(root_pda, settlement_program, chain_id)?;
    if fields.head_final_batch < batch {
        return Err(BatchError::RootNotFinal.into());
    }
    Ok(())
}

pub fn open_batch(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    chain_id: u64,
    batch: u64,
    expected_count: u32,
    settlement_program: &Pubkey,
) -> ProgramResult {
    open_batch_inner(
        program_id,
        &mut accounts.iter(),
        chain_id,
        batch,
        expected_count,
        settlement_program,
    )
}

pub fn seal_leaf(program_id: &Pubkey, accounts: &[AccountInfo], idx: u32) -> ProgramResult {
    seal_leaf_inner(program_id, &mut accounts.iter(), idx)
}

pub fn finalize_batch(program_id: &Pubkey, accounts: &[AccountInfo], step: u32) -> ProgramResult {
    finalize_batch_inner(program_id, &mut accounts.iter(), step)
}

pub fn close_batch(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    close_batch_inner(program_id, &mut accounts.iter())
}

pub fn grow_batch(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    chain_id: u64,
    batch: u64,
) -> ProgramResult {
    grow_batch_inner(program_id, &mut accounts.iter(), chain_id, batch)
}

pub fn init_batch_cursor(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    chain_id: u64,
    next_batch: u64,
    settlement_program: &Pubkey,
) -> ProgramResult {
    init_batch_cursor_inner(
        program_id,
        &mut accounts.iter(),
        chain_id,
        next_batch,
        settlement_program,
    )
}

fn open_batch_inner<'a, 'b: 'a>(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<'a, AccountInfo<'b>>,
    chain_id: u64,
    batch: u64,
    expected_count: u32,
    settlement_program: &Pubkey,
) -> ProgramResult {
    let payer = next_account_info(it)?;
    let pda = next_account_info(it)?;
    let root_pda = next_account_info(it)?;
    let cursor_pda = next_account_info(it)?;
    let sys = next_account_info(it)?;
    if !payer.is_signer || *sys.key != system_program::id() {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if expected_count == 0 {
        return Err(ProgramError::InvalidArgument);
    }
    // Only the chain's authority (from the settlement root account) may
    // open a batch id, or anyone could pre-create one with a wrong `expected_count`/
    // `settlement_program` and lock the real batcher out of it forever.
    let root = load_root(root_pda, settlement_program, chain_id)?;
    if root.authority != payer.key.to_bytes() {
        return Err(BatchError::NotChainAuthority.into());
    }
    // `batch` must be exactly the chain's next sequential id — never reused, never skippable — so a
    // stale chunk PDA from an abandoned attempt at some id can never be sealed into a *later* batch
    // opened at that same id (closed at the core).
    let cursor = load_cursor(program_id, cursor_pda, settlement_program, chain_id)?;
    if cursor.next_batch != batch {
        return Err(BatchError::CursorMismatch.into());
    }
    let sd = seeds(settlement_program, chain_id, batch);
    let (expect, bump) =
        Pubkey::find_program_address(&[&sd[0], &sd[1], &sd[2], &sd[3]], program_id);
    if expect != *pda.key {
        return Err(ProgramError::InvalidSeeds);
    }
    // v2: one `Clock::get()` call for both `open_slot` and `open_unix_ts` — the committed clock reading
    // `rome-zk-derive`'s one-sided drift bound anchors on. Not part of `acc` (the accumulator still
    // binds only the DA bytes, unchanged from v1). Read here, before any write this instruction makes
    // (the cursor bump below, the PDA create/adopt after it), so the negative-clock refusal right below
    // leaves no partial state: a negative `unix_timestamp` can never reach an account, on this chain or
    // any other, because every `OpenBatch` anywhere refuses it at this one source before it is ever
    // written down.
    let clock = Clock::get()?;
    if clock.unix_timestamp < 0 {
        return Err(BatchError::NegativeUnixTimestamp.into());
    }
    // A single `create_account` CPI can grow a *new* account by at most `MAX_PERMITTED_DATA_INCREASE`
    // bytes in this one top-level instruction, so `expected_count` above ~312 leaves cannot be created
    // at its full `account_len` here — `GrowBatch` (a separate top-level instruction, so it gets its
    // own fresh realloc allowance) finishes the job.
    let full_space =
        account_len_for(VERSION, expected_count).map_err(|_| ProgramError::InvalidAccountData)?;
    let space = full_space.min(MAX_PERMITTED_DATA_INCREASE);
    // `batch`'s address is public and predictable (`(chain_id, batch)`-derived), and it is exactly the next
    // id the cursor will ever accept — so anyone can pre-fund it with the rent-exempt minimum before this
    // transaction lands, and a bare `create_account` fails `AccountAlreadyInUse` forever after (the cursor
    // only advances inside a *successful* `OpenBatch`, and this id can never be revisited).
    // `create_or_adopt_pda` closes this at the core: a pre-funded PDA is adopted (topped up, allocated,
    // assigned) rather than left to permanently brick the chain.
    rome_zk_pda::create_or_adopt_pda(
        payer,
        pda,
        sys,
        program_id,
        space,
        &[&sd[0], &sd[1], &sd[2], &sd[3], &[bump]],
    )?;
    // Never decremented (not even by `AbandonBatch`): the next id this chain may ever open.
    let next_batch = batch
        .checked_add(1)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let mut cd = cursor_pda.try_borrow_mut_data()?;
    cd[rome_zk_layouts::cursor::OFF_NEXT_BATCH..rome_zk_layouts::cursor::OFF_NEXT_BATCH + 8]
        .copy_from_slice(&next_batch.to_le_bytes());
    drop(cd);
    let open_slot = clock.slot;
    let open_unix_ts = clock.unix_timestamp;
    let mut d = pda.try_borrow_mut_data()?;
    d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
    d[OFF_VERSION] = VERSION;
    d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].copy_from_slice(&chain_id.to_le_bytes());
    d[OFF_BATCH..OFF_BATCH + 8].copy_from_slice(&batch.to_le_bytes());
    d[OFF_OPEN_SLOT..OFF_OPEN_SLOT + 8].copy_from_slice(&open_slot.to_le_bytes());
    d[OFF_EXPECTED_COUNT..OFF_EXPECTED_COUNT + 4].copy_from_slice(&expected_count.to_le_bytes());
    // leaves_present, finalized, root, forced_root, acc, finalize_cursor, bitmap, leaf_hashes are all
    // zero already — `create_account`'s memory is zero-initialized by the runtime (same guarantee
    // `AccountInfo::realloc`'s doc comment relies on), so writing zeros here would waste CU for nothing.
    d[OFF_SETTLEMENT_PROGRAM..OFF_SETTLEMENT_PROGRAM + 32]
        .copy_from_slice(settlement_program.as_ref());
    d[OFF_AUTHORITY..OFF_AUTHORITY + 32].copy_from_slice(payer.key.as_ref());
    d[OFF_OPEN_UNIX_TS..OFF_OPEN_UNIX_TS + 8].copy_from_slice(&open_unix_ts.to_le_bytes());
    msg!(
        "batch {}/{} opened at slot {}, unix_ts {}, {} leaves expected",
        chain_id,
        batch,
        open_slot,
        open_unix_ts,
        expected_count
    );
    Ok(())
}

fn seal_leaf_inner<'a, 'b: 'a>(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<'a, AccountInfo<'b>>,
    idx: u32,
) -> ProgramResult {
    let batch_pda = next_account_info(it)?;
    let chunk_pda = next_account_info(it)?;
    if batch_pda.owner != program_id || chunk_pda.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    let mut d = batch_pda.try_borrow_mut_data()?;
    let (version, chain_id, batch, expected_count) = read_header(&d)?;
    if d.len()
        != account_len_for(version, expected_count).map_err(|_| ProgramError::InvalidAccountData)?
    {
        return Err(BatchError::BatchNotGrown.into());
    }
    if d[OFF_FINALIZED] != 0 {
        return Err(BatchError::AlreadyFinalized.into());
    }
    if idx >= expected_count {
        return Err(BatchError::IdxOutOfRange.into());
    }
    // The chunk lives under the SAME settlement program the batch was opened through (the one copy of it
    // is the batch account's own `settlement_program` field).
    let settlement_program = pubkey_at(&d, OFF_SETTLEMENT_PROGRAM);
    let expected_chunk = Pubkey::find_program_address(
        &crate::pda_seeds(&settlement_program, chain_id, batch, idx)
            .iter()
            .map(|v| v.as_slice())
            .collect::<Vec<_>>(),
        program_id,
    )
    .0;
    if expected_chunk != *chunk_pda.key {
        return Err(ProgramError::InvalidSeeds);
    }
    let hash = {
        let cd = chunk_pda.try_borrow_data()?;
        let (_authority, _chain_id, _batch, len, sealed) = crate::read_header(&cd)?;
        if !sealed {
            return Err(BatchError::ChunkNotSealed.into());
        }
        let body = &cd[crate::HEADER_LEN..crate::HEADER_LEN + len as usize];
        rome_zk_merkle::keccak256(&[body])
    };
    let lo =
        leaves_offset_for(version, expected_count).map_err(|_| ProgramError::InvalidAccountData)?;
    let bitmap_off = header_len(version).map_err(|_| ProgramError::InvalidAccountData)?;
    let present = (d[bitmap_off + (idx as usize) / 8] >> (idx % 8)) & 1 == 1;
    let slot = lo + 32 * idx as usize;
    if present {
        let existing: [u8; 32] = d[slot..slot + 32].try_into().unwrap();
        if existing != hash {
            return Err(BatchError::LeafHashMismatch.into());
        }
        return Ok(()); // same idx, same hash: no-op
    }
    d[slot..slot + 32].copy_from_slice(&hash);
    d[bitmap_off + (idx as usize) / 8] |= 1 << (idx % 8);
    let leaves_present = u32_at(&d, OFF_LEAVES_PRESENT) + 1;
    d[OFF_LEAVES_PRESENT..OFF_LEAVES_PRESENT + 4].copy_from_slice(&leaves_present.to_le_bytes());
    Ok(())
}

fn finalize_batch_inner<'a, 'b: 'a>(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<'a, AccountInfo<'b>>,
    step: u32,
) -> ProgramResult {
    let batch_pda = next_account_info(it)?;
    let authority = next_account_info(it)?;
    if batch_pda.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    let mut d = batch_pda.try_borrow_mut_data()?;
    let (version, chain_id, batch, expected_count) = read_header(&d)?;
    // The trailing signer must be this batch's own stored `authority` — same check shape
    // `close_batch_inner` uses. Read directly off the raw header bytes (already borrowed above) rather
    // than the full `rome_zk_layouts::batch::read` decode: this runs on every `FinalizeBatch` call,
    // including the per-step resumable path, so it stays on the same lightweight-offset-read discipline
    // `read_header` documents above (avoids the +23.5k CU the full decode costs on the 900-leaf path).
    if !authority.is_signer || *authority.key != pubkey_at(&d, OFF_AUTHORITY) {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if d.len()
        != account_len_for(version, expected_count).map_err(|_| ProgramError::InvalidAccountData)?
    {
        return Err(BatchError::BatchNotGrown.into());
    }
    if d[OFF_FINALIZED] != 0 {
        return Err(BatchError::AlreadyFinalized.into());
    }
    let leaves_present = u32_at(&d, OFF_LEAVES_PRESENT);
    if leaves_present != expected_count {
        return Err(BatchError::NotAllLeavesSealed.into());
    }
    let lo =
        leaves_offset_for(version, expected_count).map_err(|_| ProgramError::InvalidAccountData)?;
    let cursor = u32_at(&d, OFF_FINALIZE_CURSOR);
    let cap = if step == 0 { expected_count } else { step };
    let end = cursor.saturating_add(cap).min(expected_count);
    let h = rome_zk_merkle::keccak256;
    for i in cursor..end {
        let slot = lo + 32 * i as usize;
        let old: [u8; 32] = d[slot..slot + 32].try_into().unwrap();
        let leaf = rome_zk_merkle::indexed_leaf(&h, i, &old);
        d[slot..slot + 32].copy_from_slice(&leaf);
    }
    if end < expected_count {
        d[OFF_FINALIZE_CURSOR..OFF_FINALIZE_CURSOR + 4].copy_from_slice(&end.to_le_bytes());
        msg!(
            "finalize {}/{}: transformed {}..{}, not yet complete",
            chain_id,
            batch,
            cursor,
            end
        );
        return Ok(());
    }
    d[OFF_FINALIZE_CURSOR..OFF_FINALIZE_CURSOR + 4].copy_from_slice(&end.to_le_bytes());
    let n = expected_count as usize;
    let root = rome_zk_merkle::root_in_place(&h, &mut d[lo..lo + 32 * n], n);
    let forced_root = rome_zk_layouts::forced_empty_root(&h);
    let open_slot = u64_at(&d, OFF_OPEN_SLOT);
    let acc = rome_zk_layouts::acc(
        &h,
        chain_id,
        batch,
        open_slot,
        expected_count,
        &root,
        &forced_root,
    );
    d[OFF_ROOT..OFF_ROOT + 32].copy_from_slice(&root);
    d[OFF_FORCED_ROOT..OFF_FORCED_ROOT + 32].copy_from_slice(&forced_root);
    d[OFF_ACC..OFF_ACC + 32].copy_from_slice(&acc);
    d[OFF_FINALIZED] = 1;
    msg!(
        "batch {}/{} finalized, {} leaves, acc {:02x?}",
        chain_id,
        batch,
        expected_count,
        &acc[..8]
    );
    Ok(())
}

fn close_batch_inner<'a, 'b: 'a>(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<'a, AccountInfo<'b>>,
) -> ProgramResult {
    let authority = next_account_info(it)?;
    let batch_pda = next_account_info(it)?;
    let root_pda = next_account_info(it)?;
    let cursor_pda = next_account_info(it)?;
    if batch_pda.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    let (chain_id, batch, _expected_count) = {
        let d = batch_pda.try_borrow_data()?;
        let (version, chain_id, batch, expected_count) = read_header(&d)?;
        if !authority.is_signer || *authority.key != pubkey_at(&d, OFF_AUTHORITY) {
            return Err(ProgramError::MissingRequiredSignature);
        }
        if d[OFF_FINALIZED] == 0 {
            return Err(BatchError::NotFinalized.into());
        }
        let settlement_program = pubkey_at(&d, OFF_SETTLEMENT_PROGRAM);
        check_final_root(root_pda, &settlement_program, chain_id, batch)?;
        // Only a v3 batch carries a deposit range; a v2 batch credited nothing.
        let deposit_to = (version == rome_zk_layouts::batch::VERSION_V3)
            .then(|| u64_at(&d, rome_zk_layouts::batch::OFF_DEPOSIT_TO));
        // The cursor must be this chain's own (owner, PDA and chain id), whatever its version.
        let cursor = load_cursor(program_id, cursor_pda, &settlement_program, chain_id)?;
        if !cursor_pda.is_writable {
            return Err(ProgramError::InvalidArgument);
        }
        // `deposit_final` only moves up, and only on a v2 cursor: closing batch 3 and then batch 2 keeps
        // 3's value, and a v1 cursor has nothing to advance.
        if let (Some(to), Some(dep)) = (deposit_to, cursor.deposit) {
            if to > dep.final_ {
                let mut cd = cursor_pda.try_borrow_mut_data()?;
                cd[rome_zk_layouts::cursor::OFF_DEPOSIT_FINAL
                    ..rome_zk_layouts::cursor::OFF_DEPOSIT_FINAL + 8]
                    .copy_from_slice(&to.to_le_bytes());
            }
        }
        (chain_id, batch, expected_count)
    };
    let lamports = batch_pda.lamports();
    **batch_pda.try_borrow_mut_lamports()? = 0;
    **authority.try_borrow_mut_lamports()? += lamports;
    batch_pda.resize(0)?; // realloc(len, zero_init) -> resize(len), always zeroes growth (no-op for a shrink)
    batch_pda.assign(&system_program::id());
    msg!(
        "batch {}/{} closed, {} lamports reclaimed",
        chain_id,
        batch,
        lamports
    );
    Ok(())
}

/// Shared by `lib.rs`'s chunk-level `Close` when the batch account still exists — the batch must be
/// `finalized` **and** its covering root final (without the `finalized` check, a batch that was never
/// posted could be closed the moment *any* root happened to have `head_final_batch >= batch`, regardless of
/// this chain's own state). Driven from the batch account the chunk claims to belong to (chunk PDAs don't
/// store `settlement_program` themselves; the design keeps exactly one copy of it, in the batch account).
///
/// `settlement_program` is the owner of the root account the caller passed; the caller (`Close` in
/// `lib.rs`) has already tied the chunk and batch addresses to it. Here the batch's own recorded
/// `settlement_program` must be that same program, and the root must be that program's root for the chain.
pub fn close_chunk_check(
    program_id: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
    batch: u64,
    batch_pda: &AccountInfo,
    root_pda: &AccountInfo,
) -> ProgramResult {
    if batch_pda.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    let d = batch_pda.try_borrow_data()?;
    let f = rome_zk_layouts::batch::read(&d).map_err(|_| ProgramError::InvalidAccountData)?;
    if f.chain_id != chain_id || f.batch != batch {
        return Err(BatchError::WrongBatchAccount.into());
    }
    if f.settlement_program != settlement_program.to_bytes() {
        return Err(BatchError::WrongBatchAccount.into());
    }
    if !f.finalized {
        return Err(BatchError::NotFinalized.into());
    }
    check_final_root(root_pda, settlement_program, chain_id, batch)
}

/// Chunk `Open`'s binding check: the batch account must already exist, be owned by this program, match
/// the given `chain_id`/`batch`, not be finalized, and `idx` must be within `expected_count`; the signer
/// must be the batch's `authority`. Without this, a griefer could pre-create a chunk PDA for someone
/// else's batch id before the real sender writes to it. Returns the batch's `settlement_program` (the one
/// copy of it lives in the batch account), which the chunk's own PDA is keyed by.
pub fn open_chunk_check(
    program_id: &Pubkey,
    chain_id: u64,
    batch: u64,
    idx: u32,
    batch_pda: &AccountInfo,
    signer: &Pubkey,
) -> Result<Pubkey, ProgramError> {
    if batch_pda.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    let d = batch_pda.try_borrow_data()?;
    let f = rome_zk_layouts::batch::read(&d).map_err(|_| ProgramError::InvalidAccountData)?;
    if f.chain_id != chain_id || f.batch != batch {
        return Err(BatchError::WrongBatchAccount.into());
    }
    let settlement_program = Pubkey::new_from_array(f.settlement_program);
    let sd = seeds(&settlement_program, chain_id, batch);
    let expected = Pubkey::find_program_address(&[&sd[0], &sd[1], &sd[2], &sd[3]], program_id).0;
    if expected != *batch_pda.key {
        return Err(BatchError::WrongBatchAccount.into());
    }
    // A chunk cannot be opened under a batch that `OpenBatch` only created undersized and `GrowBatch`
    // has not yet finished growing — `SealLeaf`/`FinalizeBatch` require the same full size, so gating
    // it here too (chunk creation is the very first step of a chunk's life) fails closed as early as
    // possible.
    // `read` above accepted only v2 or v3, so the version byte names the account's own header.
    let version = d[OFF_VERSION];
    if d.len()
        != account_len_for(version, f.expected_count)
            .map_err(|_| ProgramError::InvalidAccountData)?
    {
        return Err(BatchError::BatchNotGrown.into());
    }
    if f.finalized {
        return Err(BatchError::AlreadyFinalized.into());
    }
    if idx >= f.expected_count {
        return Err(BatchError::IdxOutOfRange.into());
    }
    if f.authority != signer.to_bytes() {
        return Err(BatchError::NotBatchAuthority.into());
    }
    Ok(settlement_program)
}

pub fn abandon_batch(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    abandon_batch_inner(program_id, &mut accounts.iter())
}

/// `AbandonBatch`: authority-only, only while `finalized == 0` — a finalized batch can
/// only leave via `CloseBatch`, which requires the covering root to be final. Returns rent for a batch
/// id that was opened but never posted.
fn abandon_batch_inner<'a, 'b: 'a>(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<'a, AccountInfo<'b>>,
) -> ProgramResult {
    let authority = next_account_info(it)?;
    let batch_pda = next_account_info(it)?;
    if batch_pda.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    {
        let d = batch_pda.try_borrow_data()?;
        let f = rome_zk_layouts::batch::read(&d).map_err(|_| ProgramError::InvalidAccountData)?;
        if !authority.is_signer || authority.key.to_bytes() != f.authority {
            return Err(ProgramError::MissingRequiredSignature);
        }
        if f.finalized {
            return Err(BatchError::AlreadyFinalized.into());
        }
    }
    let lamports = batch_pda.lamports();
    **batch_pda.try_borrow_mut_lamports()? = 0;
    **authority.try_borrow_mut_lamports()? += lamports;
    batch_pda.resize(0)?; // realloc(len, zero_init) -> resize(len), always zeroes growth (no-op for a shrink)
    batch_pda.assign(&system_program::id());
    msg!("batch abandoned, {} lamports reclaimed", lamports);
    Ok(())
}

/// `GrowBatch`: permissionless — any `payer` may top up the rent for and realloc a batch account
/// `OpenBatch` created undersized. Idempotent: a no-op `Ok` once the account is already at
/// `account_len(expected_count)`; each call grows it by at most `MAX_PERMITTED_DATA_INCREASE` bytes,
/// capped so it can never exceed `account_len(expected_count)` (and therefore never needs to shrink).
fn grow_batch_inner<'a, 'b: 'a>(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<'a, AccountInfo<'b>>,
    chain_id: u64,
    batch: u64,
) -> ProgramResult {
    let payer = next_account_info(it)?;
    let pda = next_account_info(it)?;
    let sys = next_account_info(it)?;
    if !payer.is_signer || *sys.key != system_program::id() {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if pda.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    let (version, expected_count) = {
        let d = pda.try_borrow_data()?;
        let (version, h_chain_id, h_batch, expected_count) = read_header(&d)?;
        if h_chain_id != chain_id || h_batch != batch {
            return Err(BatchError::WrongBatchAccount.into());
        }
        // The batch is keyed by the settlement program it was opened through, which it records itself.
        let settlement_program = pubkey_at(&d, OFF_SETTLEMENT_PROGRAM);
        let sd = seeds(&settlement_program, chain_id, batch);
        let expected =
            Pubkey::find_program_address(&[&sd[0], &sd[1], &sd[2], &sd[3]], program_id).0;
        if expected != *pda.key {
            return Err(BatchError::WrongBatchAccount.into());
        }
        (version, expected_count)
    };
    let target =
        account_len_for(version, expected_count).map_err(|_| ProgramError::InvalidAccountData)?;
    let current = pda.data_len();
    if current >= target {
        // Already fully grown (including a batch small enough that `OpenBatch` reached `target` in one
        // shot) — no-op, never an error, so a caller need not track whether growth is already done.
        msg!(
            "batch {}/{} already at its full {} bytes, GrowBatch is a no-op",
            chain_id,
            batch,
            current
        );
        return Ok(());
    }
    let new_len = current
        .saturating_add(MAX_PERMITTED_DATA_INCREASE)
        .min(target);
    let rent_needed = Rent::get()?.minimum_balance(new_len);
    let have = pda.lamports();
    if rent_needed > have {
        invoke(
            &system_instruction::transfer(payer.key, pda.key, rent_needed - have),
            &[payer.clone(), pda.clone(), sys.clone()],
        )?;
    }
    pda.resize(new_len)?; // realloc(len, zero_init) -> resize(len), always zeroes growth (matches zero_init=true)
    msg!(
        "batch {}/{} grown to {} of {} bytes",
        chain_id,
        batch,
        new_len,
        target
    );
    Ok(())
}

/// `InitBatchCursor`: authority-gated (same check `OpenBatch` uses) bootstrap of the per-chain
/// `batch_cursor` PDA at `next_batch`. The "already initialised" guard is an explicit owner +
/// `data_len` check, never inferred from `create_account`'s own error — `create_or_adopt_pda` also
/// succeeds against a merely-griefed (pre-funded, still system-owned, zero-length) PDA, which is not
/// "already initialised" and must still bootstrap normally.
fn init_batch_cursor_inner<'a, 'b: 'a>(
    program_id: &Pubkey,
    it: &mut std::slice::Iter<'a, AccountInfo<'b>>,
    chain_id: u64,
    next_batch: u64,
    settlement_program: &Pubkey,
) -> ProgramResult {
    let payer = next_account_info(it)?;
    let cursor_pda = next_account_info(it)?;
    let root_pda = next_account_info(it)?;
    let sys = next_account_info(it)?;
    if !payer.is_signer || *sys.key != system_program::id() {
        return Err(ProgramError::MissingRequiredSignature);
    }
    let root = load_root(root_pda, settlement_program, chain_id)?;
    if root.authority != payer.key.to_bytes() {
        return Err(BatchError::NotChainAuthority.into());
    }
    let sd = cursor_seeds(settlement_program, chain_id);
    let (expect, bump) = Pubkey::find_program_address(&[&sd[0], &sd[1], &sd[2]], program_id);
    if expect != *cursor_pda.key {
        return Err(ProgramError::InvalidSeeds);
    }
    if cursor_pda.owner == program_id && cursor_pda.data_len() != 0 {
        return Err(BatchError::CursorAlreadyInitialized.into());
    }
    // The cursor is born as v2 (69 bytes): the deposit cursor starts at index 0, at the queue's seed hash
    // `h_0(settlement_program, chain_id)`, with nothing yet final.
    let space = rome_zk_layouts::cursor::LEN_V2;
    rome_zk_pda::create_or_adopt_pda(
        payer,
        cursor_pda,
        sys,
        program_id,
        space,
        &[&sd[0], &sd[1], &sd[2], &[bump]],
    )?;
    let h = rome_zk_merkle::keccak256;
    let bytes = rome_zk_layouts::cursor::write_v2(&rome_zk_layouts::cursor::CursorFields {
        chain_id,
        next_batch,
        deposit: Some(rome_zk_layouts::cursor::CursorDeposit {
            next: 0,
            hash: rome_zk_layouts::deposit::queue_seed_hash(
                &h,
                &settlement_program.to_bytes(),
                chain_id,
            ),
            final_: 0,
        }),
    })
    .ok_or(ProgramError::InvalidAccountData)?;
    cursor_pda.try_borrow_mut_data()?.copy_from_slice(&bytes);
    msg!(
        "batch cursor initialised for chain {}, next_batch = {}",
        chain_id,
        next_batch
    );
    Ok(())
}

#[cfg(test)]
mod pda_parity_tests {
    use super::*;

    /// PDA parity: this program's own `seeds`/`cursor_seeds` must equal
    /// `rome_zk_layouts::{batch, cursor}::seeds` — the single definitions every consumer shares.
    #[test]
    fn batch_seeds_matches_rome_zk_layouts_batch_seeds() {
        let sp = Pubkey::new_from_array([0x5Au8; 32]);
        assert_eq!(
            seeds(&sp, 7, 3),
            rome_zk_layouts::batch::seeds(&[0x5Au8; 32], 7, 3)
        );
    }

    #[test]
    fn cursor_seeds_matches_rome_zk_layouts_cursor_seeds() {
        let sp = Pubkey::new_from_array([0x5Au8; 32]);
        assert_eq!(
            cursor_seeds(&sp, 7),
            rome_zk_layouts::cursor::seeds(&[0x5Au8; 32], 7)
        );
    }
}
