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
    /// `InitBridgeConfig`: the signer is not the bridge program's own upgrade authority, or the
    /// `program_data` account is not the bridge's real `ProgramData` account, or the program is immutable.
    NotUpgradeAuthority = 10,
    /// `InitBridgeConfig` called a second time: the config is written once and never changed.
    BridgeConfigAlreadyInitialized = 11,
    /// `InitBridgeConfig`: the settlement program or the inbox program is the all-zero key. The config is
    /// written once, so a zero value would leave every chain without a queue for good.
    BridgeConfigProgramZero = 12,
    /// The `bridge_config` account is not the bridge's `["bridge_config"]` PDA, is not owned by this
    /// program, or does not hold a config.
    WrongBridgeConfig = 13,
    /// `InitDepositQueue`: `args.settlement_program` is not the settlement program in the bridge config.
    WrongSettlementProgram = 14,
    /// `InitDepositQueue`: the chain id is below 2^32, a reserved id. Reserved chains take no deposits.
    ReservedChainId = 15,
    /// The `root` account is not at `root::pda(bridge_config.settlement_program, chain_id)`, or is not
    /// owned by that settlement program.
    RootNotCanonical = 16,
    /// `InitDepositQueue`: the `registry` account is not at
    /// `registry::pda(bridge_config.settlement_program, chain_id)`, is not owned by that program, or does
    /// not hold a registry for this chain.
    RegistryNotCanonical = 17,
    /// `InitDepositQueue`: the chain's registry names an inbox other than the one in the bridge config.
    WrongInboxProgram = 18,
    /// The `vault_config` account is not at `vault_config_pda(bridge, settlement_program, chain_id)`, is
    /// not owned by this program, or does not hold a vault config. A queue needs the chain's vault first.
    WrongVaultConfig = 19,
    /// `InitDepositQueue`: the vault's mint has more than 9 decimals, so a deposit would not convert to
    /// whole gwei.
    MintTooManyDecimals = 20,
    /// `inclusion_deadline_secs` is below the 1 hour floor.
    DeadlineBelowFloor = 21,
    /// `inclusion_deadline_secs` is above the 24 hour ceiling.
    DeadlineAboveCeiling = 22,
    /// `max_per_batch` is above 256.
    MaxPerBatchTooLarge = 23,
    /// `max_per_block` is 0 or above `max_per_batch`.
    MaxPerBlockOutOfRange = 24,
    /// `min_amount` is 0.
    MinAmountZero = 25,
    /// `fee_lamports` is above 0.01 SOL.
    FeeTooHigh = 26,
    /// The `fee_recipient` account is not the key in the parameters, or holds less than the rent-exempt
    /// minimum for an empty account.
    FeeRecipientNotRentExempt = 27,
    /// `InitDepositQueue`: this chain already has a queue.
    QueueAlreadyInitialized = 28,
    /// The `deposit_queue` account is not at `["deposit_queue", settlement_program, chain_id]` under this
    /// program, is not owned by it, or does not hold a queue.
    WrongDepositQueue = 29,
    /// `ProposeDepositParams`: the chain's `root.challenge_window_slots` is 0.
    ChallengeWindowZero = 30,
    /// `ProposeDepositParams`: `activation_slot` is less than one challenge window from the current slot.
    ActivationTooSoon = 31,
    /// `ProposeDepositParams`: a proposal is already pending. Activate it first.
    PendingParamsExist = 32,
    /// `ActivateDepositParams`: no proposal is pending.
    NoPendingParams = 33,
    /// `ActivateDepositParams`: the proposal's activation slot has not been reached.
    ActivationNotReached = 34,
}

impl From<BridgeError> for ProgramError {
    fn from(e: BridgeError) -> Self {
        ProgramError::Custom(e as u32)
    }
}
