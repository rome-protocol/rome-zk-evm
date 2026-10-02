//! `zk-bridge` error codes (mapped to `ProgramError::Custom`).

use solana_program::program_error::ProgramError;

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeError {
    /// `InitVault` called a second time for a chain that already has a `vault_config` (owned by this
    /// program, fully written).
    VaultAlreadyInitialized = 1,
    /// `InitVault`'s `mint_decimals` argument exceeds 18 — the decimal-scale divisor
    /// `10^(18 - mint_decimals)` would need a negative exponent.
    InvalidMintDecimals = 2,
    /// `ReleaseExit`: the `exit_record` account is not owned by `vault_config.settlement_program` — either
    /// a forged account, or a settlement program mismatch. Checked before the record is ever decoded.
    WrongSettlementOwner = 3,
    /// `ReleaseExit`: `exit_record.asset != [0; 20]` — v1 releases the native asset only,
    /// matching `zk-settlement`'s own `ProveExit` restriction.
    UnsupportedAsset = 4,
    /// `ReleaseExit`: the supplied `recipient_ata` does not equal
    /// `get_associated_token_address(record.sol_recipient, vault_config.mint)` — the recipient's identity
    /// comes from the exit record, never an instruction argument, and this is the account that identity
    /// resolves to; naming any other account here is refused before the settlement CPI ever runs.
    WrongRecipientAta = 5,
    /// `ReleaseExit`: the supplied `payer_refund` does not equal `exit_record.payer` — checked here too
    /// (defense in depth; `zk-settlement`'s own `ConsumeExit` enforces the same thing on the CPI it is
    /// about to receive).
    WrongPayerRefund = 6,
    /// A CU/logic overflow while computing `mint_amount = amount / 10^(18 - mint_decimals)` (u128 amounts
    /// under v1's `type(uint128).max` exit cap never overflow in practice; named rather than panicking).
    AmountConversionOverflow = 7,
    /// `InitVault`: the `chain_authority` account either did not sign, or its key does not equal the
    /// settlement `root.authority` for `args.chain_id` — the same chain-authority gate
    /// `zk-settlement::governance::require_chain_authority` enforces for `ProposeExitConfig`
    /// (InitVault is gated by the settlement chain authority, not a baked constant).
    NotChainAuthority = 8,
    /// `InitVault`: `args.mint_decimals` does not equal the real `decimals` byte (offset 44) read off the
    /// `mint` account — refuses an operator typo before it can mis-scale every future `ReleaseExit`
    /// payout (`release.rs` uses `cfg.mint_decimals` verbatim).
    MintDecimalsMismatch = 9,
}

impl From<BridgeError> for ProgramError {
    fn from(e: BridgeError) -> Self {
        ProgramError::Custom(e as u32)
    }
}
