//! The ZisK releases Veritas knows. Each row names a release by the registry's scheme byte and carries what
//! the check needs from that release: its wrapper key and the recursion root it must be proved against.
//! The table is compiled in, so a row changes only with a program upgrade.

use crate::vk::{self, VerifyingKey};

/// Where a release stands. Open takes new registry entries and its proofs verify. Closing takes no new
/// entries and its existing entries still verify. Withdrawn is refused everywhere except retirement, and
/// its key is left out of a production build.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Open,
    Closing,
    Withdrawn,
}

/// One ZisK release.
#[derive(Clone, Copy, Debug)]
pub struct ZiskVersion {
    /// The registry's scheme byte for this release. Numbers are never reused.
    pub scheme: u8,
    /// The release's name, as ZisK tags it.
    pub name: &'static str,
    pub status: Status,
    /// The wrapper key. `None` for a withdrawn release, except in a build with the test feature.
    pub key: Option<VerifyingKey>,
    /// The `rootCVadcopFinal` every proof of this release carries, read from the release's own
    /// `ZiskVerifier.getRootCVadcopFinal()`.
    pub root_c: [u8; 32],
}

/// The scheme byte of ZisK 1.2.0-alpha.
pub const SCHEME_ZISK_1_2_0: u8 = 1;
/// The scheme byte of ZisK 1.3.1-alpha.
pub const SCHEME_ZISK_1_3_1: u8 = 2;

/// Every release, in scheme order. Withdrawn rows stay, under their name, so a number is never reused.
pub static ZISK_VERSIONS: [ZiskVersion; 2] = [
    ZiskVersion {
        scheme: SCHEME_ZISK_1_2_0,
        name: "1.2.0-alpha",
        status: Status::Withdrawn,
        #[cfg(feature = "zisk-1-2-0-test-key")]
        key: Some(vk::ZISK_1_2_0),
        #[cfg(not(feature = "zisk-1-2-0-test-key"))]
        key: None,
        root_c: vk::hex("564c2b1bcbd5932c81cfad1fa786a98372eb3d6495257c2d944544334f84382f"),
    },
    ZiskVersion {
        scheme: SCHEME_ZISK_1_3_1,
        name: "1.3.1-alpha",
        status: Status::Open,
        key: Some(vk::ZISK_1_3_1),
        root_c: vk::hex("c3f12b9f8707c6a1e96df2bf6702c2ebdfbafedabeac654644a380befe091ac4"),
    },
];

/// The release a scheme byte names, or `None` for a byte no release has. A withdrawn release is returned
/// too: the caller decides what to say about it.
pub fn zisk_version(scheme: u8) -> Option<&'static ZiskVersion> {
    ZISK_VERSIONS.iter().find(|v| v.scheme == scheme)
}
