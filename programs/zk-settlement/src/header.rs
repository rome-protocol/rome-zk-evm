//! Pure, account-free parsing shared by the legacy single-block verify paths (`UpdateRoot`,
//! `UpdateRootZisk`) and the new batch-level ZisK PLONK path (`PostRootProved`, `settle.rs`): SP1/rsp's
//! bincode public-values decode, minimal RLP header decode, and the ZisK guest's committed-hash
//! extraction. No account or instruction-dispatch logic lives here — everything here is a `&[u8] ->
//! Result<T, ProgramError>` function, unit-testable without a program-test harness.

use solana_program::program_error::ProgramError;

/// rsp-client public values = bincode(CommittedHeader{ header: alloy serde_bincode_compat::Header }).
/// For a post-Shanghai/Cancun/Prague block every Option is Some, so the layout before `extra_data` is
/// fixed. Offsets measured on a block-3 proof (797 B) and guarded by the length prefixes they imply.
pub struct ProvedHeader {
    pub parent_hash: [u8; 32],
    pub state_root: [u8; 32],
    pub number: u64,
}
pub fn parse_public_values(pv: &[u8]) -> Result<ProvedHeader, ProgramError> {
    let bad = ProgramError::InvalidInstructionData;
    if pv.len() < 605 {
        return Err(bad);
    }
    let u64at = |o: usize| u64::from_le_bytes(pv[o..o + 8].try_into().unwrap());
    if u64at(0) != 32
        || u64at(40) != 32
        || u64at(80) != 20
        || u64at(108) != 32
        || u64at(148) != 32
        || u64at(188) != 32
    {
        return Err(bad);
    }
    if pv[228] != 1 || u64at(229) != 32 || u64at(269) != 256 || u64at(533) != 32 {
        return Err(bad);
    }
    Ok(ProvedHeader {
        parent_hash: pv[8..40].try_into().unwrap(),
        state_root: pv[116..148].try_into().unwrap(),
        number: u64at(573),
    })
}

/// Minimal RLP: decode a list of byte-string items (block header). Returns items as slices.
pub fn rlp_list_items(b: &[u8]) -> Result<Vec<&[u8]>, ProgramError> {
    const BAD: ProgramError = ProgramError::InvalidInstructionData;
    let bad = || BAD;
    let (mut i, end) = match b.first().copied().ok_or_else(bad)? {
        p if p >= 0xf8 => {
            let l = (p - 0xf7) as usize;
            let n = be_len(b.get(1..1 + l).ok_or_else(bad)?);
            (1 + l, 1 + l + n)
        }
        p if p >= 0xc0 => (1, 1 + (p - 0xc0) as usize),
        _ => return Err(BAD),
    };
    if end != b.len() {
        return Err(BAD);
    }
    let mut out = vec![];
    while i < end {
        let p = b[i];
        let (hs, n) = match p {
            0..=0x7f => (0usize, 1usize),
            0x80..=0xb7 => (1, (p - 0x80) as usize),
            0xb8..=0xbf => {
                let l = (p - 0xb7) as usize;
                (1 + l, be_len(b.get(i + 1..i + 1 + l).ok_or_else(bad)?))
            }
            _ => return Err(BAD),
        };
        let s = i + hs;
        let e = s + n;
        if e > end {
            return Err(BAD);
        }
        out.push(&b[s..e]);
        i = e;
    }
    Ok(out)
}
fn be_len(b: &[u8]) -> usize {
    b.iter().fold(0usize, |a, &x| (a << 8) | x as usize)
}
pub struct ParsedHeader {
    pub parent_hash: [u8; 32],
    pub state_root: [u8; 32],
    pub number: u64,
    /// The RLP field at index 10, zero-based (`gasUsed`). The variable (bps) fee component of `PostRootProved`
    /// is bound to this proof-bound field rather than the poster's self-declared `gas_in_batch`.
    pub gas_used: u64,
}
pub fn parse_header(rlp: &[u8]) -> Result<ParsedHeader, ProgramError> {
    let it = rlp_list_items(rlp)?;
    if it.len() < 15
        || it[0].len() != 32
        || it[3].len() != 32
        || it[8].len() > 8
        || it[10].len() > 8
    {
        return Err(ProgramError::InvalidInstructionData);
    }
    Ok(ParsedHeader {
        parent_hash: it[0].try_into().unwrap(),
        state_root: it[3].try_into().unwrap(),
        number: be_len(it[8]) as u64,
        gas_used: be_len(it[10]) as u64,
    })
}

/// ZisK publicValues are 64 little-endian u64 words each holding one u32 of the guest's output buffer.
/// The guest commits the block hash with `ziskos::io::commit(&B256)` = bincode: a 1-byte length prefix
/// (0x20) followed by the 32 hash bytes, so the hash occupies output bytes [1..33] (measured on chain
/// 200100 block 1 against reth's block hash).
pub fn zisk_committed_hash(public_values: &[u8]) -> Result<[u8; 32], ProgramError> {
    let bad = ProgramError::InvalidInstructionData;
    if public_values.len() != 512 {
        return Err(bad);
    }
    let mut buf = [0u8; 36];
    for i in 0..9 {
        let w = u64::from_le_bytes(public_values[i * 8..i * 8 + 8].try_into().unwrap());
        if w > u32::MAX as u64 {
            return Err(bad);
        }
        buf[i * 4..i * 4 + 4].copy_from_slice(&(w as u32).to_le_bytes());
    }
    if buf[0] != 0x20 {
        return Err(bad);
    }
    Ok(buf[1..33].try_into().unwrap())
}

/// The full public-values struct lives in `rome_zk_layouts::public_values` (v2, 208 B).
/// `PostRootProved` binds it under registry layout 1 (`settle::bind_layout1_public_values`); the
/// header-only fallback (`registry::LAYOUT_HEADER_FALLBACK`, layout 2) still goes through
/// [`parse_header`]/[`zisk_committed_hash`] above. The batch guest commits layout 1.
#[cfg(test)]
mod tests {
    use super::*;

    fn block3_pv() -> Vec<u8> {
        // Borsh SP1Groth16Proof{proof, pv} from the block-3 artifact (chain 200099)
        let b = base64_decode(include_str!("../../../fixtures/s2-block3.groth16.ix.b64").trim());
        let n = u32::from_le_bytes(b[0..4].try_into().unwrap()) as usize;
        let m = u32::from_le_bytes(b[4 + n..8 + n].try_into().unwrap()) as usize;
        b[8 + n..8 + n + m].to_vec()
    }
    fn base64_decode(s: &str) -> Vec<u8> {
        // tiny decoder to avoid a dev-dep in an sbf crate
        let t: Vec<u8> = s.bytes().filter(|c| !c.is_ascii_whitespace()).collect();
        let val = |c: u8| -> u32 {
            match c {
                b'A'..=b'Z' => (c - b'A') as u32,
                b'a'..=b'z' => (c - b'a' + 26) as u32,
                b'0'..=b'9' => (c - b'0' + 52) as u32,
                b'+' => 62,
                b'/' => 63,
                _ => 0,
            }
        };
        let mut out = vec![];
        for ch in t.chunks(4) {
            let pad = ch.iter().filter(|&&c| c == b'=').count();
            let v = ch.iter().fold(0u32, |a, &c| (a << 6) | val(c));
            out.push((v >> 16) as u8);
            if pad < 2 {
                out.push((v >> 8) as u8);
            }
            if pad < 1 {
                out.push(v as u8);
            }
        }
        out
    }
    #[test]
    fn parses_block3_header() {
        let h = parse_public_values(&block3_pv()).unwrap();
        assert_eq!(h.number, 3);
        assert_eq!(
            hex(&h.parent_hash),
            "28968a531e25bc9074c19488b90a509eadfdf45e11c81a41bdce42b2505a2c1a"
        );
        assert_eq!(
            hex(&h.state_root),
            "918dcb4f2477bda9a77af4591d5ca341c0bfb5322065bce54e1b6ff0ed1bc700"
        );
    }
    #[test]
    fn rejects_short_or_misframed_pv() {
        assert!(parse_public_values(&block3_pv()[..600]).is_err());
        let mut pv = block3_pv();
        pv[228] = 0;
        assert!(parse_public_values(&pv).is_err());
    }
    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn header_rlp_roundtrip_and_hash() {
        use alloy_consensus::Header;
        use alloy_primitives::{B256, U256};
        let h = Header {
            number: 14,
            parent_hash: B256::repeat_byte(0x11),
            state_root: B256::repeat_byte(0x22),
            gas_limit: 30_000_000,
            gas_used: 21_000_777,
            timestamp: 1_700_000_000,
            base_fee_per_gas: Some(7),
            difficulty: U256::ZERO,
            ..Default::default()
        };
        let rlp = alloy_rlp::encode(&h);
        let p = parse_header(&rlp).unwrap();
        assert_eq!(p.number, 14);
        assert_eq!(p.parent_hash, [0x11u8; 32]);
        assert_eq!(p.state_root, [0x22u8; 32]);
        assert_eq!(p.gas_used, 21_000_777, "RLP field 10 is gasUsed");
        assert_eq!(
            solana_program::keccak::hash(&rlp).to_bytes(),
            h.hash_slow().0
        );
        let mut bad = rlp.clone();
        bad.pop();
        assert!(parse_header(&bad).is_err());
    }
    #[test]
    fn zisk_publics_hash_extraction_shape() {
        let j: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(format!(
                "{}/../../fixtures/s10/block14.calldata.json",
                env!("CARGO_MANIFEST_DIR")
            ))
            .unwrap(),
        )
        .unwrap();
        let pv =
            ::hex::decode(j["publicValues"].as_str().unwrap().trim_start_matches("0x")).unwrap();
        let h = zisk_committed_hash(&pv).unwrap();
        assert_eq!(
            hex(&h),
            "f8e4b298f68a5573da91285ae03a72e985cf6c50729400589a498b08df55f1fd"
        ); // block-14 hash (0x20 bincode prefix stripped; layout verified against reth on chain 200100 block 1)
        assert!(zisk_committed_hash(&pv[..500]).is_err());
    }

    // The full public-values struct's golden/length tests live in
    // `rome_zk_layouts::public_values`; this crate no longer owns that decoder.
}
