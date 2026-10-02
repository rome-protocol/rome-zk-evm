//! Spec 1.4 and 8.2: the built program, run as real SBPF v3 bytecode in solana-program-test.
//! Needs `target/deploy/veritas.so` (`cargo build-sbf`, see the README).
mod common;
use common::*;
use solana_program_test::ProgramTest;
use solana_sdk::{
    hash::Hash,
    instruction::{Instruction, InstructionError},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    transaction::{Transaction, TransactionError},
};
use std::sync::Once;

/// Spec 8.2: the old verifier's cost on the block-14 ABI. Veritas must not exceed it.
const CU_TARGET: u64 = 541_225;

fn program_id() -> Pubkey {
    Pubkey::new_from_array([7u8; 32])
}

static SET_DIR: Once = Once::new();

async fn start() -> (solana_program_test::BanksClient, Keypair, Hash) {
    SET_DIR.call_once(|| {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/deploy");
        std::env::set_var("SBF_OUT_DIR", dir);
    });
    let mut pt = ProgramTest::new("veritas", program_id(), None);
    pt.prefer_bpf(true);
    // The transaction carries no ComputeBudget instruction, so `compute_units_consumed` is the
    // program's own cost. The cap is the 1.4M ceiling of spec 8.2.
    pt.set_compute_max_units(1_400_000);
    pt.start().await
}

/// Sends `data` as the instruction data with no accounts; returns the result and the CU consumed.
async fn call(
    banks: &solana_program_test::BanksClient,
    payer: &Keypair,
    blockhash: Hash,
    data: Vec<u8>,
) -> (Result<(), TransactionError>, u64) {
    let ix = Instruction::new_with_bytes(program_id(), &data, vec![]);
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[payer], blockhash);
    let out = banks
        .process_transaction_with_metadata(tx)
        .await
        .expect("banks client");
    let cu = out.metadata.map(|m| m.compute_units_consumed).unwrap_or(0);
    (out.result, cu)
}

fn invalid_data() -> Result<(), TransactionError> {
    Err(TransactionError::InstructionError(
        0,
        InstructionError::InvalidInstructionData,
    ))
}

fn block14() -> Fixture {
    all_fixtures()
        .into_iter()
        .find(|f| f.name == "block14")
        .unwrap()
}

#[tokio::test]
async fn block14_succeeds_within_the_budget() {
    let (banks, payer, bh) = start().await;
    let (res, cu) = call(&banks, &payer, bh, block14().abi()).await;
    println!("MEASURED_CU block14 = {cu}");
    assert_eq!(res, Ok(()));
    assert!(
        cu > 100_000,
        "implausibly low CU {cu}: was the program really run?"
    );
    assert!(cu <= CU_TARGET, "{cu} CU is over the target of {CU_TARGET}");
}

#[tokio::test]
async fn all_four_real_proofs_succeed() {
    let (banks, payer, bh) = start().await;
    for f in all_fixtures() {
        let (res, cu) = call(&banks, &payer, bh, f.abi()).await;
        println!("MEASURED_CU {} = {cu}", f.name);
        assert_eq!(res, Ok(()), "{}", f.name);
        assert!(cu <= CU_TARGET, "{}: {cu} CU", f.name);
    }
}

#[tokio::test]
async fn a_flipped_proof_bit_fails_with_invalid_instruction_data() {
    let (banks, payer, bh) = start().await;
    let abi = block14().abi();
    // Byte 0 is part of a curve point (MALFORMED or REJECT); byte 767 is the last evaluation
    // (a well-formed field element, so REJECT). Both must give the same error. Bytes 31 and 255 are the
    // lowest bits of the x word of [a]_1 and the y word of [z]_1 (off the curve, MALFORMED).
    for (byte, bit) in [
        (0usize, 0u8),
        (1, 3),
        (31, 0),
        (100, 5),
        (255, 0),
        (500, 1),
        (767, 0),
        (767, 7),
    ] {
        let mut bad = abi.clone();
        bad[byte] ^= 1 << bit;
        let (res, cu) = call(&banks, &payer, bh, bad).await;
        println!("flip byte {byte} bit {bit}: cu {cu}");
        assert_eq!(res, invalid_data(), "byte {byte} bit {bit}");
    }
}

#[tokio::test]
async fn a_changed_public_value_fails_with_invalid_instruction_data() {
    let (banks, payer, bh) = start().await;
    let mut bad = block14().abi();
    bad[1000] ^= 1;
    let (res, _) = call(&banks, &payer, bh, bad).await;
    assert_eq!(res, invalid_data());
}

#[tokio::test]
async fn a_wrong_length_fails_with_invalid_instruction_data() {
    let (banks, payer, bh) = start().await;
    let abi = block14().abi();
    let mut longer = abi.clone();
    longer.push(0);
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty", vec![]),
        ("one byte", vec![0]),
        ("1343 bytes", abi[..1343].to_vec()),
        ("1345 bytes", longer),
        ("800 bytes", abi[..800].to_vec()),
    ];
    for (name, data) in cases {
        let (res, _) = call(&banks, &payer, bh, data).await;
        assert_eq!(res, invalid_data(), "{name}");
    }
}

/// A prover chooses a-bar so that the scalar of [z]_1 in step 9 is zero, and flips a y bit of [z]_1 so
/// that it is off the curve. The syscalls must still decode the point: the program fails with
/// `InvalidInstructionData`. Goes red if a future solana-bn254 or Agave skipped the point for a zero scalar.
#[tokio::test]
async fn an_off_curve_z_commitment_with_a_zero_scalar_fails() {
    let (banks, payer, bh) = start().await;
    let f = block14();
    let signal = veritas::zisk_public_signal(&f.program_vk, &f.public_values, &f.rootc);
    let mut proof = f.proof;
    proof[32 * 7 + 31] ^= 1;
    assert!(!on_curve_or_identity(&proof[192..256]));
    make_z_coefficient_zero(&mut proof, &signal);
    assert_eq!(z_coefficient(&proof, &signal), [0u8; 32]);
    let mut abi = f.abi();
    abi[..768].copy_from_slice(&proof);
    let (res, cu) = call(&banks, &payer, bh, abi).await;
    println!("off-curve [z]_1, zero scalar: cu {cu}");
    assert_eq!(res, invalid_data());
    // Control: the same a-bar with [z]_1 on the curve runs to the end of the computation (it is
    // REJECT, which the program also reports as InvalidInstructionData, but it costs the full pairing).
    let mut control = f.proof;
    make_z_coefficient_zero(&mut control, &signal);
    let mut abi = f.abi();
    abi[..768].copy_from_slice(&control);
    let (res, control_cu) = call(&banks, &payer, bh, abi).await;
    println!("on-curve [z]_1, zero scalar: cu {control_cu}");
    assert_eq!(res, invalid_data());
    assert!(
        control_cu > 300_000,
        "control did not run the whole verification: {control_cu} CU"
    );
}
