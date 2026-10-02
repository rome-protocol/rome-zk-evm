//! `zk-bridge`'s own account layouts and PDA derivations. Local to this program — unlike
//! `rome-zk-layouts::exit`, nothing here is read by the ZisK guest or by `zk-settlement`, so this crate
//! stays a normal Solana-program crate, not a `no_std`/guest-compatible one.
//!
//! Three PDAs per chain, keyed by `[settlement_program, chain_id]`: the chain-authority gate binds `InitVault`'s
//! *provenance* (only the real chain authority can sign a `root` owned by the settlement program it names) but, keyed
//! by `chain_id` alone, left exactly one address per chain — an attacker naming their OWN hostile settlement program
//! could still win that one slot first and permanently lock out the real authority (`VaultAlreadyInitialized`,
//! no reinit/close). Promoting `settlement_program` (already stored in `vault_config`, see below) into the
//! seed makes the real chain authority's vault address itself un-frontrunnable: an attacker's hostile
//! config lands at a *different* address (`pda(hostile_settlement, chain_id)`), never the real chain's own
//! (`pda(real_settlement, chain_id)`), which only the real settlement's `root.authority` can ever occupy.
//! See the crate README's "Fund-safety invariants".
//! - `["vault_config", settlement_program, chain_id]`, owned by this program — the small config record
//!   `InitVault` writes once and `ReleaseExit`/`Fund` read: which settlement program's `exit_record`s this
//!   vault releases against, which mint it holds, that mint's decimals, and the settlement chain authority
//!   that signed `InitVault` (recorded as `authority`, and gated by `root.authority` of that same
//!   `settlement_program` — see the crate README).
//! - `["vault", settlement_program, chain_id, mint]`, an SPL Token account owned by the **SPL Token
//!   program** (not this program) holding the actual escrowed balance.
//! - `["vault_authority", settlement_program, chain_id]`, owned by nothing (never holds data) — the PDA
//!   recorded as the SPL vault account's own `owner` field, so only this program's own `invoke_signed` can
//!   ever authorise an outgoing transfer from it.

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::pubkey::Pubkey;

pub const VAULT_CONFIG_SEED: &[u8] = b"vault_config";
pub const VAULT_SEED: &[u8] = b"vault";
pub const VAULT_AUTHORITY_SEED: &[u8] = b"vault_authority";

/// `["vault_config", settlement_program, chain_id]` under `program_id`. Keying by `settlement_program` is
/// what makes the real chain authority's vault address un-frontrunnable: an attacker naming a hostile
/// `settlement_program` can only ever occupy `pda(hostile_settlement, chain_id)`, never
/// `pda(real_settlement, chain_id)` — the module doc above has the full reasoning.
pub fn vault_config_pda(
    program_id: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[
            VAULT_CONFIG_SEED,
            settlement_program.as_ref(),
            &chain_id.to_le_bytes(),
        ],
        program_id,
    )
}

/// `["vault", settlement_program, chain_id, mint]` under `program_id` — the vault's own SPL Token account
/// address. Seeded by `mint` (not just `settlement_program`/`chain_id`) so a future multi-asset vault can
/// hold one account per mint without an address collision; v1 uses exactly one.
pub fn vault_token_pda(
    program_id: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
    mint: &Pubkey,
) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[
            VAULT_SEED,
            settlement_program.as_ref(),
            &chain_id.to_le_bytes(),
            mint.as_ref(),
        ],
        program_id,
    )
}

/// `["vault_authority", settlement_program, chain_id]` under `program_id` — the signer `ReleaseExit`'s
/// outgoing SPL transfer is `invoke_signed` under; recorded as the vault SPL account's own `owner` field at
/// `InitVault`.
pub fn vault_authority_pda(
    program_id: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[
            VAULT_AUTHORITY_SEED,
            settlement_program.as_ref(),
            &chain_id.to_le_bytes(),
        ],
        program_id,
    )
}

/// The `vault_config` account: magic `"ZKVC"`, `LEN` 110.
pub mod vault_config {
    use super::*;

    pub const MAGIC: u32 = 0x5a4b_5643; // "ZKVC"
    pub const VERSION: u8 = 1;

    pub const OFF_MAGIC: usize = 0;
    pub const OFF_VERSION: usize = 4;
    pub const OFF_CHAIN_ID: usize = 5;
    pub const OFF_SETTLEMENT_PROGRAM: usize = 13;
    pub const OFF_MINT: usize = 45;
    pub const OFF_MINT_DECIMALS: usize = 77;
    pub const OFF_AUTHORITY: usize = 78;
    /// Full fixed-size account length.
    pub const LEN: usize = 110;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub struct VaultConfigFields {
        pub chain_id: u64,
        pub settlement_program: Pubkey,
        pub mint: Pubkey,
        pub mint_decimals: u8,
        pub authority: Pubkey,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ReadError {
        TooShort { need: usize, got: usize },
        BadMagic,
        BadVersion,
    }

    pub fn read(d: &[u8]) -> Result<VaultConfigFields, ReadError> {
        if d.len() < LEN {
            return Err(ReadError::TooShort {
                need: LEN,
                got: d.len(),
            });
        }
        if u32::from_le_bytes(d[OFF_MAGIC..OFF_MAGIC + 4].try_into().unwrap()) != MAGIC {
            return Err(ReadError::BadMagic);
        }
        if d[OFF_VERSION] != VERSION {
            return Err(ReadError::BadVersion);
        }
        Ok(VaultConfigFields {
            chain_id: u64::from_le_bytes(d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].try_into().unwrap()),
            settlement_program: Pubkey::new_from_array(
                d[OFF_SETTLEMENT_PROGRAM..OFF_SETTLEMENT_PROGRAM + 32]
                    .try_into()
                    .unwrap(),
            ),
            mint: Pubkey::new_from_array(d[OFF_MINT..OFF_MINT + 32].try_into().unwrap()),
            mint_decimals: d[OFF_MINT_DECIMALS],
            authority: Pubkey::new_from_array(
                d[OFF_AUTHORITY..OFF_AUTHORITY + 32].try_into().unwrap(),
            ),
        })
    }

    pub fn write(f: &VaultConfigFields) -> [u8; LEN] {
        let mut d = [0u8; LEN];
        d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
        d[OFF_VERSION] = VERSION;
        d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].copy_from_slice(&f.chain_id.to_le_bytes());
        d[OFF_SETTLEMENT_PROGRAM..OFF_SETTLEMENT_PROGRAM + 32]
            .copy_from_slice(f.settlement_program.as_ref());
        d[OFF_MINT..OFF_MINT + 32].copy_from_slice(f.mint.as_ref());
        d[OFF_MINT_DECIMALS] = f.mint_decimals;
        d[OFF_AUTHORITY..OFF_AUTHORITY + 32].copy_from_slice(f.authority.as_ref());
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vault_config_round_trips() {
        let f = vault_config::VaultConfigFields {
            chain_id: 200101,
            settlement_program: Pubkey::new_unique(),
            mint: Pubkey::new_unique(),
            mint_decimals: 9,
            authority: Pubkey::new_unique(),
        };
        let d = vault_config::write(&f);
        assert_eq!(d.len(), vault_config::LEN);
        assert_eq!(vault_config::read(&d).unwrap(), f);
    }

    #[test]
    fn vault_config_read_refuses_bad_magic_version_len() {
        let good = vault_config::write(&vault_config::VaultConfigFields {
            chain_id: 1,
            settlement_program: Pubkey::new_unique(),
            mint: Pubkey::new_unique(),
            mint_decimals: 6,
            authority: Pubkey::new_unique(),
        });
        let mut bad_magic = good;
        bad_magic[vault_config::OFF_MAGIC] ^= 0xff;
        assert_eq!(
            vault_config::read(&bad_magic).unwrap_err(),
            vault_config::ReadError::BadMagic
        );
        let mut bad_version = good;
        bad_version[vault_config::OFF_VERSION] = 0xee;
        assert_eq!(
            vault_config::read(&bad_version).unwrap_err(),
            vault_config::ReadError::BadVersion
        );
        assert!(matches!(
            vault_config::read(&good[..good.len() - 1]).unwrap_err(),
            vault_config::ReadError::TooShort { .. }
        ));
    }

    #[test]
    fn pdas_are_deterministic_and_vary_with_their_own_inputs() {
        let program = Pubkey::new_unique();
        let settlement = Pubkey::new_unique();
        let mint = Pubkey::new_unique();

        let (c1, _) = vault_config_pda(&program, &settlement, 7);
        let (c2, _) = vault_config_pda(&program, &settlement, 7);
        assert_eq!(c1, c2);
        let (c3, _) = vault_config_pda(&program, &settlement, 8);
        assert_ne!(c1, c3);

        let (v1, _) = vault_token_pda(&program, &settlement, 7, &mint);
        let (v2, _) = vault_token_pda(&program, &settlement, 7, &Pubkey::new_unique());
        assert_ne!(
            v1, v2,
            "a different mint must derive a different vault address"
        );

        let (a1, _) = vault_authority_pda(&program, &settlement, 7);
        let (a2, _) = vault_authority_pda(&program, &settlement, 8);
        assert_ne!(a1, a2);

        // Every PDA is bound to the specific program id, not just its seeds — a different program cannot
        // reproduce (and so cannot sign for) another program's vault_authority.
        let other_program = Pubkey::new_unique();
        let (under_this, _) = vault_authority_pda(&program, &settlement, 7);
        let (under_other, _) = vault_authority_pda(&other_program, &settlement, 7);
        assert_ne!(under_this, under_other);
    }

    /// The un-frontrunnable re-keying: all three vault PDAs must vary with `settlement_program`, holding
    /// `program_id`/`chain_id` fixed — this is the exact property that makes a hostile-settlement
    /// `InitVault` land at a DIFFERENT address than the real chain's own, rather than colliding into the
    /// single create-once slot the old chain_id-only keying used.
    #[test]
    fn vault_pdas_vary_with_settlement_program() {
        let program = Pubkey::new_unique();
        let real_settlement = Pubkey::new_unique();
        let hostile_settlement = Pubkey::new_unique();
        let mint = Pubkey::new_unique();

        let (real_cfg, _) = vault_config_pda(&program, &real_settlement, 7);
        let (hostile_cfg, _) = vault_config_pda(&program, &hostile_settlement, 7);
        assert_ne!(
            real_cfg, hostile_cfg,
            "a hostile settlement_program must never collide with the real one's vault_config address"
        );

        let (real_token, _) = vault_token_pda(&program, &real_settlement, 7, &mint);
        let (hostile_token, _) = vault_token_pda(&program, &hostile_settlement, 7, &mint);
        assert_ne!(real_token, hostile_token);

        let (real_auth, _) = vault_authority_pda(&program, &real_settlement, 7);
        let (hostile_auth, _) = vault_authority_pda(&program, &hostile_settlement, 7);
        assert_ne!(real_auth, hostile_auth);
    }
}
