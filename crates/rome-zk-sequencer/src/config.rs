//! Config: a TOML file with env overrides. The sequencer signing key is never in the repo or in this
//! config file itself — only a **path** to it, resolved at startup; tests generate a random key instead.

use alloy::signers::local::PrivateKeySigner;
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::admission::AdmissionConfig;
use crate::profile::{Profile, ProfileError};

#[derive(Debug, Clone, Deserialize)]
pub struct AdmissionSettings {
    pub queue_capacity: usize,
    pub queue_timeout_secs: u64,
    pub park_expiry_secs: u64,
    pub max_tx_size: usize,
    #[serde(default = "default_max_parked_per_sender")]
    pub max_parked_per_sender: usize,
    #[serde(default = "default_max_next_nonce_entries")]
    pub max_next_nonce_entries: usize,
}

fn default_max_parked_per_sender() -> usize {
    AdmissionConfig::default().max_parked_per_sender
}

fn default_max_next_nonce_entries() -> usize {
    AdmissionConfig::default().max_next_nonce_entries
}

impl Default for AdmissionSettings {
    fn default() -> Self {
        let d = AdmissionConfig::default();
        Self {
            queue_capacity: d.queue_capacity,
            queue_timeout_secs: d.queue_timeout.as_secs(),
            park_expiry_secs: d.park_expiry.as_secs(),
            max_tx_size: d.max_tx_size,
            max_parked_per_sender: d.max_parked_per_sender,
            max_next_nonce_entries: d.max_next_nonce_entries,
        }
    }
}

impl AdmissionSettings {
    pub fn to_admission_config(&self, chain_id: u64) -> AdmissionConfig {
        AdmissionConfig {
            chain_id,
            queue_capacity: self.queue_capacity,
            queue_timeout: Duration::from_secs(self.queue_timeout_secs),
            park_expiry: Duration::from_secs(self.park_expiry_secs),
            max_tx_size: self.max_tx_size,
            max_parked_per_sender: self.max_parked_per_sender,
            max_next_nonce_entries: self.max_next_nonce_entries,
        }
    }
}

/// RPC hardening knobs — mirrors [`crate::rpc::RpcConfig`], kept as its
/// own type here so `config.rs` doesn't need to depend on `rpc.rs`'s jsonrpsee-flavored types.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct RpcSettings {
    #[serde(default = "default_max_request_body_size")]
    pub max_request_body_size: u32,
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
    #[serde(default = "default_max_subscriptions_per_connection")]
    pub max_subscriptions_per_connection: u32,
}

fn default_max_request_body_size() -> u32 {
    crate::rpc::RpcConfig::default().max_request_body_size
}
fn default_max_connections() -> u32 {
    crate::rpc::RpcConfig::default().max_connections
}
fn default_max_subscriptions_per_connection() -> u32 {
    crate::rpc::RpcConfig::default().max_subscriptions_per_connection
}

impl Default for RpcSettings {
    fn default() -> Self {
        let d = crate::rpc::RpcConfig::default();
        Self {
            max_request_body_size: d.max_request_body_size,
            max_connections: d.max_connections,
            max_subscriptions_per_connection: d.max_subscriptions_per_connection,
        }
    }
}

impl RpcSettings {
    pub fn to_rpc_config(&self) -> crate::rpc::RpcConfig {
        crate::rpc::RpcConfig {
            max_request_body_size: self.max_request_body_size,
            max_connections: self.max_connections,
            max_subscriptions_per_connection: self.max_subscriptions_per_connection,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub chain_id: u64,
    pub rpc_addr: SocketAddr,
    pub metrics_addr: SocketAddr,
    pub log_dir: PathBuf,
    #[serde(default = "default_blocks_per_segment")]
    pub blocks_per_segment: u64,
    pub sequencer_key_path: PathBuf,
    #[serde(default)]
    pub admission: AdmissionSettings,
    #[serde(default)]
    pub rpc: RpcSettings,
    /// The chain's cadence + cap profile — sub-block period, sub-blocks per block,
    /// blocks per batch, gas limits, admission headroom and the DA/prover throughput budgets, declared
    /// and validated together in one place (see [`crate::profile::Profile`]). Absent `[profile]` in the
    /// TOML takes every design default, so an existing deploy that predates this table keeps behaving
    /// exactly as it did before it existed. `ROME_ZK_SEQUENCER_SUB_BLOCK_GAS_LIMIT` still overrides
    /// `profile.sub_block_gas_limit`.
    #[serde(default)]
    pub profile: Profile,
    /// Config for `--executor reth`. Optional — absent unless that executor is
    /// selected; the mock stays default (`bin/rome-zk-sequencer.rs`'s `--executor` flag).
    #[serde(default)]
    pub reth: Option<RethSettings>,
}

/// `[reth]` section: where `RethExecutor` keeps its MDBX store and which genesis JSON it was bred
/// from (shaped like the operator's genesis template). Kept as plain data here (no dependency on
/// `rome-zk-executor-reth`, which is behind the optional `reth` cargo feature) so this crate's
/// config parsing never pulls reth's dependency graph in on its own.
#[derive(Debug, Clone, Deserialize)]
pub struct RethSettings {
    pub datadir: PathBuf,
    pub genesis_path: PathBuf,
    /// `--executor reth` serves RPC via `node::serve` UNCONDITIONALLY — this
    /// crate's own `rpc::serve` is the mock executor's path only, never reth's (see the binary's
    /// `reth_main`, which no longer branches). `http_addr`/`ws_addr` are still independently
    /// optional, each falling back to `Config::rpc_addr` when absent (`RethSettings::
    /// resolved_rpc_addrs`) — Tiber's `[reth]` section sets both explicitly (8545/8546, matching the
    /// stock `reth` container it replaces); a `[reth]`
    /// section that omits them (or omits the whole table's addresses) serves both `eth_*` and
    /// `rome_*`/the preconf feed on the ONE `rpc_addr` socket instead — reth's own `RpcServerConfig`
    /// combines http+ws onto one listener when both addresses match (`crates/rpc/rpc-builder/src/
    /// lib.rs`'s `start`, "If both are configured on the same port, we combine them into one
    /// server"), which `node::serve` gives the identical module set on both transports to satisfy
    /// (`ensure_ws_http_identical`).
    #[serde(default)]
    pub http_addr: Option<SocketAddr>,
    #[serde(default)]
    pub ws_addr: Option<SocketAddr>,
}

impl RethSettings {
    /// `node::serve`'s actual bind addresses — `http_addr`/`ws_addr` when set
    /// (Tiber's explicit 8545/8546), each independently falling back to `fallback` (`Config::rpc_addr`)
    /// otherwise. Both falling back yields the SAME address for both — the one-port case `node::serve`
    /// hands reth's own same-port combining.
    pub fn resolved_rpc_addrs(&self, fallback: SocketAddr) -> (SocketAddr, SocketAddr) {
        (
            self.http_addr.unwrap_or(fallback),
            self.ws_addr.unwrap_or(fallback),
        )
    }
}

fn default_blocks_per_segment() -> u64 {
    1_000
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("read config file {path:?}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parse config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid env override {var}={value:?}: {reason}")]
    EnvOverride {
        var: &'static str,
        value: String,
        reason: String,
    },
    #[error("read sequencer key {path:?}: {source}")]
    ReadKey {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("decode sequencer key {path:?}: {reason}")]
    DecodeKey { path: PathBuf, reason: String },
    /// The `[profile]` table failed cross-field validation (whole-seconds block time,
    /// consistent block_gas_limit, or the DA/prover throughput budgets) — see [`ProfileError`] for the
    /// exact bound each variant names.
    #[error("invalid [profile]: {0}")]
    InvalidProfile(#[from] ProfileError),
    /// `sub_block_gas_limit`/`block_gas_limit` used to be top-level
    /// `Config` fields; they now live in `[profile]`. A config file still carrying one at
    /// the old top-level location is not an error `toml`/`serde` catches on its own (an unrecognized
    /// field is silently dropped, not rejected) — this crate refuses to start on one instead, naming the
    /// stray key, rather than silently running with `[profile]`'s defaults while an operator believes
    /// their top-level override still applies.
    #[error(
        "config carries a legacy top-level `{key}` key — this moved into the [profile] table; \
         move it to [profile].{key} — refusing to start rather than silently ignore it"
    )]
    LegacyTopLevelProfileKey { key: &'static str },
}

impl Config {
    /// Load from a TOML file, then apply env overrides
    /// (`ROME_ZK_SEQUENCER_CHAIN_ID`, `ROME_ZK_SEQUENCER_RPC_ADDR`, `ROME_ZK_SEQUENCER_METRICS_ADDR`,
    /// `ROME_ZK_SEQUENCER_LOG_DIR`, `ROME_ZK_SEQUENCER_KEY_PATH`, `ROME_ZK_SEQUENCER_SUB_BLOCK_GAS_LIMIT`)
    /// — each optional, each replacing the file's value when present.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::reject_legacy_top_level_profile_keys(&text)?;
        let mut config: Config = toml::from_str(&text)?;
        config.apply_overrides_from(|k| std::env::var(k).ok())?;
        // Validate the profile after env overrides so an override that pushes gas/s
        // past the declared budgets is caught too, not just the file's own value.
        config.profile.validate()?;
        Ok(config)
    }

    /// `Config` no longer has `sub_block_gas_limit`/`block_gas_limit`
    /// fields (both moved into `[profile]`), so `toml::from_str::<Config>` silently drops
    /// either one at the TOP level of the file as an unrecognized field — it does not error, and does
    /// not warn. Parsed as a generic table (not `Config`) so this check runs whether or not the rest of
    /// the file happens to parse, and inspects only the file's OWN top level: a `[profile]`-nested
    /// `sub_block_gas_limit`/`block_gas_limit` (the correct, current location) is untouched.
    fn reject_legacy_top_level_profile_keys(text: &str) -> Result<(), ConfigError> {
        let raw: toml::Value = toml::from_str(text)?;
        let Some(table) = raw.as_table() else {
            return Ok(());
        };
        for key in ["sub_block_gas_limit", "block_gas_limit"] {
            if table.contains_key(key) {
                return Err(ConfigError::LegacyTopLevelProfileKey { key });
            }
        }
        Ok(())
    }

    /// Env-override logic, parameterized over the env lookup so it is testable without mutating the
    /// real process environment (`std::env::set_var` is `unsafe` and this crate forbids unsafe code).
    fn apply_overrides_from(
        &mut self,
        get: impl Fn(&str) -> Option<String>,
    ) -> Result<(), ConfigError> {
        // Overrides `profile.sub_block_gas_limit` (the standalone field was consolidated into the
        // `[profile]` table).
        if let Some(v) = get("ROME_ZK_SEQUENCER_SUB_BLOCK_GAS_LIMIT") {
            self.profile.sub_block_gas_limit = v.parse().map_err(|e| ConfigError::EnvOverride {
                var: "ROME_ZK_SEQUENCER_SUB_BLOCK_GAS_LIMIT",
                value: v.clone(),
                reason: format!("{e}"),
            })?;
        }
        if let Some(v) = get("ROME_ZK_SEQUENCER_CHAIN_ID") {
            self.chain_id = v.parse().map_err(|e| ConfigError::EnvOverride {
                var: "ROME_ZK_SEQUENCER_CHAIN_ID",
                value: v.clone(),
                reason: format!("{e}"),
            })?;
        }
        if let Some(v) = get("ROME_ZK_SEQUENCER_RPC_ADDR") {
            self.rpc_addr = v.parse().map_err(|e| ConfigError::EnvOverride {
                var: "ROME_ZK_SEQUENCER_RPC_ADDR",
                value: v.clone(),
                reason: format!("{e}"),
            })?;
        }
        if let Some(v) = get("ROME_ZK_SEQUENCER_METRICS_ADDR") {
            self.metrics_addr = v.parse().map_err(|e| ConfigError::EnvOverride {
                var: "ROME_ZK_SEQUENCER_METRICS_ADDR",
                value: v.clone(),
                reason: format!("{e}"),
            })?;
        }
        if let Some(v) = get("ROME_ZK_SEQUENCER_LOG_DIR") {
            self.log_dir = PathBuf::from(v);
        }
        if let Some(v) = get("ROME_ZK_SEQUENCER_KEY_PATH") {
            self.sequencer_key_path = PathBuf::from(v);
        }
        Ok(())
    }

    /// Load the sequencer's secp256k1 signing key from `sequencer_key_path`: 64 hex characters (32
    /// bytes), optionally `0x`-prefixed, optionally trailing-newline. Never a default/embedded key.
    pub fn load_signer(&self) -> Result<PrivateKeySigner, ConfigError> {
        let raw = std::fs::read_to_string(&self.sequencer_key_path).map_err(|source| {
            ConfigError::ReadKey {
                path: self.sequencer_key_path.clone(),
                source,
            }
        })?;
        let hex_str = raw.trim().trim_start_matches("0x");
        let bytes = hex::decode(hex_str).map_err(|e| ConfigError::DecodeKey {
            path: self.sequencer_key_path.clone(),
            reason: e.to_string(),
        })?;
        PrivateKeySigner::from_slice(&bytes).map_err(|e| ConfigError::DecodeKey {
            path: self.sequencer_key_path.clone(),
            reason: e.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    /// `config.example.toml` documents every surfaced knob —
    /// `admission.max_parked_per_sender`, `admission.max_next_nonce_entries`, `rpc.*`, and
    /// `sub_block_gas_limit` — that this crate's config accepts but that the earlier example (only the
    /// `Config::load` test fixtures above) never showed. It must parse to real, non-default values for
    /// the fields it deliberately overrides, so a stale or malformed example is caught here rather than
    /// only when an operator copies it and it silently falls back to defaults.
    #[test]
    fn example_config_parses_with_every_documented_knob() {
        let example = include_str!("../config.example.toml");
        let config: Config = toml::from_str(example).expect("config.example.toml must parse");

        assert_eq!(config.chain_id, 200_101);
        // The shipped example (and therefore Tiber, rendered from this exact
        // file) declares the `always` provisioning point — 40,000,000 gas/s — not the bare code
        // default (`DEFAULT_SUB_BLOCK_GAS_LIMIT`/`DEFAULT_BLOCK_GAS_LIMIT`, the 100M gas/s executor
        // ceiling a config that omits [profile] entirely would still get).
        assert_eq!(config.profile.sub_block_gas_limit, 2_000_000);
        assert_eq!(config.profile.effective_block_gas_limit(), 40_000_000);
        assert_eq!(config.profile.prover_gas_per_sec, 40_000_000);
        assert_eq!(config.profile.da_bytes_per_sec, 140_952);
        // Tiber's own profile raises blocks_per_batch from the crate default (10,
        // rome_zk_profile::DEFAULT_BLOCKS_PER_BATCH, unchanged) to 60 — one root post every 60 s at the
        // defaults above, so an idle-provisioned chain posts 6x fewer batches.
        // This is a per-profile knob, not a crate-default change.
        assert_eq!(config.profile.blocks_per_batch, 60);
        // Tiber's own default stays 0 — never seal a block with no transactions.
        assert_eq!(config.profile.empty_block_interval_secs, 0);
        // The example's [profile] table must itself be internally consistent.
        config.profile.validate().unwrap();

        // The admission knobs, which the example did not previously show.
        assert_eq!(config.admission.max_parked_per_sender, 16);
        assert_eq!(config.admission.max_next_nonce_entries, 100_000);

        // The RPC hardening knobs, also not previously shown.
        assert_eq!(config.rpc.max_request_body_size, 1024 * 1024);
        assert_eq!(config.rpc.max_connections, 10_000);
        assert_eq!(config.rpc.max_subscriptions_per_connection, 8);

        // The [reth] section, only read by `--executor reth`.
        let reth = config.reth.expect("[reth] section must parse");
        assert_eq!(
            reth.genesis_path,
            PathBuf::from("/etc/rome-zk-sequencer/genesis.json")
        );
        assert_eq!(reth.http_addr, Some("0.0.0.0:8545".parse().unwrap()));
        assert_eq!(reth.ws_addr, Some("0.0.0.0:8546".parse().unwrap()));
    }

    /// `[reth].http_addr`/`ws_addr` are optional — a `[reth]`
    /// section written before they existed (datadir + genesis_path only) must still parse, defaulting
    /// both to `None` — `resolved_rpc_addrs` is what turns that into the actual one-port fallback (see
    /// `RethSettings::http_addr`'s doc): `node::serve` runs either way now, never `rpc::serve`.
    #[test]
    fn reth_section_without_node_rpc_addrs_defaults_to_none() {
        let dir = tempdir().unwrap();
        let key_path = write(dir.path(), "key.hex", &hex::encode([7u8; 32]));
        let toml = format!(
            r#"
            chain_id = 1
            rpc_addr = "127.0.0.1:9944"
            metrics_addr = "127.0.0.1:9945"
            log_dir = "{}"
            sequencer_key_path = "{}"

            [reth]
            datadir = "/tmp/reth"
            genesis_path = "/tmp/genesis.json"
            "#,
            dir.path().join("log").display(),
            key_path.display(),
        );
        let config_path = write(dir.path(), "config.toml", &toml);
        let config = Config::load(&config_path).unwrap();
        let reth = config.reth.expect("[reth] section must parse");
        assert_eq!(reth.http_addr, None);
        assert_eq!(reth.ws_addr, None);
        assert_eq!(
            reth.resolved_rpc_addrs(config.rpc_addr),
            (config.rpc_addr, config.rpc_addr),
            "both addresses absent must fall back to the SAME rpc_addr socket"
        );
    }

    #[test]
    fn loads_toml_and_defaults_admission() {
        let dir = tempdir().unwrap();
        let key_path = write(dir.path(), "key.hex", &hex::encode([7u8; 32]));
        let toml = format!(
            r#"
            chain_id = 200101
            rpc_addr = "127.0.0.1:9944"
            metrics_addr = "127.0.0.1:9945"
            log_dir = "{}"
            sequencer_key_path = "{}"
            "#,
            dir.path().join("log").display(),
            key_path.display(),
        );
        let config_path = write(dir.path(), "config.toml", &toml);
        let config = Config::load(&config_path).unwrap();
        assert_eq!(config.chain_id, 200_101);
        assert_eq!(config.blocks_per_segment, 1_000);
        assert_eq!(
            config.admission.queue_capacity,
            AdmissionConfig::default().queue_capacity
        );
        // Absent from the TOML, defaults to the design's per-chain gas budget (via the [profile] table).
        assert_eq!(
            config.profile.sub_block_gas_limit,
            crate::executor::DEFAULT_SUB_BLOCK_GAS_LIMIT
        );
        // Absent from the TOML, defaults to RpcConfig::default().
        assert_eq!(
            config.rpc.max_connections,
            crate::rpc::RpcConfig::default().max_connections
        );
    }

    /// `ROME_ZK_SEQUENCER_SUB_BLOCK_GAS_LIMIT` overrides the TOML value
    /// (or its default), and the binary reads it through to `SpawnConfig`.
    #[test]
    fn sub_block_gas_limit_env_override_replaces_default() {
        let dir = tempdir().unwrap();
        let key_path = write(dir.path(), "key.hex", &hex::encode([7u8; 32]));
        let toml = format!(
            r#"
            chain_id = 1
            rpc_addr = "127.0.0.1:9944"
            metrics_addr = "127.0.0.1:9945"
            log_dir = "{}"
            sequencer_key_path = "{}"
            "#,
            dir.path().join("log").display(),
            key_path.display(),
        );
        let config_path = write(dir.path(), "config.toml", &toml);
        let text = std::fs::read_to_string(&config_path).unwrap();
        let mut config: Config = toml::from_str(&text).unwrap();
        assert_eq!(
            config.profile.sub_block_gas_limit,
            crate::executor::DEFAULT_SUB_BLOCK_GAS_LIMIT
        );
        config
            .apply_overrides_from(|k| {
                (k == "ROME_ZK_SEQUENCER_SUB_BLOCK_GAS_LIMIT").then(|| "1234567".to_string())
            })
            .unwrap();
        assert_eq!(config.profile.sub_block_gas_limit, 1_234_567);
    }

    #[test]
    fn env_override_replaces_file_value() {
        let dir = tempdir().unwrap();
        let key_path = write(dir.path(), "key.hex", &hex::encode([7u8; 32]));
        let toml = format!(
            r#"
            chain_id = 1
            rpc_addr = "127.0.0.1:9944"
            metrics_addr = "127.0.0.1:9945"
            log_dir = "{}"
            sequencer_key_path = "{}"
            "#,
            dir.path().join("log").display(),
            key_path.display(),
        );
        let config_path = write(dir.path(), "config.toml", &toml);
        let text = std::fs::read_to_string(&config_path).unwrap();
        let mut config: Config = toml::from_str(&text).unwrap();
        config
            .apply_overrides_from(|k| {
                (k == "ROME_ZK_SEQUENCER_CHAIN_ID").then(|| "200101".to_string())
            })
            .unwrap();
        assert_eq!(config.chain_id, 200_101);
    }

    #[test]
    fn load_signer_decodes_hex_key() {
        let dir = tempdir().unwrap();
        let key_path = write(dir.path(), "key.hex", &hex::encode([9u8; 32]));
        let toml = format!(
            r#"
            chain_id = 1
            rpc_addr = "127.0.0.1:9944"
            metrics_addr = "127.0.0.1:9945"
            log_dir = "{}"
            sequencer_key_path = "{}"
            "#,
            dir.path().join("log").display(),
            key_path.display(),
        );
        let config_path = write(dir.path(), "config.toml", &toml);
        let config = Config::load(&config_path).unwrap();
        let signer = config.load_signer().unwrap();
        assert_eq!(signer.to_bytes().as_slice(), &[9u8; 32]);
    }

    /// A config that still carries the old top-level
    /// `sub_block_gas_limit` key (moved into `[profile]`) must be refused at load time, naming the
    /// stray key — not silently start up on `[profile]`'s defaults as if the override were honored.
    #[test]
    fn legacy_top_level_sub_block_gas_limit_key_is_refused_not_silently_ignored() {
        let dir = tempdir().unwrap();
        let key_path = write(dir.path(), "key.hex", &hex::encode([7u8; 32]));
        let toml = format!(
            r#"
            chain_id = 1
            rpc_addr = "127.0.0.1:9944"
            metrics_addr = "127.0.0.1:9945"
            log_dir = "{}"
            sequencer_key_path = "{}"
            sub_block_gas_limit = 1234567
            "#,
            dir.path().join("log").display(),
            key_path.display(),
        );
        let config_path = write(dir.path(), "config.toml", &toml);
        let err = Config::load(&config_path).unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::LegacyTopLevelProfileKey {
                    key: "sub_block_gas_limit"
                }
            ),
            "expected LegacyTopLevelProfileKey{{key: \"sub_block_gas_limit\"}}, got {err:?}"
        );
    }

    /// Same as above for the other field that moved: a legacy top-level `block_gas_limit`.
    #[test]
    fn legacy_top_level_block_gas_limit_key_is_refused_not_silently_ignored() {
        let dir = tempdir().unwrap();
        let key_path = write(dir.path(), "key.hex", &hex::encode([7u8; 32]));
        let toml = format!(
            r#"
            chain_id = 1
            rpc_addr = "127.0.0.1:9944"
            metrics_addr = "127.0.0.1:9945"
            log_dir = "{}"
            sequencer_key_path = "{}"
            block_gas_limit = 50000000
            "#,
            dir.path().join("log").display(),
            key_path.display(),
        );
        let config_path = write(dir.path(), "config.toml", &toml);
        let err = Config::load(&config_path).unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::LegacyTopLevelProfileKey {
                    key: "block_gas_limit"
                }
            ),
            "expected LegacyTopLevelProfileKey{{key: \"block_gas_limit\"}}, got {err:?}"
        );
    }

    /// The CURRENT, correct location — `[profile].sub_block_gas_limit` — must NOT trip the legacy-key
    /// guard; only a stray TOP-LEVEL key is rejected.
    #[test]
    fn sub_block_gas_limit_nested_under_profile_is_not_a_legacy_key() {
        let dir = tempdir().unwrap();
        let key_path = write(dir.path(), "key.hex", &hex::encode([7u8; 32]));
        let toml = format!(
            r#"
            chain_id = 1
            rpc_addr = "127.0.0.1:9944"
            metrics_addr = "127.0.0.1:9945"
            log_dir = "{}"
            sequencer_key_path = "{}"

            [profile]
            sub_block_gas_limit = 1234567
            "#,
            dir.path().join("log").display(),
            key_path.display(),
        );
        let config_path = write(dir.path(), "config.toml", &toml);
        let config = Config::load(&config_path).unwrap();
        assert_eq!(config.profile.sub_block_gas_limit, 1_234_567);
    }
}
