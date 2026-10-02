//! Pins the PDAs this crate's `pda()` functions derive for Tiber (chain id 200101,
//! as recorded in the operator's deploy config) as string-literal addresses: five accounts already live on
//! Tiber, plus four exit accounts derived but not yet sent (see that section's own doc comment below).
//!
//! Every account's PDA seeds and derivation now live in this one crate: the on-chain programs and the
//! off-chain clients all resolve to the same `seeds()`/`pda()` functions instead of each carrying its own
//! copy of a seed array. That removed an independent check that used to exist for free — with one
//! definition, a seed typo here makes the program, the clients, the delegation tests that compare a
//! client's derivation against the program's, and even the on-chain program-test suites all agree on the
//! same *wrong* address. Nothing else in the tree would go red.
//!
//! These five tests are that check, restored. A red test here means a seed change here would strand
//! every one of these live accounts on Tiber — the deployed PDAs would no longer match what this crate
//! derives, and nothing already deployed can be pointed at a new address. The pins move only alongside a
//! Tiber chain reset (run from the operator's deploy config) that redeploys the programs and
//! re-registers the chain under new addresses.

use std::str::FromStr;

use solana_program::pubkey::Pubkey;

/// Tiber's deployed inbox program id (as recorded in the operator's deploy config).
const INBOX_PROGRAM: &str = "BiPYkygsexJhDhQPbqADc631fKjKz6pwoepwMMHoC3Kj";
/// Tiber's deployed settlement program id (same sources).
const SETTLEMENT_PROGRAM: &str = "6yWj1Az1JmHBmt1654bFx2UdPMWd6Aak2QqBDPQxpj56";
/// Tiber's chain id (same sources).
const CHAIN_ID: u64 = 200101;

fn inbox_program() -> Pubkey {
    Pubkey::from_str(INBOX_PROGRAM).unwrap()
}

fn settlement_program() -> Pubkey {
    Pubkey::from_str(SETTLEMENT_PROGRAM).unwrap()
}

#[test]
fn tiber_live_root_pda() {
    let (pda, _bump) = rome_zk_layouts::root::pda(&settlement_program(), CHAIN_ID);
    assert_eq!(
        pda,
        Pubkey::from_str("Ahhj7T3hRQJJ4iACEMs5Fcvb6BhThyUUQTgHiTZaaqWC").unwrap()
    );
}

#[test]
fn tiber_live_cursor_pda() {
    let (pda, _bump) = rome_zk_layouts::cursor::pda(&inbox_program(), CHAIN_ID);
    assert_eq!(
        pda,
        Pubkey::from_str("EiEySX3KbsJQ4YkrpAXcRMJvyKchHhFzssiMgp4d9EhX").unwrap()
    );
}

#[test]
fn tiber_live_global_config_pda() {
    let (pda, _bump) = rome_zk_layouts::global_config::pda(&settlement_program());
    assert_eq!(
        pda,
        Pubkey::from_str("5NEQQYZ9f6wPKJPGPa738QRZBuANgUh4bhE857f7njJV").unwrap()
    );
}

#[test]
fn tiber_live_reserved_allow_pda() {
    let (pda, _bump) = rome_zk_layouts::reserved_allow::pda(&settlement_program(), CHAIN_ID);
    assert_eq!(
        pda,
        Pubkey::from_str("3ozgxaWvW78nexxQgjRhkyvvbLPBYfiKYpXzk4J7KNRP").unwrap()
    );
}

#[test]
fn tiber_live_chain_config_pda() {
    let (pda, _bump) = rome_zk_layouts::chain_config::pda(&settlement_program(), CHAIN_ID);
    assert_eq!(
        pda,
        Pubkey::from_str("Bpsa9dfiWWR4GfmiZQEHEwPDJmuuUteHM95vYWC3HZjG").unwrap()
    );
}

/// Exits: the four new exit account PDAs,
/// **derived now under Tiber's live settlement program id, not yet anything on chain** — no
/// `ProposeExitConfig`/`ProveExit` has ever run against this chain, so none of these addresses hold any
/// account yet. They become live literals only once those instructions are actually sent on
/// Tiber; until then, pinning them as literals here (exactly like the five tests above pin accounts that
/// already exist) still catches a seed-order/tag/endianness regression the moment it happens, rather than
/// only once they run.
#[test]
fn exit_config_pda_derived_not_yet_on_chain() {
    let (pda, _bump) = rome_zk_layouts::exit::exit_config::pda(&settlement_program(), CHAIN_ID);
    assert_eq!(
        pda,
        Pubkey::from_str("F9H7Xfk54bzXpyBKu3pTMDKmrgrv6g2uNBHNvsiZ1Wdq").unwrap()
    );
}

/// Window 0 — the first challenge-window bucket a fresh chain would ever spend against.
#[test]
fn exit_window_zero_pda_derived_not_yet_on_chain() {
    let (pda, _bump) = rome_zk_layouts::exit::exit_window::pda(&settlement_program(), CHAIN_ID, 0);
    assert_eq!(
        pda,
        Pubkey::from_str("GMVjuTzJ2a81aU818n7FV7vsxWy8Bqu57iXDMapGK4JH").unwrap()
    );
}

/// Nullifier page 0 — nonces `0..8191`.
#[test]
fn exit_nullifier_page_zero_pda_derived_not_yet_on_chain() {
    let (pda, _bump) =
        rome_zk_layouts::exit::exit_nullifier::pda(&settlement_program(), CHAIN_ID, 0);
    assert_eq!(
        pda,
        Pubkey::from_str("3agPvZHZjFmfeQNYzmELtMSKRqrhMaMtRJ1hDzpxFtZ9").unwrap()
    );
}

/// `exit_record` is keyed by a message hash rather than a small integer — a fixed, all-zero hash pins the
/// derivation exactly as the other three do (a real hash is never all-zero, but the derivation itself
/// does not care what the hash is).
#[test]
fn exit_record_pda_for_a_fixed_hash_derived_not_yet_on_chain() {
    let (pda, _bump) =
        rome_zk_layouts::exit::exit_record::pda(&settlement_program(), CHAIN_ID, [0u8; 32]);
    assert_eq!(
        pda,
        Pubkey::from_str("4zKRen2M174kbMEzHfA9xFGyLs8Tc3kF6yZJ3x3U2Kzm").unwrap()
    );
}
