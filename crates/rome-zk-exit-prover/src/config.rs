//! TOML-rendered runtime `Config`. Chain facts (`head_final_batch`, `exit_config`'s own
//! fields, `challenge_window_slots`) are read LIVE from chain, never configured here — same rule
//! `rome-zk-prover::config`'s own doc states for its config. `deny_unknown_fields`: a typo'd or stale key
//! is a refusal to start, never a silently-ignored knob.

use serde::Deserialize;
use std::path::PathBuf;

fn default_poll_interval_ms() -> u64 {
    2000
}
fn default_compute_unit_limit() -> u32 {
    150_000
}
fn default_loaded_accounts_data_size_limit() -> u32 {
    16 * 1024
}
fn default_priority_fee_micro_lamports() -> u64 {
    1000
}
fn default_max_priority_fee_micro_lamports() -> u64 {
    200_000
}
/// 15 s (was 60), the fast-finality bound — see `rome_zk_solana_sender::DEFAULT_CONFIRM_TIMEOUT_SECS`.
fn default_confirm_timeout_secs() -> u64 {
    rome_zk_solana_sender::DEFAULT_CONFIRM_TIMEOUT_SECS
}
fn default_confirm_poll_interval_ms() -> u64 {
    rome_zk_solana_sender::DEFAULT_STATUS_POLL_INTERVAL.as_millis() as u64
}
/// The V1 envelope (4,096 B) — the hard upper bound on any signed transaction this crate ever measures or sends. A
/// smaller `--max-proof-bytes` may be configured (headroom for a future priority-fee bump), but this is never exceeded.
pub const V1_ENVELOPE_BYTES: usize = 4096;
fn default_max_proof_bytes() -> usize {
    V1_ENVELOPE_BYTES
}
fn default_metrics_addr() -> String {
    "127.0.0.1:9004".to_string()
}
/// The block a fresh [`crate::follower::Follower`] starts its `eth_getLogs` scan from — `0` (genesis)
/// unless the portal was deployed later, in which case set this to the deploy block (Tiber's own value is
/// 208399, in the operator's deploy config). Every restart re-scans from here, never from a persisted cursor.
fn default_portal_from_block() -> u64 {
    0
}
/// Consecutive `SendFailed`/RPC-error outcomes for the SAME message before [`crate::follower::Follower`]
/// gives up on it (moves it to `stuck`) — bounded so an unreachable RPC or a
/// persistently failing send does not retry forever.
fn default_max_send_attempts() -> u32 {
    5
}
/// Consecutive `QueuedForWindow` outcomes for the SAME message before [`crate::follower::Follower`] gives
/// up on it (moves it to `stuck`) — bounded the same way `max_send_attempts` bounds send
/// failures, so a persistently over-subscribed window (or a cap that never clears) does not requeue
/// forever.
fn default_max_window_requeues() -> u32 {
    3
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub chain_id: u64,
    pub settlement_program_id: String,
    /// Solana RPC — reads `root`/`pending`/`exit_config`, sends `ProveExit`.
    pub settlement_rpc: String,
    /// The L2 reth/derive node the portal lives on — `eth_getLogs`/`eth_getProof` target.
    pub verifier_rpc: String,
    pub payer_key_path: PathBuf,
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,
    #[serde(default = "default_compute_unit_limit")]
    pub compute_unit_limit: u32,
    #[serde(default = "default_loaded_accounts_data_size_limit")]
    pub loaded_accounts_data_size_limit: u32,
    #[serde(default = "default_priority_fee_micro_lamports")]
    pub priority_fee_micro_lamports: u64,
    #[serde(default = "default_max_priority_fee_micro_lamports")]
    pub max_priority_fee_micro_lamports: u64,
    #[serde(default = "default_confirm_timeout_secs")]
    pub confirm_timeout_secs: u64,
    /// The level a send must reach before it counts as sent: `"finalized"` (default) or `"confirmed"`.
    #[serde(default)]
    pub confirm_commitment: rome_zk_solana_sender::ConfirmCommitment,
    /// Pause between status checks while a send confirms (ms).
    #[serde(default = "default_confirm_poll_interval_ms")]
    pub confirm_poll_interval_ms: u64,
    /// Pre-send refusal budget (`ProofTooLarge`) — never above [`V1_ENVELOPE_BYTES`], whatever
    /// this is set to (see [`Config::effective_max_proof_bytes`]).
    #[serde(default = "default_max_proof_bytes")]
    pub max_proof_bytes: usize,
    #[serde(default = "default_metrics_addr")]
    pub metrics_addr: String,
    #[serde(default = "default_portal_from_block")]
    pub portal_from_block: u64,
    #[serde(default = "default_max_send_attempts")]
    pub max_send_attempts: u32,
    #[serde(default = "default_max_window_requeues")]
    pub max_window_requeues: u32,
}

impl Config {
    /// The [`rome_zk_solana_sender::SendTuning`] the exit prover's sends use, taken from this config.
    pub fn send_tuning(&self) -> rome_zk_solana_sender::SendTuning {
        rome_zk_solana_sender::SendTuning {
            compute_unit_limit: self.compute_unit_limit,
            loaded_accounts_data_size_limit: self.loaded_accounts_data_size_limit,
            priority_fee_micro_lamports: self.priority_fee_micro_lamports,
            max_priority_fee_micro_lamports: self.max_priority_fee_micro_lamports,
            confirm_timeout: std::time::Duration::from_secs(self.confirm_timeout_secs),
            confirm_commitment: self.confirm_commitment,
            status_poll_interval: std::time::Duration::from_millis(self.confirm_poll_interval_ms),
        }
    }

    pub fn load(path: &std::path::Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io(e.to_string()))?;
        toml::from_str(&text).map_err(|e| ConfigError::Parse(e.to_string()))
    }

    /// Never above the V1 envelope, regardless of what the TOML configured — the hard wire limit wins.
    pub fn effective_max_proof_bytes(&self) -> usize {
        self.max_proof_bytes.min(V1_ENVELOPE_BYTES)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading config: {0}")]
    Io(String),
    #[error("parsing config: {0}")]
    Parse(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_toml() -> &'static str {
        r#"
        chain_id = 200101
        settlement_program_id = "11111111111111111111111111111111"
        settlement_rpc = "http://127.0.0.1:8899"
        verifier_rpc = "http://127.0.0.1:8547"
        payer_key_path = "/tmp/payer.json"
        "#
    }

    #[test]
    fn defaults_apply_when_omitted() {
        let cfg: Config = toml::from_str(minimal_toml()).unwrap();
        assert_eq!(cfg.poll_interval_ms, 2000);
        assert_eq!(cfg.max_proof_bytes, V1_ENVELOPE_BYTES);
        assert_eq!(cfg.metrics_addr, "127.0.0.1:9004");
        assert_eq!(cfg.portal_from_block, 0);
        assert_eq!(cfg.max_send_attempts, 5);
        assert_eq!(cfg.max_window_requeues, 3);
    }

    #[test]
    fn unknown_field_is_refused() {
        let toml = format!("{}\nbogus_field = 1\n", minimal_toml());
        assert!(toml::from_str::<Config>(&toml).is_err());
    }

    #[test]
    fn effective_max_proof_bytes_never_exceeds_the_v1_envelope() {
        let mut toml = minimal_toml().to_string();
        toml.push_str("max_proof_bytes = 999999\n");
        let cfg: Config = toml::from_str(&toml).unwrap();
        assert_eq!(cfg.effective_max_proof_bytes(), V1_ENVELOPE_BYTES);
    }

    /// The fast-finality defaults reach the exit-prover's config, and `confirmed` stays selectable.
    #[test]
    fn confirm_defaults_are_finalized_and_15s_and_confirmed_is_selectable() {
        let cfg: Config = toml::from_str(minimal_toml()).unwrap();
        assert_eq!(cfg.confirm_timeout_secs, 15);
        assert_eq!(cfg.confirm_poll_interval_ms, 500);
        assert_eq!(
            cfg.confirm_commitment,
            rome_zk_solana_sender::ConfirmCommitment::Finalized
        );
        let with = |v: &str| format!("{}\nconfirm_commitment = \"{v}\"\n", minimal_toml());
        let cfg: Config = toml::from_str(&with("confirmed")).unwrap();
        assert_eq!(
            cfg.confirm_commitment,
            rome_zk_solana_sender::ConfirmCommitment::Confirmed
        );
        assert!(toml::from_str::<Config>(&with("processed")).is_err());
    }

    /// The confirm settings reach the `SendTuning` the binary hands the sender.
    #[test]
    fn send_tuning_carries_the_confirm_settings_from_config() {
        use rome_zk_solana_sender::ConfirmCommitment;
        let cfg: Config = toml::from_str(minimal_toml()).unwrap();
        let t = cfg.send_tuning();
        assert_eq!(t.confirm_commitment, ConfirmCommitment::Finalized);
        assert_eq!(t.confirm_timeout, std::time::Duration::from_secs(15));
        assert_eq!(
            t.status_poll_interval,
            std::time::Duration::from_millis(500)
        );
        let extra = format!(
            "{}\nconfirm_commitment = \"confirmed\"\nconfirm_timeout_secs = 7\nconfirm_poll_interval_ms = 123\n",
            minimal_toml()
        );
        let cfg: Config = toml::from_str(&extra).unwrap();
        let t = cfg.send_tuning();
        assert_eq!(t.confirm_commitment, ConfirmCommitment::Confirmed);
        assert_eq!(t.confirm_timeout, std::time::Duration::from_secs(7));
        assert_eq!(
            t.status_poll_interval,
            std::time::Duration::from_millis(123)
        );
        assert_eq!(t.compute_unit_limit, cfg.compute_unit_limit);
    }
}
