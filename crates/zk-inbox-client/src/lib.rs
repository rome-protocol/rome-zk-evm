//! Thin client for `programs/zk-inbox`: instruction builders, PDA derivation, account decoding, and an
//! off-chain reference implementation of the accumulator's commitment. No async runtime opinions — this
//! crate only builds `Instruction`s and decodes bytes; sending transactions is the caller's job (the
//! batcher's own async stack, or `examples/devnet_driver.rs` behind the `devnet-driver` feature here).

use solana_program::{instruction::AccountMeta, pubkey::Pubkey};
// `system_program` moved out of `solana_program`'s root re-export in the Agave 4.x line (API fallout).
use solana_system_interface::program as system_program;
use zk_inbox::batch;

pub use zk_inbox::{InboxIx, HEADER_LEN as CHUNK_HEADER_LEN, MAGIC as CHUNK_MAGIC};

/// Decodes a raw top-level instruction's data back into an [`InboxIx`] — the inverse of every `*_ix` builder above
/// (instruction decoding lives here, next to the builders, never re-implemented by a consumer). Used by the
/// settlement watcher and any other reader of on-chain transaction history; never by anything that only *builds*
/// transactions.
pub fn decode_instruction(data: &[u8]) -> Result<InboxIx, std::io::Error> {
    borsh::from_slice(data)
}

/// The account index carrying the **chunk** PDA in every `*_ix` builder's own `AccountMeta` list above — `None` for
/// an instruction that never touches a chunk account. A consumer reading transaction history (the settlement
/// watcher) uses this instead of re-deriving or hard-coding these positions itself; `tests::account_index_tests`
/// pins every value against the real builder it describes, so a builder's account order changing without this
/// function changing is caught here, not downstream.
pub fn chunk_account_index(ix: &InboxIx) -> Option<usize> {
    match ix {
        InboxIx::Open { .. } => Some(1), // open_chunk_ix: [payer, chunk, batch_acct, system_program]
        InboxIx::Write { .. } => Some(1), // write_chunk_ix: [authority, chunk]
        InboxIx::Seal { .. } => Some(1), // seal_chunk_ix: [authority, chunk]
        InboxIx::Close => Some(1),       // close_chunk_ix: [authority, chunk, batch_acct, root]
        InboxIx::SealLeaf { .. } => Some(1), // seal_leaf_ix: [batch_acct, chunk]
        _ => None,
    }
}

/// The account index carrying the **batch** PDA — `None` for an instruction that never touches a batch
/// account. Same reuse contract as [`chunk_account_index`].
pub fn batch_account_index(ix: &InboxIx) -> Option<usize> {
    match ix {
        InboxIx::Open { .. } => Some(2), // open_chunk_ix: [payer, chunk, batch_acct, system_program]
        InboxIx::Close => Some(2),       // close_chunk_ix: [authority, chunk, batch_acct, root]
        InboxIx::OpenBatch { .. } => Some(1), // open_batch_ix: [payer, batch_pda, root, cursor, system_program]
        InboxIx::GrowBatch { .. } => Some(1), // grow_batch_ix: [payer, batch_pda, system_program]
        InboxIx::AbandonBatch => Some(1),     // abandon_batch_ix: [authority, batch_pda]
        InboxIx::CloseBatch => Some(1), // close_batch_ix: [authority, batch_pda, root, cursor]
        InboxIx::SealLeaf { .. } => Some(0), // seal_leaf_ix: [batch_acct, chunk]
        InboxIx::FinalizeBatch { .. } => Some(0), // finalize_batch_ix: [batch_acct, authority]
        _ => None,
    }
}

/// `["inbox", settlement_program, chain_id, batch, idx]` — the single definition is
/// `rome_zk_layouts::chunk::pda`.
pub fn chunk_pda(
    program_id: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
    batch: u64,
    idx: u32,
) -> (Pubkey, u8) {
    rome_zk_layouts::chunk::pda(program_id, settlement_program, chain_id, batch, idx)
}

/// `["batch", settlement_program, chain_id, batch]` — the single definition is
/// `rome_zk_layouts::batch::pda`.
pub fn batch_pda(
    program_id: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
    batch: u64,
) -> (Pubkey, u8) {
    rome_zk_layouts::batch::pda(program_id, settlement_program, chain_id, batch)
}

/// `["root", chain_id]` under `settlement_program` (zk-settlement's derivation) — the single definition is
/// `rome_zk_layouts::root::pda`.
pub fn root_pda(settlement_program: &Pubkey, chain_id: u64) -> (Pubkey, u8) {
    rome_zk_layouts::root::pda(settlement_program, chain_id)
}

/// `["batch_cursor", settlement_program, chain_id]` — the single definition is
/// `rome_zk_layouts::cursor::pda`.
pub fn cursor_pda(program_id: &Pubkey, settlement_program: &Pubkey, chain_id: u64) -> (Pubkey, u8) {
    rome_zk_layouts::cursor::pda(program_id, settlement_program, chain_id)
}

/// The runtime's per-top-level-instruction realloc allowance for a new/growing account
/// (`solana_program::account_info::MAX_PERMITTED_DATA_INCREASE`) — re-exported here so callers building
/// an `OpenBatch` + `GrowBatch` plan don't need their own `solana-program` import just for this constant.
pub const MAX_PERMITTED_DATA_INCREASE: usize =
    solana_program::account_info::MAX_PERMITTED_DATA_INCREASE;

fn ix(
    program_id: &Pubkey,
    accounts: Vec<AccountMeta>,
    data: InboxIx,
) -> solana_program::instruction::Instruction {
    solana_program::instruction::Instruction {
        program_id: *program_id,
        accounts,
        data: borsh::to_vec(&data).expect("InboxIx always serializes"),
    }
}

/// Requires the batch to already exist (`OpenBatch`) and `payer` to be its `authority`.
pub fn open_chunk_ix(
    program_id: &Pubkey,
    payer: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
    batch: u64,
    idx: u32,
    size: u32,
) -> solana_program::instruction::Instruction {
    let (pda, _) = chunk_pda(program_id, settlement_program, chain_id, batch, idx);
    let (batch_acct, _) = batch_pda(program_id, settlement_program, chain_id, batch);
    ix(
        program_id,
        vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(pda, false),
            AccountMeta::new_readonly(batch_acct, false),
            AccountMeta::new_readonly(system_program::id(), false),
        ],
        InboxIx::Open {
            chain_id,
            batch,
            idx,
            size,
        },
    )
}

#[allow(clippy::too_many_arguments)]
pub fn write_chunk_ix(
    program_id: &Pubkey,
    authority: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
    batch: u64,
    idx: u32,
    offset: u32,
    data: Vec<u8>,
) -> solana_program::instruction::Instruction {
    let (pda, _) = chunk_pda(program_id, settlement_program, chain_id, batch, idx);
    ix(
        program_id,
        vec![
            AccountMeta::new_readonly(*authority, true),
            AccountMeta::new(pda, false),
        ],
        InboxIx::Write { offset, data },
    )
}

/// `body_hash` must be `keccak256(body[..len])` for the exact bytes the caller wrote — compute it with
/// [`chunk_body_hash`] from the same buffer passed to `write_chunk_ix`. The program recomputes this hash
/// from the account's own bytes and rejects the seal if it doesn't match.
#[allow(clippy::too_many_arguments)]
pub fn seal_chunk_ix(
    program_id: &Pubkey,
    authority: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
    batch: u64,
    idx: u32,
    len: u32,
    body_hash: [u8; 32],
) -> solana_program::instruction::Instruction {
    let (pda, _) = chunk_pda(program_id, settlement_program, chain_id, batch, idx);
    ix(
        program_id,
        vec![
            AccountMeta::new_readonly(*authority, true),
            AccountMeta::new(pda, false),
        ],
        InboxIx::Seal { len, body_hash },
    )
}

/// `keccak256` of a chunk body — what `Seal`'s `body_hash` must equal. Builders should call this on
/// exactly the bytes they intend to (and did) `Write`, never on a source buffer they only partially wrote
/// (that is precisely the short-seal case the program refuses at the core).
pub fn chunk_body_hash(body: &[u8]) -> [u8; 32] {
    solana_program::keccak::hashv(&[body]).to_bytes()
}

/// Closes a chunk PDA. Requires the covering root to be final: the batch account is read (for `settlement_program`)
/// and the settlement root PDA is checked for `head_final_batch >= batch`.
pub fn close_chunk_ix(
    program_id: &Pubkey,
    authority: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
    batch: u64,
    idx: u32,
) -> solana_program::instruction::Instruction {
    let (chunk, _) = chunk_pda(program_id, settlement_program, chain_id, batch, idx);
    let (batch_acct, _) = batch_pda(program_id, settlement_program, chain_id, batch);
    let (root, _) = root_pda(settlement_program, chain_id);
    ix(
        program_id,
        vec![
            AccountMeta::new(*authority, true),
            AccountMeta::new(chunk, false),
            AccountMeta::new_readonly(batch_acct, false),
            AccountMeta::new_readonly(root, false),
        ],
        InboxIx::Close,
    )
}

/// Requires `payer` to be the chain's `authority`, read from the settlement root PDA, and `batch` to equal the
/// chain's `batch_cursor.next_batch` — which this instruction then increments. Creates the batch account at
/// `min(account_len_for(version, expected_count), MAX_PERMITTED_DATA_INCREASE)` bytes; above that ceiling, follow with
/// [`grow_batch_ix`] (or use [`open_and_grow_batch_ixs`] to build the whole plan at once).
pub fn open_batch_ix(
    program_id: &Pubkey,
    payer: &Pubkey,
    chain_id: u64,
    batch: u64,
    expected_count: u32,
    settlement_program: &Pubkey,
) -> solana_program::instruction::Instruction {
    let (pda, _) = batch_pda(program_id, settlement_program, chain_id, batch);
    let (root, _) = root_pda(settlement_program, chain_id);
    let (cursor, _) = cursor_pda(program_id, settlement_program, chain_id);
    ix(
        program_id,
        vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(pda, false),
            AccountMeta::new_readonly(root, false),
            AccountMeta::new(cursor, false),
            AccountMeta::new_readonly(system_program::id(), false),
        ],
        InboxIx::OpenBatch {
            chain_id,
            batch,
            expected_count,
            settlement_program: *settlement_program,
        },
    )
}

/// Permissionless: grows a batch account `OpenBatch` created undersized, by up to `MAX_PERMITTED_DATA_INCREASE`
/// bytes toward `account_len(expected_count)`. Idempotent — a call once the account is already fully grown is a
/// no-op `Ok`.
pub fn grow_batch_ix(
    program_id: &Pubkey,
    payer: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
    batch: u64,
) -> solana_program::instruction::Instruction {
    let (pda, _) = batch_pda(program_id, settlement_program, chain_id, batch);
    ix(
        program_id,
        vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(pda, false),
            AccountMeta::new_readonly(system_program::id(), false),
        ],
        InboxIx::GrowBatch { chain_id, batch },
    )
}

/// Builds `OpenBatch` followed by however many [`grow_batch_ix`] calls are needed to reach
/// `account_len(expected_count)` from `OpenBatch`'s own capped initial size, all meant to be sent as one
/// transaction (each `GrowBatch` is its own top-level instruction, so it gets its own fresh
/// `MAX_PERMITTED_DATA_INCREASE` realloc allowance within that single transaction — no separate confirmed round
/// trip is needed between growth steps).
pub fn open_and_grow_batch_ixs(
    program_id: &Pubkey,
    payer: &Pubkey,
    chain_id: u64,
    batch: u64,
    expected_count: u32,
    settlement_program: &Pubkey,
) -> Vec<solana_program::instruction::Instruction> {
    // `OpenBatch` writes header v2 (`batch::VERSION`), so the plan is for a v2 account.
    let target = batch::account_len_for(batch::VERSION, expected_count)
        .expect("the version OpenBatch writes has a known length");
    let mut current = target.min(MAX_PERMITTED_DATA_INCREASE);
    let mut ixs = vec![open_batch_ix(
        program_id,
        payer,
        chain_id,
        batch,
        expected_count,
        settlement_program,
    )];
    while current < target {
        ixs.push(grow_batch_ix(
            program_id,
            payer,
            settlement_program,
            chain_id,
            batch,
        ));
        current = current
            .saturating_add(MAX_PERMITTED_DATA_INCREASE)
            .min(target);
    }
    ixs
}

/// Authority-gated (same check `OpenBatch` uses) bootstrap of the per-chain `batch_cursor` PDA at
/// `next_batch` — fails if the cursor already exists. Run once per chain;
/// on a chain with prior batch history it must be initialised at `root.head_pending_batch + 1` — the id
/// settlement will accept next — and never above it (a cursor above that id halts the chain: settlement's
/// `PostRoot` wants exactly `head_pending_batch + 1`). `examples/find_max_batch_id.rs` reads the root and
/// proposes that value (see [`cursor_proposal`]).
pub fn init_batch_cursor_ix(
    program_id: &Pubkey,
    payer: &Pubkey,
    chain_id: u64,
    next_batch: u64,
    settlement_program: &Pubkey,
) -> solana_program::instruction::Instruction {
    let (cursor, _) = cursor_pda(program_id, settlement_program, chain_id);
    let (root, _) = root_pda(settlement_program, chain_id);
    ix(
        program_id,
        vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(cursor, false),
            AccountMeta::new_readonly(root, false),
            AccountMeta::new_readonly(system_program::id(), false),
        ],
        InboxIx::InitBatchCursor {
            chain_id,
            next_batch,
            settlement_program: *settlement_program,
        },
    )
}

/// Permissionless: the leaf hash is recomputed on-chain from the chunk's own sealed bytes.
pub fn seal_leaf_ix(
    program_id: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
    batch: u64,
    idx: u32,
) -> solana_program::instruction::Instruction {
    let (batch_acct, _) = batch_pda(program_id, settlement_program, chain_id, batch);
    let (chunk, _) = chunk_pda(program_id, settlement_program, chain_id, batch, idx);
    ix(
        program_id,
        vec![
            AccountMeta::new(batch_acct, false),
            AccountMeta::new_readonly(chunk, false),
        ],
        InboxIx::SealLeaf { idx },
    )
}

/// Authority-gated: `authority` must be the batch's stored `authority` (the value `OpenBatch` wrote) and must sign
/// — otherwise a third party could finalize a later batch id ahead of the real poster's own, stranding it.
/// `step = 0` means "transform every remaining leaf, then combine, in this call" — use a smaller `step` to spread
/// the transform pass across several transactions for large batches; every step call needs the signer.
pub fn finalize_batch_ix(
    program_id: &Pubkey,
    authority: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
    batch: u64,
    step: u32,
) -> solana_program::instruction::Instruction {
    let (batch_acct, _) = batch_pda(program_id, settlement_program, chain_id, batch);
    ix(
        program_id,
        vec![
            AccountMeta::new(batch_acct, false),
            AccountMeta::new_readonly(*authority, true),
        ],
        InboxIx::FinalizeBatch { step },
    )
}

pub fn close_batch_ix(
    program_id: &Pubkey,
    authority: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
    batch: u64,
) -> solana_program::instruction::Instruction {
    let (batch_acct, _) = batch_pda(program_id, settlement_program, chain_id, batch);
    let (root, _) = root_pda(settlement_program, chain_id);
    let (cursor, _) = cursor_pda(program_id, settlement_program, chain_id);
    ix(
        program_id,
        vec![
            AccountMeta::new(*authority, true),
            AccountMeta::new(batch_acct, false),
            AccountMeta::new_readonly(root, false),
            AccountMeta::new(cursor, false),
        ],
        InboxIx::CloseBatch,
    )
}

/// Authority-only; only while the batch is not yet finalized. Returns rent for a batch id that was opened but never
/// posted.
pub fn abandon_batch_ix(
    program_id: &Pubkey,
    authority: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
    batch: u64,
) -> solana_program::instruction::Instruction {
    let (batch_acct, _) = batch_pda(program_id, settlement_program, chain_id, batch);
    ix(
        program_id,
        vec![
            AccountMeta::new(*authority, true),
            AccountMeta::new(batch_acct, false),
        ],
        InboxIx::AbandonBatch,
    )
}

/// Decoded view of one chunk account's 64-byte header (`magic u32 | authority [u8;32] |
/// chain_id u64 | batch u64 | idx u32 | len u32 | sealed u8 | pad`) — the single off-chain place this
/// shape is parsed, next to `programs/zk-inbox/src/lib.rs`'s own `read_header` (the on-chain owner of
/// these offsets; this crate already normally depends on that program crate and re-exports its two plain
/// constants above — [`CHUNK_HEADER_LEN`]/[`CHUNK_MAGIC`] — this adds the rest of the header fields the
/// same way, so a caller (e.g. `rome-zk-batcher`'s chain-anchor resolution) never re-derives these
/// offsets itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkHeader {
    pub authority: Pubkey,
    pub chain_id: u64,
    pub batch: u64,
    pub idx: u32,
    pub len: u32,
    pub sealed: bool,
}

/// Decodes a chunk account's header (the first [`CHUNK_HEADER_LEN`] bytes; the body — one
/// `channel::Frame::to_bytes()` encoding — follows immediately after) via
/// `rome_zk_layouts::chunk::read` (the single definition of this layout, shared with the on-chain
/// program's own `read_header`) and wraps the raw authority bytes as `solana_program::Pubkey`.
pub fn decode_chunk_header(d: &[u8]) -> Result<ChunkHeader, DecodeError> {
    let f = rome_zk_layouts::chunk::read(d).map_err(|e| match e {
        rome_zk_layouts::LayoutError::TooShort { got, .. } => DecodeError::TooShort(got),
        rome_zk_layouts::LayoutError::BadMagic => DecodeError::BadMagic,
        rome_zk_layouts::LayoutError::BadVersion => DecodeError::BadVersion,
        rome_zk_layouts::LayoutError::BadZiskWord { index }
        | rome_zk_layouts::LayoutError::BadZiskTail { index } => DecodeError::BadZiskPacking(index),
    })?;
    Ok(ChunkHeader {
        authority: Pubkey::new_from_array(f.authority),
        chain_id: f.chain_id,
        batch: f.batch,
        idx: f.idx,
        len: f.len,
        sealed: f.sealed,
    })
}

/// Decoded view of a batch account. Field-for-field, no interpretation beyond parsing bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchAccount {
    pub chain_id: u64,
    pub batch: u64,
    pub open_slot: u64,
    pub expected_count: u32,
    pub leaves_present: u32,
    pub finalized: bool,
    pub settlement_program: Pubkey,
    pub authority: Pubkey,
    pub root: [u8; 32],
    pub forced_root: [u8; 32],
    pub acc: [u8; 32],
    pub finalize_cursor: u32,
    /// v2: the committed `Clock::unix_timestamp` `OpenBatch` wrote alongside `open_slot` — the anchor
    /// `rome-zk-derive`'s one-sided drift bound checks every block's timestamp against. Not part of `acc`.
    pub open_unix_ts: i64,
    /// v3: the batch's deposit range `[from, to)` and the queue's hash-chain values at its ends.
    /// `None` for a v2 header.
    pub deposit: Option<rome_zk_layouts::batch::BatchDeposit>,
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("account too short: {0} bytes")]
    TooShort(usize),
    #[error("bad magic")]
    BadMagic,
    #[error("bad version")]
    BadVersion,
    /// Only `public_values::unpack_zisk_outputs` produces this; account decoders never do — kept so the
    /// mapping stays exhaustive when the layouts crate gains a variant.
    #[error("bad ZisK output packing at word {0}")]
    BadZiskPacking(usize),
}

/// Decodes via `rome_zk_layouts::batch::read` (the single definition of this layout, shared with the
/// on-chain program) and wraps the raw pubkey bytes as `solana_program::Pubkey`.
pub fn decode_batch_account(d: &[u8]) -> Result<BatchAccount, DecodeError> {
    let f = rome_zk_layouts::batch::read(d).map_err(|e| match e {
        rome_zk_layouts::LayoutError::TooShort { got, .. } => DecodeError::TooShort(got),
        rome_zk_layouts::LayoutError::BadMagic => DecodeError::BadMagic,
        rome_zk_layouts::LayoutError::BadVersion => DecodeError::BadVersion,
        rome_zk_layouts::LayoutError::BadZiskWord { index }
        | rome_zk_layouts::LayoutError::BadZiskTail { index } => DecodeError::BadZiskPacking(index),
    })?;
    Ok(BatchAccount {
        chain_id: f.chain_id,
        batch: f.batch,
        open_slot: f.open_slot,
        expected_count: f.expected_count,
        leaves_present: f.leaves_present,
        finalized: f.finalized,
        settlement_program: Pubkey::new_from_array(f.settlement_program),
        authority: Pubkey::new_from_array(f.authority),
        root: f.root,
        forced_root: f.forced_root,
        acc: f.acc,
        finalize_cursor: f.finalize_cursor,
        open_unix_ts: f.open_unix_ts,
        deposit: f.deposit,
    })
}

/// Decoded view of a `batch_cursor` account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchCursor {
    pub chain_id: u64,
    pub next_batch: u64,
    /// v2: the deposit cursor. `None` for a v1 cursor.
    pub deposit: Option<rome_zk_layouts::cursor::CursorDeposit>,
}

/// Decodes via `rome_zk_layouts::cursor::read` — the single definition of this layout, shared with the
/// on-chain program.
pub fn decode_batch_cursor(d: &[u8]) -> Result<BatchCursor, DecodeError> {
    let f = rome_zk_layouts::cursor::read(d).map_err(|e| match e {
        rome_zk_layouts::LayoutError::TooShort { got, .. } => DecodeError::TooShort(got),
        rome_zk_layouts::LayoutError::BadMagic => DecodeError::BadMagic,
        rome_zk_layouts::LayoutError::BadVersion => DecodeError::BadVersion,
        rome_zk_layouts::LayoutError::BadZiskWord { index }
        | rome_zk_layouts::LayoutError::BadZiskTail { index } => DecodeError::BadZiskPacking(index),
    })?;
    Ok(BatchCursor {
        chain_id: f.chain_id,
        next_batch: f.next_batch,
        deposit: f.deposit,
    })
}

/// The `InitBatchCursor` value `examples/find_max_batch_id.rs` proposes. Settlement accepts only
/// `head_pending_batch + 1` as the next batch, so that is the whole answer; a scan of inbox accounts by chain id
/// is never an input to it, because anyone can create inbox accounts for any chain id under their own settlement
/// program, and a huge planted id would otherwise carry the cursor past the id settlement waits for.
pub mod cursor_proposal {
    /// The batch id of a batch account that belongs to `chain_id` under `settlement_program`: the account sits at
    /// the settlement-keyed `batch_pda` address AND records that settlement program. Anything else (another
    /// settlement program's batch for the same chain id) yields `None`.
    pub fn settlement_keyed_batch_id(
        inbox_program: &solana_program::pubkey::Pubkey,
        settlement_program: &solana_program::pubkey::Pubkey,
        chain_id: u64,
        address: &solana_program::pubkey::Pubkey,
        data: &[u8],
    ) -> Option<u64> {
        let f = rome_zk_layouts::batch::read(data).ok()?;
        if f.chain_id != chain_id || f.settlement_program != settlement_program.to_bytes() {
            return None;
        }
        let (expected, _) = super::batch_pda(inbox_program, settlement_program, chain_id, f.batch);
        (expected == *address).then_some(f.batch)
    }

    /// `root.head_pending_batch + 1`, or an error naming the problem when a batch of this chain under THIS
    /// settlement program already exists at that id or above (a cursor there would let `OpenBatch` collide with
    /// it). `settlement_keyed_ids` are the ids `settlement_keyed_batch_id` accepted; foreign accounts never reach
    /// this function.
    pub fn propose_next_batch(
        head_pending_batch: u64,
        settlement_keyed_ids: &[u64],
    ) -> Result<u64, String> {
        let next = head_pending_batch
            .checked_add(1)
            .ok_or_else(|| "head_pending_batch is u64::MAX".to_string())?;
        if let Some(h) = settlement_keyed_ids
            .iter()
            .copied()
            .filter(|b| *b >= next)
            .max()
        {
            return Err(format!(
                "a batch account for this chain under this settlement program already exists at id {h}, \
                 at or above head_pending_batch + 1 = {next}; the cursor cannot be initialised at {next} \
                 without OpenBatch colliding with it. Inspect that account before going further"
            ));
        }
        Ok(next)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use solana_program::pubkey::Pubkey;

        fn batch_account(chain_id: u64, batch: u64, settlement: &Pubkey) -> Vec<u8> {
            use rome_zk_layouts::batch as b;
            let mut d = vec![0u8; b::account_len_for(b::VERSION, 0).unwrap()];
            d[b::OFF_MAGIC..b::OFF_MAGIC + 4].copy_from_slice(&b::MAGIC.to_le_bytes());
            d[b::OFF_VERSION] = b::VERSION;
            d[b::OFF_CHAIN_ID..b::OFF_CHAIN_ID + 8].copy_from_slice(&chain_id.to_le_bytes());
            d[b::OFF_BATCH..b::OFF_BATCH + 8].copy_from_slice(&batch.to_le_bytes());
            d[b::OFF_SETTLEMENT_PROGRAM..b::OFF_SETTLEMENT_PROGRAM + 32]
                .copy_from_slice(&settlement.to_bytes());
            d
        }

        #[test]
        fn the_proposal_is_one_past_the_settlement_head() {
            assert_eq!(propose_next_batch(0, &[]), Ok(1));
            assert_eq!(propose_next_batch(7, &[3, 7]), Ok(8));
        }

        /// A third party opens a batch with a huge id for the same chain id under its own settlement program. The
        /// account is a valid batch account at its own (foreign-keyed) address; it must not count, and the
        /// proposal stays at head_pending_batch + 1.
        #[test]
        fn a_planted_foreign_batch_id_does_not_raise_the_proposed_cursor() {
            let inbox = Pubkey::new_unique();
            let ours = Pubkey::new_unique();
            let theirs = Pubkey::new_unique();
            let chain = 200_101u64;
            let planted_id = u64::MAX / 2;

            let (foreign_addr, _) = super::super::batch_pda(&inbox, &theirs, chain, planted_id);
            let foreign = batch_account(chain, planted_id, &theirs);
            // Wrong settlement program recorded, and wrong address for ours either way.
            assert_eq!(
                settlement_keyed_batch_id(&inbox, &ours, chain, &foreign_addr, &foreign),
                None
            );
            // The same bytes copied to our address still record the foreign program: refused.
            let (our_addr, _) = super::super::batch_pda(&inbox, &ours, chain, planted_id);
            assert_eq!(
                settlement_keyed_batch_id(&inbox, &ours, chain, &our_addr, &foreign),
                None
            );

            let kept: Vec<u64> = [(foreign_addr, foreign)]
                .iter()
                .filter_map(|(a, d)| settlement_keyed_batch_id(&inbox, &ours, chain, a, d))
                .collect();
            assert_eq!(propose_next_batch(0, &kept), Ok(1));
        }

        #[test]
        fn our_own_batch_at_its_own_address_counts() {
            let inbox = Pubkey::new_unique();
            let ours = Pubkey::new_unique();
            let (addr, _) = super::super::batch_pda(&inbox, &ours, 5, 9);
            let d = batch_account(5, 9, &ours);
            assert_eq!(
                settlement_keyed_batch_id(&inbox, &ours, 5, &addr, &d),
                Some(9)
            );
            assert!(propose_next_batch(3, &[9]).is_err());
        }
    }
}

/// `getProgramAccounts` memcmp-filter helpers for `examples/find_max_batch_id.rs`: the earlier id-by-id scan only
/// probed batch PDAs — missing an abandoned batch's still-live chunk PDAs (`AbandonBatch`/ `CloseBatch` delete the
/// *batch* account, never the chunk accounts opened under it), so the highest id it ever missed is exactly the one
/// a crashed prior run left chunk PDAs for — and folded every `Err` (including a transient RPC failure) into
/// "missing", ending the scan early and, with `--submit`, bootstrapping the cursor too low. This scans both account
/// kinds directly (no per-id round trips, no early-termination heuristic); the caller takes the max `batch` id over
/// both result sets.
#[cfg(feature = "devnet-driver")]
pub mod scan {
    use solana_client::rpc_filter::{Memcmp, RpcFilterType};

    /// `getProgramAccounts` filters for chunk accounts (`zk_inbox::MAGIC` "ZKIB") belonging to
    /// `chain_id` — magic at `zk_inbox::OFF_MAGIC`, chain id at `zk_inbox::OFF_CHAIN_ID`.
    pub fn chunk_account_filters(chain_id: u64) -> Vec<RpcFilterType> {
        vec![
            RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
                zk_inbox::OFF_MAGIC,
                zk_inbox::MAGIC.to_le_bytes().to_vec(),
            )),
            RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
                zk_inbox::OFF_CHAIN_ID,
                chain_id.to_le_bytes().to_vec(),
            )),
        ]
    }

    /// `getProgramAccounts` filters for batch accounts (`rome_zk_layouts::batch::MAGIC` "ZKBT")
    /// belonging to `chain_id` — magic at `OFF_MAGIC`, chain id at `OFF_CHAIN_ID`.
    pub fn batch_account_filters(chain_id: u64) -> Vec<RpcFilterType> {
        vec![
            RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
                rome_zk_layouts::batch::OFF_MAGIC,
                rome_zk_layouts::batch::MAGIC.to_le_bytes().to_vec(),
            )),
            RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
                rome_zk_layouts::batch::OFF_CHAIN_ID,
                chain_id.to_le_bytes().to_vec(),
            )),
        ]
    }

    /// Reads the `batch` field out of a chunk account's raw bytes (offset `zk_inbox::OFF_BATCH`) —
    /// `None` if the account is too short to hold it (defensive; `getProgramAccounts`' own memcmp filter
    /// already guarantees this in practice).
    pub fn chunk_batch_id(data: &[u8]) -> Option<u64> {
        data.get(zk_inbox::OFF_BATCH..zk_inbox::OFF_BATCH + 8)
            .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    }

    /// Reads the `batch` field out of a batch account's raw bytes (offset
    /// `rome_zk_layouts::batch::OFF_BATCH`).
    pub fn batch_batch_id(data: &[u8]) -> Option<u64> {
        data.get(rome_zk_layouts::batch::OFF_BATCH..rome_zk_layouts::batch::OFF_BATCH + 8)
            .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn chunk_bytes(chain_id: u64, batch: u64) -> Vec<u8> {
            let mut d = vec![0u8; zk_inbox::HEADER_LEN];
            d[zk_inbox::OFF_MAGIC..zk_inbox::OFF_MAGIC + 4]
                .copy_from_slice(&zk_inbox::MAGIC.to_le_bytes());
            d[zk_inbox::OFF_CHAIN_ID..zk_inbox::OFF_CHAIN_ID + 8]
                .copy_from_slice(&chain_id.to_le_bytes());
            d[zk_inbox::OFF_BATCH..zk_inbox::OFF_BATCH + 8].copy_from_slice(&batch.to_le_bytes());
            d
        }

        fn batch_bytes(chain_id: u64, batch: u64) -> Vec<u8> {
            let mut d =
                vec![
                    0u8;
                    rome_zk_layouts::batch::account_len_for(rome_zk_layouts::batch::VERSION, 0)
                        .unwrap()
                ];
            d[rome_zk_layouts::batch::OFF_MAGIC..rome_zk_layouts::batch::OFF_MAGIC + 4]
                .copy_from_slice(&rome_zk_layouts::batch::MAGIC.to_le_bytes());
            d[rome_zk_layouts::batch::OFF_CHAIN_ID..rome_zk_layouts::batch::OFF_CHAIN_ID + 8]
                .copy_from_slice(&chain_id.to_le_bytes());
            d[rome_zk_layouts::batch::OFF_BATCH..rome_zk_layouts::batch::OFF_BATCH + 8]
                .copy_from_slice(&batch.to_le_bytes());
            d
        }

        /// The filters this scanner sends to `getProgramAccounts` must actually match a real chunk
        /// account for the right chain, and reject one for a different chain — proving the offsets are
        /// right against the RPC node's own match logic (`Memcmp::bytes_match`), not just against
        /// `zk_inbox`'s own constants (which could drift together with the filter code and still both be
        /// wrong).
        /// `getProgramAccounts` ANDs every filter together — a real RPC node only returns an account that
        /// matches *all* of them, so the fixture asserts the same: every filter matches the right chain's
        /// account, and at least one (the chain-id filter) rejects a different chain's (the magic filter
        /// alone can't — it doesn't encode chain id at all, so it matches either).
        #[test]
        fn chunk_account_filters_match_the_right_chain_and_reject_a_different_one() {
            let filters = chunk_account_filters(7);
            let same_chain = chunk_bytes(7, 42);
            let other_chain = chunk_bytes(8, 42);
            let memcmps: Vec<&Memcmp> = filters
                .iter()
                .map(|f| {
                    let RpcFilterType::Memcmp(m) = f else {
                        panic!("expected a Memcmp filter")
                    };
                    m
                })
                .collect();
            assert!(memcmps.iter().all(|m| m.bytes_match(&same_chain)));
            assert!(!memcmps.iter().all(|m| m.bytes_match(&other_chain)));
        }

        #[test]
        fn batch_account_filters_match_the_right_chain_and_reject_a_different_one() {
            let filters = batch_account_filters(7);
            let same_chain = batch_bytes(7, 42);
            let other_chain = batch_bytes(9, 42);
            let memcmps: Vec<&Memcmp> = filters
                .iter()
                .map(|f| {
                    let RpcFilterType::Memcmp(m) = f else {
                        panic!("expected a Memcmp filter")
                    };
                    m
                })
                .collect();
            assert!(memcmps.iter().all(|m| m.bytes_match(&same_chain)));
            assert!(!memcmps.iter().all(|m| m.bytes_match(&other_chain)));
        }

        #[test]
        fn chunk_batch_id_reads_the_pinned_offset() {
            assert_eq!(chunk_batch_id(&chunk_bytes(7, 42)), Some(42));
        }

        #[test]
        fn batch_batch_id_reads_the_pinned_offset() {
            assert_eq!(batch_batch_id(&batch_bytes(7, 42)), Some(42));
        }

        #[test]
        fn chunk_batch_id_returns_none_on_too_short_data() {
            assert_eq!(chunk_batch_id(&[0u8; 4]), None);
        }
    }
}

/// Off-chain reference for the accumulator's commitment, for verifying an on-chain `acc`
/// independently: given the plain chunk-body hashes in idx order (i.e. what `SealLeaf` writes, *before*
/// `FinalizeBatch`'s in-place `idx ‖ hash` transform), returns `(root, forced_root, acc)`. Computed via
/// `rome_zk_layouts::acc`/`forced_empty_root` with `rome_zk_merkle::keccak256` (this workspace's one
/// keccak — the on-chain program's own call uses the same function, dispatched to the syscall by
/// `target_os`), so this function's output equalling the on-chain `acc` (asserted throughout
/// `programs/zk-inbox/tests/accumulator.rs`) is exactly the on-chain/off-chain equivalence check.
pub fn reference_commitment(
    chain_id: u64,
    batch: u64,
    open_slot: u64,
    chunk_hashes: &[[u8; 32]],
) -> ([u8; 32], [u8; 32], [u8; 32]) {
    reference_commitment_with_deposits(
        chain_id,
        batch,
        open_slot,
        chunk_hashes,
        &rome_zk_layouts::batch::BatchDeposit {
            from: 0,
            to: 0,
            hash_from: [0; 32],
            hash_to: [0; 32],
        },
    )
}

/// [`reference_commitment`] for a batch whose deposit range is `range` (a v3 header's
/// `deposit_from`, `deposit_to` and the two hash-chain values): `forced_root` is
/// `rome_zk_layouts::deposit::forced_root` over the range, so an empty range (`from == to`, whatever
/// the hashes) gives the constant `forced_empty_root` and the same `(root, forced_root, acc)` as
/// [`reference_commitment`], which is this function's empty-range wrapper.
pub fn reference_commitment_with_deposits(
    chain_id: u64,
    batch: u64,
    open_slot: u64,
    chunk_hashes: &[[u8; 32]],
    range: &rome_zk_layouts::batch::BatchDeposit,
) -> ([u8; 32], [u8; 32], [u8; 32]) {
    let sk = rome_zk_merkle::keccak256 as fn(&[&[u8]]) -> [u8; 32];
    let leaves: Vec<[u8; 32]> = chunk_hashes
        .iter()
        .enumerate()
        .map(|(i, h)| rome_zk_merkle::indexed_leaf(&sk, i as u32, h))
        .collect();
    let root = rome_zk_merkle::root(&sk, &leaves);
    let forced_root = rome_zk_layouts::deposit::forced_root(
        &sk,
        range.from,
        range.to,
        &range.hash_from,
        &range.hash_to,
    );
    let expected_count = chunk_hashes.len() as u32;
    let acc = rome_zk_layouts::acc(
        &sk,
        chain_id,
        batch,
        open_slot,
        expected_count,
        &root,
        &forced_root,
    );
    (root, forced_root, acc)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins `chunk_body_hash` against an independently computed keccak256 (pycryptodome
    /// `Crypto.Hash.keccak`, digest_bits=256 — NOT the NIST SHA3 variant, which pads differently and
    /// would give a different digest) so a future accidental swap to a different hash (or a wrong
    /// domain/padding) is caught here, not only in the on-chain program tests.
    #[test]
    fn chunk_body_hash_matches_an_independently_computed_keccak256() {
        let body = b"rome-zk chunk body: the quick brown fox jumps over the lazy dog";
        let expected: [u8; 32] = [
            0xc2, 0xd8, 0x8b, 0xce, 0x25, 0x20, 0x87, 0xe4, 0x5b, 0x6d, 0x5e, 0xb9, 0x38, 0x2c,
            0x95, 0xd4, 0x54, 0xa6, 0xb9, 0x23, 0xae, 0xa1, 0xb6, 0x2b, 0xa9, 0xcd, 0x24, 0xfc,
            0x05, 0x46, 0x21, 0xe9,
        ];
        assert_eq!(chunk_body_hash(body), expected);
    }

    /// [`decode_chunk_header`] parses exactly what a chunk account's header holds — pinned
    /// against a hand-built header rather than only via a round trip through the write path.
    #[test]
    fn decode_chunk_header_reads_a_hand_built_header() {
        let authority = Pubkey::new_unique();
        let mut d = vec![0u8; zk_inbox::HEADER_LEN + 4]; // header + a little body
        d[zk_inbox::OFF_MAGIC..zk_inbox::OFF_MAGIC + 4]
            .copy_from_slice(&zk_inbox::MAGIC.to_le_bytes());
        d[zk_inbox::OFF_AUTHORITY..zk_inbox::OFF_AUTHORITY + 32]
            .copy_from_slice(authority.as_ref());
        d[zk_inbox::OFF_CHAIN_ID..zk_inbox::OFF_CHAIN_ID + 8].copy_from_slice(&7u64.to_le_bytes());
        d[zk_inbox::OFF_BATCH..zk_inbox::OFF_BATCH + 8].copy_from_slice(&3u64.to_le_bytes());
        d[zk_inbox::OFF_IDX..zk_inbox::OFF_IDX + 4].copy_from_slice(&2u32.to_le_bytes());
        d[zk_inbox::OFF_LEN..zk_inbox::OFF_LEN + 4].copy_from_slice(&4u32.to_le_bytes());
        d[zk_inbox::OFF_SEALED] = 1;
        let h = decode_chunk_header(&d).unwrap();
        assert_eq!(h.authority, authority);
        assert_eq!(h.chain_id, 7);
        assert_eq!(h.batch, 3);
        assert_eq!(h.idx, 2);
        assert_eq!(h.len, 4);
        assert!(h.sealed);
    }

    /// A too-short buffer (fewer than [`zk_inbox::HEADER_LEN`] bytes) must refuse, not read past it.
    #[test]
    fn decode_chunk_header_refuses_a_too_short_buffer() {
        let d = vec![0u8; zk_inbox::HEADER_LEN - 1];
        assert!(matches!(
            decode_chunk_header(&d),
            Err(DecodeError::TooShort(n)) if n == zk_inbox::HEADER_LEN - 1
        ));
    }

    /// Bad magic must refuse rather than silently parse garbage as a chunk header.
    #[test]
    fn decode_chunk_header_refuses_bad_magic() {
        let d = vec![0u8; zk_inbox::HEADER_LEN];
        assert!(matches!(
            decode_chunk_header(&d),
            Err(DecodeError::BadMagic)
        ));
    }

    /// `decode_instruction` is the exact inverse of every `*_ix` builder: round-trips through the same
    /// borsh bytes a real transaction carries, for a representative instruction of each accumulator
    /// shape the settlement watcher needs to tell apart at read time.
    #[test]
    fn decode_instruction_round_trips_open_batch() {
        let ix = open_batch_ix(
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            200_101,
            4003,
            290,
            &Pubkey::new_unique(),
        );
        let decoded = decode_instruction(&ix.data).unwrap();
        assert!(matches!(
            decoded,
            InboxIx::OpenBatch {
                chain_id: 200_101,
                batch: 4003,
                expected_count: 290,
                ..
            }
        ));
    }

    #[test]
    fn decode_instruction_round_trips_seal() {
        let ix = seal_chunk_ix(
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            200_101,
            4005,
            17,
            3_681,
            [7u8; 32],
        );
        let decoded = decode_instruction(&ix.data).unwrap();
        match decoded {
            InboxIx::Seal { len, body_hash } => {
                assert_eq!(len, 3_681);
                assert_eq!(body_hash, [7u8; 32]);
            }
            other => panic!("expected InboxIx::Seal, got {other:?}"),
        }
    }

    #[test]
    fn decode_instruction_rejects_garbage() {
        assert!(decode_instruction(&[0xffu8; 3]).is_err());
    }

    #[test]
    fn pda_derivations_use_the_documented_seeds() {
        let program_id = Pubkey::new_unique();
        let settlement_program = Pubkey::new_unique();
        let (a, _) = chunk_pda(&program_id, &settlement_program, 7, 3, 1);
        let (b, _) = chunk_pda(&program_id, &settlement_program, 7, 3, 1);
        assert_eq!(a, b, "PDA derivation must be deterministic");
        let (batch_a, _) = batch_pda(&program_id, &settlement_program, 7, 3);
        assert_ne!(a, batch_a, "chunk and batch PDAs must not collide");
        let other_settlement = Pubkey::new_unique();
        assert_ne!(
            a,
            chunk_pda(&program_id, &other_settlement, 7, 3, 1).0,
            "the same chunk under another settlement program is a different account"
        );
        assert_ne!(
            batch_a,
            batch_pda(&program_id, &other_settlement, 7, 3).0,
            "the same batch under another settlement program is a different account"
        );
    }

    #[test]
    fn decode_round_trips_a_hand_built_account() {
        let mut d = vec![0u8; batch::account_len_for(batch::VERSION, 1).unwrap()];
        d[0..4].copy_from_slice(&batch::MAGIC.to_le_bytes());
        d[4] = batch::VERSION;
        d[5..13].copy_from_slice(&7u64.to_le_bytes());
        d[13..21].copy_from_slice(&3u64.to_le_bytes());
        d[29..33].copy_from_slice(&1u32.to_le_bytes());
        let acct = decode_batch_account(&d).unwrap();
        assert_eq!(acct.chain_id, 7);
        assert_eq!(acct.batch, 3);
        assert_eq!(acct.expected_count, 1);
        assert!(!acct.finalized);
        assert_eq!(acct.open_unix_ts, 0);
    }

    /// Contract: a v1-shaped batch account (version byte 1, no `open_unix_ts`) is refused by name, never silently
    /// accepted or zero-padded — no migration.
    #[test]
    fn decode_batch_account_refuses_a_v1_shaped_account() {
        let mut d = vec![0u8; 202];
        d[0..4].copy_from_slice(&batch::MAGIC.to_le_bytes());
        d[4] = 1; // v1
        d[5..13].copy_from_slice(&7u64.to_le_bytes());
        assert!(matches!(
            decode_batch_account(&d).unwrap_err(),
            DecodeError::BadVersion
        ));
    }

    #[test]
    fn reference_commitment_matches_direct_merkle_reference_for_one_leaf() {
        let h = [9u8; 32];
        let (root, _, _) = reference_commitment(1, 1, 100, &[h]);
        let expected_leaf = rome_zk_merkle::indexed_leaf(
            &(rome_zk_merkle::keccak256 as fn(&[&[u8]]) -> [u8; 32]),
            0,
            &h,
        );
        assert_eq!(root, expected_leaf, "single-leaf root is the leaf itself");
    }

    fn hex32(h: &str) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (i, o) in out.iter_mut().enumerate() {
            *o = u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap();
        }
        out
    }

    // The deposit range goldens below are the deposit module's (settlement program [0x33; 32],
    // chain 7, the three records in its tests); the `acc` values come from an independent Python
    // keccak over chain 7, batch 1, open_slot 1000, one chunk hash [9; 32] (leaf = keccak(0u32 le
    // ‖ hash) = the root of a one-leaf tree).
    const SEED: &str = "7b62297f72fe3a90eecade8f81e0197b8fef15f5b4c5a10930e1fd3bffd777f3";
    const AFTER_1: &str = "8c6b4a978661d6d251ed888e8f89a4cf6b18dfdfe9cd1d1a472d6196273cdf86";
    const AFTER_3: &str = "68cd40fac4d5ed9f0cdcf6f38a56a8eaf65dd1b11cc1d23e96b70eed7a85e1b2";
    const FORCED_ROOT_0_3: &str =
        "34f7c42e5502c795aea99b9dc9b34a1095ea347f975609863ac1d0351b8dfee8";
    const FORCED_ROOT_1_3: &str =
        "0f101d970e8e9e8ff754ad5c5d86950026a3353ee17449c9191511cbbeb245dc";
    const ACC_0_3: &str = "af938948e05a4125e80ef5389c3679441f8418a9ed839818a5c3173c1e8f3468";
    const ACC_1_3: &str = "38f6ff87a66d8d17466e3aa342800034e8fa588440dd9ce35fd1dd16cfaf32db";
    const ACC_EMPTY: &str = "90b0e9679983dfa450a33d8f5bdf1ab83d5d6e7965e16d032526a757b986c6ff";

    /// With an empty range the new commitment equals the old function's, whatever hashes ride along.
    #[test]
    fn reference_commitment_with_an_empty_range_equals_the_old_one() {
        let hashes = [[9u8; 32]];
        let old = reference_commitment(7, 1, 1000, &hashes);
        assert_eq!(
            hex_of(&old.2),
            ACC_EMPTY,
            "the old function's acc is unchanged"
        );
        for (from, to) in [(0u64, 0u64), (5, 5)] {
            let range = rome_zk_layouts::batch::BatchDeposit {
                from,
                to,
                hash_from: [0xaa; 32],
                hash_to: [0xbb; 32],
            };
            assert_eq!(
                reference_commitment_with_deposits(7, 1, 1000, &hashes, &range),
                old
            );
        }
    }

    fn hex_of(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// With a real range, `forced_root` and `acc` equal the independently computed goldens; the root
    /// (inbox leaves) does not depend on the range.
    #[test]
    fn reference_commitment_with_a_range_matches_the_goldens() {
        let hashes = [[9u8; 32]];
        let (root0, _, _) = reference_commitment(7, 1, 1000, &hashes);
        for (from, to, h_from, h_to, want_forced, want_acc) in [
            (0u64, 3u64, SEED, AFTER_3, FORCED_ROOT_0_3, ACC_0_3),
            (1, 3, AFTER_1, AFTER_3, FORCED_ROOT_1_3, ACC_1_3),
        ] {
            let range = rome_zk_layouts::batch::BatchDeposit {
                from,
                to,
                hash_from: hex32(h_from),
                hash_to: hex32(h_to),
            };
            let (root, forced, acc) =
                reference_commitment_with_deposits(7, 1, 1000, &hashes, &range);
            assert_eq!(root, root0);
            assert_eq!(hex_of(&forced), want_forced);
            assert_eq!(hex_of(&acc), want_acc);
        }
    }

    #[test]
    fn decode_batch_account_reads_a_v3_header_and_a_v2_header() {
        let v3 = rome_zk_layouts::batch::BatchFields {
            chain_id: 7,
            batch: 3,
            open_slot: 100,
            expected_count: 9,
            leaves_present: 0,
            finalized: false,
            settlement_program: [1; 32],
            authority: [2; 32],
            root: [3; 32],
            forced_root: [4; 32],
            acc: [5; 32],
            finalize_cursor: 0,
            open_unix_ts: 11,
            deposit: Some(rome_zk_layouts::batch::BatchDeposit {
                from: 4,
                to: 6,
                hash_from: [6; 32],
                hash_to: [7; 32],
            }),
        };
        let mut d = vec![0u8; rome_zk_layouts::batch::account_len_for(3, 9).unwrap()];
        d[..rome_zk_layouts::batch::HEADER_LEN_V3]
            .copy_from_slice(&rome_zk_layouts::batch::write_header_v3(&v3).unwrap());
        let a = decode_batch_account(&d).unwrap();
        assert_eq!(a.deposit, v3.deposit);
        assert_eq!(a.open_unix_ts, 11);
        assert_eq!(a.expected_count, 9);

        let mut d2 = vec![0u8; rome_zk_layouts::batch::account_len_for(2, 9).unwrap()];
        d2[..rome_zk_layouts::batch::HEADER_LEN_V2].copy_from_slice(
            &rome_zk_layouts::batch::write_header(&rome_zk_layouts::batch::BatchFields {
                deposit: None,
                ..v3
            }),
        );
        let a2 = decode_batch_account(&d2).unwrap();
        assert_eq!(a2.deposit, None);
        assert_eq!(a2.open_unix_ts, 11);

        // Version 4 is refused by name.
        d[4] = 4;
        assert!(matches!(
            decode_batch_account(&d).unwrap_err(),
            DecodeError::BadVersion
        ));
    }

    /// `chunk_account_index`/`batch_account_index` must agree with every real `*_ix` builder's own `AccountMeta`
    /// order — decode the builder's own instruction data back into an `InboxIx` (the same round trip a consumer
    /// does) and check the index against the builder's own `accounts` list, never a hand-copied literal.
    #[test]
    fn chunk_and_batch_account_index_match_every_real_builder() {
        let program_id = Pubkey::new_unique();
        let settlement_program = Pubkey::new_unique();
        let payer = Pubkey::new_unique();
        let authority = Pubkey::new_unique();
        let (chunk, _) = chunk_pda(&program_id, &settlement_program, 7, 3, 1);
        let (batch_acct, _) = batch_pda(&program_id, &settlement_program, 7, 3);

        let cases: Vec<solana_program::instruction::Instruction> = vec![
            open_chunk_ix(&program_id, &payer, &settlement_program, 7, 3, 1, 100),
            write_chunk_ix(
                &program_id,
                &authority,
                &settlement_program,
                7,
                3,
                1,
                0,
                vec![1, 2, 3],
            ),
            seal_chunk_ix(
                &program_id,
                &authority,
                &settlement_program,
                7,
                3,
                1,
                100,
                [9u8; 32],
            ),
            close_chunk_ix(&program_id, &authority, &settlement_program, 7, 3, 1),
            open_batch_ix(&program_id, &payer, 7, 3, 290, &settlement_program),
            grow_batch_ix(&program_id, &payer, &settlement_program, 7, 3),
            seal_leaf_ix(&program_id, &settlement_program, 7, 3, 1),
            finalize_batch_ix(&program_id, &authority, &settlement_program, 7, 3, 0),
            close_batch_ix(&program_id, &authority, &settlement_program, 7, 3),
            abandon_batch_ix(&program_id, &authority, &settlement_program, 7, 3),
        ];

        for ix in &cases {
            let decoded = decode_instruction(&ix.data).expect("every case above must decode");
            if let Some(idx) = chunk_account_index(&decoded) {
                assert_eq!(
                    ix.accounts[idx].pubkey, chunk,
                    "chunk_account_index wrong for {decoded:?}"
                );
            }
            if let Some(idx) = batch_account_index(&decoded) {
                assert_eq!(
                    ix.accounts[idx].pubkey, batch_acct,
                    "batch_account_index wrong for {decoded:?}"
                );
            }
        }

        // Every case above touches a chunk, a batch, or both -- assert the helpers actually found
        // something for each, not just that any hit was correct when found.
        let with_chunk = [0, 1, 2, 3, 6]; // Open, Write, Seal, Close, SealLeaf
        let with_batch = [0, 3, 4, 5, 6, 7, 8, 9]; // Open, Close, OpenBatch, Grow, SealLeaf, Finalize, Close/Abandon Batch
        for i in with_chunk {
            let decoded = decode_instruction(&cases[i].data).unwrap();
            assert!(
                chunk_account_index(&decoded).is_some(),
                "case {i} must carry a chunk index"
            );
        }
        for i in with_batch {
            let decoded = decode_instruction(&cases[i].data).unwrap();
            assert!(
                batch_account_index(&decoded).is_some(),
                "case {i} must carry a batch index"
            );
        }
    }

    #[test]
    fn account_index_helpers_are_none_for_instructions_with_no_such_account() {
        assert_eq!(
            chunk_account_index(&InboxIx::OpenBatch {
                chain_id: 1,
                batch: 1,
                expected_count: 1,
                settlement_program: Pubkey::new_unique(),
            }),
            None
        );
        assert_eq!(
            batch_account_index(&InboxIx::Write {
                offset: 0,
                data: vec![]
            }),
            None
        );
        assert_eq!(
            chunk_account_index(&InboxIx::InitBatchCursor {
                chain_id: 1,
                next_batch: 0,
                settlement_program: Pubkey::new_unique(),
            }),
            None
        );
        assert_eq!(
            batch_account_index(&InboxIx::InitBatchCursor {
                chain_id: 1,
                next_batch: 0,
                settlement_program: Pubkey::new_unique(),
            }),
            None
        );
    }

    #[test]
    fn cursor_pda_derivation_is_deterministic_and_distinct_per_chain() {
        let program_id = Pubkey::new_unique();
        let settlement_program = Pubkey::new_unique();
        let (a, _) = cursor_pda(&program_id, &settlement_program, 7);
        let (b, _) = cursor_pda(&program_id, &settlement_program, 7);
        assert_eq!(a, b);
        let (c, _) = cursor_pda(&program_id, &settlement_program, 8);
        assert_ne!(a, c);
        let (batch0, _) = batch_pda(&program_id, &settlement_program, 7, 0);
        assert_ne!(a, batch0, "cursor and batch PDAs must not collide");
    }

    #[test]
    fn decode_batch_cursor_round_trips_a_hand_built_account() {
        let mut d = vec![0u8; rome_zk_layouts::cursor::LEN];
        d[0..4].copy_from_slice(&rome_zk_layouts::cursor::MAGIC.to_le_bytes());
        d[4] = rome_zk_layouts::cursor::VERSION;
        d[5..13].copy_from_slice(&11u64.to_le_bytes());
        d[13..21].copy_from_slice(&3u64.to_le_bytes());
        let c = decode_batch_cursor(&d).unwrap();
        assert_eq!(c.chain_id, 11);
        assert_eq!(c.next_batch, 3);
    }

    #[test]
    fn decode_batch_cursor_reads_a_v2_cursor_and_a_v1_cursor() {
        use rome_zk_layouts::cursor;
        let v2 = cursor::write_v2(&cursor::CursorFields {
            chain_id: 11,
            next_batch: 3,
            deposit: Some(cursor::CursorDeposit {
                next: 5,
                hash: [8; 32],
                final_: 4,
            }),
        })
        .unwrap();
        assert_eq!(v2.len(), 69);
        let c = decode_batch_cursor(&v2).unwrap();
        assert_eq!((c.chain_id, c.next_batch), (11, 3));
        assert_eq!(
            c.deposit,
            Some(cursor::CursorDeposit {
                next: 5,
                hash: [8; 32],
                final_: 4
            })
        );
        let mut d = v2.to_vec();
        d[4] = 3;
        assert!(matches!(
            decode_batch_cursor(&d).unwrap_err(),
            DecodeError::BadVersion
        ));
        let v1 = cursor::write(&cursor::CursorFields {
            chain_id: 11,
            next_batch: 3,
            deposit: None,
        });
        assert_eq!(decode_batch_cursor(&v1).unwrap().deposit, None);
    }

    /// `account_len(312) = 10,225 <= MAX_PERMITTED_DATA_INCREASE` (10,240): `OpenBatch` alone reaches
    /// full size, no `GrowBatch` needed.
    #[test]
    fn open_and_grow_plan_needs_no_grow_at_312_leaves() {
        let program_id = Pubkey::new_unique();
        let payer = Pubkey::new_unique();
        let settlement_program = Pubkey::new_unique();
        let ixs = open_and_grow_batch_ixs(&program_id, &payer, 1, 0, 312, &settlement_program);
        assert_eq!(ixs.len(), 1, "OpenBatch alone must suffice at 312 leaves");
    }

    /// `account_len(313) = 10,258 > 10,240`: exactly one `GrowBatch` is needed (this is the exact case
    /// that once failed on devnet).
    #[test]
    fn open_and_grow_plan_needs_exactly_one_grow_at_313_leaves() {
        let program_id = Pubkey::new_unique();
        let payer = Pubkey::new_unique();
        let settlement_program = Pubkey::new_unique();
        let ixs = open_and_grow_batch_ixs(&program_id, &payer, 1, 0, 313, &settlement_program);
        assert_eq!(
            ixs.len(),
            2,
            "OpenBatch + exactly one GrowBatch at 313 leaves"
        );
    }

    /// `account_len(900) = 29,123`: `OpenBatch` reaches 10,240, each `GrowBatch` covers another 10,240
    /// (capped at the target on the last one) — two grows suffice, not three (a smaller number than the
    /// batch-sizing design's three-grow example is fine: three is a safe upper bound, not a literal
    /// requirement, and this plan always computes exactly the number needed).
    #[test]
    fn open_and_grow_plan_needs_exactly_two_grows_at_900_leaves() {
        let program_id = Pubkey::new_unique();
        let payer = Pubkey::new_unique();
        let settlement_program = Pubkey::new_unique();
        let ixs = open_and_grow_batch_ixs(&program_id, &payer, 1, 0, 900, &settlement_program);
        assert_eq!(ixs.len(), 3, "OpenBatch + two GrowBatch at 900 leaves");
    }

    /// This client's `chunk_pda`/`batch_pda`/`root_pda`/`cursor_pda` are zero-logic delegates to
    /// `rome_zk_layouts`'s own derivation, and the on-chain program's own seed functions delegate to the
    /// same place — these tests prove that wrapper delegation, not that the derivation itself is
    /// correct. A seed change in `rome_zk_layouts` moves the program, this client and these tests
    /// together, so they stay green under a seed typo that would strand every already-deployed account;
    /// the real check on the deployed addresses is `rome-zk-layouts`'s own `tests/pda_pins.rs`.
    #[test]
    fn chunk_pda_matches_program_and_layouts() {
        let program = Pubkey::new_unique();
        let settlement_program = Pubkey::new_unique();
        let s = zk_inbox::pda_seeds(&settlement_program, 7, 3, 2);
        let from_program_seeds =
            Pubkey::find_program_address(&[&s[0], &s[1], &s[2], &s[3], &s[4]], &program);
        assert_eq!(
            chunk_pda(&program, &settlement_program, 7, 3, 2),
            from_program_seeds
        );
        assert_eq!(
            chunk_pda(&program, &settlement_program, 7, 3, 2),
            rome_zk_layouts::chunk::pda(&program, &settlement_program, 7, 3, 2)
        );
    }

    #[test]
    fn batch_pda_matches_program_and_layouts() {
        let program = Pubkey::new_unique();
        let settlement_program = Pubkey::new_unique();
        assert_eq!(
            batch_pda(&program, &settlement_program, 7, 3),
            rome_zk_layouts::batch::pda(&program, &settlement_program, 7, 3)
        );
    }

    #[test]
    fn cursor_pda_matches_program_and_layouts() {
        let program = Pubkey::new_unique();
        let settlement_program = Pubkey::new_unique();
        assert_eq!(
            cursor_pda(&program, &settlement_program, 7),
            rome_zk_layouts::cursor::pda(&program, &settlement_program, 7)
        );
    }

    #[test]
    fn root_pda_matches_layouts() {
        let settlement_program = Pubkey::new_unique();
        assert_eq!(
            root_pda(&settlement_program, 7),
            rome_zk_layouts::root::pda(&settlement_program, 7)
        );
    }
}
