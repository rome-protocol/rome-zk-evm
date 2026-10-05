//! The release table: each ZisK release's proofs verify under that release's row and are refused under
//! every other row, in both directions, and a proof must carry its release's pinned recursion root.
//! The fixtures are three blocks proven with ZisK 1.2.0 and the same three with ZisK 1.3.1.
mod common;
use common::*;
use solana_program::hash::hashv;
use solana_program::program_error::ProgramError;
use veritas::vk::{ZISK_1_2_0, ZISK_1_3_1};
use veritas::{verify, verify_zisk, zisk_public_signal, zisk_version, Status, ZiskVersion};

const ROOT_C_1_2_0: &str = "564c2b1bcbd5932c81cfad1fa786a98372eb3d6495257c2d944544334f84382f";
const ROOT_C_1_3_1: &str = "c3f12b9f8707c6a1e96df2bf6702c2ebdfbafedabeac654644a380befe091ac4";

fn row(scheme: u8) -> &'static ZiskVersion {
    zisk_version(scheme).expect("row")
}

fn abis(release: &str) -> Vec<(u32, Vec<u8>)> {
    RELEASE_BLOCKS
        .iter()
        .map(|&b| (b, release_fixture(release, b).abi()))
        .collect()
}

#[test]
fn the_release_table_is_frozen() {
    // A published row never changes and a number is never reused. A new release adds a row below.
    let frozen = [
        (1u8, "1.2.0-alpha", ROOT_C_1_2_0, Status::Withdrawn),
        (2u8, "1.3.1-alpha", ROOT_C_1_3_1, Status::Open),
    ];
    assert_eq!(veritas::versions::ZISK_VERSIONS.len(), frozen.len());
    for (scheme, name, root_c, status) in frozen {
        let v = row(scheme);
        assert_eq!(v.scheme, scheme);
        assert_eq!(v.name, name);
        assert_eq!(to_hex(&v.root_c), root_c, "{name}");
        assert_eq!(v.status, status, "{name}");
    }
}

#[test]
fn a_scheme_byte_no_release_has_names_no_row() {
    assert!(zisk_version(0).is_none(), "0 is the Groth16 fallback");
    for scheme in 3..=255u8 {
        assert!(zisk_version(scheme).is_none(), "scheme {scheme}");
    }
}

#[test]
fn the_1_2_0_key_exists_only_behind_the_test_feature() {
    assert_eq!(
        row(1).key.is_some(),
        cfg!(feature = "zisk-1-2-0-test-key"),
        "withdrawn row 1"
    );
    assert!(row(2).key.is_some(), "open row 2");
}

#[test]
fn the_two_keys_are_different_keys() {
    assert_ne!(ZISK_1_2_0.commitments(), ZISK_1_3_1.commitments());
    assert_eq!(ZISK_1_3_1.commitments().len(), 512);
}

#[test]
fn zisk_1_3_1_proofs_pass_under_row_2() {
    for (block, abi) in abis("1.3.1") {
        assert_eq!(verify_zisk(row(2), &abi).ok(), Some(true), "block {block}");
    }
}

#[test]
fn zisk_1_2_0_proofs_pass_under_row_1_with_the_test_key() {
    for (block, abi) in abis("1.2.0") {
        assert_eq!(verify_zisk(row(1), &abi).ok(), Some(true), "block {block}");
    }
}

#[test]
fn zisk_1_2_0_proofs_are_refused_under_row_2() {
    for (block, abi) in abis("1.2.0") {
        assert_eq!(verify_zisk(row(2), &abi).ok(), Some(false), "block {block}");
    }
}

#[test]
fn zisk_1_3_1_proofs_are_refused_under_row_1() {
    for (block, abi) in abis("1.3.1") {
        assert_eq!(verify_zisk(row(1), &abi).ok(), Some(false), "block {block}");
    }
}

/// The pairing refuses a proof under the other release's key by itself, without the recursion root pin:
/// the raw check on the proof and its own public signal says no.
#[test]
fn each_key_refuses_the_other_releases_proofs_without_the_root_pin() {
    for block in RELEASE_BLOCKS {
        let old = release_fixture("1.2.0", block);
        let new = release_fixture("1.3.1", block);
        assert_eq!(verify(&ZISK_1_2_0, &old.verify_input()).ok(), Some(true));
        assert_eq!(verify(&ZISK_1_3_1, &new.verify_input()).ok(), Some(true));
        assert_eq!(verify(&ZISK_1_3_1, &old.verify_input()).ok(), Some(false));
        assert_eq!(verify(&ZISK_1_2_0, &new.verify_input()).ok(), Some(false));
    }
}

#[test]
fn a_1_3_1_proof_with_the_1_2_0_root_c_swapped_in_is_refused() {
    let f = release_fixture("1.3.1", 14);
    let mut abi = f.abi();
    abi[800..832].copy_from_slice(&hex32(ROOT_C_1_2_0));
    assert_eq!(verify_zisk(row(2), &abi).ok(), Some(false));
    // The same with the public signal worked out from the swapped root: still refused.
    let signal = zisk_public_signal(&f.program_vk, &f.public_values, &hex32(ROOT_C_1_2_0));
    let mut raw = f.proof.to_vec();
    raw.extend_from_slice(&signal);
    assert_eq!(verify(&ZISK_1_3_1, &raw).ok(), Some(false));
}

/// A proof of a release whose root is not pinned for the row it is checked under is refused whatever
/// else it carries.
#[test]
fn every_root_c_but_the_pinned_one_is_refused() {
    let base = release_fixture("1.3.1", 166).abi();
    assert_eq!(verify_zisk(row(2), &base).ok(), Some(true));
    for byte in [800usize, 815, 831] {
        let mut abi = base.clone();
        abi[byte] ^= 1;
        assert_eq!(verify_zisk(row(2), &abi).ok(), Some(false), "byte {byte}");
    }
    let mut zero = base;
    zero[800..832].fill(0);
    assert_eq!(verify_zisk(row(2), &zero).ok(), Some(false));
}

#[test]
fn a_row_without_a_key_cannot_verify() {
    let keyless = ZiskVersion {
        key: None,
        ..*row(1)
    };
    let abi = release_fixture("1.2.0", 14).abi();
    assert_eq!(
        verify_zisk(&keyless, &abi),
        Err(ProgramError::InvalidArgument)
    );
}

#[test]
fn a_wrong_length_is_malformed_under_every_row() {
    let abi = release_fixture("1.3.1", 14).abi();
    for scheme in [1u8, 2] {
        for n in [0usize, 1, 800, 1343, 1345] {
            let mut v = abi.clone();
            v.resize(n, 0);
            assert_eq!(
                verify_zisk(row(scheme), &v),
                Err(ProgramError::InvalidInstructionData),
                "row {scheme} len {n}"
            );
        }
    }
}

#[test]
fn the_release_fixtures_carry_their_releases_pinned_root_c() {
    for (release, scheme) in [("1.2.0", 1u8), ("1.3.1", 2)] {
        for block in RELEASE_BLOCKS {
            let f = release_fixture(release, block);
            assert_eq!(f.rootc, row(scheme).root_c, "{release} block {block}");
        }
    }
}

#[test]
fn the_two_releases_prove_the_same_blocks_with_different_programs() {
    for block in RELEASE_BLOCKS {
        let old = release_fixture("1.2.0", block);
        let new = release_fixture("1.3.1", block);
        assert_ne!(old.program_vk, new.program_vk, "block {block}");
        assert_ne!(old.proof, new.proof, "block {block}");
    }
}

#[test]
fn the_release_fixtures_match_their_checksums() {
    let dir = fixture_path("zisk-releases");
    let sums = std::fs::read_to_string(dir.join("SHA256SUMS")).expect("SHA256SUMS");
    let mut checked = 0;
    for line in sums.lines().filter(|l| !l.trim().is_empty()) {
        let (want, name) = line.split_once("  ").expect("hash, two spaces, name");
        let bytes = std::fs::read(dir.join(name)).expect("fixture");
        assert_eq!(to_hex(&hashv(&[&bytes]).to_bytes()), want, "{name}");
        checked += 1;
    }
    assert_eq!(checked, 6, "three blocks for each of two releases");
}
