//! The verifying keys of ZisK's final PLONK wrapping circuit, one per ZisK release, and the curve and
//! circuit constants every release shares (specification sections 2 and 3). The ZisK 1.3.1 key is read from
//! ZisK's own generated `PlonkVerifier.sol`. The 1.2.0 key is here too, but only behind the
//! `zisk-1-2-0-test-key` feature, so a production build holds one key. The shared constants are identical in
//! both releases' files.

/// Parses a hex string (no prefix) into bytes, at compile time.
pub(crate) const fn hex<const N: usize>(s: &str) -> [u8; N] {
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

/// A verifying key: the eight preprocessed commitments of the wrapping circuit, in transcript order
/// (Q_M, Q_L, Q_R, Q_O, Q_C, S_SIGMA_1, S_SIGMA_2, S_SIGMA_3), each as x then y. Each ZisK release has its own;
/// everything else the check needs is shared.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct VerifyingKey {
    points: [[u8; 64]; 8],
}

impl VerifyingKey {
    const fn new(points: [[u8; 64]; 8]) -> Self {
        Self { points }
    }

    /// The eight commitments in transcript order, concatenated (512 bytes).
    pub fn commitments(&self) -> &[u8] {
        self.points.as_flattened()
    }

    /// Preprocessed commitment Q_M.
    pub fn q_m(&self) -> &[u8; 64] {
        &self.points[0]
    }
    /// Preprocessed commitment Q_L.
    pub fn q_l(&self) -> &[u8; 64] {
        &self.points[1]
    }
    /// Preprocessed commitment Q_R.
    pub fn q_r(&self) -> &[u8; 64] {
        &self.points[2]
    }
    /// Preprocessed commitment Q_O.
    pub fn q_o(&self) -> &[u8; 64] {
        &self.points[3]
    }
    /// Preprocessed commitment Q_C.
    pub fn q_c(&self) -> &[u8; 64] {
        &self.points[4]
    }
    /// Preprocessed commitment S_SIGMA_1.
    pub fn s_sigma_1(&self) -> &[u8; 64] {
        &self.points[5]
    }
    /// Preprocessed commitment S_SIGMA_2.
    pub fn s_sigma_2(&self) -> &[u8; 64] {
        &self.points[6]
    }
    /// Preprocessed commitment S_SIGMA_3.
    pub fn s_sigma_3(&self) -> &[u8; 64] {
        &self.points[7]
    }
}

/// ZisK 1.3.1-alpha. Read from `zisk-contracts/PlonkVerifier.sol` at tag `v1.3.1-alpha` (commit
/// 306a9c934ba4947b1d586d69b67120f8b4c41466) of github.com/0xPolygonHermez/zisk (constants `Qm`..`Qc`
/// and the three permutation commitments, as x then y).
pub const ZISK_1_3_1: VerifyingKey = VerifyingKey::new([
    point(
        "2fb9d4c02a015e4d7c14abe7c1faa60ac0ef45b14b2cb8edb053d0826e2c677f",
        "1b3c4980acc8efeb08b386fa2308b4eacdc6674954b6b9bfe87b2b963554aa3f",
    ),
    point(
        "2277f4f6c46e091f4b68ad774ab21c0be952722991f80fbb2f48d1b8aab7e3b0",
        "0067e5c439b3f69d469fc4accab6a82f75c0ef50aae82094f5b8bb5ef8740be0",
    ),
    point(
        "170bc30c451980ba21ee28135269731f638d38ea86754ae8131bbda3d0ea19d5",
        "0d04f8f99e606184dd8a8bed31966de2e48a0840e3dcb4cede848fcee55c2e1e",
    ),
    point(
        "263b4044f350eacfce294970fffb4eba0d444b1da4a27a244fa315ed4fd65d39",
        "07fadff935715b5830e547a58f0cb33f0a0812768db4792bf74862502dfe4bf0",
    ),
    point(
        "24e46654f99635938c9f582fa38eed3b554eb9e5f511df2be70731ee5dcb3ed3",
        "2b1a06f93784f9cfdaf53ec3903c595a9b225cfda29790d77a7a1c448c6b1825",
    ),
    point(
        "0e4a106b40853bdecddc4b6d2c6ab233654cc68bc2f98d76cf69dc70d20c3772",
        "29a4e5ccc3c69d926190143899255690d7128e0f5de0cb85b0782e2cf1361405",
    ),
    point(
        "06dc0dca223cd77b301f6ba08d2c4919d09ca1991897f187da92f9ec8f41a4a0",
        "135b11ac44d148560133a1e25d817583f7e90a66cecd6be61eda948f57321dc9",
    ),
    point(
        "0690d7152cafccfada807e7c9e7d1b2b8d6926785ba5f84f5973400c70afbf62",
        "1709d5a30e3602845f2166681d8d826c1f8042d5aa47186396a5fe71d8b7efa1",
    ),
]);

/// ZisK 1.2.0-alpha, for the tests only (the `zisk-1-2-0-test-key` feature): it lets them check that a proof
/// from one release is refused under the other release's key. A production build does not contain it. Read
/// from `zisk-contracts/PlonkVerifier.sol` at tag `v1.2.0-alpha` of github.com/0xPolygonHermez/zisk
/// (the selector constants `Qm`..`Qc` and the three permutation commitments, as x then y).
#[cfg(feature = "zisk-1-2-0-test-key")]
pub const ZISK_1_2_0: VerifyingKey = VerifyingKey::new([
    point(
        "047c92a41536fa812a76457d19dd94ad523007b22346e6701cc2dc6f711348ca",
        "1781132da6f070f9708633ab341ce0bab8cc9b1f672069a7b32b230026d30dbc",
    ),
    point(
        "13f4a362c5710a0403e30925cb3908a9ecac1594ad58d293413b43c439410821",
        "0af7d45f22f9ebb6b02857b9a23a4a530fbba1309ec6629c965be43ea8f421b4",
    ),
    point(
        "2604db3f4336c260f50a82ef56fab026342865fd6c8025c7b224e8773e161ac9",
        "26aa67c1fe566b1bf7f4b179a14ed8213e311ac3313cb1f26b142e61966fbea0",
    ),
    point(
        "228f543d1172489c8732e91135fe716b94bb2d0f2b6368e8b276120da6959780",
        "13e82ae8c25ae47d204b7ce6199e0af6b0c9dfb1c614d04366f1afb853c72ee7",
    ),
    point(
        "0cc69639705634e113e35dd446f35933fe11915d2a56c4396907215d4a760927",
        "23a835a5519cf9ec143cc6590a555f2517f702d2efda232d3d7108c181ec707a",
    ),
    point(
        "2af7f94e012e6f040610afbd51844d213bd6fe2d0df4da2cf5f007c42e34e0ce",
        "1e3dd5262d857374b363db3c5bfbb75bc894c5a1b21755fe619dbea23de36321",
    ),
    point(
        "1d6c79c6ae6ddd2423266113ad9a3062655828385dbffd7c62fb4d6d421e0c22",
        "0df62717de93425eedcf9dc0eebac3985522d97808288f58e9b6dfcf06454a9e",
    ),
    point(
        "24442f4568ba563356ba4bc9bfb9629f818ba4e2c33fb979f675c4cf68750cbe",
        "1e5578b05ab2e25e08e4f98932e52fd4f4c41fbe0490e3567b3eefa67fb8ebda",
    ),
]);

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
