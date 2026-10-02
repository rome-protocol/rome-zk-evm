//! The pure file layer for `profile.json`: read and classify whatever is on disk beside
//! a chain's ordered log, and write a fresh identity. This module knows nothing about the ordered log
//! itself, migration flags, or the numbering-origin check — those are orchestration concerns that stay
//! with each caller (`rome_zk_sequencer::recovery::reconcile_profile_identity` owns the sequencer's;
//! `rome_zk_batcher::config::read_profile_identity` owns the batcher's). This module is the one place
//! that decides what shape a `profile.json` file on disk actually is.

use std::path::{Path, PathBuf};

use crate::profile::ProfileIdentity;

/// Filename of the profile-identity file persisted beside the ordered log.
pub const PROFILE_JSON_FILENAME: &str = "profile.json";

/// The `profile.json` shape before `blocks_per_batch` was added — used only to detect and migrate
/// a file written before that field existed. A genuine parse failure on any of these fields is still a
/// hard [`ProfileJsonError::Parse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
pub struct LegacyProfileIdentityV0 {
    pub chain_id: u64,
    pub sub_block_ms: u64,
    pub sub_blocks_per_block: u16,
    pub sub_block_gas_limit: u64,
    pub block_gas_limit: u64,
}

/// The `profile.json` shape before `first_block` was added (i.e. `ProfileIdentity` once `blocks_per_batch` had
/// joined it) — used only to detect and reject a file written by a binary that used 0-based numbering. A genuine
/// parse failure on any of these fields is still a hard [`ProfileJsonError::Parse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
pub struct LegacyProfileIdentityV1 {
    pub chain_id: u64,
    pub sub_block_ms: u64,
    pub sub_blocks_per_block: u16,
    pub sub_block_gas_limit: u64,
    pub block_gas_limit: u64,
    pub blocks_per_batch: u64,
}

/// What [`read_profile_json`] found on disk, classified by shape. Every non-error outcome is a variant
/// here — a genuinely missing file is [`StoredProfileJson::Missing`], not an error, since "no
/// `profile.json` yet" is an ordinary, expected state for a fresh log directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredProfileJson {
    /// No file at `<log_dir>/profile.json` at all.
    Missing,
    /// A file predating `blocks_per_batch`: has every field `ProfileIdentity` had before `blocks_per_batch`
    /// joined it, and nothing else.
    V0(LegacyProfileIdentityV0),
    /// A file predating `first_block`: has `blocks_per_batch` but not `first_block`.
    V1(LegacyProfileIdentityV1),
    /// A file with every current field, including `first_block`.
    Current(ProfileIdentity),
}

#[derive(Debug, thiserror::Error)]
pub enum ProfileJsonError {
    #[error("{path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing {path:?}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// Read and classify `<log_dir>/profile.json`. A genuinely absent file is [`StoredProfileJson::Missing`]
/// (`Ok`, not an error) — any other read failure (permissions, a directory where a file is expected, and
/// so on) is [`ProfileJsonError::Io`]. Invalid JSON, or JSON that has `blocks_per_batch`/`first_block`
/// but fails to parse as [`ProfileIdentity`] with the fields present, is [`ProfileJsonError::Parse`].
pub fn read_profile_json(log_dir: &Path) -> Result<StoredProfileJson, ProfileJsonError> {
    let path = log_dir.join(PROFILE_JSON_FILENAME);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StoredProfileJson::Missing);
        }
        Err(source) => return Err(ProfileJsonError::Io { path, source }),
    };
    let value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|source| ProfileJsonError::Parse {
            path: path.clone(),
            source,
        })?;

    // A file written before `blocks_per_batch` joined the persisted identity has neither that field nor
    // `first_block` — classify by the OLDEST missing field first, so a V0 file is never mistaken for (the strictly
    // newer) V1 shape.
    if value.get("blocks_per_batch").is_none() {
        let legacy: LegacyProfileIdentityV0 =
            serde_json::from_value(value).map_err(|source| ProfileJsonError::Parse {
                path: path.clone(),
                source,
            })?;
        return Ok(StoredProfileJson::V0(legacy));
    }

    // `blocks_per_batch` present, `first_block` not.
    if value.get("first_block").is_none() {
        let legacy: LegacyProfileIdentityV1 =
            serde_json::from_value(value).map_err(|source| ProfileJsonError::Parse {
                path: path.clone(),
                source,
            })?;
        return Ok(StoredProfileJson::V1(legacy));
    }

    let current: ProfileIdentity =
        serde_json::from_value(value).map_err(|source| ProfileJsonError::Parse { path, source })?;
    Ok(StoredProfileJson::Current(current))
}

/// Write `identity` to `<log_dir>/profile.json`, creating `log_dir` (and any missing parents) if
/// necessary. Always writes the current, full shape — there is no "write a legacy file" entry point;
/// migration is exactly this function called with today's configured identity.
pub fn write_profile_identity(
    log_dir: &Path,
    identity: &ProfileIdentity,
) -> Result<(), ProfileJsonError> {
    std::fs::create_dir_all(log_dir).map_err(|source| ProfileJsonError::Io {
        path: log_dir.to_path_buf(),
        source,
    })?;
    let path = log_dir.join(PROFILE_JSON_FILENAME);
    let json = serde_json::to_string_pretty(identity)
        .expect("ProfileIdentity is plain data and always serializes");
    std::fs::write(&path, json).map_err(|source| ProfileJsonError::Io { path, source })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Profile;
    use tempfile::tempdir;

    fn identity(chain_id: u64) -> ProfileIdentity {
        ProfileIdentity::new(chain_id, &Profile::default())
    }

    #[test]
    fn a_missing_file_classifies_as_missing() {
        let dir = tempdir().unwrap();
        let stored = read_profile_json(dir.path()).unwrap();
        assert_eq!(stored, StoredProfileJson::Missing);
    }

    #[test]
    fn write_then_read_round_trips_as_current() {
        let dir = tempdir().unwrap();
        let written = identity(200_101);
        write_profile_identity(dir.path(), &written).unwrap();
        let stored = read_profile_json(dir.path()).unwrap();
        assert_eq!(stored, StoredProfileJson::Current(written));
    }

    #[test]
    fn write_creates_missing_parent_directories() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("a").join("b");
        assert!(!nested.exists());
        write_profile_identity(&nested, &identity(1)).unwrap();
        assert!(nested.join(PROFILE_JSON_FILENAME).exists());
    }

    /// A file with every V0 field (no `blocks_per_batch`, no `first_block`) classifies as `V0`, not
    /// `Current` and not `V1`.
    #[test]
    fn a_v0_shape_file_classifies_as_v0() {
        let dir = tempdir().unwrap();
        let json = serde_json::json!({
            "chain_id": 200_101u64,
            "sub_block_ms": 50u64,
            "sub_blocks_per_block": 20u16,
            "sub_block_gas_limit": 5_000_000u64,
            "block_gas_limit": 100_000_000u64,
        });
        std::fs::write(
            dir.path().join(PROFILE_JSON_FILENAME),
            serde_json::to_string(&json).unwrap(),
        )
        .unwrap();

        let stored = read_profile_json(dir.path()).unwrap();
        match stored {
            StoredProfileJson::V0(v0) => {
                assert_eq!(v0.chain_id, 200_101);
                assert_eq!(v0.block_gas_limit, 100_000_000);
            }
            other => panic!("expected V0, got {other:?}"),
        }
    }

    /// A file with `blocks_per_batch` but no `first_block` classifies as `V1`: a classifier bug that treats this
    /// shape as `Current` instead (e.g. `first_block` deserializing to 0 via a stray `#[serde(default)]`) turns
    /// this test red, and is exactly the bug both `rome-zk-sequencer`'s and `rome-zk-batcher`'s own "profile.json
    /// without first_block" tests exist to catch one level up (their refusal depends on this classification, not a
    /// `Current(..)` with a defaulted `first_block: 0` sneaking through).
    #[test]
    fn a_v1_shape_file_without_first_block_classifies_as_v1_not_current() {
        let dir = tempdir().unwrap();
        let json = serde_json::json!({
            "chain_id": 200_101u64,
            "sub_block_ms": 50u64,
            "sub_blocks_per_block": 20u16,
            "sub_block_gas_limit": 5_000_000u64,
            "block_gas_limit": 100_000_000u64,
            "blocks_per_batch": 10u64,
        });
        std::fs::write(
            dir.path().join(PROFILE_JSON_FILENAME),
            serde_json::to_string(&json).unwrap(),
        )
        .unwrap();

        let stored = read_profile_json(dir.path()).unwrap();
        match stored {
            StoredProfileJson::V1(v1) => {
                assert_eq!(v1.chain_id, 200_101);
                assert_eq!(v1.blocks_per_batch, 10);
            }
            other => panic!("expected V1, got {other:?}"),
        }
    }

    #[test]
    fn invalid_json_is_a_named_parse_error() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(PROFILE_JSON_FILENAME), "not json").unwrap();
        let err = read_profile_json(dir.path()).unwrap_err();
        assert!(matches!(err, ProfileJsonError::Parse { .. }), "{err:?}");
    }

    /// A path that exists but is a directory, not a file, is a genuine I/O error (`IsADirectory` on
    /// Unix) — never silently classified as `Missing`, which is reserved for the "nothing at that path
    /// at all" case.
    #[test]
    fn a_directory_where_a_file_is_expected_is_a_named_io_error_not_missing() {
        let dir = tempdir().unwrap();
        std::fs::create_dir(dir.path().join(PROFILE_JSON_FILENAME)).unwrap();
        let err = read_profile_json(dir.path()).unwrap_err();
        assert!(matches!(err, ProfileJsonError::Io { .. }), "{err:?}");
    }
}
