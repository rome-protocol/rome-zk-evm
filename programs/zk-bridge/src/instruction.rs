//! `BridgeIx` — the instructions this program dispatches. Borsh-serialized, discriminant
//! = the enum's own variant order (`InitVault` = 0, `Fund` = 1, `ReleaseExit` = 2, tag 3 held for the
//! deposit instruction, `InitBridgeConfig` = 4, `InitDepositQueue` = 5, `ProposeDepositParams` = 6,
//! `ActivateDepositParams` = 7) — pinned by
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

/// `InitBridgeConfig`: the two programs every deposit queue is bound to, written once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct InitBridgeConfigArgs {
    pub settlement_program: Pubkey,
    pub inbox_program: Pubkey,
}

/// The six parameters a deposit queue runs on (the layout's `DepositParams`, with the fee recipient as a
/// `Pubkey`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct DepositParamsArgs {
    pub inclusion_deadline_secs: u32,
    pub max_per_batch: u16,
    pub max_per_block: u16,
    pub min_amount: u64,
    pub fee_lamports: u64,
    pub fee_recipient: Pubkey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct InitDepositQueueArgs {
    pub chain_id: u64,
    /// Must equal the settlement program in the bridge config; named here the way `InitVaultArgs` names it.
    pub settlement_program: Pubkey,
    pub params: DepositParamsArgs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct ProposeDepositParamsArgs {
    pub chain_id: u64,
    pub activation_slot: u64,
    pub params: DepositParamsArgs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct ActivateDepositParamsArgs {
    pub chain_id: u64,
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
    /// Tag 3 is held for the deposit instruction, which a later change adds. Until then it is refused
    /// with `InvalidInstructionData`, and nothing else may take the number.
    DepositNotYetAvailable,
    /// accounts: `[payer (signer, writable), authority (signer — the bridge program's upgrade authority),
    /// bridge_config (writable, NEW, the `["bridge_config"]` PDA), program_data (read-only — this program's
    /// own `ProgramData` account), system_program]`. Written once.
    InitBridgeConfig(InitBridgeConfigArgs),
    /// accounts: `[payer (signer, writable), chain_authority (signer — must equal `root.authority`),
    /// bridge_config (read-only), root (read-only, at the config's settlement program), registry
    /// (read-only, at the config's settlement program), vault_config (read-only), fee_recipient
    /// (read-only), deposit_queue (writable, NEW), system_program]`. Gated by the chain authority.
    InitDepositQueue(InitDepositQueueArgs),
    /// accounts: `[chain_authority (signer — must equal `root.authority`), bridge_config (read-only), root
    /// (read-only, at the config's settlement program), deposit_queue (writable), fee_recipient
    /// (read-only)]`. Writes only the pending parameters and the activation slot.
    ProposeDepositParams(ProposeDepositParamsArgs),
    /// accounts: `[bridge_config (read-only), deposit_queue (writable)]`. Permissionless.
    ActivateDepositParams(ActivateDepositParamsArgs),
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
        assert_eq!(
            borsh::to_vec(&BridgeIx::DepositNotYetAvailable).unwrap()[0],
            3
        );
        let params = DepositParamsArgs {
            inclusion_deadline_secs: 43_200,
            max_per_batch: 256,
            max_per_block: 16,
            min_amount: 1,
            fee_lamports: 0,
            fee_recipient: Pubkey::new_from_array([4u8; 32]),
        };
        let cfg = BridgeIx::InitBridgeConfig(InitBridgeConfigArgs {
            settlement_program: Pubkey::new_from_array([5u8; 32]),
            inbox_program: Pubkey::new_from_array([6u8; 32]),
        });
        let queue = BridgeIx::InitDepositQueue(InitDepositQueueArgs {
            chain_id: 1 << 32,
            settlement_program: Pubkey::new_from_array([5u8; 32]),
            params,
        });
        let propose = BridgeIx::ProposeDepositParams(ProposeDepositParamsArgs {
            chain_id: 1 << 32,
            activation_slot: 9,
            params,
        });
        let activate =
            BridgeIx::ActivateDepositParams(ActivateDepositParamsArgs { chain_id: 1 << 32 });
        assert_eq!(borsh::to_vec(&cfg).unwrap()[0], 4);
        assert_eq!(borsh::to_vec(&queue).unwrap()[0], 5);
        assert_eq!(borsh::to_vec(&propose).unwrap()[0], 6);
        assert_eq!(borsh::to_vec(&activate).unwrap()[0], 7);
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
