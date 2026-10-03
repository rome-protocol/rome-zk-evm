use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    keccak, msg,
    program_error::ProgramError,
    pubkey::Pubkey,
};
// `system_program` moved out of `solana_program`'s root re-export in the Agave
// 4.x line (API fallout).
use solana_system_interface::program as system_program;

pub mod batch;

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

// Chunk header constants ("magic u32 | authority [u8;32] | chain_id u64 | batch u64 | idx u32 |
// len u32 | sealed u8 | pad") — the single definitions are `rome_zk_layouts::chunk`; re-exported
// under these names so every existing call site (this program's own `read_header`,
// `zk-inbox-client`'s `getProgramAccounts` memcmp filters, tests) is unchanged.
pub use rome_zk_layouts::chunk::{
    HEADER_LEN, MAGIC, OFF_AUTHORITY, OFF_BATCH, OFF_CHAIN_ID, OFF_IDX, OFF_LEN, OFF_MAGIC,
    OFF_SEALED,
};

#[derive(BorshSerialize, BorshDeserialize, Debug)]
pub enum InboxIx {
    /// accounts: [payer (signer, writable), pda (writable), batch pda (read-only), system_program] —
    /// the batch pda must already exist (`OpenBatch`) and `payer` must be its `authority`:
    /// otherwise anyone could pre-create a chunk PDA for someone else's batch id.
    Open {
        chain_id: u64,
        batch: u64,
        idx: u32,
        size: u32,
    },
    /// accounts: [authority (signer), pda (writable)]
    Write { offset: u32, data: Vec<u8> },
    /// accounts: [authority (signer), pda (writable)] — `body_hash` must equal
    /// `keccak256(body[..len])`: the program itself checks the bytes were actually
    /// written, so a short-seal (a hole left over from a Write the client skipped) is rejected at the
    /// core rather than only prevented client-side. The old `Seal { len }` shape (no `body_hash`) can
    /// never deserialize into this one — borsh requires every byte consumed, and the old 4-byte payload
    /// is 32 bytes short — so it is rejected as `InvalidInstructionData`, never silently accepted.
    Seal { len: u32, body_hash: [u8; 32] },
    /// accounts: [authority (signer, writable), pda (writable), batch pda (read-only), root pda
    /// (read-only)] — lamports back to authority, account emptied. If the batch pda exists (owned by
    /// this program), requires it to be `finalized` and its covering root to be final:
    /// an authority-only close is no longer sufficient on its own, since closing early
    /// would delete DA the settlement layer might still need to verify against. If the batch pda does
    /// not exist — never posted, or `AbandonBatch`ed — the chunk authority alone may close it: nothing
    /// was ever posted for that batch id, so there is no DA left to protect.
    Close,
    // --- accumulator, appended so the four discriminants above never move ---
    /// accounts: [payer (signer, writable), batch pda (writable), root pda (read-only, owned by
    /// `settlement_program`), cursor pda (writable, `["batch_cursor", chain_id]`), system_program] —
    /// `payer` must be the chain's `authority` from the root account: otherwise anyone could pre-create
    /// a batch id with a wrong `expected_count`/`settlement_program` and lock the real batcher out of
    /// it forever. `batch` must equal the cursor's `next_batch`, which this instruction then increments
    /// — ids are sequential and never reused, so an id `AbandonBatch`ed can never be re-opened (closed
    /// at the core). Creates the batch account at `min(account_len(expected_count), 10,240)` bytes
    /// (`MAX_PERMITTED_DATA_INCREASE`); `GrowBatch` grows it the rest of the way.
    OpenBatch {
        chain_id: u64,
        batch: u64,
        expected_count: u32,
        settlement_program: Pubkey,
    },
    /// accounts: [batch pda (writable), chunk pda (read-only)] — permissionless.
    SealLeaf { idx: u32 },
    /// accounts: [batch pda (writable), authority (signer, read-only)] — authority-gated:
    /// the signer must be the batch's stored `authority` (the value `OpenBatch` wrote), or a
    /// third party could finalize a later batch id ahead of the real poster's own, stranding it (the
    /// batch pda stays at account index 0, unchanged for every existing reader). `step` = 0 means "all
    /// remaining".
    FinalizeBatch { step: u32 },
    /// accounts: [authority (signer, writable), batch pda (writable), root pda (read-only)]
    CloseBatch,
    /// accounts: [authority (signer, writable), batch pda (writable)] — authority-only, only while
    /// `finalized == 0` (a finalized batch can only leave via `CloseBatch`, which requires the covering
    /// root to be final); returns rent for a batch id that was opened but never posted.
    AbandonBatch,
    // --- growth + cursor, appended so the nine discriminants above never move ---
    /// accounts: [payer (signer, writable), batch pda (writable), system_program (read-only)] —
    /// permissionless: any payer may top up the rent for and realloc a batch account created undersized
    /// by `OpenBatch` (`OpenBatch`'s single `create_account` CPI is capped at
    /// `MAX_PERMITTED_DATA_INCREASE` = 10,240 bytes, so an `expected_count` above 312 leaves needs one or
    /// more `GrowBatch` calls, each its own top-level instruction — and therefore its own fresh
    /// 10,240-byte realloc allowance — to reach `account_len(expected_count)`). Idempotent: a call once
    /// the account is already at `account_len(expected_count)` is a no-op `Ok`; nothing can grow the
    /// account past that size or shrink it.
    GrowBatch { chain_id: u64, batch: u64 },
    /// accounts: [payer (signer, writable), cursor pda (writable), root pda (read-only), system_program
    /// (read-only)] — authority-gated (same check `OpenBatch` uses: signer must be the chain's
    /// `authority` per the settlement root account). Bootstraps the per-chain `batch_cursor` PDA at
    /// `next_batch`; fails (`CursorAlreadyInitialized`) if the cursor already holds real data for this
    /// chain (checked by owner + `data_len`, not by `create_account`'s own error: a merely pre-funded,
    /// still-empty PDA must still bootstrap normally) — run once per chain. Initialise it at exactly
    /// `root.head_pending_batch + 1` of the chain's settlement root: settlement posts only that id next,
    /// so a lower value could re-open a stale id and a higher one leaves the chain waiting for a batch
    /// id the cursor has already passed.
    InitBatchCursor {
        chain_id: u64,
        next_batch: u64,
        settlement_program: Pubkey,
    },
}

/// Errors specific to the chunk lane (mapped to `ProgramError::Custom`, own namespace from
/// `batch::BatchError` — each is only ever inspected in the context of the instruction that returned it).
/// Numbered from 100 so this namespace never collides with `batch::BatchError`'s own 1-based range
/// (`ChunkError::SealHashMismatch = 1` used to alias `BatchError::IdxOutOfRange = 1`, both `Custom(1)`).
#[repr(u32)]
pub enum ChunkError {
    /// `Seal`'s `body_hash` does not equal `keccak256(body[..len])` — the account's bytes don't match
    /// what the client claims it wrote (a short-seal is unconstructable at the core).
    SealHashMismatch = 100,
    /// `Seal` on an already-sealed chunk with a different `len` than what is already stored — a sealed
    /// chunk is immutable in bytes **and** length (an authority re-Sealing a shorter
    /// `len` after `SealLeaf` would make Solana DA stop reproducing the committed leaf, undetectable at
    /// `PostRoot`). A re-Seal with the *same* `len` (and therefore, once hashed below, the same bytes) is
    /// not rejected here — it falls through to the ordinary hash check and stays an idempotent `Ok`, which
    /// is what the batcher's resubmit path relies on.
    AlreadySealed = 101,
}
impl From<ChunkError> for ProgramError {
    fn from(e: ChunkError) -> Self {
        ProgramError::Custom(e as u32)
    }
}

/// The chunk PDA's seeds, `["inbox", settlement_program, chain_id, batch, idx]` — the single definition is `rome_zk_layouts::chunk::seeds`;
/// kept under this name so every existing call site (this program's own `Open`, plus `zk-inbox-client`,
/// plus tests) is unchanged.
#[inline]
pub fn pda_seeds(settlement_program: &Pubkey, chain_id: u64, batch: u64, idx: u32) -> [Vec<u8>; 5] {
    rome_zk_layouts::chunk::seeds(&settlement_program.to_bytes(), chain_id, batch, idx)
}

/// Decodes a chunk account's fixed header via `rome_zk_layouts::chunk::read` (the single definition of
/// this layout, shared with `zk-inbox-client::decode_chunk_header`).
///
/// Returns `(authority, chain_id, batch, len, sealed)`. `chain_id`/`batch` are needed by the new
/// `Close`'s finality check (`batch.rs::close_chunk_check`) to find the right batch account; `SealLeaf`
/// (also in `batch.rs`) reads them via [`pda_seeds`] instead since it starts from the batch account.
#[inline]
pub(crate) fn read_header(d: &[u8]) -> Result<(Pubkey, u64, u64, u32, bool), ProgramError> {
    let f = rome_zk_layouts::chunk::read(d).map_err(|_| ProgramError::InvalidAccountData)?;
    Ok((
        Pubkey::new_from_array(f.authority),
        f.chain_id,
        f.batch,
        f.len,
        f.sealed,
    ))
}

pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let ix = InboxIx::try_from_slice(data).map_err(|_| ProgramError::InvalidInstructionData)?;
    let it = &mut accounts.iter();
    match ix {
        InboxIx::Open {
            chain_id,
            batch,
            idx,
            size,
        } => {
            let payer = next_account_info(it)?;
            let pda = next_account_info(it)?;
            let batch_pda = next_account_info(it)?;
            let sys = next_account_info(it)?;
            if !payer.is_signer || *sys.key != system_program::id() {
                return Err(ProgramError::MissingRequiredSignature);
            }
            // The chunk is keyed by the settlement program the batch was opened through, which the batch
            // account records (`open_chunk_check` returns it after tying the batch address to it).
            let settlement_program =
                batch::open_chunk_check(program_id, chain_id, batch, idx, batch_pda, payer.key)?;
            let seeds = pda_seeds(&settlement_program, chain_id, batch, idx);
            let (expect, bump) = Pubkey::find_program_address(
                &[&seeds[0], &seeds[1], &seeds[2], &seeds[3], &seeds[4]],
                program_id,
            );
            if expect != *pda.key {
                return Err(ProgramError::InvalidSeeds);
            }
            let space = HEADER_LEN + size as usize;
            // A chunk PDA's address is just as public and predictable as a batch PDA's — an attacker can
            // pre-fund it before the real batch authority's `Open` lands, and with sequential batch ids the
            // batcher can no longer just skip the touched index. `create_or_adopt_pda` closes this the same
            // way `OpenBatch` does: adopt a pre-funded (still system-owned, empty) PDA instead of failing
            // `AccountAlreadyInUse` forever.
            rome_zk_pda::create_or_adopt_pda(
                payer,
                pda,
                sys,
                program_id,
                space,
                &[
                    &seeds[0],
                    &seeds[1],
                    &seeds[2],
                    &seeds[3],
                    &seeds[4],
                    &[bump],
                ],
            )?;
            let mut d = pda.try_borrow_mut_data()?;
            d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
            d[OFF_AUTHORITY..OFF_AUTHORITY + 32].copy_from_slice(payer.key.as_ref());
            d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].copy_from_slice(&chain_id.to_le_bytes());
            d[OFF_BATCH..OFF_BATCH + 8].copy_from_slice(&batch.to_le_bytes());
            d[OFF_IDX..OFF_IDX + 4].copy_from_slice(&idx.to_le_bytes());
            Ok(())
        }
        InboxIx::Write {
            offset,
            data: bytes,
        } => {
            let auth = next_account_info(it)?;
            let pda = next_account_info(it)?;
            if pda.owner != program_id {
                return Err(ProgramError::IncorrectProgramId);
            }
            let mut d = pda.try_borrow_mut_data()?;
            let (authority, _, _, _, sealed) = read_header(&d)?;
            if !auth.is_signer || *auth.key != authority {
                return Err(ProgramError::MissingRequiredSignature);
            }
            if sealed {
                return Err(ProgramError::InvalidAccountData);
            }
            let start = HEADER_LEN + offset as usize;
            let end = start + bytes.len();
            if end > d.len() {
                return Err(ProgramError::AccountDataTooSmall);
            }
            d[start..end].copy_from_slice(&bytes);
            Ok(())
        }
        InboxIx::Seal { len, body_hash } => {
            let auth = next_account_info(it)?;
            let pda = next_account_info(it)?;
            if pda.owner != program_id {
                return Err(ProgramError::IncorrectProgramId);
            }
            let mut d = pda.try_borrow_mut_data()?;
            let (authority, _, _, stored_len, sealed) = read_header(&d)?;
            if !auth.is_signer || *auth.key != authority {
                return Err(ProgramError::MissingRequiredSignature);
            }
            // A sealed chunk is immutable in bytes AND length: once `SealLeaf` has
            // read a leaf's hash from this account, a re-Seal that changes `len` would make Solana DA
            // stop reproducing that committed leaf — undetectable at `PostRoot` (operator data
            // withholding). A re-Seal with the *same* `len` is the batcher's own idempotent resubmit
            // path and falls through to the ordinary hash check below, which — since the bytes are
            // unchanged — passes and leaves the account exactly as it already was.
            if sealed && len != stored_len {
                return Err(ChunkError::AlreadySealed.into());
            }
            if HEADER_LEN + len as usize > d.len() {
                return Err(ProgramError::AccountDataTooSmall);
            }
            // The bytes must actually be what the client claims: a short-seal — a
            // hole left where `Write` never landed — hashes differently from the client's claimed
            // `body_hash`, so it is rejected here rather than only being a client-side bound. One
            // `keccak::hashv` syscall over up to 3,681 B (the largest chunk body).
            let body = &d[HEADER_LEN..HEADER_LEN + len as usize];
            if keccak::hashv(&[body]).to_bytes() != body_hash {
                return Err(ChunkError::SealHashMismatch.into());
            }
            d[OFF_LEN..OFF_LEN + 4].copy_from_slice(&len.to_le_bytes());
            d[OFF_SEALED] = 1;
            Ok(())
        }
        InboxIx::Close => {
            let auth = next_account_info(it)?;
            let pda = next_account_info(it)?;
            let batch_pda = next_account_info(it)?;
            let root_pda = next_account_info(it)?;
            if pda.owner != program_id {
                return Err(ProgramError::IncorrectProgramId);
            }
            let (chain_id, batch, idx) = {
                let d = pda.try_borrow_data()?;
                let (authority, chain_id, batch, _, _) = read_header(&d)?;
                if !auth.is_signer || *auth.key != authority {
                    return Err(ProgramError::MissingRequiredSignature);
                }
                let idx = u32::from_le_bytes(d[OFF_IDX..OFF_IDX + 4].try_into().unwrap());
                (chain_id, batch, idx)
            };
            // Every inbox account of a chain is keyed by the settlement program that owns the chain. The
            // settlement program here is the owner of the root account the caller passed; the chunk's own
            // address, the batch slot's address and the root's address must all derive from it, so a root
            // under any other program can only ever reach that program's own (disjoint) accounts, never
            // this chain's. (The root's own data is read only on the live-batch path below.)
            let settlement_program = *root_pda.owner;
            let chunk_key =
                rome_zk_layouts::chunk::pda(program_id, &settlement_program, chain_id, batch, idx)
                    .0;
            if chunk_key != *pda.key {
                return Err(ProgramError::InvalidSeeds);
            }
            // If the batch pda is absent (never created, or `AbandonBatch`ed — both leave it owned by
            // the system program), nothing was ever posted for this batch id, so the chunk authority
            // (already verified above) may close unconditionally. Otherwise, the usual
            // finalized + final-root check applies.
            // The batch slot must be THE batch PDA for this (settlement program, chain, batch) regardless of
            // who owns it now: a system-owned account here means "abandoned" only if it is that PDA's
            // address. Without the address check the chunk authority could pass any system account and
            // delete DA of a live batch.
            let batch_key =
                rome_zk_layouts::batch::pda(program_id, &settlement_program, chain_id, batch).0;
            if batch_key != *batch_pda.key {
                return Err(batch::BatchError::WrongBatchAccount.into());
            }
            if *batch_pda.owner == system_program::id() {
                // Abandoned: nothing to read from the batch account, so the settlement program above is
                // taken from the root account's owner and the root must be that program's own root for
                // this chain (its address alone is checked; a root under another program cannot reach
                // this chain's chunk, because the chunk address above is derived from that program).
                let root_key = rome_zk_layouts::root::pda(&settlement_program, chain_id).0;
                if root_key != *root_pda.key {
                    return Err(ProgramError::InvalidSeeds);
                }
            } else {
                batch::close_chunk_check(
                    program_id,
                    &settlement_program,
                    chain_id,
                    batch,
                    batch_pda,
                    root_pda,
                )?;
            }
            let lamports = pda.lamports();
            **pda.try_borrow_mut_lamports()? = 0;
            **auth.try_borrow_mut_lamports()? += lamports;
            // `AccountInfo::realloc(new_len, zero_init)` was renamed to `resize(new_len)` and always
            // zeroes newly-added bytes (API fallout) — a no-op difference here since this call only
            // ever shrinks to 0.
            pda.resize(0)?;
            pda.assign(&system_program::id());
            msg!("inbox chunk closed, {} lamports reclaimed", lamports);
            Ok(())
        }
        InboxIx::OpenBatch {
            chain_id,
            batch,
            expected_count,
            settlement_program,
        } => batch::open_batch(
            program_id,
            accounts,
            chain_id,
            batch,
            expected_count,
            &settlement_program,
        ),
        InboxIx::SealLeaf { idx } => batch::seal_leaf(program_id, accounts, idx),
        InboxIx::FinalizeBatch { step } => batch::finalize_batch(program_id, accounts, step),
        InboxIx::CloseBatch => batch::close_batch(program_id, accounts),
        InboxIx::AbandonBatch => batch::abandon_batch(program_id, accounts),
        InboxIx::GrowBatch { chain_id, batch } => {
            batch::grow_batch(program_id, accounts, chain_id, batch)
        }
        InboxIx::InitBatchCursor {
            chain_id,
            next_batch,
            settlement_program,
        } => batch::init_batch_cursor(
            program_id,
            accounts,
            chain_id,
            next_batch,
            &settlement_program,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The deployed program (`EtAXw56B…`) is upgraded in place from the CI artifact — an
    /// existing instruction's borsh discriminant (its variant index) must never move, or an
    /// already-signed or already-queued `Open`/`Write`/`Seal`/`Close` instruction would be decoded as a
    /// different variant after the upgrade. Pins all eleven variants' first byte.
    #[test]
    fn instruction_discriminants_are_pinned() {
        let cases: Vec<(u8, InboxIx)> = vec![
            (
                0,
                InboxIx::Open {
                    chain_id: 0,
                    batch: 0,
                    idx: 0,
                    size: 0,
                },
            ),
            (
                1,
                InboxIx::Write {
                    offset: 0,
                    data: vec![],
                },
            ),
            (
                2,
                InboxIx::Seal {
                    len: 0,
                    body_hash: [0u8; 32],
                },
            ),
            (3, InboxIx::Close),
            (
                4,
                InboxIx::OpenBatch {
                    chain_id: 0,
                    batch: 0,
                    expected_count: 0,
                    settlement_program: Pubkey::default(),
                },
            ),
            (5, InboxIx::SealLeaf { idx: 0 }),
            (6, InboxIx::FinalizeBatch { step: 0 }),
            (7, InboxIx::CloseBatch),
            (8, InboxIx::AbandonBatch),
            (
                9,
                InboxIx::GrowBatch {
                    chain_id: 0,
                    batch: 0,
                },
            ),
            (
                10,
                InboxIx::InitBatchCursor {
                    chain_id: 0,
                    next_batch: 0,
                    settlement_program: Pubkey::default(),
                },
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

    /// The old `Seal { len }` wire shape (discriminant `2` followed by just a 4-byte `len`, no
    /// `body_hash`) must never deserialize into the new `Seal { len, body_hash }` — borsh requires every
    /// byte of the instruction data to be consumed, and the old payload is 32 bytes short of what the
    /// new shape needs, so it is rejected as a decode error rather than silently accepted with a
    /// zeroed/garbage `body_hash`.
    #[test]
    fn old_seal_wire_shape_without_body_hash_is_rejected_not_accepted() {
        let mut old_encoding = vec![2u8]; // Seal discriminant
        old_encoding.extend_from_slice(&7u32.to_le_bytes()); // len = 7, nothing else
        assert!(
            InboxIx::try_from_slice(&old_encoding).is_err(),
            "the old 5-byte Seal payload must not deserialize into the new 36-byte shape"
        );
    }

    #[test]
    fn pda_seeds_are_deterministic_and_distinct_per_idx() {
        let sp = Pubkey::new_from_array([0x5Au8; 32]);
        assert_eq!(pda_seeds(&sp, 1, 2, 3), pda_seeds(&sp, 1, 2, 3));
        assert_ne!(pda_seeds(&sp, 1, 2, 3), pda_seeds(&sp, 1, 2, 4));
        let other = Pubkey::new_from_array([0x5Bu8; 32]);
        assert_ne!(pda_seeds(&sp, 1, 2, 3), pda_seeds(&other, 1, 2, 3));
    }

    /// `zk-inbox-client::find_max_batch_id`'s `getProgramAccounts` memcmp filters read `chain_id`/`batch`
    /// at these exact offsets — pinned here (numerically, not just "whatever `read_header` happens to
    /// use") so a future header shape change is caught at the one place that would otherwise silently
    /// break an off-chain scanner nothing else here would notice.
    #[test]
    fn chunk_header_offsets_are_pinned_for_the_off_chain_memcmp_scanner() {
        assert_eq!(OFF_MAGIC, 0);
        assert_eq!(OFF_AUTHORITY, 4);
        assert_eq!(OFF_CHAIN_ID, 36);
        assert_eq!(OFF_BATCH, 44);
        assert_eq!(OFF_IDX, 52);
        assert_eq!(OFF_LEN, 56);
        assert_eq!(OFF_SEALED, 60);
    }

    /// `read_header` must decode `chain_id`/`batch` from exactly the pinned offsets above — proves the
    /// constants and the actual header layout have not drifted apart.
    #[test]
    fn read_header_decodes_chain_id_and_batch_from_the_pinned_offsets() {
        let mut d = vec![0u8; HEADER_LEN];
        d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
        d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].copy_from_slice(&7u64.to_le_bytes());
        d[OFF_BATCH..OFF_BATCH + 8].copy_from_slice(&9u64.to_le_bytes());
        let (_authority, chain_id, batch, _len, _sealed) = read_header(&d).unwrap();
        assert_eq!(chain_id, 7);
        assert_eq!(batch, 9);
    }

    /// `ChunkError`'s own namespace must never collide with `batch::BatchError`'s (1..=14) — pins both
    /// ranges apart so a future variant added to either enum without checking the other is caught here,
    /// not by an ambiguous `Custom(n)` in a program-test.
    #[test]
    fn chunk_error_namespace_never_collides_with_batch_error() {
        assert_eq!(ChunkError::SealHashMismatch as u32, 100);
        assert_eq!(ChunkError::AlreadySealed as u32, 101);
        assert!(
            (ChunkError::SealHashMismatch as u32) > 14,
            "ChunkError must stay clear of batch::BatchError's 1..=14 range"
        );
    }

    /// PDA parity: this program's own `pda_seeds` must equal `rome_zk_layouts::chunk::seeds` — the
    /// single definition every consumer (this program, `zk-inbox-client`, `zk-settlement`) is
    /// required to share. A drift here means the program and a client would derive different chunk
    /// addresses for the same `(chain_id, batch, idx)`.
    #[test]
    fn pda_seeds_matches_rome_zk_layouts_chunk_seeds() {
        let sp = Pubkey::new_from_array([0x5Au8; 32]);
        assert_eq!(
            pda_seeds(&sp, 7, 3, 2),
            rome_zk_layouts::chunk::seeds(&[0x5Au8; 32], 7, 3, 2)
        );
    }
}
