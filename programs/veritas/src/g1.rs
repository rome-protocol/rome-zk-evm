//! G1 points as 64-byte big-endian `x || y` strings (specification section 2), checked by the
//! library itself, and the three alt_bn128 operations of section 8.1.

use crate::field::{geq, limbs_from_be, limbs_to_be, sub_raw, Limbs};
use solana_program::program_error::ProgramError;

pub type G1 = [u8; 64];

const P: Limbs = limbs_from_be(&crate::vk::P);

fn malformed<E>(_: E) -> ProgramError {
    ProgramError::InvalidInstructionData
}

/// Rule 2 of section 5: both coordinates are below p. The curve equation (rule 3) is left to the
/// alt_bn128 operations, which every proof point is an input to; a failed operation is MALFORMED.
/// This check is done here because the syscalls' decoder accepts some words that are >= p.
pub fn coordinates_in_range(p: &G1) -> bool {
    let (xb, yb) = p.split_at(32);
    let (Ok(xb), Ok(yb)) = (<&[u8; 32]>::try_from(xb), <&[u8; 32]>::try_from(yb)) else {
        return false;
    };
    !geq(&limbs_from_be(xb), &P) && !geq(&limbs_from_be(yb), &P)
}

/// -P, for a point whose coordinates are below p: y becomes p - y (0 stays 0).
pub fn neg(p: &G1) -> Result<G1, ProgramError> {
    let mut out = *p;
    let y = <&[u8; 32]>::try_from(&p[32..]).map_err(malformed)?;
    let y = limbs_from_be(y);
    if geq(&y, &P) {
        return Err(ProgramError::InvalidInstructionData);
    }
    let ny = if y == [0; 4] { y } else { sub_raw(&P, &y) };
    out[32..].copy_from_slice(&limbs_to_be(&ny));
    Ok(out)
}

fn to_point(v: Vec<u8>) -> Result<G1, ProgramError> {
    G1::try_from(v.as_slice()).map_err(malformed)
}

pub fn add(a: &G1, b: &G1) -> Result<G1, ProgramError> {
    let mut input = [0u8; 128];
    input[..64].copy_from_slice(a);
    input[64..].copy_from_slice(b);
    to_point(solana_bn254::prelude::alt_bn128_g1_addition_be(&input).map_err(malformed)?)
}

pub fn mul(p: &G1, scalar: &[u8; 32]) -> Result<G1, ProgramError> {
    let mut input = [0u8; 96];
    input[..64].copy_from_slice(p);
    input[64..].copy_from_slice(scalar);
    to_point(solana_bn254::prelude::alt_bn128_g1_multiplication_be(&input).map_err(malformed)?)
}

/// The pairing check: true when the product of the pairings in `input` (k x 192 bytes) is 1.
pub fn pairing_is_one(input: &[u8; 384]) -> Result<bool, ProgramError> {
    let out = solana_bn254::prelude::alt_bn128_pairing_be(input).map_err(malformed)?;
    let out = <[u8; 32]>::try_from(out.as_slice()).map_err(malformed)?;
    let mut one = [0u8; 32];
    one[31] = 1;
    Ok(out == one)
}
