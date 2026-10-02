//! The zk-settlement pending-batch account layout (PDA `["pending", chain_id, batch]`
//! under the settlement program). One account per posted batch, created by `PostRoot`/`PostRootProved`
//! and recycled by `ClosePending` once the batch is final and no longer the head.
//!
//! No magic/version byte, unlike the root and batch accounts: this account's identity comes entirely
//! from its PDA seeds (`chain_id`, `batch`), which every caller already holds to derive the address —
//! there is no scenario where a caller has the address but not the seeds it was derived from.
//!
//! ```text
//! batch u64 | prev_batch u64 | pre_state_root [32] | first_block u64 | last_block u64 | state_root [32]
//! | block_roots_merkle [32] | inbox_commitment [32] | forced_outcome_commitment [32] | posted_slot u64
//! | status u8 | disputes_open u16 | deadline_slot u64 | parent_hash [32] | last_block_hash [32]
//! ```
//! All integers little-endian. `status`: 0 = Pending, 1 = Final, 2 = Rejected (reserved — the challenge
//! flow that produces it is a later addition; this crate only reserves the byte value).
//!
//! `parent_hash`/`last_block_hash`: every posted batch — window-elapsed or
//! immediately proved — carries its own block-hash pair so `FinalizeBatch` can write a complete,
//! per-batch-consistent `{number, parent_hash, block_hash, state_root}` tuple into the root account
//! regardless of which path finalized it, instead of leaving the window path's tuple stale. Appended
//! after `deadline_slot` so every existing offset above is unchanged.

pub const STATUS_PENDING: u8 = 0;
pub const STATUS_FINAL: u8 = 1;
pub const STATUS_REJECTED: u8 = 2;

pub const OFF_BATCH: usize = 0;
pub const OFF_PREV_BATCH: usize = 8;
pub const OFF_PRE_STATE_ROOT: usize = 16;
pub const OFF_FIRST_BLOCK: usize = 48;
pub const OFF_LAST_BLOCK: usize = 56;
pub const OFF_STATE_ROOT: usize = 64;
pub const OFF_BLOCK_ROOTS_MERKLE: usize = 96;
pub const OFF_INBOX_COMMITMENT: usize = 128;
pub const OFF_FORCED_OUTCOME_COMMITMENT: usize = 160;
pub const OFF_POSTED_SLOT: usize = 192;
pub const OFF_STATUS: usize = 200;
pub const OFF_DISPUTES_OPEN: usize = 201;
pub const OFF_DEADLINE_SLOT: usize = 203;
pub const OFF_PARENT_HASH: usize = 211;
pub const OFF_LAST_BLOCK_HASH: usize = 243;
/// Full fixed-size account length (no variable-length tail).
pub const PENDING_LEN: usize = 275;

/// `["pending", chain_id, batch]`.
#[inline]
pub fn seeds(chain_id: u64, batch: u64) -> [Vec<u8>; 3] {
    [
        b"pending".to_vec(),
        chain_id.to_le_bytes().to_vec(),
        batch.to_le_bytes().to_vec(),
    ]
}

/// Derives the pending-batch PDA under `program_id` (the settlement program) — the one place this
/// derivation is computed.
#[cfg(feature = "solana")]
#[inline]
pub fn pda(
    program_id: &solana_program::pubkey::Pubkey,
    chain_id: u64,
    batch: u64,
) -> (solana_program::pubkey::Pubkey, u8) {
    let s = seeds(chain_id, batch);
    solana_program::pubkey::Pubkey::find_program_address(&[&s[0], &s[1], &s[2]], program_id)
}

/// Field-for-field decode of a pending-batch account. Pubkeys/hashes are raw `[u8; 32]` (only `pda` above needs the real type) —
/// callers hold `program_id`/seeds and must check the account's owner and address themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingFields {
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

/// Validates minimum length and decodes every field. No magic to check (see module doc).
pub fn read(d: &[u8]) -> Result<PendingFields, crate::LayoutError> {
    if d.len() < PENDING_LEN {
        return Err(crate::LayoutError::TooShort {
            need: PENDING_LEN,
            got: d.len(),
        });
    }
    let u16_at = |o: usize| u16::from_le_bytes(d[o..o + 2].try_into().unwrap());
    let u64_at = |o: usize| u64::from_le_bytes(d[o..o + 8].try_into().unwrap());
    let b32_at = |o: usize| -> [u8; 32] { d[o..o + 32].try_into().unwrap() };
    Ok(PendingFields {
        batch: u64_at(OFF_BATCH),
        prev_batch: u64_at(OFF_PREV_BATCH),
        pre_state_root: b32_at(OFF_PRE_STATE_ROOT),
        first_block: u64_at(OFF_FIRST_BLOCK),
        last_block: u64_at(OFF_LAST_BLOCK),
        state_root: b32_at(OFF_STATE_ROOT),
        block_roots_merkle: b32_at(OFF_BLOCK_ROOTS_MERKLE),
        inbox_commitment: b32_at(OFF_INBOX_COMMITMENT),
        forced_outcome_commitment: b32_at(OFF_FORCED_OUTCOME_COMMITMENT),
        posted_slot: u64_at(OFF_POSTED_SLOT),
        status: d[OFF_STATUS],
        disputes_open: u16_at(OFF_DISPUTES_OPEN),
        deadline_slot: u64_at(OFF_DEADLINE_SLOT),
        parent_hash: b32_at(OFF_PARENT_HASH),
        last_block_hash: b32_at(OFF_LAST_BLOCK_HASH),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(batch: u64, status: u8) -> Vec<u8> {
        let mut d = vec![0u8; PENDING_LEN];
        d[OFF_BATCH..OFF_BATCH + 8].copy_from_slice(&batch.to_le_bytes());
        d[OFF_STATUS] = status;
        d
    }

    #[test]
    fn read_round_trips_a_hand_built_account() {
        let d = build(5, STATUS_FINAL);
        let f = read(&d).unwrap();
        assert_eq!(f.batch, 5);
        assert_eq!(f.status, STATUS_FINAL);
    }

    #[test]
    fn read_round_trips_parent_hash_and_last_block_hash() {
        let mut d = build(5, STATUS_FINAL);
        d[OFF_PARENT_HASH..OFF_PARENT_HASH + 32].copy_from_slice(&[0x11u8; 32]);
        d[OFF_LAST_BLOCK_HASH..OFF_LAST_BLOCK_HASH + 32].copy_from_slice(&[0x22u8; 32]);
        let f = read(&d).unwrap();
        assert_eq!(f.parent_hash, [0x11u8; 32]);
        assert_eq!(f.last_block_hash, [0x22u8; 32]);
    }

    #[test]
    fn read_rejects_too_short() {
        let d = vec![0u8; PENDING_LEN - 1];
        assert!(matches!(
            read(&d).unwrap_err(),
            crate::LayoutError::TooShort { .. }
        ));
    }

    #[test]
    fn offsets_do_not_overlap_and_fit_len() {
        // Every field's [off, off+size) must stay inside PENDING_LEN and not clobber a neighbor —
        // regression guard for a future field insertion.
        let spans = [
            (OFF_BATCH, 8),
            (OFF_PREV_BATCH, 8),
            (OFF_PRE_STATE_ROOT, 32),
            (OFF_FIRST_BLOCK, 8),
            (OFF_LAST_BLOCK, 8),
            (OFF_STATE_ROOT, 32),
            (OFF_BLOCK_ROOTS_MERKLE, 32),
            (OFF_INBOX_COMMITMENT, 32),
            (OFF_FORCED_OUTCOME_COMMITMENT, 32),
            (OFF_POSTED_SLOT, 8),
            (OFF_STATUS, 1),
            (OFF_DISPUTES_OPEN, 2),
            (OFF_DEADLINE_SLOT, 8),
            (OFF_PARENT_HASH, 32),
            (OFF_LAST_BLOCK_HASH, 32),
        ];
        let mut sorted = spans;
        sorted.sort_by_key(|(o, _)| *o);
        let mut cursor = 0usize;
        for (off, len) in sorted {
            assert!(off >= cursor, "field at {off} overlaps previous field");
            cursor = off + len;
        }
        assert!(cursor <= PENDING_LEN);
    }

    #[test]
    fn seeds_golden_bytes_for_fixed_inputs() {
        let s = seeds(0x0102_0304_0506_0708, 9);
        assert_eq!(s[0], b"pending".to_vec());
        assert_eq!(s[1], vec![0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
        assert_eq!(s[2], vec![9, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[cfg(feature = "solana")]
    #[test]
    fn pda_is_deterministic_and_varies_with_batch() {
        let program = solana_program::pubkey::Pubkey::new_unique();
        let (a1, _) = pda(&program, 7, 1);
        let (a2, _) = pda(&program, 7, 1);
        assert_eq!(a1, a2);
        let (b, _) = pda(&program, 7, 2);
        assert_ne!(a1, b);
    }
}
