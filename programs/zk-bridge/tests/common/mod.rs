//! Shared real-BPF test rig for `programs/zk-bridge`: loads the real, `cargo build-sbf`-compiled `.so`
//! for `zk_bridge` + `zk_settlement` + the REAL SPL Token program (built straight from the pinned
//! `spl-token = "=9.0.0"` crate's own source — see `../README.md`). Most tests hand-build the
//! settlement/vault accounts directly (`pt.add_account`), the same "fixture, not a full ceremony"
//! pattern `programs/zk-settlement/tests/exit_consume.rs` already establishes for `ConsumeExit` —
//! `ReleaseExit`'s own logic does not care how a record reached PROVED or how a vault reached funded,
//! only that it did.
//!
//! `#![allow(dead_code)]`: each `tests/*.rs` file compiles this module as its own separate copy (the
//! standard Rust integration-test shape) — a helper only `release_exit.rs` needs reads as dead code inside
//! `vault.rs`'s copy, and vice versa.

#![allow(dead_code)]

use rome_zk_layouts::exit::{exit_config, exit_record};
use solana_program::pubkey::Pubkey;
use solana_program_test::ProgramTestContext;
use solana_sdk::account::Account;
use solana_system_interface::program as system_program;

pub const CHAIN_ID: u64 = 200101;
pub const MESSAGE_HASH: [u8; 32] = [0x77u8; 32];

pub fn settlement_program_id() -> Pubkey {
    rome_zk_testkit::fixed_settlement_program_id()
}
pub fn bridge_program_id() -> Pubkey {
    rome_zk_testkit::fixed_zk_bridge_program_id()
}
pub fn token_program_id() -> Pubkey {
    rome_zk_testkit::spl_token_program_id()
}
/// The real, well-known Associated Token Account program id — `zk_bridge::token::
/// ASSOCIATED_TOKEN_PROGRAM_ID` re-exported here so every test file loads it the same way it loads
/// `token_program_id()` above (the `release-exit` client tool's idempotent-create ix CPIs
/// this program for real in `release_exit.rs`'s own real-BPF test).
pub fn associated_token_program_id() -> Pubkey {
    zk_bridge::token::ASSOCIATED_TOKEN_PROGRAM_ID
}

/// The bridge's `["bridge_config"]` address.
pub fn bridge_config_pda() -> Pubkey {
    rome_zk_layouts::deposit_queue::bridge_config::pda(&bridge_program_id()).0
}

/// The bridge config, owned by the bridge, naming `settlement` as the canonical settlement program.
pub fn bridge_config_account(settlement: Pubkey) -> Account {
    use rome_zk_layouts::deposit_queue::bridge_config;
    let mut d = vec![0u8; bridge_config::LEN];
    bridge_config::write(
        &mut d,
        &bridge_config::BridgeConfigFields {
            settlement_program: settlement.to_bytes(),
            inbox_program: rome_zk_testkit::fixed_inbox_program_id().to_bytes(),
        },
    );
    Account {
        lamports: rome_zk_testkit::rent_exempt(d.len()),
        data: d,
        owner: bridge_program_id(),
        executable: false,
        rent_epoch: 0,
    }
}

pub fn funded_account(lamports: u64) -> Account {
    Account {
        lamports,
        data: vec![],
        owner: system_program::id(),
        executable: false,
        rent_epoch: 0,
    }
}

/// A hand-built, valid SPL Token `Mint` account (owned by the real SPL Token program) — the standard
/// 82-byte layout (`mint_authority: COption<Pubkey>` 36B, `supply: u64` 8B, `decimals: u8`,
/// `is_initialized: u8`, `freeze_authority: COption<Pubkey>` 36B), unchanged since the program's original
/// release. No mint/freeze authority (`COption::None` = a zero u32 tag); this program's own `token`
/// module never decodes a mint's fields, so it does not need `zk_bridge::token` re-exports for this.
pub fn mint_account(decimals: u8) -> Account {
    let mut data = vec![0u8; zk_bridge::token::MINT_LEN];
    // mint_authority: COption::None (bytes 0..36 stay zero)
    // supply: 0 (bytes 36..44 stay zero)
    data[44] = decimals;
    data[45] = 1; // is_initialized = true
                  // freeze_authority: COption::None (bytes 46..82 stay zero)
    Account {
        lamports: rome_zk_testkit::rent_exempt(data.len()),
        data,
        owner: token_program_id(),
        executable: false,
        rent_epoch: 0,
    }
}

/// A hand-built, valid SPL Token `Account` (owned by the real SPL Token program) — the standard 165-byte
/// layout (`mint` 32B, `owner` 32B, `amount: u64` 8B, `delegate: COption<Pubkey>` 36B, `state: u8`,
/// `is_native: COption<u64>` 12B, `delegated_amount: u64` 8B, `close_authority: COption<Pubkey>` 36B).
/// `owner` here is the token account's own authority field (a wallet, or the `vault_authority` PDA for the
/// vault's own account) — not the Solana account owner, which is always the token program for an SPL
/// account. `state = 1` (`Initialized`); no delegate, no native-SOL wrapping, no close authority.
pub fn token_account(mint: &Pubkey, owner: &Pubkey, amount: u64) -> Account {
    let mut data = vec![0u8; zk_bridge::token::TOKEN_ACCOUNT_LEN];
    data[0..32].copy_from_slice(mint.as_ref());
    data[32..64].copy_from_slice(owner.as_ref());
    data[64..72].copy_from_slice(&amount.to_le_bytes());
    // delegate: COption::None (72..108 stay zero)
    data[108] = 1; // state = Initialized
                   // is_native: COption::None (109..121 stay zero)
                   // delegated_amount: 0 (121..129 stay zero)
                   // close_authority: COption::None (129..165 stay zero)
    Account {
        lamports: rome_zk_testkit::rent_exempt(data.len()),
        data,
        owner: token_program_id(),
        executable: false,
        rent_epoch: 0,
    }
}

/// The token account's `amount` field only — everything this workspace's own tests need to assert on.
pub fn decode_token_amount(d: &[u8]) -> u64 {
    zk_bridge::token::read_token_amount(d)
}

/// Base `ProgramTest` loading `zk_bridge` + `zk_settlement` + the real SPL Token program + the real
/// Associated Token Account program (`release_exit.rs`'s idempotent-create test CPIs it for
/// real; every other test simply never references it). Callers add whatever accounts their test needs
/// before `start_with_context`.
///
/// The bridge config is in place, naming `settlement_program_id()` as the canonical settlement program: every
/// instruction under test here that takes the config (`InitVault`, `ReleaseExit`) reads it. A test that needs
/// another config adds its own account at `bridge_config_pda()`, which replaces this one.
pub fn base_program_test() -> solana_program_test::ProgramTest {
    let mut pt = base_program_test_without_config();
    pt.add_account(
        bridge_config_pda(),
        bridge_config_account(settlement_program_id()),
    );
    pt
}

/// [`base_program_test`] without the bridge config account.
pub fn base_program_test_without_config() -> solana_program_test::ProgramTest {
    rome_zk_testkit::program_test(
        &[
            rome_zk_testkit::ProgramSpec::new("zk_bridge", bridge_program_id()),
            rome_zk_testkit::ProgramSpec::upgradeable("zk_settlement", settlement_program_id()),
            rome_zk_testkit::ProgramSpec::new("spl_token", token_program_id()),
            rome_zk_testkit::ProgramSpec::new(
                "spl_associated_token_account",
                associated_token_program_id(),
            ),
        ],
        true,
    )
}

/// The settlement `root` account for `CHAIN_ID`, owned by `settlement_program_id()`, with `authority` set
/// to the given chain-authority pubkey — the account `InitVault`'s chain-authority gate reads.
/// `rome_zk_testkit::root_account_with_authority` already builds the exact layout `rome_zk_layouts::root`
/// defines; this just pins the owner every `InitVault` test needs.
pub fn root_account(authority: &Pubkey) -> Account {
    rome_zk_testkit::root_account_with_authority(CHAIN_ID, authority, settlement_program_id())
}

/// The `["root", chain_id]` PDA under `settlement_program_id()` for `CHAIN_ID` — the address `InitVault`'s
/// gate expects the `root` account to live at.
pub fn root_pda() -> Pubkey {
    rome_zk_layouts::root::pda(&settlement_program_id(), CHAIN_ID).0
}

pub fn vault_config_account(
    settlement_program: Pubkey,
    mint: Pubkey,
    mint_decimals: u8,
    authority: Pubkey,
) -> Account {
    let d =
        zk_bridge::state::vault_config::write(&zk_bridge::state::vault_config::VaultConfigFields {
            chain_id: CHAIN_ID,
            settlement_program,
            mint,
            mint_decimals,
            authority,
        })
        .to_vec();
    Account {
        lamports: rome_zk_testkit::rent_exempt(d.len()),
        data: d,
        owner: bridge_program_id(),
        executable: false,
        rent_epoch: 0,
    }
}

/// A hand-built PROVED `exit_record`, owned by `owner` (normally `settlement_program_id()`; the
/// wrong-settlement-owner test passes something else). `asset` defaults to native (`[0; 20]`) — pass a
/// non-zero value for the unsupported-asset test.
#[allow(clippy::too_many_arguments)]
pub fn exit_record_account(
    owner: Pubkey,
    sol_recipient: [u8; 32],
    amount: u128,
    payer: Pubkey,
    asset: [u8; 20],
    status: u8,
) -> Account {
    let d = exit_record::write(&exit_record::ExitRecordFields {
        chain_id: CHAIN_ID,
        batch: 1,
        message_hash: MESSAGE_HASH,
        sol_recipient,
        amount,
        window_index: 0,
        proved_slot: 0,
        status,
        payer: payer.to_bytes(),
        asset,
    })
    .to_vec();
    Account {
        lamports: rome_zk_testkit::rent_exempt(d.len()),
        data: d,
        owner,
        executable: false,
        rent_epoch: 0,
    }
}

pub fn exit_config_account(owner: Pubkey, bridge_program: Pubkey) -> Account {
    let d = exit_config::write(&exit_config::ExitConfigFields {
        chain_id: CHAIN_ID,
        exit_portal: [0x11u8; 20],
        bridge_program: bridge_program.to_bytes(),
        pending_exit_portal: [0u8; 20],
        pending_bridge_program: [0u8; 32],
        pending_exit_cap: 0,
        pending_poster_bond: 0,
        activation_slot: 0,
        pending_mask: 0,
    })
    .to_vec();
    Account {
        lamports: rome_zk_testkit::rent_exempt(d.len()),
        data: d,
        owner,
        executable: false,
        rent_epoch: 0,
    }
}

pub async fn get_account(ctx: &mut ProgramTestContext, key: Pubkey) -> Option<Account> {
    ctx.banks_client.get_account(key).await.unwrap()
}
