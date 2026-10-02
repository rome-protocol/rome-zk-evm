//! The verification computation: specification sections 5 (validation), 6 (transcript), 7 (steps 5-12).

use crate::field::Fr;
use crate::g1::{self, G1};
use crate::{vk, Trace};
use solana_program::keccak;
use solana_program::program_error::ProgramError;

pub const PROOF_LEN: usize = 768;

const fn fr(b: &[u8; 32]) -> Fr {
    match Fr::from_be_bytes(b) {
        Some(x) => x,
        None => panic!("constant not below r"),
    }
}

/// n = 2^24.
const N: Fr = Fr::from_u64(1 << vk::POWER);
const OMEGA: Fr = fr(&vk::OMEGA);
const K1: Fr = Fr::from_u64(vk::K1);
const K2: Fr = Fr::from_u64(vk::K2);

fn malformed() -> ProgramError {
    ProgramError::InvalidInstructionData
}

/// Keccak-256 of the parts, read as a big-endian integer and reduced mod r.
fn challenge(parts: &[&[u8]]) -> Fr {
    Fr::reduce_be_bytes(&keccak::hashv(parts).to_bytes())
}

fn point(proof: &[u8], i: usize) -> Result<&G1, ProgramError> {
    proof
        .get(64 * i..64 * i + 64)
        .and_then(|s| <&G1>::try_from(s).ok())
        .ok_or_else(malformed)
}

fn eval(proof: &[u8], i: usize) -> Result<Fr, ProgramError> {
    let s = proof
        .get(32 * i..32 * i + 32)
        .and_then(|s| <&[u8; 32]>::try_from(s).ok());
    s.and_then(Fr::from_be_bytes).ok_or_else(malformed)
}

/// L_1(z) = (z^n - 1) / (n (z - 1)), or `None` when z = 1 (rule 6: no inverse exists).
pub fn lagrange_1(z: &Fr, z_h: &Fr) -> Option<Fr> {
    let denom = N.mul(&z.sub(&Fr::ONE));
    if denom.is_zero() {
        return None;
    }
    Some(z_h.mul(&denom.inverse()))
}

/// acc + scalar * p
fn add_mul(acc: &G1, p: &G1, scalar: &Fr) -> Result<G1, ProgramError> {
    g1::add(acc, &g1::mul(p, &scalar.to_be_bytes())?)
}

/// Runs the whole verification and keeps every intermediate value. `proof` is 768 bytes and
/// `signal` 32. MALFORMED (section 5) is `Err(InvalidInstructionData)`.
pub fn run(proof: &[u8], signal: &[u8]) -> Result<Trace, ProgramError> {
    if proof.len() != PROOF_LEN || signal.len() != 32 {
        return Err(malformed());
    }
    let signal = <&[u8; 32]>::try_from(signal).map_err(|_| malformed())?;

    // Section 5: coordinate range, evaluation range, signal range. The curve check (rule 3) is done by
    // the alt_bn128 operations below, which take every one of the nine points as an input.
    for i in 0..9 {
        if !g1::coordinates_in_range(point(proof, i)?) {
            return Err(malformed());
        }
    }
    let a_bar = eval(proof, 18)?;
    let b_bar = eval(proof, 19)?;
    let c_bar = eval(proof, 20)?;
    let s1_bar = eval(proof, 21)?;
    let s2_bar = eval(proof, 22)?;
    let zw_bar = eval(proof, 23)?;
    let w1 = Fr::from_be_bytes(signal).ok_or_else(malformed)?;

    let (pa, pb, pc, pz) = (
        point(proof, 0)?,
        point(proof, 1)?,
        point(proof, 2)?,
        point(proof, 3)?,
    );
    let (t_lo, t_mid, t_hi) = (point(proof, 4)?, point(proof, 5)?, point(proof, 6)?);
    let (w_z, w_zw) = (point(proof, 7)?, point(proof, 8)?);

    // Section 6: the transcript.
    let beta = challenge(&[&vk::COMMITMENTS, signal, &proof[0..192]]);
    let beta_b = beta.to_be_bytes();
    let gamma = challenge(&[&beta_b]);
    let gamma_b = gamma.to_be_bytes();
    let alpha = challenge(&[&beta_b, &gamma_b, &proof[192..256]]);
    let alpha_b = alpha.to_be_bytes();
    let z = challenge(&[&alpha_b, &proof[256..448]]);
    let z_b = z.to_be_bytes();
    let v = challenge(&[&z_b, &proof[576..768]]);
    let u = challenge(&[&proof[448..576]]);

    // Section 7, steps 5 and 6 (rule 6 on the way).
    let z_n = z.pow2k(vk::POWER);
    let z_h = z_n.sub(&Fr::ONE);
    let l1 = lagrange_1(&z, &z_h).ok_or_else(malformed)?;
    // Step 7.
    let pi = w1.mul(&l1).neg();
    // Step 8.
    let alpha2 = alpha.square();
    let g = &gamma;
    let t1 = a_bar.add(&beta.mul(&s1_bar)).add(g);
    let t2 = b_bar.add(&beta.mul(&s2_bar)).add(g);
    let t3 = c_bar.add(g);
    let perm_tail = t1.mul(&t2).mul(&zw_bar);
    let r0 = pi
        .sub(&l1.mul(&alpha2))
        .sub(&alpha.mul(&perm_tail).mul(&t3));

    // Step 9.
    let bz = beta.mul(&z);
    let f1 = a_bar.add(&bz).add(g);
    let f2 = b_bar.add(&bz.mul(&K1)).add(g);
    let f3 = c_bar.add(&bz.mul(&K2)).add(g);
    let coef_z = f1
        .mul(&f2)
        .mul(&f3)
        .mul(&alpha)
        .add(&l1.mul(&alpha2))
        .add(&u);
    let coef_s3 = perm_tail.mul(&alpha).mul(&beta).neg();
    let zh_zn = z_h.mul(&z_n);
    let zh_z2n = zh_zn.mul(&z_n);
    let mut d = vk::Q_C;
    d = add_mul(&d, &vk::Q_M, &a_bar.mul(&b_bar))?;
    d = add_mul(&d, &vk::Q_L, &a_bar)?;
    d = add_mul(&d, &vk::Q_R, &b_bar)?;
    d = add_mul(&d, &vk::Q_O, &c_bar)?;
    d = add_mul(&d, pz, &coef_z)?;
    d = add_mul(&d, &vk::S_SIGMA_3, &coef_s3)?;
    d = add_mul(&d, t_lo, &z_h.neg())?;
    d = add_mul(&d, t_mid, &zh_zn.neg())?;
    d = add_mul(&d, t_hi, &zh_z2n.neg())?;

    // Step 10.
    let v2 = v.square();
    let v3 = v2.mul(&v);
    let v4 = v2.square();
    let v5 = v4.mul(&v);
    let mut f = d;
    f = add_mul(&f, pa, &v)?;
    f = add_mul(&f, pb, &v2)?;
    f = add_mul(&f, pc, &v3)?;
    f = add_mul(&f, &vk::S_SIGMA_1, &v4)?;
    f = add_mul(&f, &vk::S_SIGMA_2, &v5)?;

    // Step 11.
    let e_scalar = r0
        .neg()
        .add(&v.mul(&a_bar))
        .add(&v2.mul(&b_bar))
        .add(&v3.mul(&c_bar))
        .add(&v4.mul(&s1_bar))
        .add(&v5.mul(&s2_bar))
        .add(&u.mul(&zw_bar));
    let e = g1::mul(&vk::G1_GENERATOR, &e_scalar.to_be_bytes())?;

    // Step 12.
    let p_l = add_mul(w_z, w_zw, &u)?;
    let mut p_r = g1::mul(w_z, &z_b)?;
    p_r = add_mul(&p_r, w_zw, &u.mul(&z).mul(&OMEGA))?;
    p_r = g1::add(&p_r, &f)?;
    p_r = g1::add(&p_r, &g1::neg(&e)?)?;

    // e(P_L, [x]_2) = e(P_R, [1]_2)  <=>  e(-P_L, [x]_2) * e(P_R, [1]_2) = 1
    let mut pairs = [0u8; 384];
    pairs[..64].copy_from_slice(&g1::neg(&p_l)?);
    pairs[64..192].copy_from_slice(&vk::X_G2);
    pairs[192..256].copy_from_slice(&p_r);
    pairs[256..].copy_from_slice(&vk::ONE_G2);
    let accepted = g1::pairing_is_one(&pairs)?;

    Ok(Trace {
        beta: beta_b,
        gamma: gamma_b,
        alpha: alpha_b,
        z: z_b,
        v: v.to_be_bytes(),
        u: u.to_be_bytes(),
        z_n: z_n.to_be_bytes(),
        z_h: z_h.to_be_bytes(),
        l1: l1.to_be_bytes(),
        pi: pi.to_be_bytes(),
        r0: r0.to_be_bytes(),
        e_scalar: e_scalar.to_be_bytes(),
        d,
        f,
        e,
        p_l,
        p_r,
        accepted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn z_equal_to_one_has_no_lagrange_value() {
        let z_h = Fr::ONE.pow2k(vk::POWER).sub(&Fr::ONE);
        assert!(lagrange_1(&Fr::ONE, &z_h).is_none());
        let two = Fr::from_u64(2);
        assert!(lagrange_1(&two, &two.pow2k(vk::POWER).sub(&Fr::ONE)).is_some());
    }
}
