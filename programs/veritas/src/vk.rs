//! The verifying key of ZisK's final PLONK wrapping circuit (specification section 3), and the
//! curve constants of section 2. Every value is copied from the specification's tables.

/// Parses a hex string (no prefix) into bytes, at compile time.
const fn hex<const N: usize>(s: &str) -> [u8; N] {
    let b = s.as_bytes();
    assert!(b.len() == 2 * N);
    let mut out = [0u8; N];
    let mut i = 0;
    while i < N {
        out[i] = (nibble(b[2 * i]) << 4) | nibble(b[2 * i + 1]);
        i += 1;
    }
    out
}

const fn nibble(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        _ => panic!("bad hex digit"),
    }
}

const fn point(x: &str, y: &str) -> [u8; 64] {
    let x = hex::<32>(x);
    let y = hex::<32>(y);
    let mut out = [0u8; 64];
    let mut i = 0;
    while i < 32 {
        out[i] = x[i];
        out[32 + i] = y[i];
        i += 1;
    }
    out
}

const fn concat(points: &[[u8; 64]; 8]) -> [u8; 512] {
    let mut out = [0u8; 512];
    let mut i = 0;
    while i < 8 {
        let mut j = 0;
        while j < 64 {
            out[64 * i + j] = points[i][j];
            j += 1;
        }
        i += 1;
    }
    out
}

/// Base field modulus p, big-endian.
pub const P: [u8; 32] = hex("30644e72e131a029b85045b68181585d97816a916871ca8d3c208c16d87cfd47");
/// Scalar field modulus r, big-endian.
pub const R: [u8; 32] = hex("30644e72e131a029b85045b68181585d2833e84879b9709143e1f593f0000001");

/// log2 of the domain size: n = 2^24.
pub const POWER: u32 = 24;
/// The domain generator omega, big-endian.
pub const OMEGA: [u8; 32] = hex("0c9fabc7845d50d2852e2a0371c6441f145e0db82e8326961c25f1e3e32b045b");
/// Coset shifts k1 and k2.
pub const K1: u64 = 2;
pub const K2: u64 = 3;

/// Preprocessed commitment Q_M (x then y, big-endian).
pub const Q_M: [u8; 64] = point(
    "047c92a41536fa812a76457d19dd94ad523007b22346e6701cc2dc6f711348ca",
    "1781132da6f070f9708633ab341ce0bab8cc9b1f672069a7b32b230026d30dbc",
);

/// Preprocessed commitment Q_L (x then y, big-endian).
pub const Q_L: [u8; 64] = point(
    "13f4a362c5710a0403e30925cb3908a9ecac1594ad58d293413b43c439410821",
    "0af7d45f22f9ebb6b02857b9a23a4a530fbba1309ec6629c965be43ea8f421b4",
);

/// Preprocessed commitment Q_R (x then y, big-endian).
pub const Q_R: [u8; 64] = point(
    "2604db3f4336c260f50a82ef56fab026342865fd6c8025c7b224e8773e161ac9",
    "26aa67c1fe566b1bf7f4b179a14ed8213e311ac3313cb1f26b142e61966fbea0",
);

/// Preprocessed commitment Q_O (x then y, big-endian).
pub const Q_O: [u8; 64] = point(
    "228f543d1172489c8732e91135fe716b94bb2d0f2b6368e8b276120da6959780",
    "13e82ae8c25ae47d204b7ce6199e0af6b0c9dfb1c614d04366f1afb853c72ee7",
);

/// Preprocessed commitment Q_C (x then y, big-endian).
pub const Q_C: [u8; 64] = point(
    "0cc69639705634e113e35dd446f35933fe11915d2a56c4396907215d4a760927",
    "23a835a5519cf9ec143cc6590a555f2517f702d2efda232d3d7108c181ec707a",
);

/// Preprocessed commitment S_SIGMA_1 (x then y, big-endian).
pub const S_SIGMA_1: [u8; 64] = point(
    "2af7f94e012e6f040610afbd51844d213bd6fe2d0df4da2cf5f007c42e34e0ce",
    "1e3dd5262d857374b363db3c5bfbb75bc894c5a1b21755fe619dbea23de36321",
);

/// Preprocessed commitment S_SIGMA_2 (x then y, big-endian).
pub const S_SIGMA_2: [u8; 64] = point(
    "1d6c79c6ae6ddd2423266113ad9a3062655828385dbffd7c62fb4d6d421e0c22",
    "0df62717de93425eedcf9dc0eebac3985522d97808288f58e9b6dfcf06454a9e",
);

/// Preprocessed commitment S_SIGMA_3 (x then y, big-endian).
pub const S_SIGMA_3: [u8; 64] = point(
    "24442f4568ba563356ba4bc9bfb9629f818ba4e2c33fb979f675c4cf68750cbe",
    "1e5578b05ab2e25e08e4f98932e52fd4f4c41fbe0490e3567b3eefa67fb8ebda",
);

/// The eight commitments in transcript order, concatenated (512 bytes).
pub const COMMITMENTS: [u8; 512] =
    concat(&[Q_M, Q_L, Q_R, Q_O, Q_C, S_SIGMA_1, S_SIGMA_2, S_SIGMA_3]);

/// [1]_1, the G1 generator (1, 2).
pub const G1_GENERATOR: [u8; 64] = {
    let mut g = [0u8; 64];
    g[31] = 1;
    g[63] = 2;
    g
};

/// [x]_2 in the 128-byte syscall encoding x1 || x0 || y1 || y0.
pub const X_G2: [u8; 128] = {
    let x1 = hex::<32>("26186a2d65ee4d2f9c9a5b91f86597d35f192cd120caf7e935d8443d1938e23d");
    let x0 = hex::<32>("30441fd1b5d3370482c42152a8899027716989a6996c2535bc9f7fee8aaef79e");
    let y1 = hex::<32>("1970ea81dd6992adfbc571effb03503adbbb6a857f578403c6c40e22d65b3c02");
    let y0 = hex::<32>("054793348f12c0cf5622c340573cb277586319de359ab9389778f689786b1e48");
    let mut out = [0u8; 128];
    let mut i = 0;
    while i < 32 {
        out[i] = x1[i];
        out[32 + i] = x0[i];
        out[64 + i] = y1[i];
        out[96 + i] = y0[i];
        i += 1;
    }
    out
};

/// [1]_2 in the 128-byte syscall encoding x1 || x0 || y1 || y0.
pub const ONE_G2: [u8; 128] = {
    let x0 = hex::<32>("1800deef121f1e76426a00665e5c4479674322d4f75edadd46debd5cd992f6ed");
    let x1 = hex::<32>("198e9393920d483a7260bfb731fb5d25f1aa493335a9e71297e485b7aef312c2");
    let y0 = hex::<32>("12c85ea5db8c6deb4aab71808dcb408fe3d1e7690c43d37b4ce6cc0166fa7daa");
    let y1 = hex::<32>("090689d0585ff075ec9e99ad690c3395bc4b313370b38ef355acdadcd122975b");
    let mut out = [0u8; 128];
    let mut i = 0;
    while i < 32 {
        out[i] = x1[i];
        out[32 + i] = x0[i];
        out[64 + i] = y1[i];
        out[96 + i] = y0[i];
        i += 1;
    }
    out
};
