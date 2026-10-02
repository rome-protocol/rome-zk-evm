//! Fixture loading and small big-integer helpers shared by the test files.
//! The proof file is decoded as specification section 9.2 describes.
#![allow(dead_code)]

pub mod expected;

use std::path::PathBuf;

pub type W = [u8; 32];

pub const P: W = veritas::vk::P;
pub const R: W = veritas::vk::R;

pub fn fixture_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(rel)
}

pub fn hex_to_bytes(s: &str) -> Vec<u8> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    assert!(s.len().is_multiple_of(2), "odd hex length");
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).expect("hex digit"))
        .collect()
}

pub fn hex32(s: &str) -> W {
    let v = hex_to_bytes(s);
    let mut out = [0u8; 32];
    assert!(v.len() <= 32);
    out[32 - v.len()..].copy_from_slice(&v);
    out
}

pub fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Reads `"key": "value"` out of one of the flat calldata JSON files.
pub fn json_str(doc: &str, key: &str) -> String {
    let k = format!("\"{key}\"");
    let at = doc.find(&k).unwrap_or_else(|| panic!("missing key {key}"));
    let rest = &doc[at + k.len()..];
    let open = rest.find('"').expect("opening quote");
    let rest = &rest[open + 1..];
    let close = rest.find('"').expect("closing quote");
    rest[..close].to_string()
}

// ---- 256-bit big-endian arithmetic (no overflow tricks; used only to build mutations) ----

pub fn add_be(a: &W, b: &W) -> (W, bool) {
    let mut out = [0u8; 32];
    let mut carry = 0u16;
    for i in (0..32).rev() {
        let s = a[i] as u16 + b[i] as u16 + carry;
        out[i] = s as u8;
        carry = s >> 8;
    }
    (out, carry != 0)
}

pub fn sub_be(a: &W, b: &W) -> (W, bool) {
    let mut out = [0u8; 32];
    let mut borrow = 0i16;
    for i in (0..32).rev() {
        let mut d = a[i] as i16 - b[i] as i16 - borrow;
        if d < 0 {
            d += 256;
            borrow = 1;
        } else {
            borrow = 0;
        }
        out[i] = d as u8;
    }
    (out, borrow != 0)
}

pub fn small(n: u8) -> W {
    let mut w = [0u8; 32];
    w[31] = n;
    w
}

/// (a + 1) mod m, for a < m.
pub fn inc_mod(a: &W, m: &W) -> W {
    let (s, _) = add_be(a, &small(1));
    if &s == m {
        [0u8; 32]
    } else {
        s
    }
}

/// p - y, for a non-zero y < p.
pub fn neg_y(y: &W) -> W {
    sub_be(&P, y).0
}

// ---- the real proofs ----

#[derive(Clone)]
pub struct Fixture {
    pub name: &'static str,
    pub proof: [u8; 768],
    pub program_vk: [u8; 32],
    pub rootc: [u8; 32],
    pub public_values: [u8; 512],
    /// The public signal the specification states for this fixture (section 9.1).
    pub signal: W,
}

impl Fixture {
    pub fn abi(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(1344);
        v.extend_from_slice(&self.proof);
        v.extend_from_slice(&self.program_vk);
        v.extend_from_slice(&self.rootc);
        v.extend_from_slice(&self.public_values);
        assert_eq!(v.len(), 1344);
        v
    }
    pub fn proof_vec(&self) -> Vec<u8> {
        self.proof.to_vec()
    }
    /// proof || signal, the input of `verify` (signal taken as the specification states it).
    pub fn verify_input(&self) -> Vec<u8> {
        let mut v = self.proof.to_vec();
        v.extend_from_slice(&self.signal);
        v
    }
}

fn arr<const N: usize>(v: &[u8]) -> [u8; N] {
    v.try_into()
        .unwrap_or_else(|_| panic!("expected {N} bytes, got {}", v.len()))
}

pub fn calldata(file: &'static str, name: &'static str) -> Fixture {
    let doc = std::fs::read_to_string(fixture_path(&format!("s10/{file}"))).expect("read calldata");
    Fixture {
        name,
        proof: arr(&hex_to_bytes(&json_str(&doc, "proofBytes"))),
        program_vk: arr(&hex_to_bytes(&json_str(&doc, "programVK"))),
        rootc: arr(&hex_to_bytes(&json_str(&doc, "rootCVadcopFinal"))),
        public_values: arr(&hex_to_bytes(&json_str(&doc, "publicValues"))),
        signal: arr(&hex_to_bytes(&json_str(&doc, "publicSignal"))),
    }
}

// ---- bincode 2 (standard configuration, variable-length integers), section 9.2 ----

pub struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Reader { b, at: 0 }
    }
    fn take(&mut self, n: usize) -> &'a [u8] {
        let s = &self.b[self.at..self.at + n];
        self.at += n;
        s
    }
    pub fn int(&mut self) -> u64 {
        let first = self.take(1)[0];
        match first {
            0..=250 => first as u64,
            251 => u16::from_le_bytes(arr(self.take(2))) as u64,
            252 => u32::from_le_bytes(arr(self.take(4))) as u64,
            253 => u64::from_le_bytes(arr(self.take(8))),
            _ => panic!("unsupported varint marker {first}"),
        }
    }
    pub fn bytes(&mut self) -> Vec<u8> {
        let n = self.int() as usize;
        self.take(n).to_vec()
    }
    pub fn string(&mut self) -> String {
        String::from_utf8(self.bytes()).expect("utf8")
    }
    pub fn ints(&mut self) -> Vec<u64> {
        let n = self.int() as usize;
        (0..n).map(|_| self.int()).collect()
    }
    pub fn remaining(&self) -> usize {
        self.b.len() - self.at
    }
}

/// The fields of a proof file that matter to the tests.
pub struct ProofFile {
    pub proof: [u8; 768],
    pub protocol: String,
    pub curve: String,
    pub n_public: u64,
    pub power: u64,
    pub k1: String,
    pub k2: String,
    /// Eight commitments as decimal (x, y, z) strings.
    pub commitments: Vec<[String; 3]>,
    /// [x]_2 as three decimal pairs.
    pub x2: Vec<[String; 2]>,
    pub omega: String,
    pub abi: Vec<u8>,
}

pub fn decode_proof_file(bytes: &[u8]) -> ProofFile {
    let mut r = Reader::new(bytes);
    assert_eq!(r.int(), 1, "proof kind PLONK");
    let proof: [u8; 768] = arr(&r.bytes());
    let vadcop_key = r.ints();
    assert_eq!(vadcop_key.len(), 4);
    let protocol = r.string();
    let curve = r.string();
    let n_public = r.int();
    let power = r.int();
    let k1 = r.string();
    let k2 = r.string();
    let commitments: Vec<[String; 3]> = (0..8)
        .map(|_| [r.string(), r.string(), r.string()])
        .collect();
    let x2: Vec<[String; 2]> = (0..3).map(|_| [r.string(), r.string()]).collect();
    let omega = r.string();
    let public_data = r.bytes();
    assert_eq!(public_data.len(), 256);
    let full = r.ints();
    assert_eq!(full.len(), 69);
    assert_eq!(full[0], 1, "flag word");
    let root_c = r.ints();
    assert_eq!(root_c.len(), 4);
    let program_vk = r.ints();
    assert_eq!(program_vk.len(), 4);
    assert_eq!(
        &full[1..5],
        &program_vk[..],
        "field 11 words 1..4 equal field 13"
    );
    assert_eq!(r.int(), 0, "hash mode");
    assert_eq!(r.remaining(), 0, "no bytes left over");

    let mut abi = proof.to_vec();
    for w in &program_vk {
        abi.extend_from_slice(&w.to_be_bytes());
    }
    for w in &root_c {
        abi.extend_from_slice(&w.to_be_bytes());
    }
    for w in &full[5..69] {
        abi.extend_from_slice(&w.to_le_bytes());
    }
    assert_eq!(abi.len(), 1344);
    ProofFile {
        proof,
        protocol,
        curve,
        n_public,
        power,
        k1,
        k2,
        commitments,
        x2,
        omega,
        abi,
    }
}

pub fn fixture_from_bin(rel: &str, name: &'static str, signal: &str) -> Fixture {
    let bytes = std::fs::read(fixture_path(rel)).expect("read proof file");
    let pf = decode_proof_file(&bytes);
    Fixture {
        name,
        proof: pf.proof,
        program_vk: arr(&pf.abi[768..800]),
        rootc: arr(&pf.abi[800..832]),
        public_values: arr(&pf.abi[832..1344]),
        signal: hex32(signal),
    }
}

/// Decimal string to 32 big-endian bytes.
pub fn dec_to_w(s: &str) -> W {
    let mut out = [0u8; 32];
    for ch in s.bytes() {
        assert!(ch.is_ascii_digit());
        let mut carry = (ch - b'0') as u32;
        for i in (0..32).rev() {
            let t = out[i] as u32 * 10 + carry;
            out[i] = t as u8;
            carry = t >> 8;
        }
        assert_eq!(carry, 0, "decimal value too large");
    }
    out
}

pub fn all_fixtures() -> Vec<Fixture> {
    vec![
        calldata("block14.calldata.json", "block14"),
        calldata("block166.calldata.json", "block166"),
        calldata("block169.calldata.json", "block169"),
        fixture_from_bin(
            "prover-input/txv1-dev-reset6-batch-1.plonk.bin",
            "reset6",
            "1770d4cae1135674046695ae830bb4af47c72c1694e1617823374c3c3cd943bc",
        ),
    ]
}

// ---- modular arithmetic on 256-bit words (tests only; slow and plainly correct) ----
//
// Double-and-add on four little-endian u64 limbs, with every operand below the modulus. Both moduli
// are below 2^254, so a sum of two reduced values never overflows 256 bits.

type Limbs = [u64; 4];

fn to_limbs(w: &W) -> Limbs {
    let mut l = [0u64; 4];
    for (i, limb) in l.iter_mut().enumerate() {
        let at = 24 - 8 * i;
        *limb = u64::from_be_bytes(w[at..at + 8].try_into().unwrap());
    }
    l
}

fn from_limbs(l: &Limbs) -> W {
    let mut w = [0u8; 32];
    for (i, limb) in l.iter().enumerate() {
        let at = 24 - 8 * i;
        w[at..at + 8].copy_from_slice(&limb.to_be_bytes());
    }
    w
}

fn limbs_ge(a: &Limbs, b: &Limbs) -> bool {
    for i in (0..4).rev() {
        if a[i] != b[i] {
            return a[i] > b[i];
        }
    }
    true
}

fn limbs_sub(a: &Limbs, b: &Limbs) -> Limbs {
    let mut out = [0u64; 4];
    let mut borrow = false;
    for i in 0..4 {
        let (d1, b1) = a[i].overflowing_sub(b[i]);
        let (d2, b2) = d1.overflowing_sub(borrow as u64);
        out[i] = d2;
        borrow = b1 || b2;
    }
    out
}

fn limbs_add_mod(a: &Limbs, b: &Limbs, m: &Limbs) -> Limbs {
    let mut out = [0u64; 4];
    let mut carry = false;
    for i in 0..4 {
        let (s1, c1) = a[i].overflowing_add(b[i]);
        let (s2, c2) = s1.overflowing_add(carry as u64);
        out[i] = s2;
        carry = c1 || c2;
    }
    assert!(!carry, "modulus must be below 2^255");
    if limbs_ge(&out, m) {
        out = limbs_sub(&out, m);
    }
    out
}

/// `a` reduced mod `m` by repeated subtraction (at most a handful of rounds for our moduli).
pub fn reduce_mod(a: &W, m: &W) -> W {
    let (mut l, ml) = (to_limbs(a), to_limbs(m));
    while limbs_ge(&l, &ml) {
        l = limbs_sub(&l, &ml);
    }
    from_limbs(&l)
}

pub fn add_mod(a: &W, b: &W, m: &W) -> W {
    from_limbs(&limbs_add_mod(&to_limbs(a), &to_limbs(b), &to_limbs(m)))
}

pub fn sub_mod(a: &W, b: &W, m: &W) -> W {
    add_mod(a, &neg_mod(b, m), m)
}

pub fn neg_mod(a: &W, m: &W) -> W {
    if *a == [0u8; 32] {
        *a
    } else {
        sub_be(m, a).0
    }
}

pub fn mul_mod(a: &W, b: &W, m: &W) -> W {
    let (a, b, m) = (to_limbs(a), to_limbs(b), to_limbs(m));
    let mut acc = [0u64; 4];
    for bit in (0..256).rev() {
        acc = limbs_add_mod(&acc, &acc, &m);
        if (b[bit / 64] >> (bit % 64)) & 1 == 1 {
            acc = limbs_add_mod(&acc, &a, &m);
        }
    }
    from_limbs(&acc)
}

/// `base^exp mod m`, `exp` a 256-bit big-endian integer.
pub fn pow_mod(base: &W, exp: &W, m: &W) -> W {
    let mut acc = reduce_mod(&small(1), m);
    for bit in (0..256).rev() {
        acc = mul_mod(&acc, &acc, m);
        if (exp[31 - bit / 8] >> (bit % 8)) & 1 == 1 {
            acc = mul_mod(&acc, base, m);
        }
    }
    acc
}

/// The inverse of a non-zero `a` modulo the prime `m` (Fermat).
pub fn inv_mod(a: &W, m: &W) -> W {
    assert_ne!(*a, [0u8; 32], "zero has no inverse");
    pow_mod(a, &sub_be(m, &small(2)).0, m)
}

/// Spec section 2, evaluated directly: is `(x, y)` on y^2 = x^3 + 3 over F_p, or the identity (64 zero bytes)?
/// Both words must already be below p (rule 2); the caller checks the range first.
pub fn on_curve_or_identity(point: &[u8]) -> bool {
    assert_eq!(point.len(), 64);
    if point.iter().all(|&b| b == 0) {
        return true;
    }
    let x: W = point[..32].try_into().unwrap();
    let y: W = point[32..].try_into().unwrap();
    let x3 = mul_mod(&mul_mod(&x, &x, &P), &x, &P);
    let rhs = add_mod(&x3, &small(3), &P);
    mul_mod(&y, &y, &P) == rhs
}

// ---- the transcript and the z-coefficient of step 9, computed in the test ----

/// The challenges of spec section 6 for a proof and a public signal.
pub struct Chal {
    pub beta: W,
    pub gamma: W,
    pub alpha: W,
    pub zeta: W,
    pub u: W,
}

fn keccak_mod_r(parts: &[&[u8]]) -> W {
    reduce_mod(&solana_program::keccak::hashv(parts).to_bytes(), &R)
}

pub fn challenges(proof: &[u8; 768], signal: &W) -> Chal {
    let beta = keccak_mod_r(&[&veritas::vk::COMMITMENTS, signal, &proof[0..192]]);
    let gamma = keccak_mod_r(&[&beta]);
    let alpha = keccak_mod_r(&[&beta, &gamma, &proof[192..256]]);
    let zeta = keccak_mod_r(&[&alpha, &proof[256..448]]);
    let u = keccak_mod_r(&[&proof[448..576]]);
    Chal {
        beta,
        gamma,
        alpha,
        zeta,
        u,
    }
}

/// zeta^n with n = 2^24: 24 squarings.
pub fn zeta_n(zeta: &W) -> W {
    let mut zn = *zeta;
    for _ in 0..veritas::vk::POWER {
        zn = mul_mod(&zn, &zn, &R);
    }
    zn
}

/// L_1(zeta) = (zeta^n - 1) / (n (zeta - 1)), n = 2^24.
pub fn lagrange_1(zeta: &W) -> W {
    let zn = zeta_n(zeta);
    let zh = sub_mod(&zn, &small(1), &R);
    let mut n = [0u8; 32];
    n[28..].copy_from_slice(&(1u32 << veritas::vk::POWER).to_be_bytes());
    let denom = mul_mod(&n, &sub_mod(zeta, &small(1), &R), &R);
    mul_mod(&zh, &inv_mod(&denom, &R), &R)
}

/// The scalar of [z]_1 in step 9 of spec section 7, for the evaluations in `proof` (words 18 to 23).
pub fn z_coefficient(proof: &[u8; 768], signal: &W) -> W {
    let c = challenges(proof, signal);
    let (a, b, cc) = (word_of(proof, 18), word_of(proof, 19), word_of(proof, 20));
    let bz = mul_mod(&c.beta, &c.zeta, &R);
    let k = |k: u8| mul_mod(&bz, &small(k), &R);
    let f = |e: &W, kz: &W| add_mod(&add_mod(e, kz, &R), &c.gamma, &R);
    let prod = mul_mod(
        &mul_mod(&f(&a, &bz), &f(&b, &k(veritas::vk::K1 as u8)), &R),
        &f(&cc, &k(veritas::vk::K2 as u8)),
        &R,
    );
    let alpha2 = mul_mod(&c.alpha, &c.alpha, &R);
    let l1_alpha2 = mul_mod(&lagrange_1(&c.zeta), &alpha2, &R);
    add_mod(
        &add_mod(&mul_mod(&prod, &c.alpha, &R), &l1_alpha2, &R),
        &c.u,
        &R,
    )
}

fn word_of(buf: &[u8], i: usize) -> W {
    buf[32 * i..32 * i + 32].try_into().unwrap()
}

/// Sets proof word 18 (a-bar) so that the scalar of [z]_1 in step 9 is exactly zero, and returns it.
/// The challenges do not depend on a-bar (spec section 6), so the equation is linear in it:
/// (a + beta zeta + gamma) F2 F3 alpha = -(L_1 alpha^2 + u).
pub fn make_z_coefficient_zero(proof: &mut [u8; 768], signal: &W) {
    let c = challenges(proof, signal);
    let (b, cc) = (word_of(proof, 19), word_of(proof, 20));
    let bz = mul_mod(&c.beta, &c.zeta, &R);
    let f = |e: &W, mult: u8| {
        let kz = mul_mod(&bz, &small(mult), &R);
        add_mod(&add_mod(e, &kz, &R), &c.gamma, &R)
    };
    let f2f3 = mul_mod(
        &f(&b, veritas::vk::K1 as u8),
        &f(&cc, veritas::vk::K2 as u8),
        &R,
    );
    let denom = mul_mod(&f2f3, &c.alpha, &R);
    assert_ne!(denom, [0u8; 32], "no solution: alpha F2 F3 is zero");
    let alpha2 = mul_mod(&c.alpha, &c.alpha, &R);
    let rhs = neg_mod(
        &add_mod(&mul_mod(&lagrange_1(&c.zeta), &alpha2, &R), &c.u, &R),
        &R,
    );
    let f1 = mul_mod(&rhs, &inv_mod(&denom, &R), &R);
    let a = sub_mod(&sub_mod(&f1, &bz, &R), &c.gamma, &R);
    proof[18 * 32..19 * 32].copy_from_slice(&a);
    assert_eq!(z_coefficient(proof, signal), [0u8; 32]);
}

/// [D]_1 of step 9, computed in the test with the solana-bn254 group operations, for a proof whose nine
/// points are on the curve. It pins the test's scalars (including the z-coefficient) to the library's.
pub fn d_by_hand(proof: &[u8; 768], signal: &W) -> [u8; 64] {
    use solana_bn254::prelude::{alt_bn128_g1_addition_be, alt_bn128_g1_multiplication_be};
    let c = challenges(proof, signal);
    let ev = |i: usize| word_of(proof, i);
    let (a, b, cc, s1, s2, zw) = (ev(18), ev(19), ev(20), ev(21), ev(22), ev(23));
    let t1 = add_mod(&add_mod(&a, &mul_mod(&c.beta, &s1, &R), &R), &c.gamma, &R);
    let t2 = add_mod(&add_mod(&b, &mul_mod(&c.beta, &s2, &R), &R), &c.gamma, &R);
    let perm_tail = mul_mod(&mul_mod(&t1, &t2, &R), &zw, &R);
    let coef_s3 = neg_mod(
        &mul_mod(&mul_mod(&perm_tail, &c.alpha, &R), &c.beta, &R),
        &R,
    );
    let zn = zeta_n(&c.zeta);
    let zh = sub_mod(&zn, &small(1), &R);
    let zh_zn = mul_mod(&zh, &zn, &R);
    let zh_z2n = mul_mod(&zh_zn, &zn, &R);
    let terms: [(&[u8], W); 9] = [
        (&veritas::vk::Q_M, mul_mod(&a, &b, &R)),
        (&veritas::vk::Q_L, a),
        (&veritas::vk::Q_R, b),
        (&veritas::vk::Q_O, cc),
        (&proof[192..256], z_coefficient(proof, signal)),
        (&veritas::vk::S_SIGMA_3, coef_s3),
        (&proof[256..320], neg_mod(&zh, &R)),
        (&proof[320..384], neg_mod(&zh_zn, &R)),
        (&proof[384..448], neg_mod(&zh_z2n, &R)),
    ];
    let mut d = veritas::vk::Q_C;
    for (point, scalar) in terms {
        let mut mi = [0u8; 96];
        mi[..64].copy_from_slice(point);
        mi[64..].copy_from_slice(&scalar);
        let m = alt_bn128_g1_multiplication_be(&mi).expect("multiplication");
        let mut ai = [0u8; 128];
        ai[..64].copy_from_slice(&d);
        ai[64..].copy_from_slice(&m);
        d = alt_bn128_g1_addition_be(&ai)
            .expect("addition")
            .try_into()
            .unwrap();
    }
    d
}
