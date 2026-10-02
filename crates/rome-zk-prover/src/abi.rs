//! Builds the layout-1 `proof_abi` bytes `post_root_proved_ix` sends — from a [`crate::calldata::RecordChecked`]
//! ONLY: an unchecked [`crate::calldata::Calldata`] cannot reach this
//! module's function at all — the type system enforces that `check_against_record` ran first, rather
//! than a caller having to remember to call it before building the ABI. See the `compile_fail` doctest
//! below.

use crate::calldata::RecordChecked;

/// Assembles the 1,344-byte layout-1 ABI (`zk_settlement_client::layout1_proof_abi`, the one owner of
/// the byte layout) from a record-checked proof. The only public entry point into this crate's
/// ABI-building path — there is no sibling function that takes a raw `&Calldata`.
///
/// ```compile_fail
/// // A raw `Calldata` (not a `RecordChecked`) must NOT compile here — `check_against_record` is not
/// // optional, it is the only way to obtain the type this function accepts.
/// fn build(cd: &rome_zk_prover::calldata::Calldata) -> Vec<u8> {
///     rome_zk_prover::abi::layout1_from(cd).unwrap()
/// }
/// ```
pub fn layout1_from(
    checked: &RecordChecked,
) -> Result<Vec<u8>, zk_settlement_client::ProofAbiError> {
    let cd = checked.calldata();
    zk_settlement_client::layout1_proof_abi(
        &cd.proof_bytes_768,
        &cd.program_vk,
        &cd.root_c,
        &cd.publics_512,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calldata::{check_against_record, from_zisk_proof_file};
    use crate::config::VkeyOfRecord;

    fn gate_proof_bytes() -> Vec<u8> {
        std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/prover-input/txv1-dev-reset6-batch-1.plonk.bin"
        ))
        .expect("fixtures/prover-input/txv1-dev-reset6-batch-1.plonk.bin")
    }

    fn tiber_vkey_of_record() -> VkeyOfRecord {
        VkeyOfRecord::load(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/vkeys/tiber-200101-layout1.json"
        )))
        .expect("load vkey of record")
    }

    /// The happy path: a record-checked gate proof builds the exact 1,344-byte ABI
    /// (`zk_settlement_client::LAYOUT1_PROOF_ABI_LEN`) and verifies on the host.
    #[test]
    fn a_record_checked_gate_proof_builds_an_abi_that_verifies() {
        let cd = from_zisk_proof_file(&gate_proof_bytes()).expect("decode");
        let record = tiber_vkey_of_record();
        let checked = check_against_record(&cd, &record).expect("checks clean");
        let abi = layout1_from(&checked).expect("assemble abi");
        assert_eq!(abi.len(), zk_settlement_client::LAYOUT1_PROOF_ABI_LEN);
        assert!(veritas::verify_zisk(&abi).expect("verify_zisk"));
    }
}
