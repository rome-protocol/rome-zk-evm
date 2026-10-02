//! Chain config loading. A `ZISK_GENESIS_PATH` custom-chain patch on `crates/clients/reth/input/src/lib.rs` might be
//! expected; checked directly against the fork at the pinned tree (v0.12.0, branch `rome-guest`'s own base), it is not
//! present there: `fetch_chain_config` in that file only recognizes four named chains
//! (`Mainnet`/`Sepolia`/`Hoodi`/`Holesky`) and refuses any other chain id — which Tiber (200101) is. This module reads
//! the chain's genesis file directly as a standard `alloy_genesis::Genesis` document (the same shape
//! `fetch_chain_config` ultimately returns a `ChainConfig` from for its four known chains) rather than relying on an
//! env-var override this crate's source does not have.

use std::path::Path;

use alloy_genesis::{ChainConfig, Genesis};

#[derive(Debug, thiserror::Error)]
pub enum GenesisError {
    #[error("read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("parse {path} as a genesis document: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_json::Error,
    },
}

/// Loads `.config` (the `ChainConfig`) out of a standard genesis JSON file.
pub fn load_chain_config(path: &Path) -> Result<ChainConfig, GenesisError> {
    let raw = std::fs::read_to_string(path).map_err(|source| GenesisError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let genesis: Genesis = serde_json::from_str(&raw).map_err(|source| GenesisError::Parse {
        path: path.display().to_string(),
        source,
    })?;
    Ok(genesis.config)
}

/// sha256 of the genesis file's own raw bytes — hex-encoded, hashed the identical way
/// `guest-rome`'s `build.rs` hashes the SAME embedded genesis file, so a sidecar's
/// `provenance.genesis_sha256` and the ELF's own build-warning hash are directly comparable (the fact,
/// not a re-derivation of it: both hash the file's raw bytes, never the parsed `ChainConfig`).
pub fn genesis_sha256(path: &Path) -> Result<String, GenesisError> {
    use sha2::{Digest, Sha256};
    let raw = std::fs::read(path).map_err(|source| GenesisError::Read {
        path: path.display().to_string(),
        source,
    })?;
    Ok(hex::encode(Sha256::digest(&raw)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_a_missing_file_by_name() {
        let err = load_chain_config(std::path::Path::new("/nonexistent/genesis.json")).unwrap_err();
        assert!(matches!(err, GenesisError::Read { .. }));
    }

    #[test]
    fn refuses_malformed_json_by_name() {
        // A per-process, per-test-run path (never a fixed shared name): a persistent self-hosted runner is not a
        // fresh container per job, so a fixed name under the system temp dir can be left behind by an earlier run
        // under a different user/permission set (`Os { code: 13, kind: PermissionDenied }` writing to a stale
        // `rome-zk-prover-input-test-genesis` dir), which a unique-per-run name makes structurally impossible.
        let dir = std::env::temp_dir().join(format!(
            "rome-zk-prover-input-test-genesis-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.json");
        std::fs::write(&path, b"not json").unwrap();
        let err = load_chain_config(&path).unwrap_err();
        assert!(matches!(err, GenesisError::Parse { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
