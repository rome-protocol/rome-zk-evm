//! A genesis.json read the way the node reads it: its chain id, the hash and state root of the genesis block the
//! node builds from it (through reth's own chain spec, the one the sequencer runs), and the sha256 of the file's
//! bytes (the hash the guest build embeds and reports).

use crate::error::OpsError;
use sha2::{Digest, Sha256};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenesisInfo {
    pub chain_id: u64,
    /// Hash of block 0, as the node computes it.
    pub block_hash: [u8; 32],
    /// State root of block 0.
    pub state_root: [u8; 32],
    /// sha256 of the file's raw bytes, lowercase hex.
    pub sha256: String,
}

/// The exit portal's address in every rollup genesis.
pub const EXIT_PORTAL_ADDRESS: &str = "0x4200000000000000000000000000000000000016";

/// The published exit portal runtime, the bytes every rollup genesis puts at [`EXIT_PORTAL_ADDRESS`].
pub const EXIT_PORTAL_RUNTIME_HEX: &str =
    include_str!("../../../contracts/exit-portal/RomeExitPortal.runtime.hex");

fn portal_runtime() -> Vec<u8> {
    let hex = EXIT_PORTAL_RUNTIME_HEX.trim();
    hex::decode(hex.strip_prefix("0x").unwrap_or(hex)).expect("the published runtime is hex")
}

/// What the rollup template renders, and nothing else: the exit portal with exactly the published runtime, no
/// storage and no balance, and no code or storage at any other address (the template's only other entry is the one
/// optional backed account, which has a balance and nothing more).
pub fn check_alloc(path: &Path) -> Result<(), OpsError> {
    let raw = std::fs::read(path).map_err(|e| {
        OpsError::usage(
            "GenesisUnreadable",
            format!("cannot read {}: {e}", path.display()),
        )
    })?;
    let genesis: alloy_genesis::Genesis = serde_json::from_slice(&raw).map_err(|e| {
        OpsError::usage(
            "GenesisInvalid",
            format!("{} is not a genesis file: {e}", path.display()),
        )
    })?;
    let is_portal =
        |a: &dyn std::fmt::Display| a.to_string().eq_ignore_ascii_case(EXIT_PORTAL_ADDRESS);
    let not_canonical = |why: String| {
        OpsError::usage(
            "PortalNotCanonical",
            format!(
                "{}: the exit portal {EXIT_PORTAL_ADDRESS} {why}",
                path.display()
            ),
        )
    };
    let Some((_, account)) = genesis.alloc.iter().find(|(a, _)| is_portal(*a)) else {
        return Err(not_canonical("is not in the genesis alloc".into()));
    };
    if account.code.as_deref().map(|c| c.to_vec()) != Some(portal_runtime()) {
        return Err(not_canonical(
            "does not hold exactly the published runtime".into(),
        ));
    }
    if account.storage.as_ref().is_some_and(|s| !s.is_empty()) {
        return Err(not_canonical("has storage".into()));
    }
    if !account.balance.is_zero() {
        return Err(not_canonical("has a balance".into()));
    }
    for (address, account) in &genesis.alloc {
        if is_portal(address) {
            continue;
        }
        let has_code = account.code.as_ref().is_some_and(|c| !c.is_empty());
        let has_storage = account.storage.as_ref().is_some_and(|s| !s.is_empty());
        if has_code || has_storage {
            return Err(OpsError::usage(
                "GenesisAllocUnexpected",
                format!(
                    "{}: the genesis puts {} at {address}; a rollup genesis has code only at the exit portal and no storage anywhere",
                    path.display(),
                    if has_code { "code" } else { "storage" }
                ),
            ));
        }
    }
    Ok(())
}

pub fn load(path: &Path) -> Result<GenesisInfo, OpsError> {
    let raw = std::fs::read(path).map_err(|e| {
        OpsError::usage(
            "GenesisUnreadable",
            format!("cannot read {}: {e}", path.display()),
        )
    })?;
    let genesis: alloy_genesis::Genesis = serde_json::from_slice(&raw).map_err(|e| {
        OpsError::usage(
            "GenesisInvalid",
            format!("{} is not a genesis file: {e}", path.display()),
        )
    })?;
    let chain_id = genesis.config.chain_id;
    let spec = reth_chainspec::ChainSpec::from_genesis(genesis);
    Ok(GenesisInfo {
        chain_id,
        block_hash: spec.genesis_hash().0,
        state_root: spec.genesis_header().state_root.0,
        sha256: hex::encode(Sha256::digest(&raw)),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A small genesis for `chain_id` with one funded account; `balance` changes the state root.
    pub fn genesis_json(chain_id: u64, balance: &str) -> String {
        format!(
            r#"{{"config":{{"chainId":{chain_id},"homesteadBlock":0,"eip150Block":0,"eip155Block":0,"eip158Block":0,"byzantiumBlock":0,"constantinopleBlock":0,"petersburgBlock":0,"istanbulBlock":0,"berlinBlock":0,"londonBlock":0,"terminalTotalDifficulty":0}},"nonce":"0x0","timestamp":"0x0","extraData":"0x","gasLimit":"0x1c9c380","difficulty":"0x0","mixHash":"0x0000000000000000000000000000000000000000000000000000000000000000","coinbase":"0x0000000000000000000000000000000000000000","alloc":{{"{EXIT_PORTAL_ADDRESS}":{{"balance":"0x0","code":"0x{}"}},"0x1111111111111111111111111111111111111111":{{"balance":"{balance}"}}}},"baseFeePerGas":"0x7"}}"#,
            EXIT_PORTAL_RUNTIME_HEX.trim().trim_start_matches("0x")
        )
    }

    pub fn write_genesis(tag: &str, chain_id: u64, balance: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "rome-zk-ops-genesis-{}-{tag}.json",
            std::process::id()
        ));
        std::fs::write(&p, genesis_json(chain_id, balance)).unwrap();
        p
    }

    #[test]
    fn reads_the_chain_id_and_a_stable_block() {
        let p = write_genesis("gen-a", 4_295_391_538, "0x0");
        let a = load(&p).unwrap();
        let b = load(&p).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.chain_id, 4_295_391_538);
        assert_eq!(a.sha256.len(), 64);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn a_different_alloc_gives_a_different_state_root_and_hash() {
        let p1 = write_genesis("gen-b1", 77, "0x0");
        let p2 = write_genesis("gen-b2", 77, "0x5");
        let (a, b) = (load(&p1).unwrap(), load(&p2).unwrap());
        assert_ne!(a.state_root, b.state_root);
        assert_ne!(a.block_hash, b.block_hash);
        assert_ne!(a.sha256, b.sha256);
        std::fs::remove_file(&p1).ok();
        std::fs::remove_file(&p2).ok();
    }

    fn alloc_file(tag: &str, edit: impl FnOnce(&mut serde_json::Value)) -> std::path::PathBuf {
        let mut v: serde_json::Value = serde_json::from_str(&genesis_json(77, "0x5")).unwrap();
        edit(&mut v["alloc"]);
        let p = std::env::temp_dir().join(format!(
            "rome-zk-ops-genesis-{}-{tag}.json",
            std::process::id()
        ));
        std::fs::write(&p, v.to_string()).unwrap();
        p
    }

    fn alloc_refusal(tag: &str, edit: impl FnOnce(&mut serde_json::Value)) -> OpsError {
        let p = alloc_file(tag, edit);
        let r = check_alloc(&p);
        std::fs::remove_file(&p).ok();
        r.unwrap_err()
    }

    #[test]
    fn the_rendered_shape_passes_the_alloc_check() {
        // The portal as the template renders it, plus one backed account with a balance and nothing else.
        let p = alloc_file("alloc-ok", |_| {});
        assert_eq!(check_alloc(&p), Ok(()));
        std::fs::remove_file(&p).ok();
        // Empty storage and empty code on another account are nothing.
        let p = alloc_file("alloc-ok2", |a| {
            a["0x1111111111111111111111111111111111111111"]["storage"] = serde_json::json!({});
            a["0x1111111111111111111111111111111111111111"]["code"] = "0x".into();
        });
        assert_eq!(check_alloc(&p), Ok(()));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn what_the_rollup_template_renders_passes_the_alloc_check() {
        // The template as `rollup init` fills it: the portal predeploy, with and without the one backed account.
        let template = include_str!("../../../deploy/rollup/genesis.json.template");
        for (tag, backed) in [
            ("tpl-plain", ""),
            (
                "tpl-backed",
                r#", "0x1111111111111111111111111111111111111111": { "balance": "0x14d1120d7b160000" }"#,
            ),
        ] {
            let rendered = template
                .replace("__CHAIN_ID__", "77")
                .replace("__GAS_LIMIT_HEX__", "0x1c9c380")
                .replace(
                    "__FEE_RECIPIENT__",
                    "0x2222222222222222222222222222222222222222",
                )
                .replace(
                    "__EXIT_PORTAL_RUNTIME__",
                    EXIT_PORTAL_RUNTIME_HEX.trim().trim_start_matches("0x"),
                )
                .replace("__BACKED_ALLOC__", backed);
            let p = std::env::temp_dir().join(format!(
                "rome-zk-ops-genesis-{}-{tag}.json",
                std::process::id()
            ));
            std::fs::write(&p, rendered).unwrap();
            let r = check_alloc(&p);
            std::fs::remove_file(&p).ok();
            assert_eq!(r, Ok(()), "{tag}");
        }
    }

    #[test]
    fn a_portal_that_is_not_the_published_one_is_refused() {
        let portal = EXIT_PORTAL_ADDRESS;
        type Edit = Box<dyn FnOnce(&mut serde_json::Value)>;
        let cases: Vec<(&str, Edit)> = vec![
            (
                "p-absent",
                Box::new(move |a| {
                    a.as_object_mut().unwrap().remove(portal);
                }),
            ),
            (
                "p-nocode",
                Box::new(move |a| {
                    a[portal].as_object_mut().unwrap().remove("code");
                }),
            ),
            (
                "p-othercode",
                Box::new(move |a| {
                    a[portal]["code"] = "0x6000".into();
                }),
            ),
            (
                "p-extra",
                Box::new(move |a| {
                    let c = a[portal]["code"].as_str().unwrap().to_string();
                    a[portal]["code"] = format!("{c}00").into();
                }),
            ),
            (
                "p-storage",
                Box::new(move |a| {
                    a[portal]["storage"] = serde_json::json!({
                        "0x0000000000000000000000000000000000000000000000000000000000000001":
                        "0x0000000000000000000000000000000000000000000000000000000000000001"
                    });
                }),
            ),
            (
                "p-balance",
                Box::new(move |a| {
                    a[portal]["balance"] = "0x1".into();
                }),
            ),
        ];
        for (tag, edit) in cases {
            let e = alloc_refusal(tag, edit);
            assert_eq!(e.name, "PortalNotCanonical", "{tag}: {e}");
            assert_eq!(e.exit_code, 2, "{tag}");
        }
    }

    #[test]
    fn code_or_storage_at_another_address_is_refused() {
        let other = "0x1111111111111111111111111111111111111111";
        let e = alloc_refusal("o-code", |a| a[other]["code"] = "0x6000".into());
        assert_eq!(e.name, "GenesisAllocUnexpected", "{e}");
        assert!(e.detail.contains("code"), "{e}");
        let e = alloc_refusal("o-storage", |a| {
            a[other]["storage"] = serde_json::json!({
                "0x0000000000000000000000000000000000000000000000000000000000000001":
                "0x0000000000000000000000000000000000000000000000000000000000000001"
            });
        });
        assert_eq!(e.name, "GenesisAllocUnexpected", "{e}");
        assert!(e.detail.contains("storage"), "{e}");
        let e = alloc_refusal("o-new", |a| {
            a["0x2222222222222222222222222222222222222222"] =
                serde_json::json!({"balance": "0x0", "code": "0x00"});
        });
        assert_eq!(e.name, "GenesisAllocUnexpected", "{e}");
    }

    #[test]
    fn an_unreadable_or_invalid_file_is_refused_by_name() {
        assert_eq!(
            load(Path::new("/nonexistent/genesis.json"))
                .unwrap_err()
                .name,
            "GenesisUnreadable"
        );
        let p = std::env::temp_dir().join(format!(
            "rome-zk-ops-genesis-{}-bad.json",
            std::process::id()
        ));
        std::fs::write(&p, "not json").unwrap();
        assert_eq!(load(&p).unwrap_err().name, "GenesisInvalid");
        std::fs::remove_file(&p).ok();
    }
}
