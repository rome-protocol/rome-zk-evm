//! TOML config for the `rome-zk-derive` binary — same shape convention as `rome-zk-sequencer`/
//! `rome-zk-batcher`'s own `config.rs` (a plain `serde`-derived struct loaded once at startup).
//!
//! The on-disk `datadir/cursor` file and `start_at_batch` are gone. Persisting a batch-id cursor to disk
//! was exactly the bug class "a restart with cursor > 0 mis-maps": nothing tied that file's contents to
//! what the engine's real chain actually had. `channel_timeout_slots`/`channel_timeout_batches` are gone
//! too — they existed only for the deleted `open_slot`-gap heuristic and had no other consumer.
//!
//! [`DEFAULT_BLOCKS_PER_BATCH`] replaces this crate's former import of
//! `rome_zk_executor_api::BLOCKS_PER_BATCH` as the `blocks_per_batch` default — that constant is a
//! *different* quantity (the fixed epoch `prev_randao` divides by; since renamed
//! `PREV_RANDAO_EPOCH_BLOCKS`) from the batch-size cap this crate and the batcher enforce. Neither this
//! crate nor the batcher depends on either executor-api name for the batch cap any more.
//!
//! [`DEFAULT_BLOCKS_PER_BATCH`] is a re-export of
//! [`rome_zk_profile::DEFAULT_BLOCKS_PER_BATCH`] — the one home the sequencer, the batcher and this
//! crate all read the design default from, rather than three independently-pinned copies of the number
//! 10.
//!
//! [`Config`] names one resume-relevant field again —
//! `settlement_program_id` — but never a batch id or a block number directly: the binary reads the
//! settlement root account under that program (`crate::resume::resume_anchor`) and verifies it against
//! the engine's own chain before trusting it as a resume point; a config value alone is never enough
//! (nothing ties a hand-set batch id to what the engine's real chain actually has, the same bug class
//! named above). `--from-batch <N>` stays the explicit, engine-verified override.

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::Deserialize;
use solana_program::pubkey::Pubkey;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub chain_id: u64,
    /// Solana RPC endpoint this node reads the inbox from — always at FINALIZED commitment.
    /// Read-only: this node builds no transaction, so no funded keypair is ever needed.
    pub solana_rpc_url: String,
    #[serde(deserialize_with = "deserialize_pubkey")]
    pub inbox_program_id: Pubkey,
    /// The settlement program this chain's root account lives under
    /// (`zk_settlement_client::root_pda`) — read once at startup to find the resume anchor
    /// (`crate::resume::resume_anchor`), never touched again after that.
    #[serde(deserialize_with = "deserialize_pubkey")]
    pub settlement_program_id: Pubkey,
    pub reth: RethEngine,
    /// The cap a batch's block count must not exceed — default
    /// [`DEFAULT_BLOCKS_PER_BATCH`], configurable per profile.
    #[serde(default = "default_blocks_per_batch")]
    pub blocks_per_batch: u64,
    #[serde(default = "default_max_open_channels")]
    pub max_open_channels: usize,
    /// The chain's own `chain_config.max_drift_secs` is the authoritative
    /// source (`crate::chain_bound::chain_drift_bound`, read at startup) — this field is no longer a
    /// value the binary wires directly. `None` (the key absent, the template's committed default) means
    /// "use whatever the chain says"; `Some(v)` is an assertion that must equal the chain's value,
    /// refused by name otherwise (`reconcile_drift_bound`, `ConfigError::DriftBoundMismatch`) — a stale
    /// or wrong TOML value can no longer silently override what the chain and the guest enforce.
    /// See `crate::batch_queue::DriftBound`.
    pub max_drift_secs: Option<u64>,
    /// Where this process serves `GET /metrics`
    /// (`rome_zk_metrics_http::serve_on`), mirroring `rome-zk-batcher::config::Config::metrics_addr`'s own
    /// shape. Env override `ROME_ZK_DERIVE_METRICS_ADDR` (`Config::load`, via `apply_env_overrides`).
    /// Tiber's own rendered value (`0.0.0.0:9003`, compose-network only) is set in its deploy config; the
    /// default here is loopback.
    #[serde(default = "default_metrics_addr")]
    pub metrics_addr: SocketAddr,
}

/// Mirrors `rome-zk-batcher::config::default_metrics_addr`'s own doc — a defaulted field
/// since existing deploys never declared this key. `9003` sits next to the batcher's
/// `9002` and the sequencer's own port, none of which collide.
fn default_metrics_addr() -> SocketAddr {
    "127.0.0.1:9003"
        .parse()
        .expect("default_metrics_addr: literal must parse")
}

/// Derive reads `max_drift_secs` from `chain_config` at start (its TOML
/// value becomes an override that must equal the chain's, refused by name otherwise) so derive, guest
/// and program enforce one number. A pure function (no I/O) so both directions are trivial to test:
/// `toml: None` (the key omitted, the deploy template's default) defers entirely to `chain`; `Some(v) ==
/// chain` is accepted (a redundant but harmless assertion); any other `Some(v)` is refused by name,
/// naming both values, rather than silently wired as the bound the old `Config::max_drift_secs` default
/// used to be.
pub fn reconcile_drift_bound(toml: Option<u64>, chain: u64) -> Result<u64, ConfigError> {
    match toml {
        None => Ok(chain),
        Some(v) if v == chain => Ok(chain),
        Some(v) => Err(ConfigError::DriftBoundMismatch { toml: v, chain }),
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RethEngine {
    /// Plain (unauthenticated) RPC endpoint — used only for `testing_buildBlockV1` (see `engine.rs`'s
    /// module doc for why the forced-tx-list build goes through this, not the authenticated port).
    pub rpc_url: String,
    /// The real, JWT-authenticated Engine API endpoint.
    pub engine_url: String,
    pub jwt_secret_path: PathBuf,
}

/// Never `rome_zk_executor_api::BLOCKS_PER_BATCH`/`PREV_RANDAO_EPOCH_BLOCKS` — see this
/// module's doc. Re-exported from [`rome_zk_profile::DEFAULT_BLOCKS_PER_BATCH`], the one
/// crate that now owns this numeric default — pinned to 10 blocks per batch.
pub const DEFAULT_BLOCKS_PER_BATCH: u64 = rome_zk_profile::DEFAULT_BLOCKS_PER_BATCH as u64;

fn default_blocks_per_batch() -> u64 {
    DEFAULT_BLOCKS_PER_BATCH
}
fn default_max_open_channels() -> usize {
    16
}

fn deserialize_pubkey<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Pubkey, D::Error> {
    let s = String::deserialize(d)?;
    s.parse().map_err(serde::de::Error::custom)
}

impl Config {
    pub fn load(path: &std::path::Path) -> Result<Self, ConfigError> {
        let text =
            std::fs::read_to_string(path).map_err(|e| ConfigError::Read(path.to_path_buf(), e))?;
        let mut cfg: Self =
            toml::from_str(&text).map_err(|e| ConfigError::Parse(path.to_path_buf(), e))?;
        cfg.apply_env_overrides(|k| std::env::var(k).ok())?;
        Ok(cfg)
    }

    /// Env-override logic, parameterized over the env lookup so it is testable without
    /// mutating the real process environment — mirrors `rome_zk_batcher::config::Config::apply_env_overrides`'s
    /// own shape. Currently one variable: `ROME_ZK_DERIVE_METRICS_ADDR`.
    fn apply_env_overrides(
        &mut self,
        get: impl Fn(&str) -> Option<String>,
    ) -> Result<(), ConfigError> {
        if let Some(v) = get("ROME_ZK_DERIVE_METRICS_ADDR") {
            self.metrics_addr = v.parse().map_err(|e| ConfigError::EnvOverride {
                var: "ROME_ZK_DERIVE_METRICS_ADDR",
                value: v.clone(),
                reason: format!("{e}"),
            })?;
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("read config {0:?}: {1}")]
    Read(PathBuf, std::io::Error),
    #[error("parse config {0:?}: {1}")]
    Parse(PathBuf, toml::de::Error),
    /// `derive.toml`'s `max_drift_secs` disagrees with the chain's own
    /// `chain_config.max_drift_secs` — the chain is authoritative, never the TOML.
    #[error(
        "derive.toml max_drift_secs = {toml} but chain_config says {chain} — the chain's value is \
         authoritative; drop the key or match it"
    )]
    DriftBoundMismatch { toml: u64, chain: u64 },
    /// An env override (currently only `ROME_ZK_DERIVE_METRICS_ADDR`) whose value fails
    /// to parse into its field's type.
    #[error("env override {var}={value:?}: {reason}")]
    EnvOverride {
        var: &'static str,
        value: String,
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_minimal_config_and_fills_in_defaults() {
        let program_id = Pubkey::new_unique();
        let settlement_program_id = Pubkey::new_unique();
        let toml_text = format!(
            r#"
            chain_id = 200101
            solana_rpc_url = "http://127.0.0.1:8899"
            inbox_program_id = "{program_id}"
            settlement_program_id = "{settlement_program_id}"

            [reth]
            rpc_url = "http://127.0.0.1:8545"
            engine_url = "http://127.0.0.1:8551"
            jwt_secret_path = "/tmp/jwt.hex"
            "#
        );
        let cfg: Config = toml::from_str(&toml_text).unwrap();
        assert_eq!(cfg.chain_id, 200_101);
        assert_eq!(cfg.inbox_program_id, program_id);
        assert_eq!(cfg.settlement_program_id, settlement_program_id);
        assert_eq!(cfg.blocks_per_batch, DEFAULT_BLOCKS_PER_BATCH);
        assert_eq!(cfg.max_open_channels, 16);
        // No default any more — an omitted key means "defer entirely to chain_config",
        // decided at startup by `reconcile_drift_bound`, never a TOML-side value.
        assert_eq!(cfg.max_drift_secs, None);
    }

    /// The "given" half through the real loader — a present key parses to `Some` and
    /// a disagreeing chain value is refused by name with both numbers.
    #[test]
    fn a_present_max_drift_secs_key_loads_as_some_and_must_match_the_chain() {
        let toml_text = r#"
            chain_id = 200101
            solana_rpc_url = "http://127.0.0.1:8899"
            inbox_program_id = "11111111111111111111111111111111"
            settlement_program_id = "11111111111111111111111111111111"
            max_drift_secs = 30
            [reth]
            rpc_url = "http://127.0.0.1:8545"
            engine_url = "http://127.0.0.1:8551"
            jwt_secret_path = "/tmp/jwt.hex"
            "#;
        let cfg: Config = toml::from_str(toml_text).unwrap();
        assert_eq!(cfg.max_drift_secs, Some(30));
        match reconcile_drift_bound(cfg.max_drift_secs, 60).unwrap_err() {
            ConfigError::DriftBoundMismatch { toml, chain } => assert_eq!((toml, chain), (30, 60)),
            other => panic!("expected DriftBoundMismatch, got {other:?}"),
        }
        assert_eq!(reconcile_drift_bound(cfg.max_drift_secs, 30).unwrap(), 30);
    }

    #[test]
    fn blocks_per_batch_is_overridable_per_profile() {
        let program_id = Pubkey::new_unique();
        let settlement_program_id = Pubkey::new_unique();
        let toml_text = format!(
            r#"
            chain_id = 200101
            solana_rpc_url = "http://127.0.0.1:8899"
            inbox_program_id = "{program_id}"
            settlement_program_id = "{settlement_program_id}"
            blocks_per_batch = 5

            [reth]
            rpc_url = "http://127.0.0.1:8545"
            engine_url = "http://127.0.0.1:8551"
            jwt_secret_path = "/tmp/jwt.hex"
            "#
        );
        let cfg: Config = toml::from_str(&toml_text).unwrap();
        assert_eq!(cfg.blocks_per_batch, 5);
    }

    /// `DEFAULT_BLOCKS_PER_BATCH` is pinned to 10 blocks per batch (≤ 900 frames), independent of
    /// `rome_zk_executor_api`'s `BLOCKS_PER_BATCH`/`PREV_RANDAO_EPOCH_BLOCKS` (a different quantity —
    /// this module's own doc). Changing it means changing the batch shape, not silently drifting a default.
    #[test]
    fn default_blocks_per_batch_is_pinned_to_the_batch_shape() {
        assert_eq!(DEFAULT_BLOCKS_PER_BATCH, 10);
    }

    // --- derive.toml's max_drift_secs is an override that must equal
    // the chain's, refused by name otherwise. reconcile_drift_bound is the pure function that decides
    // this, kept apart from any I/O so both directions (accept, refuse) are trivial to assert. ---

    #[test]
    fn reconcile_drift_bound_uses_the_chain_value_when_toml_is_absent() {
        assert_eq!(reconcile_drift_bound(None, 45).unwrap(), 45);
    }

    #[test]
    fn reconcile_drift_bound_accepts_a_toml_override_that_matches_the_chain() {
        assert_eq!(reconcile_drift_bound(Some(60), 60).unwrap(), 60);
    }

    /// A TOML `max_drift_secs = 30` against a chain whose `chain_config` says 60 must not be silently
    /// accepted and wired as the bound — this is the named refusal that must exist instead.
    #[test]
    fn reconcile_drift_bound_refuses_a_toml_override_that_disagrees_with_the_chain() {
        let err = reconcile_drift_bound(Some(30), 60).unwrap_err();
        match err {
            ConfigError::DriftBoundMismatch { toml, chain } => {
                assert_eq!(toml, 30);
                assert_eq!(chain, 60);
            }
            other => panic!("expected DriftBoundMismatch, got {other:?}"),
        }
    }

    // --- metrics_addr default + env override. Mirrors
    // rome_zk_batcher::config's own tests for the identical shape. ---

    #[test]
    fn default_metrics_addr_is_127_0_0_1_9003() {
        assert_eq!(default_metrics_addr(), "127.0.0.1:9003".parse().unwrap());
    }

    fn minimal_config() -> Config {
        let toml_text = format!(
            r#"
            chain_id = 200101
            solana_rpc_url = "http://127.0.0.1:8899"
            inbox_program_id = "{}"
            settlement_program_id = "{}"

            [reth]
            rpc_url = "http://127.0.0.1:8545"
            engine_url = "http://127.0.0.1:8551"
            jwt_secret_path = "/tmp/jwt.hex"
            "#,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        toml::from_str(&toml_text).unwrap()
    }

    /// `metrics_addr` is a defaulted field: omitting it yields the loopback default.
    #[test]
    fn an_omitted_metrics_addr_defaults_to_127_0_0_1_9003() {
        let cfg = minimal_config();
        assert_eq!(cfg.metrics_addr, default_metrics_addr());
    }

    /// `ROME_ZK_DERIVE_METRICS_ADDR` overrides the config file's own `metrics_addr`, mirroring the
    /// batcher's `ROME_ZK_BATCHER_METRICS_ADDR`.
    #[test]
    fn metrics_addr_env_override_replaces_the_configured_value() {
        let mut cfg = minimal_config();
        assert_eq!(cfg.metrics_addr, default_metrics_addr());
        cfg.apply_env_overrides(|k| {
            (k == "ROME_ZK_DERIVE_METRICS_ADDR").then(|| "127.0.0.1:9111".to_string())
        })
        .unwrap();
        assert_eq!(cfg.metrics_addr, "127.0.0.1:9111".parse().unwrap());
    }

    /// Mutation target: an env override that fails to parse is refused by name, never silently ignored
    /// or left at whatever the TOML said.
    #[test]
    fn metrics_addr_env_override_that_fails_to_parse_is_refused_by_name() {
        let mut cfg = minimal_config();
        let err = cfg
            .apply_env_overrides(|k| {
                (k == "ROME_ZK_DERIVE_METRICS_ADDR").then(|| "not-an-addr".to_string())
            })
            .unwrap_err();
        assert!(
            matches!(err, ConfigError::EnvOverride { var, .. } if var == "ROME_ZK_DERIVE_METRICS_ADDR"),
            "got {err:?}"
        );
    }
}
