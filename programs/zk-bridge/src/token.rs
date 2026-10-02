//! Hand-rolled SPL Token / Associated-Token-Account wire format — instruction encoding, the two program
//! ids, and the ATA address formula. No `spl-token`/`spl-token-interface`/
//! `spl-associated-token-account-interface` dependency. This module was written when the workspace sat on
//! `solana-program = "=2.1.6"` and the SPL crates of that day pulled a second, different `Pubkey` type into
//! the lockfile. On the current `solana-program = "=4.1.0"` that is no longer so: the current SPL releases
//! resolve onto the one `solana-program` 4.1.0 and accept its `Pubkey` and `Instruction` directly (checked
//! with a throwaway crate seeded from the workspace `Cargo.lock`). Replacing this hand-rolled wire with
//! those crates is a separate decision that the move to 4.1.0 did not take. The instruction formats below
//! (`TokenInstruction::Transfer`/`InitializeAccount3`, the standard 165-byte token account layout, the ATA
//! seeds) are small, public, and unchanged since the SPL Token program's original release — the same kind
//! of fixed-format encoding `rome_zk_layouts` already hand-rolls for every other account this workspace
//! reads, applied here to a program we do not own.
//!
//! **The real, deployed `TokenkegQ…`/`ATokenGP…` programs are exactly what these constants/instructions
//! target** — nothing here is a reimplementation of SPL Token's own logic (this program never executes SPL
//! Token code, only calls it via CPI), just its wire format.

use solana_program::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};

/// `TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA` — the standard SPL Token program.
pub const TOKEN_PROGRAM_ID: Pubkey =
    solana_program::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

/// `ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL` — the standard Associated-Token-Account program.
pub const ASSOCIATED_TOKEN_PROGRAM_ID: Pubkey =
    solana_program::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

/// The fixed size of an SPL Token `Account` (mint 32 + owner 32 + amount 8 + delegate option 36 + state 1 +
/// is_native option 12 + delegated_amount 8 + close_authority option 36 = 165 bytes) — unchanged since the
/// program's original release.
pub const TOKEN_ACCOUNT_LEN: usize = 165;

/// The fixed size of an SPL Token `Mint` account (82 bytes) — unchanged since the program's original
/// release.
pub const MINT_LEN: usize = 82;

const OFF_AMOUNT: usize = 64;

/// Reads an SPL Token account's `amount` field (offset 64, 8 bytes LE) — the one field this program's own
/// tests need to read back; everything else about the account is opaque to it (the program itself never
/// decodes a token account — it only ever CPIs `Transfer`/`InitializeAccount3` and lets the SPL Token
/// program enforce its own state).
pub fn read_token_amount(data: &[u8]) -> u64 {
    u64::from_le_bytes(data[OFF_AMOUNT..OFF_AMOUNT + 8].try_into().unwrap())
}

/// `["<wallet>", "<token_program_id>", "<mint>"]` under the Associated-Token-Account program — the exact
/// formula `spl-associated-token-account`'s own `get_associated_token_address_with_program_id` uses.
pub fn get_associated_token_address(wallet: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[wallet.as_ref(), TOKEN_PROGRAM_ID.as_ref(), mint.as_ref()],
        &ASSOCIATED_TOKEN_PROGRAM_ID,
    )
    .0
}

/// `TokenInstruction::Transfer { amount }` (discriminant 3): `[source (writable), destination (writable),
/// authority (read-only, signer — single-authority form, never multisig)]`.
pub fn transfer_ix(
    source: &Pubkey,
    destination: &Pubkey,
    authority: &Pubkey,
    amount: u64,
) -> Instruction {
    let mut data = Vec::with_capacity(9);
    data.push(3u8);
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction {
        program_id: TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*source, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*authority, true),
        ],
        data,
    }
}

/// `TokenInstruction::InitializeAccount3 { owner }` (discriminant 18): `[account (writable), mint
/// (read-only)]` — no signer, no rent sysvar (that is exactly what distinguishes "3" from the original
/// `InitializeAccount`).
pub fn initialize_account3_ix(account: &Pubkey, mint: &Pubkey, owner: &Pubkey) -> Instruction {
    let mut data = Vec::with_capacity(33);
    data.push(18u8);
    data.extend_from_slice(owner.as_ref());
    Instruction {
        program_id: TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*account, false),
            AccountMeta::new_readonly(*mint, false),
        ],
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_ids_match_the_well_known_addresses() {
        assert_eq!(
            TOKEN_PROGRAM_ID.to_string(),
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
        );
        assert_eq!(
            ASSOCIATED_TOKEN_PROGRAM_ID.to_string(),
            "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"
        );
    }

    #[test]
    fn transfer_ix_encodes_discriminant_and_amount() {
        let (s, d, a) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let ix = transfer_ix(&s, &d, &a, 42);
        assert_eq!(ix.program_id, TOKEN_PROGRAM_ID);
        assert_eq!(ix.data[0], 3);
        assert_eq!(&ix.data[1..9], &42u64.to_le_bytes());
        assert_eq!(ix.accounts.len(), 3);
        assert!(ix.accounts[2].is_signer);
    }

    #[test]
    fn initialize_account3_ix_encodes_discriminant_and_owner() {
        let (acc, mint, owner) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let ix = initialize_account3_ix(&acc, &mint, &owner);
        assert_eq!(ix.data[0], 18);
        assert_eq!(&ix.data[1..33], owner.as_ref());
        assert_eq!(ix.accounts.len(), 2);
        assert!(!ix.accounts.iter().any(|m| m.is_signer));
    }

    #[test]
    fn read_token_amount_reads_the_right_offset() {
        let mut d = [0u8; TOKEN_ACCOUNT_LEN];
        d[OFF_AMOUNT..OFF_AMOUNT + 8].copy_from_slice(&123_456u64.to_le_bytes());
        assert_eq!(read_token_amount(&d), 123_456);
    }

    #[test]
    fn get_associated_token_address_is_deterministic_and_varies_with_inputs() {
        let wallet = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let a1 = get_associated_token_address(&wallet, &mint);
        let a2 = get_associated_token_address(&wallet, &mint);
        assert_eq!(a1, a2);
        let a3 = get_associated_token_address(&Pubkey::new_unique(), &mint);
        assert_ne!(a1, a3);
    }
}
