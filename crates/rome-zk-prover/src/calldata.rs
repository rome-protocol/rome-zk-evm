//! Rust port of the earlier Python decoder `zisk_calldata.py` (lines 20–44): decodes a ZisK `Proof` file
//! (bincode 2, standard varint config, `Plonk` variant) into the four ABI fields
//! `programs/veritas` and `zk-settlement-client::layout1_proof_abi` consume, plus the
//! derived PLONK public signal.
//!
//! The wire format is hand-rolled bincode 2 (the same thing the Python decoder hand-rolls — no
//! `bincode` crate dependency here, matching it byte for byte):
//!
//! ```text
//! tag: varint                        // Proof enum discriminant; 1 == Plonk
//! proof_bytes: vec<u8>                // 768 bytes
//! vadcop_vk: vec<u64>
//! protocol: string, curve: string, n_public: varint, power: varint, k1: string, k2: string
//! [ [string; 3]; 8 ]                  // Qm..S3
//! [ [string; 2]; 3 ]                  // X_2
//! w: string
//! publics_data: vec<u8>               // PublicValues.data (ptr is #[serde(skip)])
//! publics_full: vec<u64>              // 1+4+64 (vadcop_final flag present) or 4+64 (stripped)
//! rootc: vec<u64>                     // 4 words
//! program_vk: vec<u64>                // 4 words
//! hash_mode: varint
//! ```
//!
//! `programVK`/`rootCVadcopFinal` serialize each `u64` word big-endian (4 words -> 32 bytes);
//! `publics_full[4..]` (the 64 public-output words) serialize little-endian (64 words -> 512
//! bytes) — the ZisK ABI `rome_zk_layouts::public_values::unpack_zisk_outputs` reads back.
//! `publicSignal = sha256(programVK ‖ publicValues ‖ rootC) mod r`, computed via
//! `veritas::zisk_public_signal` (the same reduction the on-chain verifier does), never
//! reimplemented here.

use veritas::zisk_public_signal;

/// The ZisK ABI's public-values byte length (`rome_zk_layouts::public_values::ZISK_PUBLIC_VALUES_LEN`,
/// re-stated here to avoid a dependency on `rome-zk-layouts` for one constant already owned by
/// `veritas`'s ABI doc — 64 little-endian `u64` words).
const ZISK_PUBLIC_VALUES_LEN: usize = 512;
const PROOF_BYTES_LEN: usize = 768;

/// A decoded ZisK PLONK proof file, ready to hand to
/// [`zk_settlement_client::layout1_proof_abi`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Calldata {
    pub program_vk: [u8; 32],
    pub root_c: [u8; 32],
    pub publics_512: [u8; ZISK_PUBLIC_VALUES_LEN],
    pub proof_bytes_768: [u8; PROOF_BYTES_LEN],
    pub public_signal: [u8; 32],
}

/// Refused by name — never a silent truncation or a generic parse failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CalldataError {
    /// The file ran out of bytes mid-decode, or had bytes left over after the last field —
    /// covers both a truncated file and one with trailing garbage appended.
    #[error("proof file is truncated or has trailing bytes")]
    TrailingOrShort,
    /// The `Proof` enum's discriminant was not 1 (`Plonk`).
    #[error("not a Plonk proof (variant {0})")]
    NotPlonk(u64),
    /// `publics_full[..4]` (the packaged public-output vkey word) did not match the proof's own
    /// `program_vk` field — the two are meant to be the same value, doubly encoded.
    #[error("publics_full[..4] does not match program_vk")]
    ProgramVkMismatch,
    /// Any other shape violation the wire format itself rules out (wrong `proof_bytes` length,
    /// an unexpected `publics_full`/`rootc`/`program_vk` word count).
    #[error("malformed proof file: {0}")]
    Malformed(&'static str),
    /// The leading vadcop_final flag word (when `publics_full` carries one) was not `1`. Stricter
    /// than the Python reference decoder, which strips whatever value is there — see the flag
    /// strip's own comment.
    #[error("vadcop_final flag word is {0}, expected 1")]
    BadVadcopFlag(u64),
    /// [`check_against_record`]: the decoded proof's `program_vk` disagrees with the vkey of
    /// record's `programVK` — this proof was not made against the ELF this chain has registered.
    #[error("program_vk {got} is not the vkey of record's programVK {expected}")]
    ProgramVkNotOfRecord { got: String, expected: String },
    /// [`check_against_record`]: the decoded proof's `root_c` disagrees with the vkey of record's
    /// `rootCVadcopFinal`.
    #[error("root_c {got} is not the vkey of record's rootCVadcopFinal {expected}")]
    RootCNotOfRecord { got: String, expected: String },
}

/// A [`Calldata`] whose `program_vk`/`root_c` have been checked against a [`crate::config::VkeyOfRecord`]
/// by [`check_against_record`] — the ONLY way to construct one. The crate's ABI-building path
/// ([`crate::abi::layout1_from`]) accepts only `&RecordChecked`, never a raw `&Calldata` — so "the record
/// check ran before the ABI was built" is a compile-time property of the caller's own types, not a
/// convention to re-verify by reading call order. See `crate::abi`'s own `compile_fail`
/// doctest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordChecked(Calldata);

impl RecordChecked {
    /// The checked calldata's fields — read-only; there is no way to construct a `RecordChecked` other
    /// than through [`check_against_record`], so any `&RecordChecked` in hand has already passed it.
    pub fn calldata(&self) -> &Calldata {
        &self.0
    }
}

/// Checks a decoded proof against the vkey of record and, only on success, wraps it as a
/// [`RecordChecked`] — must run before any `layout1_proof_abi` is built from `calldata`.
/// `Config::load_vkey_of_record` has already checked the JSON's own fields and the ELF sha; this function adds
/// the decoded-proof comparison. The REGISTRY comparison (is the vkey of record itself still the chain's
/// active layout-1 entry) is `crate::anchor`'s `VkeyNotActive` refusal.
pub fn check_against_record(
    calldata: &Calldata,
    record: &crate::config::VkeyOfRecord,
) -> Result<RecordChecked, CalldataError> {
    if calldata.program_vk != record.program_vk {
        return Err(CalldataError::ProgramVkNotOfRecord {
            got: format!("0x{}", hex::encode(calldata.program_vk)),
            expected: format!("0x{}", hex::encode(record.program_vk)),
        });
    }
    if calldata.root_c != record.root_c {
        return Err(CalldataError::RootCNotOfRecord {
            got: format!("0x{}", hex::encode(calldata.root_c)),
            expected: format!("0x{}", hex::encode(record.root_c)),
        });
    }
    Ok(RecordChecked(calldata.clone()))
}

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Reader { b, i: 0 }
    }

    fn u8(&mut self) -> Result<u8, CalldataError> {
        let v = *self.b.get(self.i).ok_or(CalldataError::TrailingOrShort)?;
        self.i += 1;
        Ok(v)
    }

    fn raw(&mut self, n: usize) -> Result<&'a [u8], CalldataError> {
        let end = self
            .i
            .checked_add(n)
            .ok_or(CalldataError::TrailingOrShort)?;
        let s = self
            .b
            .get(self.i..end)
            .ok_or(CalldataError::TrailingOrShort)?;
        self.i = end;
        Ok(s)
    }

    /// bincode 2's standard varint integer encoding: `< 251` is the byte itself; `251`/`252`/`253`
    /// prefix a 2/4/8-byte little-endian tail (u16/u32/u64); `254`/`255` (u128) are never expected
    /// on this wire (the Python decoder raises on them too) and are treated as truncation/malformed.
    fn varint(&mut self) -> Result<u64, CalldataError> {
        let t = self.u8()?;
        match t {
            0..=250 => Ok(t as u64),
            251 => Ok(u16::from_le_bytes(self.raw(2)?.try_into().unwrap()) as u64),
            252 => Ok(u32::from_le_bytes(self.raw(4)?.try_into().unwrap()) as u64),
            253 => Ok(u64::from_le_bytes(self.raw(8)?.try_into().unwrap())),
            _ => Err(CalldataError::Malformed(
                "u128 varint not expected on this wire",
            )),
        }
    }

    fn vec_u8(&mut self) -> Result<Vec<u8>, CalldataError> {
        let n = self.varint()? as usize;
        Ok(self.raw(n)?.to_vec())
    }

    fn vec_u64(&mut self) -> Result<Vec<u64>, CalldataError> {
        let n = self.varint()? as usize;
        (0..n).map(|_| self.varint()).collect()
    }

    /// A bincode `String` is a `vec<u8>` of UTF-8 bytes; the calldata decoder never reads the
    /// content (`protocol`/`curve`/vkey coordinate strings), only needs to skip past it.
    fn skip_string(&mut self) -> Result<(), CalldataError> {
        self.vec_u8()?;
        Ok(())
    }

    fn at_end(&self) -> bool {
        self.i == self.b.len()
    }
}

fn words_be32(words: &[u64]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, w) in words.iter().enumerate() {
        out[i * 8..i * 8 + 8].copy_from_slice(&w.to_be_bytes());
    }
    out
}

fn words_le(words: &[u64]) -> [u8; ZISK_PUBLIC_VALUES_LEN] {
    let mut out = [0u8; ZISK_PUBLIC_VALUES_LEN];
    for (i, w) in words.iter().enumerate() {
        out[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
    }
    out
}

/// Decode a `cargo-zisk prove --plonk` output file into the four ABI fields plus the derived
/// public signal. Port of `zisk_calldata.py:20-44`.
pub fn from_zisk_proof_file(bytes: &[u8]) -> Result<Calldata, CalldataError> {
    let mut r = Reader::new(bytes);

    let tag = r.varint()?;
    if tag != 1 {
        return Err(CalldataError::NotPlonk(tag));
    }

    let proof_bytes = r.vec_u8()?;
    if proof_bytes.len() != PROOF_BYTES_LEN {
        return Err(CalldataError::Malformed("proof_bytes must be 768 bytes"));
    }

    let _vadcop_vk = r.vec_u64()?;

    // PlonkVkey: protocol, curve, nPublic, power, k1, k2, Qm..S3 (8x3 strings), X_2 (3x2 strings), w.
    r.skip_string()?; // protocol
    r.skip_string()?; // curve
    let _n_public = r.varint()?;
    let _power = r.varint()?;
    r.skip_string()?; // k1
    r.skip_string()?; // k2
    for _ in 0..8 {
        for _ in 0..3 {
            r.skip_string()?;
        }
    }
    for _ in 0..3 {
        for _ in 0..2 {
            r.skip_string()?;
        }
    }
    r.skip_string()?; // w

    let _publics_data = r.vec_u8()?;
    let mut publics_full = r.vec_u64()?;
    let rootc = r.vec_u64()?;
    let program_vk = r.vec_u64()?;
    let _hash_mode = r.varint()?;

    if !r.at_end() {
        return Err(CalldataError::TrailingOrShort);
    }

    // `program_publics()`: strip the leading vadcop_final flag when present. The Python decoder
    // is silent on the flag's own value (it strips whatever word is there); this port is
    // stricter — value 1 is the only vadcop_final flag this crate's callers ever expect, and a
    // proof file with some other value there refuses by name rather than silently proceeding on
    // a flag word it never checked. No soundness impact either way (this file never reaches the
    // on-chain verifier), but a wrong flag value is worth surfacing rather than swallowing.
    if publics_full.len() == 1 + 4 + 64 {
        if publics_full[0] != 1 {
            return Err(CalldataError::BadVadcopFlag(publics_full[0]));
        }
        publics_full.remove(0);
    }
    if publics_full.len() != 4 + 64 {
        return Err(CalldataError::Malformed(
            "publics_full must be 68 words after any flag strip",
        ));
    }
    if rootc.len() != 4 {
        return Err(CalldataError::Malformed("rootc must be 4 words"));
    }
    if program_vk.len() != 4 {
        return Err(CalldataError::Malformed("program_vk must be 4 words"));
    }
    if publics_full[..4] != program_vk[..] {
        return Err(CalldataError::ProgramVkMismatch);
    }

    let program_vk_bytes = words_be32(&program_vk);
    let root_c_bytes = words_be32(&rootc);
    let publics_512 = words_le(&publics_full[4..]);
    let public_signal = zisk_public_signal(&program_vk_bytes, &publics_512, &root_c_bytes);

    let mut proof_bytes_768 = [0u8; PROOF_BYTES_LEN];
    proof_bytes_768.copy_from_slice(&proof_bytes);

    Ok(Calldata {
        program_vk: program_vk_bytes,
        root_c: root_c_bytes,
        publics_512,
        proof_bytes_768,
        public_signal,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // A tiny bincode-2 (standard varint) encoder — the inverse of `Reader` above — used only to
    // build small, self-contained synthetic proof files for exercising one specific decode-time
    // refusal at a time, without depending on a real fixture's byte layout.
    fn enc_varint(buf: &mut Vec<u8>, v: u64) {
        if v <= 250 {
            buf.push(v as u8);
        } else if v <= u16::MAX as u64 {
            buf.push(251);
            buf.extend_from_slice(&(v as u16).to_le_bytes());
        } else if v <= u32::MAX as u64 {
            buf.push(252);
            buf.extend_from_slice(&(v as u32).to_le_bytes());
        } else {
            buf.push(253);
            buf.extend_from_slice(&v.to_le_bytes());
        }
    }
    fn enc_vec_u8(buf: &mut Vec<u8>, bytes: &[u8]) {
        enc_varint(buf, bytes.len() as u64);
        buf.extend_from_slice(bytes);
    }
    fn enc_vec_u64(buf: &mut Vec<u8>, words: &[u64]) {
        enc_varint(buf, words.len() as u64);
        for w in words {
            enc_varint(buf, *w);
        }
    }
    fn enc_string(buf: &mut Vec<u8>, s: &str) {
        enc_vec_u8(buf, s.as_bytes());
    }

    /// A synthetic `Proof::Plonk` file: every field the decoder skips is left empty/zero, so only
    /// `proof_bytes`, `publics_full`, `rootc` and `program_vk` need to vary per test. Defaults to
    /// a shape that decodes cleanly (flag=1, `publics_full[..4] == program_vk`).
    struct SyntheticProof {
        tag: u64,
        proof_bytes: Vec<u8>,
        publics_full: Vec<u64>,
        rootc: Vec<u64>,
        program_vk: Vec<u64>,
    }
    impl Default for SyntheticProof {
        fn default() -> Self {
            let mut publics_full = vec![1u64]; // vadcop_final flag
            publics_full.extend([7u64; 4]); // program_vk word, doubly encoded
            publics_full.extend([0u64; 64]); // public outputs
            SyntheticProof {
                tag: 1,
                proof_bytes: vec![0u8; PROOF_BYTES_LEN],
                publics_full,
                rootc: vec![7, 7, 7, 7],
                program_vk: vec![7, 7, 7, 7],
            }
        }
    }
    impl SyntheticProof {
        fn encode(&self) -> Vec<u8> {
            let mut b = Vec::new();
            enc_varint(&mut b, self.tag);
            enc_vec_u8(&mut b, &self.proof_bytes);
            enc_vec_u64(&mut b, &[]); // vadcop_vk
            enc_string(&mut b, ""); // protocol
            enc_string(&mut b, ""); // curve
            enc_varint(&mut b, 0); // n_public
            enc_varint(&mut b, 0); // power
            enc_string(&mut b, ""); // k1
            enc_string(&mut b, ""); // k2
            for _ in 0..8 {
                for _ in 0..3 {
                    enc_string(&mut b, ""); // Qm..S3
                }
            }
            for _ in 0..3 {
                for _ in 0..2 {
                    enc_string(&mut b, ""); // X_2
                }
            }
            enc_string(&mut b, ""); // w
            enc_vec_u8(&mut b, &[]); // publics_data
            enc_vec_u64(&mut b, &self.publics_full);
            enc_vec_u64(&mut b, &self.rootc);
            enc_vec_u64(&mut b, &self.program_vk);
            enc_varint(&mut b, 0); // hash_mode
            b
        }
    }

    #[test]
    fn a_synthetic_proof_with_the_default_shape_decodes_clean() {
        from_zisk_proof_file(&SyntheticProof::default().encode()).expect("decode");
    }

    #[test]
    fn proof_bytes_of_the_wrong_length_is_malformed_by_name() {
        let sp = SyntheticProof {
            proof_bytes: vec![0u8; 5],
            ..SyntheticProof::default()
        };
        assert_eq!(
            from_zisk_proof_file(&sp.encode()).unwrap_err(),
            CalldataError::Malformed("proof_bytes must be 768 bytes")
        );
    }

    #[test]
    fn publics_full_of_the_wrong_word_count_is_malformed_by_name() {
        let sp = SyntheticProof {
            publics_full: vec![7u64; 10], // neither 69 (with flag) nor 68 (without)
            ..SyntheticProof::default()
        };
        assert_eq!(
            from_zisk_proof_file(&sp.encode()).unwrap_err(),
            CalldataError::Malformed("publics_full must be 68 words after any flag strip")
        );
    }

    #[test]
    fn rootc_of_the_wrong_word_count_is_malformed_by_name() {
        let sp = SyntheticProof {
            rootc: vec![7u64; 3],
            ..SyntheticProof::default()
        };
        assert_eq!(
            from_zisk_proof_file(&sp.encode()).unwrap_err(),
            CalldataError::Malformed("rootc must be 4 words")
        );
    }

    #[test]
    fn program_vk_of_the_wrong_word_count_is_malformed_by_name() {
        let sp = SyntheticProof {
            program_vk: vec![7u64; 3],
            ..SyntheticProof::default()
        };
        // `publics_full[..4]` still has 4 words (`[7,7,7,7]`) but `program_vk` itself does not —
        // the word-count check fires before the two are ever compared.
        assert_eq!(
            from_zisk_proof_file(&sp.encode()).unwrap_err(),
            CalldataError::Malformed("program_vk must be 4 words")
        );
    }

    #[test]
    fn a_vadcop_flag_word_other_than_one_is_refused_by_name() {
        let mut sp = SyntheticProof::default();
        sp.publics_full[0] = 2;
        assert_eq!(
            from_zisk_proof_file(&sp.encode()).unwrap_err(),
            CalldataError::BadVadcopFlag(2)
        );
    }

    #[test]
    fn a_raw_varint_tag_byte_of_254_is_malformed_by_name() {
        // bincode 2's standard varint reserves 254/255 for a u128 tail, never expected on this
        // wire — a single raw byte is enough, the reader errors before reading anything else.
        assert_eq!(
            from_zisk_proof_file(&[254u8]).unwrap_err(),
            CalldataError::Malformed("u128 varint not expected on this wire")
        );
    }

    #[test]
    fn a_raw_varint_tag_byte_of_255_is_malformed_by_name() {
        assert_eq!(
            from_zisk_proof_file(&[255u8]).unwrap_err(),
            CalldataError::Malformed("u128 varint not expected on this wire")
        );
    }

    fn fixture_bytes() -> Vec<u8> {
        std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/s10/block14.zisk.bin"
        ))
        .expect("fixtures/s10/block14.zisk.bin")
    }

    fn fixture_json() -> serde_json::Value {
        let s = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/s10/block14.calldata.json"
        ))
        .expect("fixtures/s10/block14.calldata.json");
        serde_json::from_str(&s).unwrap()
    }

    fn h(v: &serde_json::Value, key: &str) -> Vec<u8> {
        hex::decode(v[key].as_str().unwrap().trim_start_matches("0x")).unwrap()
    }

    #[test]
    fn block14_decodes_byte_for_byte_against_the_python_reference() {
        let cd = from_zisk_proof_file(&fixture_bytes()).expect("decode");
        let j = fixture_json();
        assert_eq!(cd.program_vk.to_vec(), h(&j, "programVK"));
        assert_eq!(cd.root_c.to_vec(), h(&j, "rootCVadcopFinal"));
        assert_eq!(cd.publics_512.to_vec(), h(&j, "publicValues"));
        assert_eq!(cd.proof_bytes_768.to_vec(), h(&j, "proofBytes"));
        assert_eq!(cd.public_signal.to_vec(), h(&j, "publicSignal"));
    }

    #[test]
    fn block14_abi_verifies_on_the_host() {
        let cd = from_zisk_proof_file(&fixture_bytes()).expect("decode");
        let abi = zk_settlement_client::layout1_proof_abi(
            &cd.proof_bytes_768,
            &cd.program_vk,
            &cd.root_c,
            &cd.publics_512,
        )
        .expect("assemble abi");
        assert!(veritas::verify_zisk(&abi).expect("verify_zisk"));
    }

    #[test]
    fn truncated_by_one_byte_is_trailing_or_short() {
        let mut b = fixture_bytes();
        b.pop();
        assert_eq!(
            from_zisk_proof_file(&b).unwrap_err(),
            CalldataError::TrailingOrShort
        );
    }

    #[test]
    fn one_extra_trailing_byte_is_trailing_or_short() {
        let mut b = fixture_bytes();
        b.push(0);
        assert_eq!(
            from_zisk_proof_file(&b).unwrap_err(),
            CalldataError::TrailingOrShort
        );
    }

    #[test]
    fn tag_zero_is_not_plonk() {
        let mut b = fixture_bytes();
        b[0] = 0; // the varint-encoded enum tag is the first byte, 1 (Plonk) -> 0
        assert_eq!(
            from_zisk_proof_file(&b).unwrap_err(),
            CalldataError::NotPlonk(0)
        );
    }

    #[test]
    fn program_vk_mismatch_is_refused_by_name() {
        // Flip a byte inside the trailing `program_vk` vec<u64> (the last 4 words = last 32
        // bytes before hash_mode's one trailing varint byte) without touching `publics_full`'s
        // own copy of the same value earlier in the file — the two must then disagree.
        let mut b = fixture_bytes();
        let n = b.len();
        b[n - 2] ^= 0xff;
        assert_eq!(
            from_zisk_proof_file(&b).unwrap_err(),
            CalldataError::ProgramVkMismatch
        );
    }

    fn gate_proof_calldata() -> Calldata {
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/prover-input/txv1-dev-reset6-batch-1.plonk.bin"
        ))
        .expect("fixtures/prover-input/txv1-dev-reset6-batch-1.plonk.bin");
        from_zisk_proof_file(&bytes).expect("decode gate proof")
    }

    fn tiber_vkey_of_record() -> crate::config::VkeyOfRecord {
        crate::config::VkeyOfRecord::load(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/vkeys/tiber-200101-layout1.json"
        )))
        .expect("load vkey of record")
    }

    #[test]
    fn the_real_gate_proof_checks_clean_against_the_vkey_of_record() {
        let cd = gate_proof_calldata();
        let record = tiber_vkey_of_record();
        check_against_record(&cd, &record).expect("the real gate proof must match its own record");
    }

    #[test]
    fn a_program_vk_not_of_record_is_refused_by_name() {
        // Flip one byte of the real gate proof's own `program_vk` — the shape a proof made
        // against a different (unregistered) ELF would decode to.
        let mut cd = gate_proof_calldata();
        cd.program_vk[0] ^= 0xff;
        let record = tiber_vkey_of_record();
        let err = check_against_record(&cd, &record).unwrap_err();
        assert!(
            matches!(err, CalldataError::ProgramVkNotOfRecord { .. }),
            "expected ProgramVkNotOfRecord, got {err:?}"
        );
    }

    #[test]
    fn a_root_c_not_of_record_is_refused_by_name() {
        let mut cd = gate_proof_calldata();
        cd.root_c[0] ^= 0xff;
        let record = tiber_vkey_of_record();
        let err = check_against_record(&cd, &record).unwrap_err();
        assert!(
            matches!(err, CalldataError::RootCNotOfRecord { .. }),
            "expected RootCNotOfRecord, got {err:?}"
        );
    }
}
