//! Spec 9.3: the intermediate values of the real proofs, step by step.
mod common;
use common::expected::*;
use common::*;
use veritas::trace;

fn pt(p: &(&str, &str)) -> [u8; 64] {
    let mut out = [0u8; 64];
    out[..32].copy_from_slice(&hex32(p.0));
    out[32..].copy_from_slice(&hex32(p.1));
    out
}

#[test]
fn challenges_and_pairing_points_match_for_all_four_proofs() {
    for (i, f) in all_fixtures().iter().enumerate() {
        let t = trace(&f.proof, &f.signal).expect("well-formed");
        let c = &CHALLENGES[i];
        assert_eq!(t.beta, hex32(c.beta), "{} beta", f.name);
        assert_eq!(t.gamma, hex32(c.gamma), "{} gamma", f.name);
        assert_eq!(t.alpha, hex32(c.alpha), "{} alpha", f.name);
        assert_eq!(t.z, hex32(c.z), "{} z", f.name);
        assert_eq!(t.v, hex32(c.v), "{} v", f.name);
        assert_eq!(t.u, hex32(c.u), "{} u", f.name);
        assert_eq!(t.p_l, pt(&PAIRS[i].0), "{} P_L", f.name);
        assert_eq!(t.p_r, pt(&PAIRS[i].1), "{} P_R", f.name);
        assert!(t.accepted, "{}", f.name);
    }
}

#[test]
fn block14_steps_5_to_12() {
    let f = &all_fixtures()[0];
    let t = trace(&f.proof, &f.signal).unwrap();
    assert_eq!(t.z_n, hex32(B14_ZN), "z^n");
    assert_eq!(t.z_h, hex32(B14_ZH), "Z_H");
    assert_eq!(t.l1, hex32(B14_L1), "L_1");
    assert_eq!(t.pi, hex32(B14_PI), "PI");
    assert_eq!(t.r0, hex32(B14_R0), "r0");
    assert_eq!(t.e_scalar, hex32(B14_EE), "e_E");
    assert_eq!(t.d, pt(&B14_D), "[D]");
    assert_eq!(t.f, pt(&B14_F), "[F]");
    assert_eq!(t.e, pt(&B14_E), "[E]");
    assert_eq!(t.p_l, pt(&B14_PL), "P_L");
    assert_eq!(t.p_r, pt(&B14_PR), "P_R");
}
