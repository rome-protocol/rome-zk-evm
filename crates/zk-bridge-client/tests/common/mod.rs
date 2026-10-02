//! Shared real-BPF test rig for `crates/zk-bridge-client`'s own integration tests — a REDUCED copy of
//! `programs/zk-bridge/tests/common/mod.rs`'s own helpers (mint accounts, SPL token accounts, `vault_config`
//! accounts, the shared `base_program_test`): that module is private to its own crate's test binaries and cannot be
//! imported across the crate boundary, and "each `tests/*.rs` file compiles this module as its own separate copy"
//! is already this workspace's own established pattern for per-test-binary helpers (see that file's own doc) — this
//! just repeats it one crate over. Every encoding here calls the REAL producer
//! (`zk_bridge::state::vault_config::write`, `zk_bridge::token::{TOKEN_ACCOUNT_LEN, MINT_LEN, read_token_amount}`),
//! never a hand-rolled layout.

#![allow(dead_code)]

use solana_program::pubkey::Pubkey;
// `system_program` moved out of `solana_program`'s root re-export in the Agave 4.x line (API fallout).
use solana_sdk::account::Account;
use solana_system_interface::program as system_program;

pub const CHAIN_ID: u64 = 200101;

pub fn bridge_program_id() -> Pubkey {
    rome_zk_testkit::fixed_zk_bridge_program_id()
}
pub fn token_program_id() -> Pubkey {
    rome_zk_testkit::spl_token_program_id()
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

/// A hand-built, valid SPL Token `Mint` account — same 82-byte layout
/// `programs/zk-bridge/tests/common/mod.rs::mint_account` documents in full.
pub fn mint_account(decimals: u8) -> Account {
    let mut data = vec![0u8; zk_bridge::token::MINT_LEN];
    data[44] = decimals;
    data[45] = 1; // is_initialized = true
    Account {
        lamports: rome_zk_testkit::rent_exempt(data.len()),
        data,
        owner: token_program_id(),
        executable: false,
        rent_epoch: 0,
    }
}

/// A hand-built, valid SPL Token `Account` — same 165-byte layout
/// `programs/zk-bridge/tests/common/mod.rs::token_account` documents in full.
pub fn token_account(mint: &Pubkey, owner: &Pubkey, amount: u64) -> Account {
    let mut data = vec![0u8; zk_bridge::token::TOKEN_ACCOUNT_LEN];
    data[0..32].copy_from_slice(mint.as_ref());
    data[32..64].copy_from_slice(owner.as_ref());
    data[64..72].copy_from_slice(&amount.to_le_bytes());
    data[108] = 1; // state = Initialized
    Account {
        lamports: rome_zk_testkit::rent_exempt(data.len()),
        data,
        owner: token_program_id(),
        executable: false,
        rent_epoch: 0,
    }
}

/// A `vault_config` account owned by `bridge_program_id()`, with `settlement_program`/`mint`/
/// `mint_decimals`/`authority` exactly as given — deliberately does NOT derive its own address; callers
/// place it wherever the scenario under test needs (see `vault_example.rs`'s own mismatch fixture,
/// which places one at the PDA for a DIFFERENT settlement program than the one recorded inside it).
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

/// Loads the real `zk_bridge` + real SPL Token program `.so` files (built by `cargo build-sbf` — see
/// `Makefile`'s `remote-test` target, which builds every `programs/*/` before running `cargo test`).
/// Neither program is actually INVOKED by this crate's own tests (they only fetch+decode account state
/// hand-placed by `pt.add_account`), but loading the real binaries keeps this rig genuinely "real-BPF"
/// rather than a bare in-memory bank with no programs at all.
pub fn base_program_test() -> solana_program_test::ProgramTest {
    rome_zk_testkit::program_test(
        &[
            rome_zk_testkit::ProgramSpec::new("zk_bridge", bridge_program_id()),
            rome_zk_testkit::ProgramSpec::new("spl_token", token_program_id()),
        ],
        true,
    )
}

pub async fn get_account(
    ctx: &mut solana_program_test::ProgramTestContext,
    key: Pubkey,
) -> Option<Account> {
    ctx.banks_client.get_account(key).await.unwrap()
}
