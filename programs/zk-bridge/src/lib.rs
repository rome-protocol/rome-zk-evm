//! `zk-bridge`: THE VAULT — the minimal SPL escrow program that releases a proved settlement exit to
//! `record.sol_recipient`. Instructions: `InitVault`, `Fund`, `ReleaseExit`, and the deposit setup:
//! `InitBridgeConfig`, `InitDepositQueue`, `ProposeDepositParams`, `ActivateDepositParams`, and the deposit
//! path: `Deposit` and `CloseDeposit`. Custody lives here;
//! verification (`ProveExit`/`ConsumeExit`) lives in `programs/zk-settlement`, which this program never
//! modifies — it only CPIs `ConsumeExit`, signed by its own `["exit_consumer", chain_id]` PDA
//! (`rome_zk_layouts::exit::exit_consumer_pda`), which is the entire mechanism by which "only the
//! registered bridge may release" holds (a keypair can never sign as a PDA).
//!
//! Deployed for real (unlike a test-only stub bridge): once a governance cycle names this program's id
//! as a chain's `exit_config.bridge_program`, its `exit_consumer` PDA is the only address that can
//! ever satisfy `zk-settlement::exit::consume_exit`'s authorisation check for that chain.
//!
//! v1 scope: native asset only, one mint per chain. `Deposit` locks tokens in the vault and queues a credit
//! on the chain; `Fund` stays as the plain top-up — see `README.md` for the full fund-safety writeup.

pub mod bridge_config;
pub mod close_deposit;
pub mod deposit;
pub mod deposit_queue;
pub mod errors;
pub mod fund;
pub mod init_vault;
pub mod instruction;
pub mod release;
pub mod state;
pub mod token;

pub use instruction::{
    ActivateDepositParamsArgs, BridgeIx, CloseDepositArgs, DepositArgs, DepositParamsArgs,
    FundArgs, InitBridgeConfigArgs, InitDepositQueueArgs, InitVaultArgs, ProposeDepositParamsArgs,
    ReleaseExitArgs,
};
pub use state::{vault_authority_pda, vault_config_pda, vault_token_pda};

use borsh::BorshDeserialize;
use solana_program::{
    account_info::AccountInfo, entrypoint::ProgramResult, program_error::ProgramError,
    pubkey::Pubkey,
};

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let ix = BridgeIx::try_from_slice(data).map_err(|_| ProgramError::InvalidInstructionData)?;
    let it = &mut accounts.iter();
    match ix {
        BridgeIx::InitVault(args) => init_vault::init_vault(program_id, it, args),
        BridgeIx::Fund(args) => fund::fund(program_id, it, args),
        BridgeIx::ReleaseExit(args) => release::release_exit(program_id, it, args),
        BridgeIx::Deposit(args) => deposit::deposit(program_id, it, args),
        BridgeIx::InitBridgeConfig(args) => bridge_config::init_bridge_config(program_id, it, args),
        BridgeIx::InitDepositQueue(args) => deposit_queue::init_deposit_queue(program_id, it, args),
        BridgeIx::ProposeDepositParams(args) => {
            deposit_queue::propose_deposit_params(program_id, it, args)
        }
        BridgeIx::ActivateDepositParams(args) => {
            deposit_queue::activate_deposit_params(program_id, it, args)
        }
        BridgeIx::CloseDeposit(args) => close_deposit::close_deposit(program_id, it, args),
    }
}
