//! The proof public-values struct (`registry::LAYOUT_ZISK_V1`): the
//! accumulator- and drift-bound layout `PostRootProved`'s layout-1 path binds a proof to, and the ZisK
//! guest commits.
//!
//! **208 bytes, v2.** v2 added the guest's drift-bound inputs
//! (`open_unix_ts`, `max_drift_secs`) and the batch's summed `gas_used` (the proof-bound fee
//! term) to the 184-byte v1 struct this crate defined before its first on-chain use — since no program
//! or guest ever consumed layout 1 (`PostRootProved` refused every `layout_id` but
//! `registry::LAYOUT_HEADER_FALLBACK`), redefining it here is greenfield, not a migration.
//!
//! ```text
//! chain_id u64 | first_number u64 | last_number u64 | open_unix_ts u64 | max_drift_secs u64 | gas_used u64
//! | parent_hash [32] | last_block_hash [32] | state_root [32] | inbox_commitment [32] | forced_outcome_commitment [32]
//! ```
//! All integers little-endian, no reserved bytes: 6 × 8 B + 5 × 32 B = 208 B exactly.
//!
//! **ZisK packing.** The guest's output buffer is 64 `u32` words, each carried in this program's ABI as
//! one little-endian `u64` (`header::zisk_committed_hash`'s doc; `veritas`'s `zisk_public_signal`). 208 bytes pack
//! into 52 such words (`208 / 4`); the remaining 12 of 64 are zero. [`pack_zisk_outputs`] builds that
//! 64-word (512-byte) buffer from a 208-byte blob; [`unpack_zisk_outputs`] is its inverse, refusing a
//! non-zero tail word or any word that does not fit in a `u32` (a guest output can never be a real
//! ZisK-committed public-values buffer otherwise) — both refusals fail by name, not silently truncate.

/// Full account/blob length: 6 `u64` fields + 5 32-byte hashes, no reserved tail.
pub const PUBLIC_VALUES_LEN: usize = 208;

pub const OFF_CHAIN_ID: usize = 0;
pub const OFF_FIRST_NUMBER: usize = 8;
pub const OFF_LAST_NUMBER: usize = 16;
pub const OFF_OPEN_UNIX_TS: usize = 24;
pub const OFF_MAX_DRIFT_SECS: usize = 32;
pub const OFF_GAS_USED: usize = 40;
pub const OFF_PARENT_HASH: usize = 48;
pub const OFF_LAST_BLOCK_HASH: usize = 80;
pub const OFF_STATE_ROOT: usize = 112;
pub const OFF_INBOX_COMMITMENT: usize = 144;
pub const OFF_FORCED_OUTCOME_COMMITMENT: usize = 176;

/// The number of ZisK `u32` output words 208 bytes packs into (`52 = 208 / 4`); the guest's remaining
/// `64 - 52 = 12` words must be zero.
pub const ZISK_WORDS_USED: usize = PUBLIC_VALUES_LEN / 4;
/// Total ZisK output words: 64 raw 32-bit registers the GUEST itself commits (256 raw bytes total,
/// `ziskos::io::zkvm_io::set_output`'s own 64-slot limit) — the PROVER packages those same 64 words into
/// the on-chain reader's 512-byte, 64-`u64`-word `publicValues` (`header::zisk_committed_hash`'s doc /
/// `veritas`), external to what the guest's own `commit_slice` calls produce (see
/// `guest-rome/src/run.rs`'s own doc for the full guest-vs-prover packaging split).
pub const ZISK_WORDS_TOTAL: usize = 64;
/// The ZisK ABI's public-values byte length: 64 little-endian `u64` words, one `u32` each.
pub const ZISK_PUBLIC_VALUES_LEN: usize = ZISK_WORDS_TOTAL * 8;

/// Field-for-field decode of the v2 public-values struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicValues {
    pub chain_id: u64,
    pub first_number: u64,
    pub last_number: u64,
    pub open_unix_ts: u64,
    pub max_drift_secs: u64,
    pub gas_used: u64,
    pub parent_hash: [u8; 32],
    pub last_block_hash: [u8; 32],
    pub state_root: [u8; 32],
    pub inbox_commitment: [u8; 32],
    pub forced_outcome_commitment: [u8; 32],
}

/// Validates the exact length (`PUBLIC_VALUES_LEN`, no slack) and decodes every field.
pub fn read(d: &[u8]) -> Result<PublicValues, crate::LayoutError> {
    if d.len() != PUBLIC_VALUES_LEN {
        return Err(crate::LayoutError::TooShort {
            need: PUBLIC_VALUES_LEN,
            got: d.len(),
        });
    }
    let u64_at = |o: usize| u64::from_le_bytes(d[o..o + 8].try_into().unwrap());
    let b32_at = |o: usize| -> [u8; 32] { d[o..o + 32].try_into().unwrap() };
    Ok(PublicValues {
        chain_id: u64_at(OFF_CHAIN_ID),
        first_number: u64_at(OFF_FIRST_NUMBER),
        last_number: u64_at(OFF_LAST_NUMBER),
        open_unix_ts: u64_at(OFF_OPEN_UNIX_TS),
        max_drift_secs: u64_at(OFF_MAX_DRIFT_SECS),
        gas_used: u64_at(OFF_GAS_USED),
        parent_hash: b32_at(OFF_PARENT_HASH),
        last_block_hash: b32_at(OFF_LAST_BLOCK_HASH),
        state_root: b32_at(OFF_STATE_ROOT),
        inbox_commitment: b32_at(OFF_INBOX_COMMITMENT),
        forced_outcome_commitment: b32_at(OFF_FORCED_OUTCOME_COMMITMENT),
    })
}

/// Encodes a [`PublicValues`] to its exact 208-byte blob (the inverse of [`read`]) — the
/// guest builds `PublicValues` from its own computed fields and must commit them as bytes, not just
/// decode a caller-supplied blob.
pub fn write(pv: &PublicValues) -> [u8; PUBLIC_VALUES_LEN] {
    let mut d = [0u8; PUBLIC_VALUES_LEN];
    d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].copy_from_slice(&pv.chain_id.to_le_bytes());
    d[OFF_FIRST_NUMBER..OFF_FIRST_NUMBER + 8].copy_from_slice(&pv.first_number.to_le_bytes());
    d[OFF_LAST_NUMBER..OFF_LAST_NUMBER + 8].copy_from_slice(&pv.last_number.to_le_bytes());
    d[OFF_OPEN_UNIX_TS..OFF_OPEN_UNIX_TS + 8].copy_from_slice(&pv.open_unix_ts.to_le_bytes());
    d[OFF_MAX_DRIFT_SECS..OFF_MAX_DRIFT_SECS + 8].copy_from_slice(&pv.max_drift_secs.to_le_bytes());
    d[OFF_GAS_USED..OFF_GAS_USED + 8].copy_from_slice(&pv.gas_used.to_le_bytes());
    d[OFF_PARENT_HASH..OFF_PARENT_HASH + 32].copy_from_slice(&pv.parent_hash);
    d[OFF_LAST_BLOCK_HASH..OFF_LAST_BLOCK_HASH + 32].copy_from_slice(&pv.last_block_hash);
    d[OFF_STATE_ROOT..OFF_STATE_ROOT + 32].copy_from_slice(&pv.state_root);
    d[OFF_INBOX_COMMITMENT..OFF_INBOX_COMMITMENT + 32].copy_from_slice(&pv.inbox_commitment);
    d[OFF_FORCED_OUTCOME_COMMITMENT..OFF_FORCED_OUTCOME_COMMITMENT + 32]
        .copy_from_slice(&pv.forced_outcome_commitment);
    d
}

/// Packs a 208-byte public-values blob into the ZisK guest's 64-`u32`-word output ABI (each word carried
/// as one little-endian `u64` in the 512-byte on-chain buffer — `header::zisk_committed_hash`'s doc):
/// words `0..52` hold the 208 bytes 4 at a time, words `52..64` are zero.
pub fn pack_zisk_outputs(pv: &[u8; PUBLIC_VALUES_LEN]) -> [u64; ZISK_WORDS_TOTAL] {
    let mut out = [0u64; ZISK_WORDS_TOTAL];
    for (i, word) in out.iter_mut().enumerate().take(ZISK_WORDS_USED) {
        let b = i * 4;
        let w = u32::from_le_bytes(pv[b..b + 4].try_into().unwrap());
        *word = w as u64;
    }
    out
}

/// Inverse of [`pack_zisk_outputs`]: `input` must be exactly [`ZISK_PUBLIC_VALUES_LEN`] (512) bytes — 64
/// little-endian `u64` words, each holding one `u32`. Refuses by name (not a silent truncation) if any
/// word does not fit in a `u32`, or if any of the 12 tail words (`52..64`) is non-zero — a real
/// ZisK-committed v2 public-values buffer can never carry either.
pub fn unpack_zisk_outputs(input: &[u8]) -> Result<[u8; PUBLIC_VALUES_LEN], crate::LayoutError> {
    if input.len() != ZISK_PUBLIC_VALUES_LEN {
        return Err(crate::LayoutError::TooShort {
            need: ZISK_PUBLIC_VALUES_LEN,
            got: input.len(),
        });
    }
    let word_at =
        |i: usize| -> u64 { u64::from_le_bytes(input[i * 8..i * 8 + 8].try_into().unwrap()) };
    let mut out = [0u8; PUBLIC_VALUES_LEN];
    for i in 0..ZISK_WORDS_USED {
        let w = word_at(i);
        if w > u32::MAX as u64 {
            return Err(crate::LayoutError::BadZiskWord { index: i });
        }
        out[i * 4..i * 4 + 4].copy_from_slice(&(w as u32).to_le_bytes());
    }
    for i in ZISK_WORDS_USED..ZISK_WORDS_TOTAL {
        if word_at(i) != 0 {
            return Err(crate::LayoutError::BadZiskTail { index: i });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build() -> Vec<u8> {
        let mut d = vec![0u8; PUBLIC_VALUES_LEN];
        d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].copy_from_slice(&200100u64.to_le_bytes());
        d[OFF_FIRST_NUMBER..OFF_FIRST_NUMBER + 8].copy_from_slice(&11u64.to_le_bytes());
        d[OFF_LAST_NUMBER..OFF_LAST_NUMBER + 8].copy_from_slice(&20u64.to_le_bytes());
        d[OFF_OPEN_UNIX_TS..OFF_OPEN_UNIX_TS + 8].copy_from_slice(&1_700_000_000u64.to_le_bytes());
        d[OFF_MAX_DRIFT_SECS..OFF_MAX_DRIFT_SECS + 8].copy_from_slice(&60u64.to_le_bytes());
        d[OFF_GAS_USED..OFF_GAS_USED + 8].copy_from_slice(&21_000_777u64.to_le_bytes());
        d[OFF_PARENT_HASH..OFF_PARENT_HASH + 32].copy_from_slice(&[0x11u8; 32]);
        d[OFF_LAST_BLOCK_HASH..OFF_LAST_BLOCK_HASH + 32].copy_from_slice(&[0x22u8; 32]);
        d[OFF_STATE_ROOT..OFF_STATE_ROOT + 32].copy_from_slice(&[0x33u8; 32]);
        d[OFF_INBOX_COMMITMENT..OFF_INBOX_COMMITMENT + 32].copy_from_slice(&[0x44u8; 32]);
        d[OFF_FORCED_OUTCOME_COMMITMENT..OFF_FORCED_OUTCOME_COMMITMENT + 32]
            .copy_from_slice(&[0x55u8; 32]);
        d
    }

    /// The guest commits `PublicValues` fields, not a raw byte blob — it needs a byte
    /// WRITER, not just `read`. `write` must be the exact inverse of `read`: encoding a `PublicValues` and
    /// decoding it back must reproduce every field, and the encoded bytes must equal a blob built the
    /// independent way (`build()`, used by every other golden test in this file).
    #[test]
    fn write_is_the_inverse_of_read() {
        let d = build();
        let pv = read(&d).unwrap();
        let encoded = write(&pv);
        assert_eq!(
            encoded.to_vec(),
            d,
            "write must reproduce read's own input byte-for-byte"
        );
        assert_eq!(read(&encoded).unwrap(), pv);
    }

    /// Golden test: pins the field order/offsets/endianness against a synthetic blob built independently
    /// of `read` — the byte authority the guest must match.
    #[test]
    fn round_trips_a_synthetic_blob_at_the_pinned_offsets() {
        let d = build();
        let got = read(&d).unwrap();
        assert_eq!(got.chain_id, 200100);
        assert_eq!(got.first_number, 11);
        assert_eq!(got.last_number, 20);
        assert_eq!(got.open_unix_ts, 1_700_000_000);
        assert_eq!(got.max_drift_secs, 60);
        assert_eq!(got.gas_used, 21_000_777);
        assert_eq!(got.parent_hash, [0x11u8; 32]);
        assert_eq!(got.last_block_hash, [0x22u8; 32]);
        assert_eq!(got.state_root, [0x33u8; 32]);
        assert_eq!(got.inbox_commitment, [0x44u8; 32]);
        assert_eq!(got.forced_outcome_commitment, [0x55u8; 32]);
    }

    #[test]
    fn is_exactly_208_bytes_with_no_reserved_tail() {
        assert_eq!(PUBLIC_VALUES_LEN, 208);
        assert_eq!(OFF_FORCED_OUTCOME_COMMITMENT + 32, PUBLIC_VALUES_LEN);
    }

    #[test]
    fn rejects_wrong_length() {
        assert!(read(&[0u8; 100]).is_err());
        assert!(
            read(&[0u8; 184]).is_err(),
            "184 (v1) is no longer valid — 208 is exact"
        );
        assert!(read(&build()).is_ok());
    }

    /// Swapping two offsets (contract) must make the golden test fail — proved here by
    /// building a blob at the WRONG offset for `max_drift_secs` (using `OFF_GAS_USED`'s slot instead) and
    /// asserting the decode disagrees with the correct blob.
    #[test]
    fn offset_mutation_is_detected_by_the_golden_test() {
        let correct = read(&build()).unwrap();
        let mut swapped = build();
        // Overwrite max_drift_secs's true slot with gas_used's value and vice versa — if a future edit
        // ever swapped OFF_MAX_DRIFT_SECS and OFF_GAS_USED, this is the shape that would go undetected
        // without a golden test tied to independently-computed offsets.
        let max_drift = swapped[OFF_MAX_DRIFT_SECS..OFF_MAX_DRIFT_SECS + 8].to_vec();
        let gas_used = swapped[OFF_GAS_USED..OFF_GAS_USED + 8].to_vec();
        swapped[OFF_MAX_DRIFT_SECS..OFF_MAX_DRIFT_SECS + 8].copy_from_slice(&gas_used);
        swapped[OFF_GAS_USED..OFF_GAS_USED + 8].copy_from_slice(&max_drift);
        let got = read(&swapped).unwrap();
        assert_ne!(
            got, correct,
            "a swapped-offset blob must decode differently"
        );
    }

    #[test]
    fn pack_unpack_round_trips() {
        let d = build();
        let arr: [u8; PUBLIC_VALUES_LEN] = d.clone().try_into().unwrap();
        let words = pack_zisk_outputs(&arr);
        assert_eq!(words.len(), ZISK_WORDS_TOTAL);
        for &w in &words[ZISK_WORDS_USED..] {
            assert_eq!(w, 0, "tail words beyond 52 must be zero");
        }
        for &w in &words {
            assert!(w <= u32::MAX as u64);
        }
        let mut buf = Vec::with_capacity(ZISK_PUBLIC_VALUES_LEN);
        for w in words {
            buf.extend_from_slice(&w.to_le_bytes());
        }
        let unpacked = unpack_zisk_outputs(&buf).unwrap();
        assert_eq!(unpacked, arr);
        assert_eq!(read(&unpacked).unwrap(), read(&d).unwrap());
    }

    #[test]
    fn unpack_rejects_wrong_length() {
        assert!(unpack_zisk_outputs(&[0u8; 500]).is_err());
        assert!(unpack_zisk_outputs(&[0u8; 512]).is_ok());
    }

    #[test]
    fn unpack_rejects_a_word_over_u32_max() {
        let mut buf = vec![0u8; ZISK_PUBLIC_VALUES_LEN];
        buf[0..8].copy_from_slice(&((u32::MAX as u64) + 1).to_le_bytes());
        assert_eq!(
            unpack_zisk_outputs(&buf),
            Err(crate::LayoutError::BadZiskWord { index: 0 }),
            "an over-u32 word is refused under its own name, with the word index"
        );
    }

    /// Contract: drop the tail-zero check and this test must go red. (The real fixture's
    /// bincode-hash publics are NOT such a case — measured, its words 9..64 are all zero, so it unpacks
    /// structurally and is refused by the first binding check instead; see `settlement.rs`'s layout-1
    /// fixture test.)
    #[test]
    fn unpack_rejects_a_non_zero_tail_word() {
        let arr = [0u8; PUBLIC_VALUES_LEN];
        let words = pack_zisk_outputs(&arr);
        let mut buf = Vec::with_capacity(ZISK_PUBLIC_VALUES_LEN);
        for w in words {
            buf.extend_from_slice(&w.to_le_bytes());
        }
        // Corrupt the last tail word (word 63) to non-zero.
        let last = ZISK_WORDS_TOTAL - 1;
        buf[last * 8..last * 8 + 8].copy_from_slice(&7u64.to_le_bytes());
        assert_eq!(
            unpack_zisk_outputs(&buf),
            Err(crate::LayoutError::BadZiskTail { index: last }),
            "a non-zero tail word must be refused under its own name, with the word index"
        );
    }
}
