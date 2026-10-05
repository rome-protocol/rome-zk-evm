//! Veritas: verifies ZisK's PLONK proofs on Solana. Built from its specification alone.

use field::Fr;
use solana_program::hash;
use solana_program::program_error::ProgramError;

mod field;
mod g1;
mod verify;
pub mod versions;
pub mod vk;

pub use versions::{zisk_version, Status, ZiskVersion};
pub use vk::VerifyingKey;

/// Length of `verify_zisk`'s input: the ABI of specification section 1.5.
pub const ZISK_ABI_LEN: usize = 1344;
/// Length of `verify`'s input: the proof followed by the public signal.
pub const VERIFY_LEN: usize = 800;

/// Verifies a proof of one ZisK release, given in the 1,344-byte ABI of section 1.5: proof, `programVK`,
/// `rootCVadcopFinal`, `publicValues`. `Ok(true)` accepts, `Ok(false)` rejects, and `InvalidInstructionData`
/// means the input is malformed (section 5). A proof whose `rootCVadcopFinal` is not the release's pinned
/// one is rejected before anything is hashed. A release without a key (withdrawn, in a production build)
/// gives `InvalidArgument`.
pub fn verify_zisk(version: &ZiskVersion, data: &[u8]) -> Result<bool, ProgramError> {
    if data.len() != ZISK_ABI_LEN {
        return Err(ProgramError::InvalidInstructionData);
    }
    let key = version.key.as_ref().ok_or(ProgramError::InvalidArgument)?;
    if data[800..832] != version.root_c {
        return Ok(false);
    }
    let signal = zisk_public_signal(&data[768..800], &data[832..1344], &data[800..832]);
    Ok(verify::run(key, &data[..768], &signal)?.accepted)
}

/// The public signal of section 1.5: SHA-256 of `programVK || publicValues || rootCVadcopFinal`,
/// reduced mod r, as a 32-byte big-endian word. It hashes what it is given and cannot fail.
pub fn zisk_public_signal(program_vk: &[u8], public_values: &[u8], rootc: &[u8]) -> [u8; 32] {
    let digest = hash::hashv(&[program_vk, public_values, rootc]).to_bytes();
    Fr::reduce_be_bytes(&digest).to_be_bytes()
}

/// Verifies a proof under `key`, given as 768 proof bytes followed by the 32-byte public signal.
pub fn verify(key: &VerifyingKey, data: &[u8]) -> Result<bool, ProgramError> {
    if data.len() != VERIFY_LEN {
        return Err(ProgramError::InvalidInstructionData);
    }
    Ok(verify::run(key, &data[..768], &data[768..])?.accepted)
}

/// Every intermediate value of one verification (specification sections 6, 7 and 9.3). For tests.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trace {
    pub beta: [u8; 32],
    pub gamma: [u8; 32],
    pub alpha: [u8; 32],
    pub z: [u8; 32],
    pub v: [u8; 32],
    pub u: [u8; 32],
    pub z_n: [u8; 32],
    pub z_h: [u8; 32],
    pub l1: [u8; 32],
    pub pi: [u8; 32],
    pub r0: [u8; 32],
    pub e_scalar: [u8; 32],
    pub d: [u8; 64],
    pub f: [u8; 64],
    pub e: [u8; 64],
    pub p_l: [u8; 64],
    pub p_r: [u8; 64],
    pub accepted: bool,
}

#[doc(hidden)]
pub fn trace(key: &VerifyingKey, proof: &[u8], signal: &[u8]) -> Result<Trace, ProgramError> {
    verify::run(key, proof, signal)
}

#[cfg(not(feature = "no-entrypoint"))]
mod entrypoint {
    //! The program instruction of specification section 1.4: the instruction data is one release byte (the
    //! registry's scheme byte) followed by the 1,344-byte ABI; accounts and the program id are ignored;
    //! success is exactly `verify_zisk` returning `Ok(true)` under that release; REJECT, MALFORMED and an
    //! unknown release all fail with `InvalidInstructionData`.
    use solana_program::{
        account_info::AccountInfo, entrypoint, entrypoint::ProgramResult,
        program_error::ProgramError, pubkey::Pubkey,
    };

    entrypoint!(process_instruction);

    fn process_instruction(
        _program_id: &Pubkey,
        _accounts: &[AccountInfo],
        data: &[u8],
    ) -> ProgramResult {
        let Some((&scheme, abi)) = data.split_first() else {
            return Err(ProgramError::InvalidInstructionData);
        };
        let Some(version) = super::zisk_version(scheme) else {
            return Err(ProgramError::InvalidInstructionData);
        };
        match super::verify_zisk(version, abi) {
            Ok(true) => Ok(()),
            Ok(false) | Err(_) => Err(ProgramError::InvalidInstructionData),
        }
    }
}
