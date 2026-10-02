//! Prime-field arithmetic for the two BN254 fields (specification section 2), in Montgomery form on
//! four 64-bit limbs. Both moduli are below 2^254, which lets the multiplication skip the extra
//! carry word. Every operation keeps its result fully reduced, so equality is limb equality.

pub type Limbs = [u64; 4];

const fn mac(a: u64, b: u64, c: u64, carry: u64) -> (u64, u64) {
    let t = a as u128 + (b as u128) * (c as u128) + carry as u128;
    (t as u64, (t >> 64) as u64)
}

const fn adc(a: u64, b: u64, carry: u64) -> (u64, u64) {
    let t = a as u128 + b as u128 + carry as u128;
    (t as u64, (t >> 64) as u64)
}

const fn sbb(a: u64, b: u64, borrow: u64) -> (u64, u64) {
    let t = (a as u128)
        .wrapping_sub(b as u128)
        .wrapping_sub(borrow as u128);
    (t as u64, ((t >> 64) as u64) & 1)
}

/// a >= b as 256-bit integers.
pub const fn geq(a: &Limbs, b: &Limbs) -> bool {
    let mut i = 4;
    while i > 0 {
        i -= 1;
        if a[i] != b[i] {
            return a[i] > b[i];
        }
    }
    true
}

/// a - b, ignoring the final borrow.
pub const fn sub_raw(a: &Limbs, b: &Limbs) -> Limbs {
    let mut out = [0u64; 4];
    let mut borrow = 0;
    let mut i = 0;
    while i < 4 {
        let (d, br) = sbb(a[i], b[i], borrow);
        out[i] = d;
        borrow = br;
        i += 1;
    }
    out
}

const fn add_raw(a: &Limbs, b: &Limbs) -> Limbs {
    let mut out = [0u64; 4];
    let mut carry = 0;
    let mut i = 0;
    while i < 4 {
        let (s, c) = adc(a[i], b[i], carry);
        out[i] = s;
        carry = c;
        i += 1;
    }
    out
}

/// -m^-1 mod 2^64, by Newton's iteration.
pub const fn neg_inv(m0: u64) -> u64 {
    let mut inv: u64 = 1;
    let mut i = 0;
    while i < 7 {
        inv = inv.wrapping_mul(2u64.wrapping_sub(m0.wrapping_mul(inv)));
        i += 1;
    }
    inv.wrapping_neg()
}

/// 2^k mod m, by doubling 1 k times. Needs m < 2^255.
pub const fn pow2_mod(m: &Limbs, k: u32) -> Limbs {
    let mut x: Limbs = [1, 0, 0, 0];
    let mut i = 0;
    while i < k {
        x = add_raw(&x, &x);
        if geq(&x, m) {
            x = sub_raw(&x, m);
        }
        i += 1;
    }
    x
}

pub const fn mont_mul(a: &Limbs, b: &Limbs, m: &Limbs, inv: u64) -> Limbs {
    // schoolbook product into eight limbs
    let mut t = [0u64; 8];
    let mut i = 0;
    while i < 4 {
        let mut carry = 0;
        let mut j = 0;
        while j < 4 {
            let (lo, hi) = mac(t[i + j], a[i], b[j], carry);
            t[i + j] = lo;
            carry = hi;
            j += 1;
        }
        t[i + 4] = carry;
        i += 1;
    }
    // Montgomery reduction, one limb at a time
    let mut i = 0;
    while i < 4 {
        let k = t[i].wrapping_mul(inv);
        let mut carry = 0;
        let mut j = 0;
        while j < 4 {
            let (lo, hi) = mac(t[i + j], k, m[j], carry);
            t[i + j] = lo;
            carry = hi;
            j += 1;
        }
        let mut idx = i + 4;
        while carry != 0 && idx < 8 {
            let (s, c) = adc(t[idx], 0, carry);
            t[idx] = s;
            carry = c;
            idx += 1;
        }
        i += 1;
    }
    let r = [t[4], t[5], t[6], t[7]];
    if geq(&r, m) {
        sub_raw(&r, m)
    } else {
        r
    }
}

/// Defines a field type over the given modulus (little-endian limbs).
macro_rules! mont_field {
    ($name:ident, $modulus:expr) => {
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        pub struct $name($crate::field::Limbs);

        #[allow(dead_code)]
        impl $name {
            pub const MODULUS: $crate::field::Limbs = $modulus;
            const INV: u64 = $crate::field::neg_inv(Self::MODULUS[0]);
            const R1: $crate::field::Limbs = $crate::field::pow2_mod(&Self::MODULUS, 256);
            const R2: $crate::field::Limbs = $crate::field::pow2_mod(&Self::MODULUS, 512);
            const R3: $crate::field::Limbs =
                $crate::field::mont_mul(&Self::R2, &Self::R2, &Self::MODULUS, Self::INV);
            pub const ZERO: Self = Self([0; 4]);
            pub const ONE: Self = Self(Self::R1);

            #[inline]
            pub const fn mul(&self, o: &Self) -> Self {
                Self($crate::field::mont_mul(
                    &self.0,
                    &o.0,
                    &Self::MODULUS,
                    Self::INV,
                ))
            }
            #[inline]
            pub const fn square(&self) -> Self {
                self.mul(self)
            }
            #[inline]
            pub const fn add(&self, o: &Self) -> Self {
                Self($crate::field::add_mod(&self.0, &o.0, &Self::MODULUS))
            }
            #[inline]
            pub const fn sub(&self, o: &Self) -> Self {
                Self($crate::field::sub_mod(&self.0, &o.0, &Self::MODULUS))
            }
            #[inline]
            pub const fn neg(&self) -> Self {
                Self::ZERO.sub(self)
            }
            pub const fn is_zero(&self) -> bool {
                self.0[0] | self.0[1] | self.0[2] | self.0[3] == 0
            }
            /// From canonical limbs (the caller guarantees they are below the modulus).
            pub const fn from_canonical_limbs(l: &$crate::field::Limbs) -> Self {
                Self($crate::field::mont_mul(
                    l,
                    &Self::R2,
                    &Self::MODULUS,
                    Self::INV,
                ))
            }
            pub const fn from_u64(x: u64) -> Self {
                Self::from_canonical_limbs(&[x, 0, 0, 0])
            }
            pub const fn to_canonical_limbs(self) -> $crate::field::Limbs {
                $crate::field::mont_mul(&self.0, &[1, 0, 0, 0], &Self::MODULUS, Self::INV)
            }
            /// Big-endian bytes to a field element; `None` if the value is not below the modulus.
            pub const fn from_be_bytes(b: &[u8; 32]) -> Option<Self> {
                let l = $crate::field::limbs_from_be(b);
                if $crate::field::geq(&l, &Self::MODULUS) {
                    None
                } else {
                    Some(Self::from_canonical_limbs(&l))
                }
            }
            pub const fn to_be_bytes(self) -> [u8; 32] {
                $crate::field::limbs_to_be(&self.to_canonical_limbs())
            }
            /// Raises to the power given as little-endian limbs (square and multiply, high bit first).
            pub fn pow(&self, exp: &$crate::field::Limbs) -> Self {
                let mut acc = Self::ONE;
                let mut started = false;
                for limb in exp.iter().rev() {
                    for bit in (0..64).rev() {
                        if started {
                            acc = acc.square();
                        }
                        if (limb >> bit) & 1 == 1 {
                            acc = if started { acc.mul(self) } else { *self };
                            started = true;
                        }
                    }
                }
                acc
            }
            /// self^(2^k)
            pub fn pow2k(&self, k: u32) -> Self {
                let mut acc = *self;
                for _ in 0..k {
                    acc = acc.square();
                }
                acc
            }
            /// The inverse by the binary extended Euclid algorithm; zero maps to zero.
            pub fn inverse(&self) -> Self {
                if self.is_zero() {
                    return Self::ZERO;
                }
                // self.0 = a*R, so the plain inverse is a^-1 * R^-1; times R^3 / R gives a^-1 * R.
                let inv = $crate::field::inv_plain(&self.0, &Self::MODULUS, Self::INV);
                Self($crate::field::mont_mul(
                    &inv,
                    &Self::R3,
                    &Self::MODULUS,
                    Self::INV,
                ))
            }
            /// The inverse by Fermat's little theorem (slow; the reference for tests).
            #[cfg(test)]
            pub fn inverse_fermat(&self) -> Self {
                let mut e = Self::MODULUS;
                e[0] -= 2;
                self.pow(&e)
            }
        }
    };
}

/// x >> k for 1 <= k <= 63.
fn shr_small(x: &mut Limbs, k: u32) {
    x[0] = (x[0] >> k) | (x[1] << (64 - k));
    x[1] = (x[1] >> k) | (x[2] << (64 - k));
    x[2] = (x[2] >> k) | (x[3] << (64 - k));
    x[3] >>= k;
}

/// x * 2^-k mod m for x < m and 1 <= k <= 63, where `ninv` is -m^-1 mod 2^64: adds the multiple
/// of m that clears the low k bits, then shifts them out.
fn halve_k(x: &mut Limbs, k: u32, m: &Limbs, ninv: u64) {
    let t = x[0].wrapping_mul(ninv) & ((1u64 << k) - 1);
    let mut w = [0u64; 5];
    let mut carry = 0u64;
    let mut i = 0;
    while i < 4 {
        let (lo, hi) = mac(x[i], t, m[i], carry);
        w[i] = lo;
        carry = hi;
        i += 1;
    }
    w[4] = carry;
    let mut i = 0;
    while i < 4 {
        x[i] = (w[i] >> k) | (w[i + 1] << (64 - k));
        i += 1;
    }
    // x/2^k + t*m/2^k < m + m/2^k, so one subtraction reduces it
    if geq(x, m) {
        *x = sub_raw(x, m);
    }
}

fn is_one(x: &Limbs) -> bool {
    x[0] == 1 && x[1] | x[2] | x[3] == 0
}

fn trailing_zeros(x: &Limbs) -> u32 {
    let mut i = 0;
    while i < 3 && x[i] == 0 {
        i += 1;
    }
    64 * i as u32 + x[i].trailing_zeros()
}

/// The inverse of x mod the odd prime m, for 0 < x < m, by the binary extended Euclid algorithm
/// (invariants x1 * x = u and x2 * x = v mod m). Runs a few hundred rounds of cheap word
/// operations, far fewer cycles on SBF than Fermat's 380 field multiplications.
pub fn inv_plain(x: &Limbs, m: &Limbs, ninv: u64) -> Limbs {
    let (mut u, mut v) = (*x, *m);
    let (mut x1, mut x2): (Limbs, Limbs) = ([1, 0, 0, 0], [0; 4]);
    loop {
        let mut tz = trailing_zeros(&u);
        while tz > 0 {
            let k = if tz > 63 { 63 } else { tz };
            shr_small(&mut u, k);
            halve_k(&mut x1, k, m, ninv);
            tz -= k;
        }
        if is_one(&u) {
            return x1;
        }
        let mut tz = trailing_zeros(&v);
        while tz > 0 {
            let k = if tz > 63 { 63 } else { tz };
            shr_small(&mut v, k);
            halve_k(&mut x2, k, m, ninv);
            tz -= k;
        }
        if is_one(&v) {
            return x2;
        }
        if geq(&u, &v) {
            u = sub_raw(&u, &v);
            x1 = sub_mod(&x1, &x2, m);
        } else {
            v = sub_raw(&v, &u);
            x2 = sub_mod(&x2, &x1, m);
        }
    }
}

pub const fn add_mod(a: &Limbs, b: &Limbs, m: &Limbs) -> Limbs {
    let s = add_raw(a, b);
    if geq(&s, m) {
        sub_raw(&s, m)
    } else {
        s
    }
}

pub const fn sub_mod(a: &Limbs, b: &Limbs, m: &Limbs) -> Limbs {
    if geq(a, b) {
        sub_raw(a, b)
    } else {
        add_raw(&sub_raw(a, b), m)
    }
}

pub const fn limbs_from_be(b: &[u8; 32]) -> Limbs {
    let mut out = [0u64; 4];
    let mut i = 0;
    while i < 4 {
        let mut w = 0u64;
        let mut j = 0;
        while j < 8 {
            w = (w << 8) | b[(3 - i) * 8 + j] as u64;
            j += 1;
        }
        out[i] = w;
        i += 1;
    }
    out
}

pub const fn limbs_to_be(l: &Limbs) -> [u8; 32] {
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 4 {
        let w = l[i];
        let mut j = 0;
        while j < 8 {
            out[(3 - i) * 8 + j] = (w >> (56 - 8 * j)) as u8;
            j += 1;
        }
        i += 1;
    }
    out
}

mont_field!(Fr, limbs_from_be(&crate::vk::R));

impl Fr {
    /// Reduces any 256-bit big-endian value mod r (at most five conditional subtractions).
    pub fn reduce_be_bytes(b: &[u8; 32]) -> Fr {
        let mut l = limbs_from_be(b);
        while geq(&l, &Self::MODULUS) {
            l = sub_raw(&l, &Self::MODULUS);
        }
        Fr::from_canonical_limbs(&l)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn next(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    #[test]
    fn inverse_matches_fermat_and_multiplies_back_to_one() {
        let mut s = 0x9e37_79b9_7f4a_7c15u64;
        for round in 0..300 {
            let mut l = [next(&mut s), next(&mut s), next(&mut s), next(&mut s) >> 3];
            if round % 7 == 0 {
                l = [next(&mut s) & 0xff, 0, 0, 0]; // small values
            }
            if round % 11 == 0 {
                l = [next(&mut s), 0, 0, next(&mut s) >> 3]; // long runs of zero limbs
            }
            while geq(&l, &Fr::MODULUS) {
                l = sub_raw(&l, &Fr::MODULUS);
            }
            let a = Fr::from_canonical_limbs(&l);
            if a.is_zero() {
                continue;
            }
            assert_eq!(a.inverse(), a.inverse_fermat());
            assert_eq!(a.mul(&a.inverse()), Fr::ONE);
        }
        assert_eq!(Fr::ZERO.inverse(), Fr::ZERO);
        assert_eq!(Fr::ONE.inverse(), Fr::ONE);
        let minus_one = Fr::ONE.neg();
        assert_eq!(minus_one.inverse(), minus_one);
    }
}
