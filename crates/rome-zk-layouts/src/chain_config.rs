//! The zk-settlement per-chain config account layout ("ZKCC", PDA `["chain_config", chain_id]` under the
//! settlement program).
//!
//! Holds the registration fields that have no room in the existing 202-byte root account —
//! created new by `InitChainV2` for a newly registered chain, or by `MigrateChainV2` (registry-
//! authority-only) for a chain that predates this account (Tiber's one registry write) — deliberately kept
//! out of `root`/`registry` so neither of those accounts' `MIN_LEN`/`REGISTRY_LEN` ever changes.
//!
//! The deposit escrow (permissionless `InitChain` only) is not a separate field — it is the
//! account's own lamport balance above its rent-exempt minimum; `deposit_lamports` records how much of
//! that balance is the locked deposit (0 for a reserved-id chain, which locks none). `RefundDeposit`
//! zeroes it and sets `deposit_refunded`; `ReclaimChain` sweeps whatever lamports remain (rent included)
//! to the treasury and closes the account.
//!
//! "1 final root" (`RefundDeposit`'s first trigger) is read directly off the root account's own
//! `head_final_batch >= 1` — finality is strictly sequential starting at batch 1, so that field already
//! *is* the final-root count; a redundant counter here would be state that could drift from it for no
//! reason. `posted_batches` below has no such substitute (there is no existing "batches ever posted"
//! counter anywhere) and is incremented by `PostRoot`/`PostRootProved` on every call.
//!
//! ```text
//! magic 'ZKCC' u32 | version u8 | chain_id u64 | reserved u8 | deposit_lamports u64
//! | deposit_refunded u8 | registered_slot u64 | posted_batches u32 | fee_base_lamports u64
//! | fee_bps u32 | max_drift_secs u64 (v2 only)
//! ```
//! All integers little-endian.
//!
//! **v2: `max_drift_secs` appended.** `PostRootProved`'s layout-1 path binds the
//! guest's committed `max_drift_secs` public value to this field — an explicit `InitChainV2`/`MigrateChainV2`
//! argument, never a program constant (ops values are runtime parameters). `read` accepts BOTH a v1
//! account (`VERSION_V1` = 1, `LEN_V1` = 47 bytes, `max_drift_secs: None`) and a v2 account (`VERSION` =
//! 2, `LEN_V2` = 55 bytes, `max_drift_secs: Some(_)`) — refusing v1 outright would break every existing
//! chain's `charge_fee_and_count_post` (called on EVERY `PostRoot`/`PostRootProved`) the moment the
//! program upgrades, before anyone has had a chance to `MigrateChainV2` v1 accounts forward. `write` always
//! writes the current v2 shape (needs a 55-byte buffer) — there is no code path that writes v1 anymore;
//! `MigrateChainV2` is the one-time v1→v2 bring-forward (`programs/zk-settlement/src/governance.rs`).

pub const MAGIC: u32 = 0x5a4b_4343; // "ZKCC"
/// The version this crate no longer writes, but still reads (a pre-existing v1 account until migrated).
pub const VERSION_V1: u8 = 1;
/// The version `write` produces and `InitChainV2`/`MigrateChainV2` create going forward.
pub const VERSION: u8 = 2;

pub const OFF_MAGIC: usize = 0;
pub const OFF_VERSION: usize = 4;
pub const OFF_CHAIN_ID: usize = 5;
pub const OFF_RESERVED: usize = 13;
pub const OFF_DEPOSIT_LAMPORTS: usize = 14;
pub const OFF_DEPOSIT_REFUNDED: usize = 22;
pub const OFF_REGISTERED_SLOT: usize = 23;
pub const OFF_POSTED_BATCHES: usize = 31;
pub const OFF_FEE_BASE_LAMPORTS: usize = 35;
pub const OFF_FEE_BPS: usize = 43;
/// v2: appended after `fee_bps`, never inserted mid-header — every offset above is
/// unchanged from v1.
pub const OFF_MAX_DRIFT_SECS: usize = 47;
/// v1's full fixed-size account length (no `max_drift_secs`).
pub const LEN_V1: usize = 47;
/// v2's full fixed-size account length (`LEN_V1 + 8` for `max_drift_secs`).
pub const LEN_V2: usize = 55;

/// `["chain_config", chain_id]`.
#[inline]
pub fn seeds(chain_id: u64) -> [Vec<u8>; 2] {
    [b"chain_config".to_vec(), chain_id.to_le_bytes().to_vec()]
}

/// Derives the chain-config PDA under `program_id` (the settlement program) — the one place this
/// derivation is computed.
#[cfg(feature = "solana")]
#[inline]
pub fn pda(
    program_id: &solana_program::pubkey::Pubkey,
    chain_id: u64,
) -> (solana_program::pubkey::Pubkey, u8) {
    let s = seeds(chain_id);
    solana_program::pubkey::Pubkey::find_program_address(&[&s[0], &s[1]], program_id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainConfigFields {
    pub chain_id: u64,
    pub reserved: bool,
    pub deposit_lamports: u64,
    pub deposit_refunded: bool,
    pub registered_slot: u64,
    pub posted_batches: u32,
    pub fee_base_lamports: u64,
    pub fee_bps: u32,
    /// `None` for a v1 account (not yet migrated); `Some` for v2. `PostRootProved`'s layout-1 path
    /// refuses `None` by name (`DriftBoundUnset`) rather than treating it as any particular
    /// number — an unmigrated chain has no drift bound to check a proof against yet.
    pub max_drift_secs: Option<u64>,
}

/// Accepts a v1 account (`VERSION_V1`, exactly `LEN_V1` bytes or more, `max_drift_secs: None`) or a v2
/// account (`VERSION`, at least `LEN_V2` bytes, `max_drift_secs: Some`); refuses any other version
/// (`BadVersion`) and anything shorter than `LEN_V1` (`TooShort`) up front. Reading v1 successfully — not
/// refusing it — is deliberate (this module's doc): every existing chain's fee-charging path must keep
/// working across the program upgrade that introduces v2, before `MigrateChainV2` has run for it.
pub fn read(d: &[u8]) -> Result<ChainConfigFields, crate::LayoutError> {
    if d.len() < LEN_V1 {
        return Err(crate::LayoutError::TooShort {
            need: LEN_V1,
            got: d.len(),
        });
    }
    if u32::from_le_bytes(d[OFF_MAGIC..OFF_MAGIC + 4].try_into().unwrap()) != MAGIC {
        return Err(crate::LayoutError::BadMagic);
    }
    let version = d[OFF_VERSION];
    let max_drift_secs = match version {
        VERSION_V1 => None,
        VERSION => {
            if d.len() < LEN_V2 {
                return Err(crate::LayoutError::TooShort {
                    need: LEN_V2,
                    got: d.len(),
                });
            }
            Some(u64::from_le_bytes(
                d[OFF_MAX_DRIFT_SECS..OFF_MAX_DRIFT_SECS + 8]
                    .try_into()
                    .unwrap(),
            ))
        }
        _ => return Err(crate::LayoutError::BadVersion),
    };
    let u32_at = |o: usize| u32::from_le_bytes(d[o..o + 4].try_into().unwrap());
    let u64_at = |o: usize| u64::from_le_bytes(d[o..o + 8].try_into().unwrap());
    Ok(ChainConfigFields {
        chain_id: u64_at(OFF_CHAIN_ID),
        reserved: d[OFF_RESERVED] != 0,
        deposit_lamports: u64_at(OFF_DEPOSIT_LAMPORTS),
        deposit_refunded: d[OFF_DEPOSIT_REFUNDED] != 0,
        registered_slot: u64_at(OFF_REGISTERED_SLOT),
        posted_batches: u32_at(OFF_POSTED_BATCHES),
        fee_base_lamports: u64_at(OFF_FEE_BASE_LAMPORTS),
        fee_bps: u32_at(OFF_FEE_BPS),
        max_drift_secs,
    })
}

/// Writes back exactly the version `f` carries: `f.max_drift_secs == Some(_)` writes the full v2 record
/// (`VERSION`, needs a `>= LEN_V2`-byte buffer); `None` writes the v1 record (`VERSION_V1`, needs only
/// `>= LEN_V1` bytes) and never touches `OFF_MAX_DRIFT_SECS`. A caller that only ever mutates fields
/// `read` already decoded (`set_fee`, `charge_fee_and_count_post`, `refund_deposit`) writes back the same
/// version it read — never upgrading an unmigrated v1 account's shape as a side effect of an unrelated
/// write, and never indexing past a 47-byte buffer's end. Only `InitChainV2` (fresh v2) and `MigrateChainV2`
/// (v1→v2, after its own realloc) ever construct a `ChainConfigFields` with `max_drift_secs: Some` against
/// a account that wasn't already that size.
pub fn write(d: &mut [u8], f: &ChainConfigFields) {
    d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
    d[OFF_VERSION] = if f.max_drift_secs.is_some() {
        VERSION
    } else {
        VERSION_V1
    };
    d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].copy_from_slice(&f.chain_id.to_le_bytes());
    d[OFF_RESERVED] = f.reserved as u8;
    d[OFF_DEPOSIT_LAMPORTS..OFF_DEPOSIT_LAMPORTS + 8]
        .copy_from_slice(&f.deposit_lamports.to_le_bytes());
    d[OFF_DEPOSIT_REFUNDED] = f.deposit_refunded as u8;
    d[OFF_REGISTERED_SLOT..OFF_REGISTERED_SLOT + 8]
        .copy_from_slice(&f.registered_slot.to_le_bytes());
    d[OFF_POSTED_BATCHES..OFF_POSTED_BATCHES + 4].copy_from_slice(&f.posted_batches.to_le_bytes());
    d[OFF_FEE_BASE_LAMPORTS..OFF_FEE_BASE_LAMPORTS + 8]
        .copy_from_slice(&f.fee_base_lamports.to_le_bytes());
    d[OFF_FEE_BPS..OFF_FEE_BPS + 4].copy_from_slice(&f.fee_bps.to_le_bytes());
    if let Some(max_drift_secs) = f.max_drift_secs {
        d[OFF_MAX_DRIFT_SECS..OFF_MAX_DRIFT_SECS + 8]
            .copy_from_slice(&max_drift_secs.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_v1() -> ChainConfigFields {
        ChainConfigFields {
            chain_id: 200101,
            reserved: true,
            deposit_lamports: 0,
            deposit_refunded: false,
            registered_slot: 12345,
            posted_batches: 0,
            fee_base_lamports: 1_000_000,
            fee_bps: 0,
            max_drift_secs: None,
        }
    }

    fn sample_v2() -> ChainConfigFields {
        ChainConfigFields {
            max_drift_secs: Some(60),
            ..sample_v1()
        }
    }

    #[test]
    fn v1_write_then_read_round_trips_at_len_v1() {
        let mut d = vec![0u8; LEN_V1];
        write(&mut d, &sample_v1());
        assert_eq!(d[OFF_VERSION], VERSION_V1);
        assert_eq!(read(&d).unwrap(), sample_v1());
    }

    #[test]
    fn v2_write_then_read_round_trips_at_len_v2() {
        let mut d = vec![0u8; LEN_V2];
        write(&mut d, &sample_v2());
        assert_eq!(d[OFF_VERSION], VERSION);
        assert_eq!(read(&d).unwrap(), sample_v2());
    }

    /// The premise the fee path relies on: a v1-shaped account (47 bytes, `VERSION_V1`) is read
    /// successfully with `max_drift_secs: None`, not refused — the fee-charging path on every
    /// `PostRoot`/`PostRootProved` must keep working before `MigrateChainV2` runs.
    #[test]
    fn read_accepts_a_v1_account_with_max_drift_secs_none() {
        let mut d = vec![0u8; LEN_V1];
        write(&mut d, &sample_v1());
        let got = read(&d).unwrap();
        assert_eq!(got.max_drift_secs, None);
    }

    #[test]
    fn read_accepts_a_v2_account_with_max_drift_secs_some() {
        let mut d = vec![0u8; LEN_V2];
        write(&mut d, &sample_v2());
        let got = read(&d).unwrap();
        assert_eq!(got.max_drift_secs, Some(60));
    }

    #[test]
    fn read_rejects_a_version_that_is_neither_v1_nor_v2() {
        let mut d = vec![0u8; LEN_V2];
        write(&mut d, &sample_v2());
        d[OFF_VERSION] = 3;
        assert_eq!(read(&d).unwrap_err(), crate::LayoutError::BadVersion);
        d[OFF_VERSION] = 0xff;
        assert_eq!(read(&d).unwrap_err(), crate::LayoutError::BadVersion);
    }

    #[test]
    fn read_rejects_too_short_for_v1() {
        let d = vec![0u8; LEN_V1 - 1];
        assert!(matches!(
            read(&d).unwrap_err(),
            crate::LayoutError::TooShort { .. }
        ));
    }

    /// A v2-tagged account whose buffer was truncated back to v1 length must be refused as too short,
    /// not silently decoded with a garbage/zeroed `max_drift_secs`.
    #[test]
    fn read_rejects_a_v2_tagged_account_shorter_than_len_v2() {
        let mut d = vec![0u8; LEN_V2];
        write(&mut d, &sample_v2());
        d.truncate(LEN_V1);
        assert!(matches!(
            read(&d).unwrap_err(),
            crate::LayoutError::TooShort { .. }
        ));
    }

    #[test]
    fn write_never_indexes_past_len_v1_when_max_drift_secs_is_none() {
        // Regression for a panic that must not happen: `charge_fee_and_count_post` writes back
        // whatever version it read, against a buffer exactly the account's real (unmigrated) size.
        let mut d = vec![0u8; LEN_V1];
        write(&mut d, &sample_v1()); // must not panic indexing OFF_MAX_DRIFT_SECS on a 47-byte buffer
        assert_eq!(d.len(), LEN_V1);
    }

    #[test]
    fn seeds_golden_bytes_for_a_fixed_chain_id() {
        let s = seeds(0x0102_0304_0506_0708);
        assert_eq!(s[0], b"chain_config".to_vec());
        assert_eq!(s[1], vec![0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
    }

    #[cfg(feature = "solana")]
    #[test]
    fn pda_is_deterministic_and_varies_with_chain_id() {
        let program = solana_program::pubkey::Pubkey::new_unique();
        let (a1, _) = pda(&program, 7);
        let (a2, _) = pda(&program, 7);
        assert_eq!(a1, a2);
        let (b, _) = pda(&program, 8);
        assert_ne!(a1, b);
    }
}
