//! The zk-settlement root account layout ("ZKRT", PDA `["root", chain_id]` under the settlement program). zk-inbox
//! reads this account (never writes it) for two checks: `OpenBatch` requires its signer to be `authority`, and
//! `CloseBatch`/chunk `Close` require `head_final_batch >= batch`. zk-settlement is the account's owner and creates it
//! with this layout (`InitChainV2`). An older 88-byte root account, from before this layout, is too short for this
//! layout, so these reads reject it rather than misread it (intentional, not a bug).
//!
//! ```text
//! magic 'ZKRT' u32 | chain_id u64 | number u64 | parent_hash [32] | state_root [32] | block_hash [32]
//! | updates u32 | profile u8 | challenge_window_slots u32 | prove_window_slots u32 | proving_policy u8
//! | poster_bond u64 | exit_cap_per_window u64 | authority [32] | head_pending_batch u64
//! | head_final_batch u64 | pending_count u32 | max_pending u32
//! ```
//! All integers little-endian.
//!
//! **Units (exits):** `exit_cap_per_window` counts **gwei of the native asset per challenge window** — `0`
//! means exits are disabled for this chain (fail-closed default); native asset only in v1, per-asset caps are a
//! later seam. `poster_bond` counts **lamports** — a number only for now (the escrow and the cap/bond unit
//! reconciliation ship later; nothing here enforces a relation between the two fields' units today). Both are
//! written by `InitChainV2`/`MigrateChainV2` and, through `ProposeExitConfig`/`ActivateExitConfig`, by that
//! activation-delayed governance path — this crate only defines the bytes.

pub const MAGIC: u32 = 0x5a4b_5254; // "ZKRT"

pub const OFF_MAGIC: usize = 0;
pub const OFF_CHAIN_ID: usize = 4;
pub const OFF_NUMBER: usize = 12;
pub const OFF_PARENT_HASH: usize = 20;
pub const OFF_STATE_ROOT: usize = 52;
pub const OFF_BLOCK_HASH: usize = 84;
pub const OFF_UPDATES: usize = 116;
pub const OFF_PROFILE: usize = 120;
pub const OFF_CHALLENGE_WINDOW_SLOTS: usize = 121;
pub const OFF_PROVE_WINDOW_SLOTS: usize = 125;
pub const OFF_PROVING_POLICY: usize = 129;
pub const OFF_POSTER_BOND: usize = 130;
pub const OFF_EXIT_CAP_PER_WINDOW: usize = 138;
pub const OFF_AUTHORITY: usize = 146;
pub const OFF_HEAD_PENDING_BATCH: usize = 178;
pub const OFF_HEAD_FINAL_BATCH: usize = 186;
pub const OFF_PENDING_COUNT: usize = 194;
pub const OFF_MAX_PENDING: usize = 198;
/// Full fixed-size account length (no variable-length tail in this layout).
pub const MIN_LEN: usize = 202;

/// `["root", chain_id]` — the seeds `find_program_address` (and every `invoke_signed` that must sign as
/// this PDA) needs, in order.
#[inline]
pub fn seeds(chain_id: u64) -> [Vec<u8>; 2] {
    [b"root".to_vec(), chain_id.to_le_bytes().to_vec()]
}

/// Derives the root PDA under `program_id` (the settlement program). The one place this derivation is
/// computed — `programs/zk-settlement`, `programs/zk-inbox` (which only reads this account) and every
/// client call this instead of recomputing the seeds.
#[cfg(feature = "solana")]
#[inline]
pub fn pda(
    program_id: &solana_program::pubkey::Pubkey,
    chain_id: u64,
) -> (solana_program::pubkey::Pubkey, u8) {
    let s = seeds(chain_id);
    solana_program::pubkey::Pubkey::find_program_address(&[&s[0], &s[1]], program_id)
}

/// Field-for-field decode of a root account. Pubkeys/hashes are raw `[u8; 32]` in every field below —
/// only [`pda`] needs the real `solana_program::pubkey::Pubkey` type. Does not check the account's owner
/// or PDA seeds — callers hold `program_id`/`settlement_program` and must check those themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootFields {
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
    pub authority: [u8; 32],
    pub head_pending_batch: u64,
    pub head_final_batch: u64,
    pub pending_count: u32,
    pub max_pending: u32,
}

/// Validates magic + minimum length and decodes every field.
pub fn read(d: &[u8]) -> Result<RootFields, crate::LayoutError> {
    if d.len() < MIN_LEN {
        return Err(crate::LayoutError::TooShort {
            need: MIN_LEN,
            got: d.len(),
        });
    }
    if u32::from_le_bytes(d[OFF_MAGIC..OFF_MAGIC + 4].try_into().unwrap()) != MAGIC {
        return Err(crate::LayoutError::BadMagic);
    }
    let u32_at = |o: usize| u32::from_le_bytes(d[o..o + 4].try_into().unwrap());
    let u64_at = |o: usize| u64::from_le_bytes(d[o..o + 8].try_into().unwrap());
    let b32_at = |o: usize| -> [u8; 32] { d[o..o + 32].try_into().unwrap() };
    Ok(RootFields {
        chain_id: u64_at(OFF_CHAIN_ID),
        number: u64_at(OFF_NUMBER),
        parent_hash: b32_at(OFF_PARENT_HASH),
        state_root: b32_at(OFF_STATE_ROOT),
        block_hash: b32_at(OFF_BLOCK_HASH),
        updates: u32_at(OFF_UPDATES),
        profile: d[OFF_PROFILE],
        challenge_window_slots: u32_at(OFF_CHALLENGE_WINDOW_SLOTS),
        prove_window_slots: u32_at(OFF_PROVE_WINDOW_SLOTS),
        proving_policy: d[OFF_PROVING_POLICY],
        poster_bond: u64_at(OFF_POSTER_BOND),
        exit_cap_per_window: u64_at(OFF_EXIT_CAP_PER_WINDOW),
        authority: b32_at(OFF_AUTHORITY),
        head_pending_batch: u64_at(OFF_HEAD_PENDING_BATCH),
        head_final_batch: u64_at(OFF_HEAD_FINAL_BATCH),
        pending_count: u32_at(OFF_PENDING_COUNT),
        max_pending: u32_at(OFF_MAX_PENDING),
    })
}

/// Encodes a root account (the inverse of [`read`]). Returns exactly [`MIN_LEN`] bytes.
#[inline]
pub fn write(f: &RootFields) -> [u8; MIN_LEN] {
    let mut d = [0u8; MIN_LEN];
    d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
    d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].copy_from_slice(&f.chain_id.to_le_bytes());
    d[OFF_NUMBER..OFF_NUMBER + 8].copy_from_slice(&f.number.to_le_bytes());
    d[OFF_PARENT_HASH..OFF_PARENT_HASH + 32].copy_from_slice(&f.parent_hash);
    d[OFF_STATE_ROOT..OFF_STATE_ROOT + 32].copy_from_slice(&f.state_root);
    d[OFF_BLOCK_HASH..OFF_BLOCK_HASH + 32].copy_from_slice(&f.block_hash);
    d[OFF_UPDATES..OFF_UPDATES + 4].copy_from_slice(&f.updates.to_le_bytes());
    d[OFF_PROFILE] = f.profile;
    d[OFF_CHALLENGE_WINDOW_SLOTS..OFF_CHALLENGE_WINDOW_SLOTS + 4]
        .copy_from_slice(&f.challenge_window_slots.to_le_bytes());
    d[OFF_PROVE_WINDOW_SLOTS..OFF_PROVE_WINDOW_SLOTS + 4]
        .copy_from_slice(&f.prove_window_slots.to_le_bytes());
    d[OFF_PROVING_POLICY] = f.proving_policy;
    d[OFF_POSTER_BOND..OFF_POSTER_BOND + 8].copy_from_slice(&f.poster_bond.to_le_bytes());
    d[OFF_EXIT_CAP_PER_WINDOW..OFF_EXIT_CAP_PER_WINDOW + 8]
        .copy_from_slice(&f.exit_cap_per_window.to_le_bytes());
    d[OFF_AUTHORITY..OFF_AUTHORITY + 32].copy_from_slice(&f.authority);
    d[OFF_HEAD_PENDING_BATCH..OFF_HEAD_PENDING_BATCH + 8]
        .copy_from_slice(&f.head_pending_batch.to_le_bytes());
    d[OFF_HEAD_FINAL_BATCH..OFF_HEAD_FINAL_BATCH + 8]
        .copy_from_slice(&f.head_final_batch.to_le_bytes());
    d[OFF_PENDING_COUNT..OFF_PENDING_COUNT + 4].copy_from_slice(&f.pending_count.to_le_bytes());
    d[OFF_MAX_PENDING..OFF_MAX_PENDING + 4].copy_from_slice(&f.max_pending.to_le_bytes());
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(chain_id: u64, authority: [u8; 32], head_final_batch: u64) -> Vec<u8> {
        write(&RootFields {
            chain_id,
            number: 0,
            parent_hash: [0u8; 32],
            state_root: [0u8; 32],
            block_hash: [0u8; 32],
            updates: 0,
            profile: 0,
            challenge_window_slots: 0,
            prove_window_slots: 0,
            proving_policy: 0,
            poster_bond: 0,
            exit_cap_per_window: 0,
            authority,
            head_pending_batch: 0,
            head_final_batch,
            pending_count: 0,
            max_pending: 0,
        })
        .to_vec()
    }

    /// Byte-golden test (contract): a **literal** 202-byte vector (every byte position
    /// written as a number, never through `OFF_*`/`MAGIC`) decodes to the exact fields `write` encoded.
    #[test]
    fn write_produces_a_known_byte_vector() {
        let f = RootFields {
            chain_id: 11,
            number: 5,
            parent_hash: [0x66u8; 32],
            state_root: [0x77u8; 32],
            block_hash: [0x88u8; 32],
            updates: 2,
            profile: 1,
            challenge_window_slots: 100,
            prove_window_slots: 50,
            proving_policy: 3,
            poster_bond: 1_000,
            exit_cap_per_window: 2_000,
            authority: [9u8; 32],
            head_pending_batch: 4,
            head_final_batch: 42,
            pending_count: 6,
            max_pending: 10,
        };
        let d = write(&f);
        #[rustfmt::skip]
        let expected: [u8; MIN_LEN] = [
            // magic 'ZKRT' = 0x5a4b_5254 LE
            0x54, 0x52, 0x4b, 0x5a,
            // chain_id = 11 LE u64
            11, 0, 0, 0, 0, 0, 0, 0,
            // number = 5 LE u64
            5, 0, 0, 0, 0, 0, 0, 0,
            // parent_hash: 32 bytes of 0x66
            0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
            0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
            0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
            0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
            // state_root: 32 bytes of 0x77
            0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77,
            0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77,
            0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77,
            0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77,
            // block_hash: 32 bytes of 0x88
            0x88, 0x88, 0x88, 0x88, 0x88, 0x88, 0x88, 0x88,
            0x88, 0x88, 0x88, 0x88, 0x88, 0x88, 0x88, 0x88,
            0x88, 0x88, 0x88, 0x88, 0x88, 0x88, 0x88, 0x88,
            0x88, 0x88, 0x88, 0x88, 0x88, 0x88, 0x88, 0x88,
            // updates = 2 LE u32
            2, 0, 0, 0,
            // profile
            1,
            // challenge_window_slots = 100 LE u32
            100, 0, 0, 0,
            // prove_window_slots = 50 LE u32
            50, 0, 0, 0,
            // proving_policy
            3,
            // poster_bond = 1000 LE u64
            0xe8, 0x03, 0, 0, 0, 0, 0, 0,
            // exit_cap_per_window = 2000 LE u64
            0xd0, 0x07, 0, 0, 0, 0, 0, 0,
            // authority: 32 bytes of 9
            9, 9, 9, 9, 9, 9, 9, 9,
            9, 9, 9, 9, 9, 9, 9, 9,
            9, 9, 9, 9, 9, 9, 9, 9,
            9, 9, 9, 9, 9, 9, 9, 9,
            // head_pending_batch = 4 LE u64
            4, 0, 0, 0, 0, 0, 0, 0,
            // head_final_batch = 42 LE u64
            42, 0, 0, 0, 0, 0, 0, 0,
            // pending_count = 6 LE u32
            6, 0, 0, 0,
            // max_pending = 10 LE u32
            10, 0, 0, 0,
        ];
        assert_eq!(d, expected);
        assert_eq!(read(&d).unwrap(), f);
    }

    #[test]
    fn read_round_trips_a_hand_built_account() {
        let authority = [9u8; 32];
        let d = build(11, authority, 42);
        let f = read(&d).unwrap();
        assert_eq!(f.chain_id, 11);
        assert_eq!(f.authority, authority);
        assert_eq!(f.head_final_batch, 42);
    }

    #[test]
    fn read_rejects_bad_magic() {
        let mut d = build(1, [0u8; 32], 0);
        d[OFF_MAGIC] ^= 0xff;
        assert_eq!(read(&d).unwrap_err(), crate::LayoutError::BadMagic);
    }

    #[test]
    fn read_rejects_too_short() {
        // An older 88-byte zk-settlement root account (from before this layout): too short for this layout, by
        // design (module doc) — `read` must reject it, not panic on an out-of-bounds slice.
        let d = vec![0u8; 88];
        assert!(matches!(
            read(&d).unwrap_err(),
            crate::LayoutError::TooShort { .. }
        ));
    }

    /// Golden test (contract): pins `seeds`'s byte layout — a program tag then the chain id as 8
    /// little-endian bytes, nothing else — so a future reorder or endianness slip is caught here before
    /// it ever reaches the parity tests in the programs/clients that consume this function.
    #[test]
    fn seeds_golden_bytes_for_a_fixed_chain_id() {
        let s = seeds(0x0102_0304_0506_0708);
        assert_eq!(s[0], b"root".to_vec());
        assert_eq!(s[1], vec![0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
    }

    #[cfg(feature = "solana")]
    #[test]
    fn pda_is_deterministic_and_varies_with_chain_id_and_program() {
        let program = solana_program::pubkey::Pubkey::new_unique();
        let other_program = solana_program::pubkey::Pubkey::new_unique();
        let (a1, _) = pda(&program, 7);
        let (a2, _) = pda(&program, 7);
        assert_eq!(a1, a2, "same inputs must derive the same address");
        let (b, _) = pda(&program, 8);
        assert_ne!(
            a1, b,
            "a different chain id must derive a different address"
        );
        let (c, _) = pda(&other_program, 7);
        assert_ne!(
            a1, c,
            "a different program id must derive a different address"
        );
    }
}
