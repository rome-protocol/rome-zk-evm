//! The zk-settlement global config account layout ("ZKGL", PDA `["global_config"]` under the settlement
//! program, one per deployment). Created once by `InitGlobalConfig`; `registry_authority` (a multisig
//! vault; the timelock lives on the multisig side) and `treasury` are mutated afterwards only by that
//! same authority (`SetTreasury` for `treasury`; `registry_authority` itself rotates only through the
//! two-step `ProposeRegistryAuthority`/`AcceptRegistryAuthority` described below).
//!
//! ```text
//! magic 'ZKGL' u32 | version u8 | registry_authority [32] | treasury [32]
//! | permissionless_init_enabled u8 | reclaim_window_slots u64 | deposit_lamports u64
//! | default_fee_base_lamports u64 | default_fee_bps u32 | pending_registry_authority [32]
//! ```
//! All integers little-endian. `permissionless_init_enabled` defaults `false` at `InitGlobalConfig` —
//! mainnet gate: permissionless `InitChain` stays off on public deployments until the reclaim/nonce
//! tests exist, flipped by `registry_authority` alone.
//!
//! `pending_registry_authority`: two-step rotation —
//! `ProposeRegistryAuthority` writes this field (the current authority signs, rejects `Pubkey::default()`
//! as `new`), `AcceptRegistryAuthority` (the pending key signs) copies it into `registry_authority` and
//! resets this field to `Pubkey::default()` (the "no rotation pending" sentinel — indistinguishable from
//! "never proposed", which is fine: there is nothing to distinguish it from). Appended at the end so the
//! offsets above never move.

pub const MAGIC: u32 = 0x5a4b_474c; // "ZKGL"
pub const VERSION: u8 = 1;

pub const OFF_MAGIC: usize = 0;
pub const OFF_VERSION: usize = 4;
pub const OFF_REGISTRY_AUTHORITY: usize = 5;
pub const OFF_TREASURY: usize = 37;
pub const OFF_PERMISSIONLESS_INIT_ENABLED: usize = 69;
pub const OFF_RECLAIM_WINDOW_SLOTS: usize = 70;
pub const OFF_DEPOSIT_LAMPORTS: usize = 78;
pub const OFF_DEFAULT_FEE_BASE_LAMPORTS: usize = 86;
pub const OFF_DEFAULT_FEE_BPS: usize = 94;
pub const OFF_PENDING_REGISTRY_AUTHORITY: usize = 98;
/// Full fixed-size account length.
pub const LEN: usize = 130;

/// `["global_config"]` — one global config account per program deployment, no chain-specific component.
#[inline]
pub fn seeds() -> [&'static [u8]; 1] {
    [b"global_config"]
}

/// Derives the global-config PDA under `program_id` (the settlement program) — the one place this
/// derivation is computed.
#[cfg(feature = "solana")]
#[inline]
pub fn pda(program_id: &solana_program::pubkey::Pubkey) -> (solana_program::pubkey::Pubkey, u8) {
    solana_program::pubkey::Pubkey::find_program_address(&seeds(), program_id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlobalConfigFields {
    pub registry_authority: [u8; 32],
    pub treasury: [u8; 32],
    pub permissionless_init_enabled: bool,
    pub reclaim_window_slots: u64,
    pub deposit_lamports: u64,
    pub default_fee_base_lamports: u64,
    pub default_fee_bps: u32,
    pub pending_registry_authority: [u8; 32],
}

/// Validates magic + version + minimum length and decodes every field. Layout structs carry raw
/// `[u8; 32]`; only `pda()` uses `solana_program::Pubkey` — callers hold `program_id` and must check the
/// account's owner and PDA seeds themselves.
pub fn read(d: &[u8]) -> Result<GlobalConfigFields, crate::LayoutError> {
    if d.len() < LEN {
        return Err(crate::LayoutError::TooShort {
            need: LEN,
            got: d.len(),
        });
    }
    if u32::from_le_bytes(d[OFF_MAGIC..OFF_MAGIC + 4].try_into().unwrap()) != MAGIC {
        return Err(crate::LayoutError::BadMagic);
    }
    if d[OFF_VERSION] != VERSION {
        return Err(crate::LayoutError::BadVersion);
    }
    let u32_at = |o: usize| u32::from_le_bytes(d[o..o + 4].try_into().unwrap());
    let u64_at = |o: usize| u64::from_le_bytes(d[o..o + 8].try_into().unwrap());
    let b32_at = |o: usize| -> [u8; 32] { d[o..o + 32].try_into().unwrap() };
    Ok(GlobalConfigFields {
        registry_authority: b32_at(OFF_REGISTRY_AUTHORITY),
        treasury: b32_at(OFF_TREASURY),
        permissionless_init_enabled: d[OFF_PERMISSIONLESS_INIT_ENABLED] != 0,
        reclaim_window_slots: u64_at(OFF_RECLAIM_WINDOW_SLOTS),
        deposit_lamports: u64_at(OFF_DEPOSIT_LAMPORTS),
        default_fee_base_lamports: u64_at(OFF_DEFAULT_FEE_BASE_LAMPORTS),
        default_fee_bps: u32_at(OFF_DEFAULT_FEE_BPS),
        pending_registry_authority: b32_at(OFF_PENDING_REGISTRY_AUTHORITY),
    })
}

/// Writes every field (used at `InitGlobalConfig`, and by `SetTreasury` for just the one field).
pub fn write(d: &mut [u8], f: &GlobalConfigFields) {
    d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
    d[OFF_VERSION] = VERSION;
    d[OFF_REGISTRY_AUTHORITY..OFF_REGISTRY_AUTHORITY + 32].copy_from_slice(&f.registry_authority);
    d[OFF_TREASURY..OFF_TREASURY + 32].copy_from_slice(&f.treasury);
    d[OFF_PERMISSIONLESS_INIT_ENABLED] = f.permissionless_init_enabled as u8;
    d[OFF_RECLAIM_WINDOW_SLOTS..OFF_RECLAIM_WINDOW_SLOTS + 8]
        .copy_from_slice(&f.reclaim_window_slots.to_le_bytes());
    d[OFF_DEPOSIT_LAMPORTS..OFF_DEPOSIT_LAMPORTS + 8]
        .copy_from_slice(&f.deposit_lamports.to_le_bytes());
    d[OFF_DEFAULT_FEE_BASE_LAMPORTS..OFF_DEFAULT_FEE_BASE_LAMPORTS + 8]
        .copy_from_slice(&f.default_fee_base_lamports.to_le_bytes());
    d[OFF_DEFAULT_FEE_BPS..OFF_DEFAULT_FEE_BPS + 4]
        .copy_from_slice(&f.default_fee_bps.to_le_bytes());
    d[OFF_PENDING_REGISTRY_AUTHORITY..OFF_PENDING_REGISTRY_AUTHORITY + 32]
        .copy_from_slice(&f.pending_registry_authority);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> GlobalConfigFields {
        GlobalConfigFields {
            registry_authority: [1u8; 32],
            treasury: [2u8; 32],
            permissionless_init_enabled: false,
            reclaim_window_slots: 5_184_000,
            deposit_lamports: 25_000_000_000,
            default_fee_base_lamports: 1_000_000,
            default_fee_bps: 0,
            pending_registry_authority: [0u8; 32],
        }
    }

    #[test]
    fn write_then_read_round_trips() {
        let mut d = vec![0u8; LEN];
        write(&mut d, &sample());
        assert_eq!(read(&d).unwrap(), sample());
    }

    /// The two-step rotation's in-flight state must itself round-trip, not just the
    /// all-zero "nothing pending" default.
    #[test]
    fn write_then_read_round_trips_a_pending_authority() {
        let mut f = sample();
        f.pending_registry_authority = [7u8; 32];
        let mut d = vec![0u8; LEN];
        write(&mut d, &f);
        assert_eq!(read(&d).unwrap(), f);
    }

    #[test]
    fn read_rejects_bad_magic() {
        let mut d = vec![0u8; LEN];
        write(&mut d, &sample());
        d[OFF_MAGIC] ^= 0xff;
        assert_eq!(read(&d).unwrap_err(), crate::LayoutError::BadMagic);
    }

    #[test]
    fn read_rejects_too_short() {
        let d = vec![0u8; LEN - 1];
        assert!(matches!(
            read(&d).unwrap_err(),
            crate::LayoutError::TooShort { .. }
        ));
    }

    #[test]
    fn seeds_is_the_fixed_tag() {
        assert_eq!(seeds(), [b"global_config".as_slice()]);
    }

    #[cfg(feature = "solana")]
    #[test]
    fn pda_is_deterministic_and_varies_with_program() {
        let program = solana_program::pubkey::Pubkey::new_unique();
        let other = solana_program::pubkey::Pubkey::new_unique();
        let (a1, _) = pda(&program);
        let (a2, _) = pda(&program);
        assert_eq!(a1, a2);
        let (b, _) = pda(&other);
        assert_ne!(a1, b);
    }
}
