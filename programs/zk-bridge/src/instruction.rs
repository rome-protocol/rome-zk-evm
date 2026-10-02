//! `BridgeIx` — the three instructions this program dispatches. Borsh-serialized, discriminant
//! = the enum's own variant order (`InitVault` = 0, `Fund` = 1, `ReleaseExit` = 2) — pinned by
//! `discriminants_are_pinned` below, the same "never let a refactor silently renumber a shipped wire
//! format" rule `zk-settlement::SettleIx` follows.

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::pubkey::Pubkey;

#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct InitVaultArgs {
    pub chain_id: u64,
    pub mint: Pubkey,
    pub mint_decimals: u8,
    /// Required by `vault_config`'s own schema (it records settlement_program, mint and mint_decimals) —
    /// there is no other input path for it, so it is an explicit argument here. See the crate README.
    pub settlement_program: Pubkey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct FundArgs {
    pub chain_id: u64,
    pub amount: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct ReleaseExitArgs {
    pub chain_id: u64,
    pub message_hash: [u8; 32],
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub enum BridgeIx {
    /// accounts: `[payer (signer, writable), vault_config (writable, NEW), mint (read-only), vault_token
    /// (writable, NEW, PDA-owned by the SPL Token program), vault_authority (read-only, PDA, never holds
    /// data), chain_authority (signer — must equal the settlement `root.authority` for `chain_id`), root
    /// (read-only — the settlement `["root", chain_id]` PDA, owned by `settlement_program`), token_program,
    /// system_program]`. Gated by the settlement chain authority — see `init_vault.rs`'s module doc.
    InitVault(InitVaultArgs),
    /// accounts: `[funder (signer), funder_token_account (writable), vault_config (read-only),
    /// vault_token (writable), token_program]`. Permissionless.
    Fund(FundArgs),
    /// accounts: `[vault_config (read-only), exit_config (read-only), exit_record (writable),
    /// exit_consumer (read-only — this program's own `["exit_consumer", chain_id]` PDA, CPI-signed),
    /// settlement_program (read-only, executable), payer_refund (writable), vault_token (writable),
    /// vault_authority (read-only), recipient_ata (writable), token_program]`. Permissionless — funds
    /// always land at `record.sol_recipient`'s ATA regardless of who submits the transaction.
    ReleaseExit(ReleaseExitArgs),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pinned literally (never via the enum's own discriminant, which would make a mutated ordering
    /// mutate its own check) — the same "layout offsets pinned literally" rule `rome-zk-layouts` and
    /// `zk-settlement` both already apply to their own wire formats.
    #[test]
    fn discriminants_are_pinned() {
        let init = BridgeIx::InitVault(InitVaultArgs {
            chain_id: 1,
            mint: Pubkey::new_from_array([1u8; 32]),
            mint_decimals: 6,
            settlement_program: Pubkey::new_from_array([2u8; 32]),
        });
        let fund = BridgeIx::Fund(FundArgs {
            chain_id: 1,
            amount: 1,
        });
        let release = BridgeIx::ReleaseExit(ReleaseExitArgs {
            chain_id: 1,
            message_hash: [3u8; 32],
        });
        assert_eq!(borsh::to_vec(&init).unwrap()[0], 0);
        assert_eq!(borsh::to_vec(&fund).unwrap()[0], 1);
        assert_eq!(borsh::to_vec(&release).unwrap()[0], 2);
    }

    #[test]
    fn round_trips_through_borsh() {
        let args = ReleaseExitArgs {
            chain_id: 200101,
            message_hash: [0x42u8; 32],
        };
        let ix = BridgeIx::ReleaseExit(args);
        let bytes = borsh::to_vec(&ix).unwrap();
        let decoded: BridgeIx = borsh::from_slice(&bytes).unwrap();
        match decoded {
            BridgeIx::ReleaseExit(a) => assert_eq!(a, args),
            _ => panic!("wrong variant decoded"),
        }
    }
}
