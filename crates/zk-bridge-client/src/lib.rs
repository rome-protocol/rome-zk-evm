//! Thin client for `programs/zk-bridge`: instruction builders, PDA derivation, and account decoding
//! (mirrors `zk-settlement-client`'s own shape). No async runtime opinions — this crate only builds
//! `Instruction`s and decodes bytes; sending transactions is the caller's job.

pub mod vault_tool;

use solana_program::{instruction::AccountMeta, pubkey::Pubkey};
use zk_bridge::{
    BridgeIx, CloseDepositArgs, DepositArgs, FundArgs, InitVaultArgs, ReleaseExitArgs,
};

pub use zk_bridge::state::{vault_authority_pda, vault_config_pda, vault_token_pda};

fn ix(
    program_id: &Pubkey,
    accounts: Vec<AccountMeta>,
    data: BridgeIx,
) -> solana_program::instruction::Instruction {
    solana_program::instruction::Instruction {
        program_id: *program_id,
        accounts,
        data: borsh::to_vec(&data).expect("BridgeIx always serializes"),
    }
}

/// The recipient's ATA for the vault's mint — the exact address `ReleaseExit` itself derives and checks
/// `recipient_ata` against. Callers build the real `ReleaseExit` transaction with this.
pub fn recipient_ata(recipient: &Pubkey, mint: &Pubkey) -> Pubkey {
    zk_bridge::token::get_associated_token_address(recipient, mint)
}

/// Associated Token Program `CreateIdempotent` (discriminant 1, single-byte instruction data): creates the
/// recipient's ATA for `mint` if it does not already exist, and is a documented no-op — never an error — if it
/// does. The `release-exit` example prepends this ix to every `ReleaseExit` it sends, so `ReleaseExit` does not
/// revert when `record.sol_recipient` has no ATA yet (handled at the CLIENT):
/// `programs/zk-bridge/src/release.rs:133-137` derives and checks `recipient_ata`, it never creates it —
/// provisioning is the caller's job.
///
/// Hand-rolled like `zk_bridge::token`'s own `transfer_ix`/`initialize_account3_ix` (replacing the hand-rolled
/// wire with the `spl-associated-token-account-interface` crate is a separate decision that has not
/// been taken; see that module's own doc) — the wire format is small, public, and unchanged since the
/// Associated Token Account program's original release.
///
/// accounts: `[payer (signer, writable), ata (writable), wallet (read-only), mint (read-only),
/// system_program (read-only), token_program (read-only)]` — the exact order and mutability
/// `spl-associated-token-account`'s own `create_associated_token_account_idempotent` instruction builder
/// uses.
pub fn create_recipient_ata_idempotent_ix(
    payer: &Pubkey,
    recipient: &Pubkey,
    mint: &Pubkey,
) -> solana_program::instruction::Instruction {
    let ata = recipient_ata(recipient, mint);
    solana_program::instruction::Instruction {
        program_id: zk_bridge::token::ASSOCIATED_TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(ata, false),
            AccountMeta::new_readonly(*recipient, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(solana_system_interface::program::id(), false),
            AccountMeta::new_readonly(zk_bridge::token::TOKEN_PROGRAM_ID, false),
        ],
        data: vec![1u8],
    }
}

/// accounts: `[payer (signer, writable), vault_config (writable, NEW), mint (read-only), vault_token
/// (writable, NEW), vault_authority (read-only), chain_authority (signer — must equal the settlement
/// `root.authority` for `chain_id`; see `programs/zk-bridge/README.md`), root (read-only — the settlement `["root",
/// chain_id]` PDA, derived here from `settlement_program`), token_program, system_program]`.
pub fn init_vault_ix(
    program_id: &Pubkey,
    payer: &Pubkey,
    chain_id: u64,
    mint: Pubkey,
    mint_decimals: u8,
    settlement_program: Pubkey,
    chain_authority: &Pubkey,
) -> solana_program::instruction::Instruction {
    let (vault_config, _) = vault_config_pda(program_id, &settlement_program, chain_id);
    let (vault_token, _) = vault_token_pda(program_id, &settlement_program, chain_id, &mint);
    let (vault_authority, _) = vault_authority_pda(program_id, &settlement_program, chain_id);
    let (root, _) = rome_zk_layouts::root::pda(&settlement_program, chain_id);
    ix(
        program_id,
        vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(vault_config, false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new(vault_token, false),
            AccountMeta::new_readonly(vault_authority, false),
            AccountMeta::new_readonly(*chain_authority, true),
            AccountMeta::new_readonly(root, false),
            AccountMeta::new_readonly(zk_bridge::token::TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(solana_system_interface::program::id(), false),
        ],
        BridgeIx::InitVault(InitVaultArgs {
            chain_id,
            mint,
            mint_decimals,
            settlement_program,
        }),
    )
}

/// accounts: `[funder (signer), funder_token_account (writable), vault_config (read-only), vault_token (writable),
/// token_program]`. Permissionless. `settlement_program` names which vault (of possibly several per chain — one
/// un-frontrunnable per settlement) this funds; callers normally read it off the on-chain `vault_config` they
/// intend to fund via [`decode_vault_config_account`] rather than choosing it freely, since the program
/// independently re-derives `vault_token` from it.
pub fn fund_ix(
    program_id: &Pubkey,
    funder: &Pubkey,
    funder_token_account: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
    mint: &Pubkey,
    amount: u64,
) -> solana_program::instruction::Instruction {
    let (vault_config, _) = vault_config_pda(program_id, settlement_program, chain_id);
    let (vault_token, _) = vault_token_pda(program_id, settlement_program, chain_id, mint);
    ix(
        program_id,
        vec![
            AccountMeta::new_readonly(*funder, true),
            AccountMeta::new(*funder_token_account, false),
            AccountMeta::new_readonly(vault_config, false),
            AccountMeta::new(vault_token, false),
            AccountMeta::new_readonly(zk_bridge::token::TOKEN_PROGRAM_ID, false),
        ],
        BridgeIx::Fund(FundArgs { chain_id, amount }),
    )
}

/// accounts: `[vault_config (read-only), exit_config (read-only), exit_record (writable), exit_consumer
/// (read-only), settlement_program (read-only, executable), payer_refund (writable), vault_token
/// (writable), vault_authority (read-only), recipient_ata (writable), token_program]`. Permissionless.
/// `mint`/`settlement_program`/`recipient` are read by the caller off the on-chain `vault_config`/
/// `exit_record` accounts (e.g. via [`decode_vault_config_account`]/`zk_settlement_client::
/// decode_exit_record_account`) — never chosen freely, since the program independently re-derives and
/// checks every one of them.
#[allow(clippy::too_many_arguments)]
pub fn release_exit_ix(
    program_id: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
    message_hash: [u8; 32],
    mint: &Pubkey,
    exit_config: &Pubkey,
    exit_record: &Pubkey,
    payer_refund: &Pubkey,
    recipient: &Pubkey,
) -> solana_program::instruction::Instruction {
    let (vault_config, _) = vault_config_pda(program_id, settlement_program, chain_id);
    let (vault_token, _) = vault_token_pda(program_id, settlement_program, chain_id, mint);
    let (vault_authority, _) = vault_authority_pda(program_id, settlement_program, chain_id);
    let (exit_consumer, _) = rome_zk_layouts::exit::exit_consumer_pda(chain_id, program_id);
    let recipient_ata = recipient_ata(recipient, mint);
    ix(
        program_id,
        vec![
            AccountMeta::new_readonly(vault_config, false),
            AccountMeta::new_readonly(*exit_config, false),
            AccountMeta::new(*exit_record, false),
            AccountMeta::new_readonly(exit_consumer, false),
            AccountMeta::new_readonly(*settlement_program, false),
            AccountMeta::new(*payer_refund, false),
            AccountMeta::new(vault_token, false),
            AccountMeta::new_readonly(vault_authority, false),
            AccountMeta::new(recipient_ata, false),
            AccountMeta::new_readonly(zk_bridge::token::TOKEN_PROGRAM_ID, false),
        ],
        BridgeIx::ReleaseExit(ReleaseExitArgs {
            chain_id,
            message_hash,
        }),
    )
}

/// The wrapped-SOL mint: the mint a vault holds when a chain's native asset is SOL.
pub const NATIVE_MINT: Pubkey =
    solana_program::pubkey!("So11111111111111111111111111111111111111112");

/// The `deposit_queue` PDA of a chain under the bridge program.
pub fn deposit_queue_pda(
    program_id: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
) -> (Pubkey, u8) {
    rome_zk_layouts::deposit_queue::deposit_queue::pda(
        program_id,
        &settlement_program.to_bytes(),
        chain_id,
    )
}

/// The `deposit_record` PDA of deposit `index` under the bridge program.
pub fn deposit_record_pda(
    program_id: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
    index: u64,
) -> (Pubkey, u8) {
    rome_zk_layouts::deposit_queue::deposit_record::pda(
        program_id,
        &settlement_program.to_bytes(),
        chain_id,
        index,
    )
}

/// `Deposit`: locks `amount` of the vault's mint (in base units) and queues a credit of the same value, in
/// gwei, to `l2_recipient`.
///
/// `index` is the queue's current `count`: the number of the record this deposit creates, read off the
/// on-chain queue just before sending. If another deposit lands first the record address no longer matches
/// and the program refuses by name; rebuild with the new count and send again. `fee_recipient` is the key in
/// the queue's active parameters. `depositor` signs and pays the record's rent and the fee; the record's
/// sender is `depositor`.
///
/// accounts: `[depositor (signer, writable), depositor_token (writable), vault_config (read-only),
/// vault_token (writable), deposit_queue (writable), deposit_record (writable), exit_config (read-only),
/// fee_recipient (writable), token_program, system_program, root (read-only), registry (read-only)]`.
#[allow(clippy::too_many_arguments)]
pub fn deposit_ix(
    program_id: &Pubkey,
    depositor: &Pubkey,
    depositor_token: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
    mint: &Pubkey,
    index: u64,
    fee_recipient: &Pubkey,
    amount: u64,
    l2_recipient: [u8; 20],
) -> solana_program::instruction::Instruction {
    let (vault_config, _) = vault_config_pda(program_id, settlement_program, chain_id);
    let (vault_token, _) = vault_token_pda(program_id, settlement_program, chain_id, mint);
    let (queue, _) = deposit_queue_pda(program_id, settlement_program, chain_id);
    let (record, _) = deposit_record_pda(program_id, settlement_program, chain_id, index);
    let (exit_config, _) = rome_zk_layouts::exit::exit_config::pda(settlement_program, chain_id);
    let (root, _) = rome_zk_layouts::root::pda(settlement_program, chain_id);
    let (registry, _) = rome_zk_layouts::registry::pda(settlement_program, chain_id);
    ix(
        program_id,
        vec![
            AccountMeta::new(*depositor, true),
            AccountMeta::new(*depositor_token, false),
            AccountMeta::new_readonly(vault_config, false),
            AccountMeta::new(vault_token, false),
            AccountMeta::new(queue, false),
            AccountMeta::new(record, false),
            AccountMeta::new_readonly(exit_config, false),
            AccountMeta::new(*fee_recipient, false),
            AccountMeta::new_readonly(zk_bridge::token::TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(solana_system_interface::program::id(), false),
            AccountMeta::new_readonly(root, false),
            AccountMeta::new_readonly(registry, false),
        ],
        BridgeIx::Deposit(DepositArgs {
            chain_id,
            amount,
            l2_recipient,
        }),
    )
}

/// The instructions that turn `lamports` of the wallet's SOL into wrapped SOL in its own associated token
/// account, ready to be the `depositor_token` of a `Deposit` into a vault whose mint is [`NATIVE_MINT`]:
/// create the account if it is missing, move the lamports in, and sync the token balance to them. Put them
/// ahead of `deposit_ix` in the same transaction. Returns the token account's address with the instructions.
pub fn wrap_sol_ixs(
    wallet: &Pubkey,
    lamports: u64,
) -> (Pubkey, Vec<solana_program::instruction::Instruction>) {
    let ata = recipient_ata(wallet, &NATIVE_MINT);
    let create = create_recipient_ata_idempotent_ix(wallet, wallet, &NATIVE_MINT);
    let transfer = solana_system_interface::instruction::transfer(wallet, &ata, lamports);
    // SPL Token `SyncNative`: instruction 17, one writable account (the wrapped-SOL token account).
    let sync = solana_program::instruction::Instruction {
        program_id: zk_bridge::token::TOKEN_PROGRAM_ID,
        accounts: vec![AccountMeta::new(ata, false)],
        data: vec![17u8],
    };
    (ata, vec![create, transfer, sync])
}

/// `CloseDeposit`: refunds deposit `index`'s record rent to its sender once a finalized batch has credited
/// the deposit and that batch is final. Permissionless; `sender` is the record's sender (read it off the
/// record), and the rent always goes there.
///
/// accounts: `[bridge_config (read-only), registry (read-only), cursor (read-only), deposit_record
/// (writable), sender (writable)]`. `inbox_program` is the one the bridge config names.
pub fn close_deposit_ix(
    program_id: &Pubkey,
    inbox_program: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
    index: u64,
    sender: &Pubkey,
) -> solana_program::instruction::Instruction {
    let (config, _) = rome_zk_layouts::deposit_queue::bridge_config::pda(program_id);
    let (registry, _) = rome_zk_layouts::registry::pda(settlement_program, chain_id);
    let (cursor, _) = rome_zk_layouts::cursor::pda(inbox_program, settlement_program, chain_id);
    let (record, _) = deposit_record_pda(program_id, settlement_program, chain_id, index);
    ix(
        program_id,
        vec![
            AccountMeta::new_readonly(config, false),
            AccountMeta::new_readonly(registry, false),
            AccountMeta::new_readonly(cursor, false),
            AccountMeta::new(record, false),
            AccountMeta::new(*sender, false),
        ],
        BridgeIx::CloseDeposit(CloseDepositArgs { chain_id, index }),
    )
}

/// Decoded view of the `vault_config` account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaultConfigAccount {
    pub chain_id: u64,
    pub settlement_program: Pubkey,
    pub mint: Pubkey,
    pub mint_decimals: u8,
    pub authority: Pubkey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("account too short: need {need}, got {got}")]
    TooShort { need: usize, got: usize },
    #[error("bad magic")]
    BadMagic,
    #[error("bad version")]
    BadVersion,
}

impl From<zk_bridge::state::vault_config::ReadError> for DecodeError {
    fn from(e: zk_bridge::state::vault_config::ReadError) -> Self {
        match e {
            zk_bridge::state::vault_config::ReadError::TooShort { need, got } => {
                DecodeError::TooShort { need, got }
            }
            zk_bridge::state::vault_config::ReadError::BadMagic => DecodeError::BadMagic,
            zk_bridge::state::vault_config::ReadError::BadVersion => DecodeError::BadVersion,
        }
    }
}

pub fn decode_vault_config_account(d: &[u8]) -> Result<VaultConfigAccount, DecodeError> {
    let f = zk_bridge::state::vault_config::read(d)?;
    Ok(VaultConfigAccount {
        chain_id: f.chain_id,
        settlement_program: f.settlement_program,
        mint: f.mint,
        mint_decimals: f.mint_decimals,
        authority: f.authority,
    })
}

/// Settlement verification: `vault_config`'s address is a PDA derived FROM a caller-supplied `settlement_program`
/// (`vault_config_pda`), so nothing on the wire stops a stale, mistyped, or wrong-network `--settlement` value from
/// resolving to SOME account this program owns whose *recorded* `settlement_program` field disagrees with what the
/// caller intended. `fund_ix`'s own doc already says callers "normally read [settlement_program] off the on-chain
/// vault_config" rather than trusting a freely-chosen value — this makes that a checked, named refusal instead of
/// an unenforced convention. Callers (the `vault` example's `fund` subcommand) run this BEFORE building or sending
/// `fund_ix`, so a mismatch never reaches a transaction, let alone a send.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "VaultSettlementMismatch: vault_config.settlement_program ({actual}) does not match the settlement \
     program this command was given ({expected}) — refusing before any funds move"
)]
pub struct VaultSettlementMismatch {
    pub expected: Pubkey,
    pub actual: Pubkey,
}

pub fn check_vault_settlement(
    cfg: &VaultConfigAccount,
    expected_settlement: &Pubkey,
) -> Result<(), VaultSettlementMismatch> {
    if cfg.settlement_program != *expected_settlement {
        return Err(VaultSettlementMismatch {
            expected: *expected_settlement,
            actual: cfg.settlement_program,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recipient_ata_matches_the_program_side_derivation() {
        let recipient = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let expected = zk_bridge::token::get_associated_token_address(&recipient, &mint);
        assert_eq!(recipient_ata(&recipient, &mint), expected);
    }

    #[test]
    fn create_recipient_ata_idempotent_ix_has_the_right_shape() {
        let payer = Pubkey::new_unique();
        let recipient = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let ix = create_recipient_ata_idempotent_ix(&payer, &recipient, &mint);

        assert_eq!(ix.program_id, zk_bridge::token::ASSOCIATED_TOKEN_PROGRAM_ID);
        assert_eq!(ix.data, vec![1u8], "CreateIdempotent discriminant");
        assert_eq!(ix.accounts.len(), 6);
        assert_eq!(ix.accounts[0].pubkey, payer);
        assert!(ix.accounts[0].is_signer && ix.accounts[0].is_writable);
        assert_eq!(ix.accounts[1].pubkey, recipient_ata(&recipient, &mint));
        assert!(ix.accounts[1].is_writable && !ix.accounts[1].is_signer);
        assert_eq!(ix.accounts[2].pubkey, recipient);
        assert!(!ix.accounts[2].is_writable && !ix.accounts[2].is_signer);
        assert_eq!(ix.accounts[3].pubkey, mint);
        assert_eq!(
            ix.accounts[4].pubkey,
            solana_system_interface::program::id()
        );
        assert_eq!(ix.accounts[5].pubkey, zk_bridge::token::TOKEN_PROGRAM_ID);
    }

    #[test]
    fn instruction_builders_produce_the_right_discriminant() {
        let program_id = Pubkey::new_unique();
        let ix = init_vault_ix(
            &program_id,
            &Pubkey::new_unique(),
            1,
            Pubkey::new_unique(),
            6,
            Pubkey::new_unique(),
            &Pubkey::new_unique(),
        );
        assert_eq!(ix.program_id, program_id);
        assert_eq!(ix.data[0], 0); // InitVault
        assert_eq!(ix.accounts.len(), 9);
        assert!(ix.accounts[5].is_signer, "chain_authority must sign");
        assert!(!ix.accounts[6].is_signer, "root is read-only");

        let ix = fund_ix(
            &program_id,
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            1,
            &Pubkey::new_unique(),
            1_000,
        );
        assert_eq!(ix.data[0], 1); // Fund
        assert_eq!(ix.accounts.len(), 5);

        let ix = release_exit_ix(
            &program_id,
            &Pubkey::new_unique(),
            1,
            [0u8; 32],
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
        );
        assert_eq!(ix.data[0], 2); // ReleaseExit
        assert_eq!(ix.accounts.len(), 10);
    }

    #[test]
    fn check_vault_settlement_matches_and_mismatches() {
        let settlement = Pubkey::new_unique();
        let cfg = VaultConfigAccount {
            chain_id: 1,
            settlement_program: settlement,
            mint: Pubkey::new_unique(),
            mint_decimals: 6,
            authority: Pubkey::new_unique(),
        };
        assert!(check_vault_settlement(&cfg, &settlement).is_ok());

        let wrong = Pubkey::new_unique();
        let err = check_vault_settlement(&cfg, &wrong).expect_err("must refuse a mismatch");
        assert_eq!(err.expected, wrong);
        assert_eq!(err.actual, settlement);
        assert!(err.to_string().starts_with("VaultSettlementMismatch"));
    }
}
