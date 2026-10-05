//! Spec 9.4: inputs that must be refused, and what each must give.
mod common;
use common::*;
use solana_program::program_error::ProgramError;
use veritas::vk::ZISK_1_2_0;
use veritas::{zisk_public_signal, zisk_version};

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Out {
    Accept,
    Reject,
    Malformed,
}

fn classify(r: Result<bool, ProgramError>) -> Out {
    match r {
        Ok(true) => Out::Accept,
        Ok(false) => Out::Reject,
        Err(ProgramError::InvalidInstructionData) => Out::Malformed,
        Err(e) => panic!("unexpected error {e:?}"),
    }
}

/// The fixtures here are ZisK 1.2.0 proofs, checked under the 1.2.0 key the test feature provides.
fn zisk(abi: &[u8]) -> Out {
    classify(veritas::verify_zisk(zisk_version(1).unwrap(), abi))
}
fn ver(input: &[u8]) -> Out {
    classify(veritas::verify(&ZISK_1_2_0, input))
}

fn block14() -> Fixture {
    all_fixtures().remove(0)
}

/// Replaces the 32-byte word `i` of a buffer.
fn set_word(buf: &mut [u8], i: usize, w: &W) {
    buf[32 * i..32 * i + 32].copy_from_slice(w);
}
fn word(buf: &[u8], i: usize) -> W {
    buf[32 * i..32 * i + 32].try_into().unwrap()
}

/// Section 5, applied independently of the library: range, then curve, then evaluation range.
/// The curve check is the equation of spec section 2 evaluated here (`on_curve_or_identity`), not any
/// library's point decoder.
fn expected_class(proof: &[u8]) -> Out {
    for i in 0..18 {
        if word(proof, i) >= P {
            return Out::Malformed;
        }
    }
    for k in 0..9 {
        if !on_curve_or_identity(&proof[64 * k..64 * k + 64]) {
            return Out::Malformed;
        }
    }
    for i in 18..24 {
        if word(proof, i) >= R {
            return Out::Malformed;
        }
    }
    Out::Reject
}

#[test]
fn m1_wrong_lengths() {
    let f = block14();
    let abi = f.abi();
    for n in [0usize, 1, 799, 800, 1343, 1345, 2688] {
        let mut v = abi.clone();
        v.resize(n, 0);
        assert_eq!(zisk(&v), Out::Malformed, "verify_zisk len {n}");
    }
    let input = f.verify_input();
    for n in [0usize, 768, 799, 801, 1344] {
        let mut v = input.clone();
        v.resize(n, 0);
        assert_eq!(ver(&v), Out::Malformed, "verify len {n}");
    }
}

#[test]
fn m2_coordinate_not_below_p() {
    let f = block14();
    let p1 = P;
    let (p_plus_1, _) = add_be(&P, &small(1));
    for i in 0..18 {
        for w in [p1, p_plus_1, [0xff; 32]] {
            let mut proof = f.proof;
            set_word(&mut proof, i, &w);
            let mut abi = f.abi();
            abi[..768].copy_from_slice(&proof);
            assert_eq!(zisk(&abi), Out::Malformed, "word {i} = {}", to_hex(&w));
            let mut input = proof.to_vec();
            input.extend_from_slice(&f.signal);
            assert_eq!(ver(&input), Out::Malformed, "verify word {i}");
        }
    }
}

#[test]
fn m3_flag_bits_in_y() {
    let f = block14();
    for k in 0..9 {
        let y = 2 * k + 1;
        for flag in [0x80u8, 0x40u8] {
            let mut proof = f.proof;
            proof[32 * y] |= flag;
            let mut abi = f.abi();
            abi[..768].copy_from_slice(&proof);
            assert_eq!(zisk(&abi), Out::Malformed, "commitment {k} flag {flag:#x}");
        }
        // also the y word plus 2^255 as a number, for the x word's neighbour case
        let mut proof = f.proof;
        let (s, _) = add_be(&word(&proof, y), &{
            let mut t = [0u8; 32];
            t[0] = 0x80;
            t
        });
        set_word(&mut proof, y, &s);
        let mut input = proof.to_vec();
        input.extend_from_slice(&f.signal);
        assert_eq!(ver(&input), Out::Malformed, "commitment {k} + 2^255");
    }
}

#[test]
fn m4_off_curve_by_one_bit() {
    let f = block14();
    for i in 0..18 {
        let mut proof = f.proof;
        proof[32 * i + 31] ^= 1;
        let mut abi = f.abi();
        abi[..768].copy_from_slice(&proof);
        assert_eq!(zisk(&abi), expected_class(&proof), "word {i}");
        assert_eq!(zisk(&abi), Out::Malformed, "word {i}");
    }
}

/// A prover can make the scalar of [z]_1 in step 9 exactly zero by choosing a-bar
/// (the challenges do not depend on it). If the point were only decoded when its scalar is non-zero, an
/// off-curve [z]_1 would slip through. It must still be MALFORMED.
#[test]
fn m4_off_curve_z_commitment_with_zero_scalar() {
    let f = block14();
    let signal = zisk_public_signal(&f.program_vk, &f.public_values, &f.rootc);
    // The test's own arithmetic agrees with the library on the real proof before it is trusted.
    let t = veritas::trace(&ZISK_1_2_0, &f.proof, &signal).expect("well-formed");
    assert_eq!(challenges(&ZISK_1_2_0, &f.proof, &signal).alpha, t.alpha);
    assert_eq!(
        d_by_hand(&ZISK_1_2_0, &f.proof, &signal),
        t.d,
        "step 9 by hand, real proof"
    );
    // [z]_1 is proof words 6 (x) and 7 (y). Flip the lowest bit of y: off the curve.
    let mut proof = f.proof;
    proof[32 * 7 + 31] ^= 1;
    assert!(!on_curve_or_identity(&proof[192..256]));
    make_z_coefficient_zero(&ZISK_1_2_0, &mut proof, &signal);
    assert_eq!(z_coefficient(&ZISK_1_2_0, &proof, &signal), [0u8; 32]);
    assert_eq!(expected_class(&proof), Out::Malformed);
    let mut abi = f.abi();
    abi[..768].copy_from_slice(&proof);
    assert_eq!(zisk(&abi), Out::Malformed);
    let mut input = proof.to_vec();
    input.extend_from_slice(&signal);
    assert_eq!(ver(&input), Out::Malformed);
    // Control: the same a-bar with [z]_1 left on the curve is well-formed, its scalar is zero in the
    // library too (step 9 recomputed by hand matches), and the proof is simply rejected.
    let mut control = f.proof;
    make_z_coefficient_zero(&ZISK_1_2_0, &mut control, &signal);
    let tc = veritas::trace(&ZISK_1_2_0, &control, &signal).expect("well-formed");
    assert_eq!(
        d_by_hand(&ZISK_1_2_0, &control, &signal),
        tc.d,
        "step 9 by hand, zero scalar"
    );
    assert!(!tc.accepted);
}

#[test]
fn m5_evaluation_not_below_r() {
    let f = block14();
    for i in 18..24 {
        let own = word(&f.proof, i);
        let own_plus_r = add_be(&own, &R).0;
        for w in [R, own_plus_r, [0xff; 32]] {
            let mut proof = f.proof;
            set_word(&mut proof, i, &w);
            let mut abi = f.abi();
            abi[..768].copy_from_slice(&proof);
            assert_eq!(zisk(&abi), Out::Malformed, "eval {i} = {}", to_hex(&w));
        }
    }
}

#[test]
fn m6_signal_not_below_r() {
    let f = block14();
    for s in [R, [0xff; 32]] {
        let mut input = f.proof.to_vec();
        input.extend_from_slice(&s);
        assert_eq!(ver(&input), Out::Malformed);
    }
}

#[test]
fn r1_negated_commitment() {
    let f = block14();
    for k in 0..9 {
        let mut proof = f.proof;
        let y = word(&proof, 2 * k + 1);
        set_word(&mut proof, 2 * k + 1, &neg_y(&y));
        let mut abi = f.abi();
        abi[..768].copy_from_slice(&proof);
        assert_eq!(zisk(&abi), Out::Reject, "negated commitment {k}");
    }
}

#[test]
fn r2_identity_commitment() {
    let f = block14();
    for k in 0..9 {
        let mut proof = f.proof;
        proof[64 * k..64 * k + 64].fill(0);
        let mut abi = f.abi();
        abi[..768].copy_from_slice(&proof);
        assert_eq!(zisk(&abi), Out::Reject, "identity commitment {k}");
    }
}

#[test]
fn r3_evaluation_plus_one() {
    let f = block14();
    for i in 18..24 {
        let mut proof = f.proof;
        let w = inc_mod(&word(&proof, i), &R);
        set_word(&mut proof, i, &w);
        let mut abi = f.abi();
        abi[..768].copy_from_slice(&proof);
        assert_eq!(zisk(&abi), Out::Reject, "evaluation {i}");
    }
}

#[test]
fn r4_swaps() {
    let f = block14();
    let mut proof = f.proof;
    let (a, b) = (proof[448..512].to_vec(), proof[512..576].to_vec());
    proof[448..512].copy_from_slice(&b);
    proof[512..576].copy_from_slice(&a);
    let mut abi = f.abi();
    abi[..768].copy_from_slice(&proof);
    assert_eq!(zisk(&abi), Out::Reject, "openings swapped");
    for (i, j) in [(18usize, 19usize), (20, 21), (22, 23), (18, 23)] {
        let mut proof = f.proof;
        let (wi, wj) = (word(&proof, i), word(&proof, j));
        set_word(&mut proof, i, &wj);
        set_word(&mut proof, j, &wi);
        let mut abi = f.abi();
        abi[..768].copy_from_slice(&proof);
        assert_eq!(zisk(&abi), Out::Reject, "evaluations {i} and {j} swapped");
    }
}

#[test]
fn r5_wrong_signal() {
    let f = block14();
    let mut plus1 = f.proof.to_vec();
    plus1.extend_from_slice(&inc_mod(&f.signal, &R));
    assert_eq!(ver(&plus1), Out::Reject);
    let mut flipped = f.proof.to_vec();
    let mut s = f.signal;
    s[31] ^= 1;
    flipped.extend_from_slice(&s);
    assert_eq!(ver(&flipped), Out::Reject);
}

#[test]
fn r6_bit_flips_in_the_bound_fields() {
    let f = block14();
    let abi = f.abi();
    // every bit of programVK and rootC, and a spread of bits in publicValues
    let mut positions: Vec<usize> = (768 * 8..832 * 8).collect();
    positions.extend((832 * 8..1344 * 8).step_by(61));
    for bit in positions {
        let mut v = abi.clone();
        v[bit / 8] ^= 1 << (bit % 8);
        assert_eq!(zisk(&v), Out::Reject, "bit {bit}");
    }
}

#[test]
fn r7_mixed_fixtures() {
    let fs = all_fixtures();
    for (i, a) in fs.iter().enumerate() {
        for (j, b) in fs.iter().enumerate() {
            if i == j {
                continue;
            }
            let mut abi = a.proof.to_vec();
            abi.extend_from_slice(&b.program_vk);
            abi.extend_from_slice(&b.rootc);
            abi.extend_from_slice(&b.public_values);
            assert_eq!(
                zisk(&abi),
                Out::Reject,
                "proof {} with data of {}",
                a.name,
                b.name
            );
        }
    }
}

#[test]
fn r8_zero_proof() {
    let f = block14();
    let mut input = vec![0u8; 768];
    input.extend_from_slice(&f.signal);
    assert_eq!(ver(&input), Out::Reject);
    let mut abi = f.abi();
    abi[..768].fill(0);
    assert_eq!(zisk(&abi), Out::Reject);
}

/// X1: every single-bit flip of block 14's 768 proof bytes, classified by the rules.
#[test]
fn x1_every_single_bit_flip_of_the_block14_proof() {
    let f = block14();
    let signal = zisk_public_signal(&f.program_vk, &f.public_values, &f.rootc);
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(16);
    let bits: Vec<usize> = (0..768 * 8).collect();
    let chunk = bits.len().div_ceil(threads);
    let counts = std::thread::scope(|s| {
        let handles: Vec<_> = bits
            .chunks(chunk)
            .map(|c| {
                let f = &f;
                s.spawn(move || {
                    let (mut m, mut r) = (0usize, 0usize);
                    for &bit in c {
                        let mut proof = f.proof;
                        proof[bit / 8] ^= 1 << (bit % 8);
                        let want = expected_class(&proof);
                        let mut input = proof.to_vec();
                        input.extend_from_slice(&signal);
                        let got = ver(&input);
                        assert_eq!(got, want, "bit {bit}");
                        if got == Out::Malformed {
                            m += 1
                        } else {
                            r += 1
                        }
                    }
                    (m, r)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
    });
    assert_eq!(counts.0 + counts.1, 6144);
    println!("X1: {} malformed, {} rejected", counts.0, counts.1);
}
