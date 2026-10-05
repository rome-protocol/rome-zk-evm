//! TOML-rendered runtime `Config`. Chain facts (`head_final_batch`, `exit_config`'s own
//! fields, `challenge_window_slots`) are read LIVE from chain, never configured here — same rule
//! `rome-zk-prover::config`'s own doc states for its config. `deny_unknown_fields`: a typo'd or stale key
//! is a refusal to start, never a silently-ignored knob.

use serde::Deserialize;
use std::path::PathBuf;

fn default_poll_interval_ms() -> u64 {
    2000
}
/// The compute budget a `ProveExit` send asks for. Its one real measurement is 40,248 units for a small proof
/// (three account nodes, two storage nodes), taken by `prove_exit_cu_and_tx_size_with_anvil_fixture` in
/// `programs/zk-settlement/tests/exit_prove.rs`. That is a floor: the cost grows with every node, and a proof
/// that fills the transaction holds many more. So the default stays well above the measured figure, at the
/// 120,000 the design allows for a full-size proof plus a margin, instead of being cut down to the small one.
fn default_compute_unit_limit() -> u32 {
    150_000
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
/// The widest block range one `eth_getLogs` call asks for. reth refuses anything over 100,000 blocks by default;
/// 10,000 stays far inside that and inside what hosted nodes allow.
fn default_max_log_range() -> u64 {
    10_000
}
/// Pay a proved exit out as soon as it is proved, with `ReleaseExit`. On by default.
fn default_auto_release() -> bool {
    true
}
/// A payout below this many lamports (raw units of the vault's mint) is not released automatically when the recipient
/// has no wrapped SOL token account: creating the account costs the exit payer rent, and anyone could send dust to a
/// fresh address to drain it. 10,000,000 lamports is 0.01 SOL, several times the rent. Such a withdrawal waits for a
/// manual `release-exit`.
fn default_release_create_account_min_lamports() -> u64 {
    crate::release::DEFAULT_CREATE_ACCOUNT_MIN_LAMPORTS
}
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

/// The data length of every account a `ProveExit` loads, in the instruction's own order, followed by the
/// settlement program's account and its ProgramData (the network charges both, though neither is listed in the
/// instruction). The fee payer and the exit record are `0`: a plain wallet holds no data, and the record is
/// created by the call itself. The batch's pending account, the window and the nullifier page are taken at their
/// fixed layout size, which is what they hold once they exist, so the figure is never short.
pub fn prove_exit_account_lens(
    root_len: usize,
    exit_config_len: usize,
    system_program_len: usize,
    program_len: usize,
    program_data_len: usize,
) -> Vec<usize> {
    vec![
        0, // fee payer
        root_len,
        rome_zk_layouts::pending::PENDING_LEN,
        exit_config_len,
        0, // the exit record, created by the call
        rome_zk_layouts::exit::exit_window::LEN,
        rome_zk_layouts::exit::exit_nullifier::LEN,
        system_program_len,
        program_len,
        program_data_len,
    ]
}

/// What `ProveExit` needs loaded, in bytes: the network's per-account formula over `account_data_lens`, rounded up to
/// whole 32 KiB pages (the unit a V1 transaction is charged in).
pub fn required_loaded_accounts_limit(account_data_lens: &[usize]) -> u32 {
    const PAGE: u32 = 32 * 1024;
    rome_zk_solana_sender::required_loaded_accounts_bytes(account_data_lens).div_ceil(PAGE) * PAGE
}

/// A configured `loaded_accounts_data_size_limit` is below what `ProveExit` loads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "configured loaded_accounts_data_size_limit={configured} is below the {required} bytes a ProveExit loads \
     (the settlement program's own data is most of it); raise it, or remove the setting to use the computed value"
)]
pub struct LoadedAccountsLimitTooLow {
    pub configured: u32,
    pub required: u32,
}

/// The limit a send uses: the computed requirement when nothing is configured, the configured value when it covers
/// the requirement, and a refusal by name when it does not (never silently raised).
pub fn resolve_loaded_accounts_limit(
    configured: Option<u32>,
    account_data_lens: &[usize],
) -> Result<u32, LoadedAccountsLimitTooLow> {
    let required = required_loaded_accounts_limit(account_data_lens);
    match configured {
        None => Ok(required),
        Some(configured) if configured < required => Err(LoadedAccountsLimitTooLow {
            configured,
            required,
        }),
        Some(configured) => Ok(configured),
    }
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
    /// Unset (the default): the binary works out what `ProveExit` loads from the live account sizes (see
    /// [`required_loaded_accounts_limit`]). A value set here must be at least that, or the binary refuses to start.
    #[serde(default)]
    pub loaded_accounts_data_size_limit: Option<u32>,
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
    /// Blocks per `eth_getLogs` call while scanning the portal. At least 1.
    #[serde(default = "default_max_log_range")]
    pub max_log_range: u64,
    #[serde(default = "default_max_send_attempts")]
    pub max_send_attempts: u32,
    #[serde(default = "default_max_window_requeues")]
    pub max_window_requeues: u32,
    /// Send `ReleaseExit` for every proved exit, so the user is paid without anyone running a command.
    #[serde(default = "default_auto_release")]
    pub auto_release: bool,
    /// See [`default_release_create_account_min_lamports`].
    #[serde(default = "default_release_create_account_min_lamports")]
    pub release_create_account_min_lamports: u64,
}

impl Config {
    /// The [`rome_zk_solana_sender::SendTuning`] the exit prover's sends use, taken from this config. The
    /// loaded-accounts limit is not a config value on its own: the caller passes the one
    /// [`resolve_loaded_accounts_limit`] returned.
    pub fn send_tuning(
        &self,
        loaded_accounts_data_size_limit: u32,
    ) -> rome_zk_solana_sender::SendTuning {
        rome_zk_solana_sender::SendTuning {
            compute_unit_limit: self.compute_unit_limit,
            loaded_accounts_data_size_limit,
            priority_fee_micro_lamports: self.priority_fee_micro_lamports,
            max_priority_fee_micro_lamports: self.max_priority_fee_micro_lamports,
            confirm_timeout: std::time::Duration::from_secs(self.confirm_timeout_secs),
            confirm_commitment: self.confirm_commitment,
            status_poll_interval: std::time::Duration::from_millis(self.confirm_poll_interval_ms),
        }
    }

    pub fn load(path: &std::path::Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io(e.to_string()))?;
        let cfg: Self = toml::from_str(&text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        if cfg.max_log_range == 0 {
            return Err(ConfigError::Invalid(
                "max_log_range must be at least 1".to_string(),
            ));
        }
        Ok(cfg)
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
    #[error("config: {0}")]
    Invalid(String),
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
        assert_eq!(cfg.max_log_range, 10_000);
        assert_eq!(cfg.max_window_requeues, 3);
        assert!(cfg.auto_release);
        assert_eq!(cfg.release_create_account_min_lamports, 10_000_000);
    }

    #[test]
    fn the_release_settings_can_be_set() {
        let toml = format!(
            "{}\nauto_release = false\nrelease_create_account_min_lamports = 5\n",
            minimal_toml()
        );
        let cfg: Config = toml::from_str(&toml).unwrap();
        assert!(!cfg.auto_release);
        assert_eq!(cfg.release_create_account_min_lamports, 5);
    }

    #[test]
    fn a_zero_log_range_is_refused_at_load() {
        let dir = std::env::temp_dir().join(format!("exit-prover-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("zero-range.toml");
        std::fs::write(&path, format!("{}\nmax_log_range = 0\n", minimal_toml())).unwrap();
        let err = Config::load(&path).unwrap_err();
        std::fs::remove_dir_all(&dir).ok();
        assert!(err.to_string().contains("max_log_range"), "{err}");
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
        let t = cfg.send_tuning(65_536);
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
        let t = cfg.send_tuning(65_536);
        assert_eq!(t.confirm_commitment, ConfirmCommitment::Confirmed);
        assert_eq!(t.confirm_timeout, std::time::Duration::from_secs(7));
        assert_eq!(
            t.status_poll_interval,
            std::time::Duration::from_millis(123)
        );
        assert_eq!(t.compute_unit_limit, cfg.compute_unit_limit);
        assert_eq!(t.loaded_accounts_data_size_limit, 65_536);
    }

    /// The live settlement program's data is 310,464 bytes: the computed requirement is ten pages, far above the
    /// 16 KiB the config used to default to.
    #[test]
    fn the_requirement_covers_a_large_program_data_account() {
        let lens = prove_exit_account_lens(202, 142, 14, 36, 310_464);
        let required = required_loaded_accounts_limit(&lens);
        assert_eq!(required, 10 * 32 * 1024);
        assert!(required as usize > 310_464);
        assert_eq!(required % (32 * 1024), 0);
    }

    /// Dropping the program data from the sum collapses the requirement, so the data is what drives it.
    #[test]
    fn the_program_data_length_drives_the_requirement() {
        let with =
            required_loaded_accounts_limit(&prove_exit_account_lens(202, 142, 14, 36, 310_464));
        let without = required_loaded_accounts_limit(&prove_exit_account_lens(202, 142, 14, 36, 0));
        assert!(without < with / 4, "{without} vs {with}");
    }

    #[test]
    fn an_unset_limit_uses_the_computed_requirement() {
        let lens = prove_exit_account_lens(202, 142, 14, 36, 310_464);
        assert_eq!(
            resolve_loaded_accounts_limit(None, &lens),
            Ok(required_loaded_accounts_limit(&lens))
        );
        let cfg: Config = toml::from_str(minimal_toml()).unwrap();
        assert_eq!(cfg.loaded_accounts_data_size_limit, None);
    }

    #[test]
    fn a_configured_limit_below_the_requirement_is_refused_by_name() {
        let lens = prove_exit_account_lens(202, 142, 14, 36, 310_464);
        let required = required_loaded_accounts_limit(&lens);
        let err = resolve_loaded_accounts_limit(Some(required - 1), &lens).unwrap_err();
        assert_eq!(err.required, required);
        assert!(err.to_string().contains("loaded_accounts_data_size_limit"));
        assert_eq!(
            resolve_loaded_accounts_limit(Some(required), &lens),
            Ok(required)
        );
        assert_eq!(
            resolve_loaded_accounts_limit(Some(required + 4096), &lens),
            Ok(required + 4096)
        );
    }
}
