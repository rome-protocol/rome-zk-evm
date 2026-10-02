//! Spec 9.1 (the real proofs), 9.2 (decoding) and 1.5 (the public signal).
mod common;
use common::*;
use solana_program::hash::hashv;
use veritas::{verify, verify_zisk, zisk_public_signal};

#[test]
fn proof_files_decode_with_nothing_left_over() {
    let b14 = std::fs::read(fixture_path("s10/block14.zisk.bin")).unwrap();
    assert_eq!(
        to_hex(&hashv(&[&b14]).to_bytes()),
        "149ec89354a278d6a3433747146defb5de0dd044a382f628860a16c45353c568"
    );
    let pf = decode_proof_file(&b14);
    assert_eq!(
        (pf.protocol.as_str(), pf.curve.as_str()),
        ("plonk", "bn128")
    );
    assert_eq!((pf.n_public, pf.power), (1, 24));
    assert_eq!((pf.k1.as_str(), pf.k2.as_str()), ("2", "3"));
    // the ABI built from the .bin is byte-identical to block14.calldata.json's
    assert_eq!(pf.abi, calldata("block14.calldata.json", "block14").abi());

    let rb = std::fs::read(fixture_path(
        "prover-input/txv1-dev-reset6-batch-1.plonk.bin",
    ))
    .unwrap();
    assert_eq!(
        to_hex(&hashv(&[&rb]).to_bytes()),
        "5f3d79603aabb2a45aef5332b199a660fff448d654615475a833e92826022f3b"
    );
    decode_proof_file(&rb);
}

#[test]
fn fixture_hashes_match_the_spec_table() {
    let want = [
        (
            "e9829b6b0a24a05fcb3341b586471d9b572c715eb2dfe24637bacacad8f26f90",
            "619f058a8906c1550f7fc5b55c53c3983609f7cee0bb206127b425f92b02e942",
        ),
        (
            "58e833635b13d61d81cd4500aba2eb569f928344fa0f0dd0f0eefed87facc089",
            "cedf844d6ce42ec294d794a74bcf4f5d5b63549d9d17d34dfa863ebca9a1e46a",
        ),
        (
            "3383d22eb17131c180ba8037cbda94605a62e2b6c87e9028cd2ddc01aaca0da3",
            "6e5784a109b92a7fcfc37edfafe053801bf17bbb585f0b557352a8174783541b",
        ),
        (
            "ce0bf49d8356d574cccdbd29281d8ae1152bc1ade44c23be19eae789a3294e15",
            "d654cd548a4b4ce8fcfc067cf6e83baf1fe98d2d999dc22262798e9a7bacc5af",
        ),
    ];
    for (f, (proof, abi)) in all_fixtures().iter().zip(want) {
        assert_eq!(to_hex(&hashv(&[&f.proof]).to_bytes()), proof, "{}", f.name);
        assert_eq!(to_hex(&hashv(&[&f.abi()]).to_bytes()), abi, "{}", f.name);
    }
}

#[test]
fn verifying_key_constants_equal_the_proof_files() {
    for rel in [
        "s10/block14.zisk.bin",
        "prover-input/txv1-dev-reset6-batch-1.plonk.bin",
    ] {
        let pf = decode_proof_file(&std::fs::read(fixture_path(rel)).unwrap());
        let ours = [
            veritas::vk::Q_M,
            veritas::vk::Q_L,
            veritas::vk::Q_R,
            veritas::vk::Q_O,
            veritas::vk::Q_C,
            veritas::vk::S_SIGMA_1,
            veritas::vk::S_SIGMA_2,
            veritas::vk::S_SIGMA_3,
        ];
        for (c, want) in pf.commitments.iter().zip(ours) {
            assert_eq!(&dec_to_w(&c[0])[..], &want[..32], "{rel} x");
            assert_eq!(&dec_to_w(&c[1])[..], &want[32..], "{rel} y");
            assert_eq!(c[2], "1");
        }
        // [x]_2: the file lists (c0, c1) pairs; the syscall encoding is x1 x0 y1 y0
        let x = &veritas::vk::X_G2;
        assert_eq!(&dec_to_w(&pf.x2[0][0])[..], &x[32..64], "x0");
        assert_eq!(&dec_to_w(&pf.x2[0][1])[..], &x[0..32], "x1");
        assert_eq!(&dec_to_w(&pf.x2[1][0])[..], &x[96..128], "y0");
        assert_eq!(&dec_to_w(&pf.x2[1][1])[..], &x[64..96], "y1");
        assert_eq!(dec_to_w(&pf.omega), veritas::vk::OMEGA);
        assert_eq!(veritas::vk::COMMITMENTS.len(), 512);
    }
}

#[test]
fn public_signal_matches_calldata_fixtures() {
    for (file, name, want) in [
        (
            "block14.calldata.json",
            "block14",
            "0f338098f9447880a24956da8cf0897d9ee684d830107f2ec6427da80f7afc11",
        ),
        (
            "block166.calldata.json",
            "block166",
            "2d842ba4fcf2fde701af21f28ae9f6b3df5d8abb1728f18bff7174b10d02bb15",
        ),
        (
            "block169.calldata.json",
            "block169",
            "0f5e8bd0b854357bb38764961356107b47a27415e08999767b6ca842af2742f3",
        ),
    ] {
        let f = calldata(file, name);
        assert_eq!(to_hex(&f.signal), want, "{name} file signal");
        let got = zisk_public_signal(&f.program_vk, &f.public_values, &f.rootc);
        assert_eq!(got, f.signal, "{name}");
    }
}

#[test]
fn public_signal_of_reset6_matches_the_spec() {
    let f = fixture_from_bin(
        "prover-input/txv1-dev-reset6-batch-1.plonk.bin",
        "reset6",
        "1770d4cae1135674046695ae830bb4af47c72c1694e1617823374c3c3cd943bc",
    );
    assert_eq!(
        zisk_public_signal(&f.program_vk, &f.public_values, &f.rootc),
        f.signal
    );
}

#[test]
fn public_signal_is_below_r_and_hashes_whatever_it_is_given() {
    // different lengths must not panic; the order is vk || public values || rootc
    let s = zisk_public_signal(&[1, 2, 3], &[], &[9]);
    let mut reduced = hashv(&[&[1, 2, 3], &[9]]).to_bytes();
    while reduced >= R {
        reduced = sub_be(&reduced, &R).0;
    }
    assert_eq!(s, reduced);
    assert!(s < R);
}

#[test]
fn every_real_proof_is_accepted() {
    for f in all_fixtures() {
        assert_eq!(
            verify_zisk(&f.abi()).ok(),
            Some(true),
            "verify_zisk {}",
            f.name
        );
        assert_eq!(
            verify(&f.verify_input()).ok(),
            Some(true),
            "verify {}",
            f.name
        );
        // verify_zisk equals verify on proof || zisk_public_signal
        let sig = zisk_public_signal(&f.program_vk, &f.public_values, &f.rootc);
        let mut v = f.proof.to_vec();
        v.extend_from_slice(&sig);
        assert_eq!(
            verify(&v).ok(),
            Some(true),
            "verify of derived signal {}",
            f.name
        );
    }
}
