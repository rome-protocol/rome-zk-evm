//! A TEST-ONLY stand-in for the real bridge program (`programs/zk-bridge`) — the real-BPF
//! `ConsumeExit` tests load this the same way `programs/zk-settlement`'s own test suite already loads
//! `zk-inbox` as a second real program (`tests/settlement.rs`): via `solana-program-test`'s `prefer_bpf`,
//! from a genuinely `cargo build-sbf`-compiled `.so`, never a host-native shortcut.
//!
//! **Never a shipped, deployed program.** This crate lives under
//! `programs/zk-settlement/tests/fixtures/`, its own single-crate `[workspace]` (see this directory's
//! `Cargo.toml`), outside the root workspace's `programs/*` member glob — the `test` CI job builds its
//! `.so` by explicit manifest path (never the `programs/*/` loop the `build-sbf` job's release-artifact
//! upload uses), so it can never be mistaken for, or accidentally published alongside, a real deployed
//! program.
//!
//! **What it does.** One instruction: decode `(chain_id: u64 LE, message_hash: [u8; 32])` from its own
//! instruction data, then CPI `programs/zk-settlement`'s `ConsumeExit` (discriminant 29), signing the
//! `exit_consumer` PDA `["exit_consumer", chain_id]` UNDER ITS OWN PROGRAM ID via `invoke_signed` — the
//! one thing only a program's own runtime identity can do, never a keypair (a PDA has no private key).
//! This is exactly the mechanism the real `zk-bridge` program uses, and exactly what
//! `ConsumeExit`'s `NotBridgeProgram` guard tests for: whether the SIGNED `exit_consumer` PDA equals
//! `exit_config.bridge_program`'s own derivation. A test that wants the "wrong bridge" refusal simply
//! configures `exit_config.bridge_program` to some OTHER pubkey — this stub always signs its own PDA
//! regardless, so mismatching `exit_config.bridge_program` is exactly what makes that call refuse.
//!
//! Accounts (this program's OWN instruction, in order): `[settlement_program (read-only, executable),
//! bridge_signer (the `exit_consumer` PDA under THIS program's id — not itself a signer of the outer
//! instruction; becomes one only for the inner CPI, via `invoke_signed`), exit_config (read-only),
//! exit_record (writable), payer_refund (writable)]` — the same four accounts `ConsumeExit` itself reads,
//! passed straight through in the CPI.
#![forbid(unsafe_code)]

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    log::sol_log_compute_units,
    msg,
    program::invoke_signed,
    program_error::ProgramError,
    pubkey::Pubkey,
};

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

/// `["exit_consumer", chain_id]` — must byte-for-byte match `rome_zk_layouts::exit::exit_consumer_seeds`
/// (this crate deliberately does not depend on `rome_zk_layouts` — it stays a minimal, self-contained test
/// fixture; the seed tag and byte order are pinned independently here, and `programs/zk-settlement`'s own
/// `exit_consumer_pda_is_deterministic_and_varies_with_chain_id_and_bridge_program` test is what catches
/// either side drifting).
const EXIT_CONSUMER_TAG: &[u8; 13] = b"exit_consumer";

pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    if instruction_data.len() != 40 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let chain_id = u64::from_le_bytes(instruction_data[0..8].try_into().unwrap());
    let message_hash: [u8; 32] = instruction_data[8..40].try_into().unwrap();

    let it = &mut accounts.iter();
    let settlement_program = next_account_info(it)?;
    let bridge_signer = next_account_info(it)?;
    let exit_config = next_account_info(it)?;
    let exit_record = next_account_info(it)?;
    let payer_refund = next_account_info(it)?;

    let chain_id_bytes = chain_id.to_le_bytes();
    let (expect_signer, bump) =
        Pubkey::find_program_address(&[EXIT_CONSUMER_TAG, &chain_id_bytes], program_id);
    if expect_signer != *bridge_signer.key {
        return Err(ProgramError::InvalidSeeds);
    }

    // `ConsumeExit`'s own borsh wire shape: [discriminant 29][chain_id: u64 LE][message_hash: 32 bytes] —
    // a struct-variant enum's borsh encoding is its index byte followed by its fields in declaration
    // order, each borsh-serialized (a fixed-size `u64` as 8 LE bytes, a fixed-size `[u8; 32]` raw with no
    // length prefix) — the exact bytes `borsh::to_vec(&SettleIx::ConsumeExit(...))` would produce.
    let mut data = Vec::with_capacity(41);
    data.push(29u8);
    data.extend_from_slice(&chain_id.to_le_bytes());
    data.extend_from_slice(&message_hash);

    let cpi_ix = Instruction {
        program_id: *settlement_program.key,
        accounts: vec![
            AccountMeta::new_readonly(*bridge_signer.key, true),
            AccountMeta::new_readonly(*exit_config.key, false),
            AccountMeta::new(*exit_record.key, false),
            AccountMeta::new(*payer_refund.key, false),
        ],
        data,
    };

    msg!("stub bridge: CPI-ing ConsumeExit, chain {}", chain_id);
    sol_log_compute_units(); // "before" marker — the test diffs this against the "after" marker below
                             // to isolate ConsumeExit's own CU cost from this stub's surrounding overhead.
    invoke_signed(
        &cpi_ix,
        &[
            bridge_signer.clone(),
            exit_config.clone(),
            exit_record.clone(),
            payer_refund.clone(),
        ],
        &[&[EXIT_CONSUMER_TAG, &chain_id_bytes, &[bump]]],
    )?;
    sol_log_compute_units(); // "after" marker

    Ok(())
}
