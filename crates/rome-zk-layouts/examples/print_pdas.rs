//! Prints the five chain-scoped PDAs a Tiber-shaped chain reset needs to move together: root, batch
//! cursor, global config, reserved-allow marker and chain config (the "PDAs" table in the deploy
//! config, `crates/rome-zk-layouts/tests/pda_pins.rs`'s five pinned literals). `global_config` is
//! scoped to the settlement program alone (no chain id in its seeds — one config per program, not per chain);
//! the other four take `chain_id`.
//!
//! Two output shapes, both printed every run so the operator moves the deploy config's PDAs table and the pins
//! together: first the Markdown table rows in the exact column order that table uses (`| account | program | address
//! |`), then the Rust literal shape `pda_pins.rs`'s five tests assert against (`Pubkey::from_str("...").unwrap()`, one
//! per named test function) so a diff against that file is a straight paste.
//!
//! Usage: `cargo run -p rome-zk-layouts --example print_pdas -- <inbox_program_id> <settlement_program_id> <chain_id>`

use rome_zk_layouts::{chain_config, cursor, global_config, reserved_allow, root};
use solana_program::pubkey::Pubkey;
use std::str::FromStr;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    assert!(
        argv.len() == 3,
        "usage: print_pdas <inbox_program_id> <settlement_program_id> <chain_id>"
    );
    let inbox_program = Pubkey::from_str(&argv[0]).expect("bad inbox_program_id");
    let settlement_program = Pubkey::from_str(&argv[1]).expect("bad settlement_program_id");
    let chain_id: u64 = argv[2].parse().expect("bad chain_id");

    let (root_pda, _) = root::pda(&settlement_program, chain_id);
    let (cursor_pda, _) = cursor::pda(&inbox_program, &settlement_program, chain_id);
    let (global_config_pda, _) = global_config::pda(&settlement_program);
    let (reserved_allow_pda, _) = reserved_allow::pda(&settlement_program, chain_id);
    let (chain_config_pda, _) = chain_config::pda(&settlement_program, chain_id);

    println!("-- Markdown (PDAs table) --");
    println!("| account | program | address |");
    println!("|---|---|---|");
    println!("| root | zk-settlement | `{root_pda}` |");
    println!("| batch cursor | zk-inbox | `{cursor_pda}` |");
    println!("| global config | zk-settlement | `{global_config_pda}` |");
    println!(
        "| reserved-allow marker (chain {chain_id}) | zk-settlement | `{reserved_allow_pda}` |"
    );
    println!("| chain config (chain {chain_id}) | zk-settlement | `{chain_config_pda}` |");
    println!();
    println!("-- Rust literal (crates/rome-zk-layouts/tests/pda_pins.rs) --");
    println!("tiber_live_root_pda:            Pubkey::from_str(\"{root_pda}\").unwrap()");
    println!("tiber_live_cursor_pda:          Pubkey::from_str(\"{cursor_pda}\").unwrap()");
    println!("tiber_live_global_config_pda:   Pubkey::from_str(\"{global_config_pda}\").unwrap()");
    println!("tiber_live_reserved_allow_pda:  Pubkey::from_str(\"{reserved_allow_pda}\").unwrap()");
    println!("tiber_live_chain_config_pda:    Pubkey::from_str(\"{chain_config_pda}\").unwrap()");
}
