//! Runtime `Config` and the vkey-of-record loader: the ONE owner of
//! `programVK`/`rootCVadcopFinal`/`elf_sha256`/`chain_id`/`layout_id` is the vkey JSON fixture
//! (`fixtures/vkeys/<chain>-layout1.json`) — this module never invents a literal for any of those
//! fields, it only reads and validates that file.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

fn default_poll_interval_ms() -> u64 {
    2000
}
fn default_compute_unit_limit() -> u32 {
    700_000
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
fn default_max_prove_attempts() -> u32 {
    3
}
fn default_prove_timeout_secs() -> u64 {
    1800
}
fn default_finalize_walk() -> u64 {
    8
}
fn default_verifier_behind_alarm_polls() -> u32 {
    30
}
fn default_stale_anchor_alarm_polls() -> u32 {
    10
}
fn default_fetch_alarm_polls() -> u32 {
    30
}
/// Binds to loopback only by default: this process's own `/metrics`
/// endpoint has no reason to be reachable off-box. A devnet compose overrides this to `0.0.0.0` so its
/// own scrape sidecar can reach it.
fn default_metrics_addr() -> String {
    "127.0.0.1:9003".to_string()
}

/// TOML-rendered runtime config. Chain facts (`max_drift_secs`, registry entries, `head_*`, `proving_policy`, block
/// range) are read LIVE from chain, never configured here. `payer_key_path` names WHERE the payer keypair file lives;
/// the key bytes themselves are never read into this struct.
///
/// `deny_unknown_fields`: a typo'd or stale key in the TOML is a refusal to start, never a
/// silently-ignored knob (`Config::load` surfaces it as [`ConfigError::Parse`], named by field).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub chain_id: u64,
    pub inbox_program_id: String,
    pub settlement_program_id: String,
    pub solana_rpc_url: String,
    pub verifier_rpc_url: String,
    pub genesis_path: PathBuf,
    pub elf_path: PathBuf,
    pub vkey_json: PathBuf,
    pub zisk_home: PathBuf,
    #[serde(default)]
    pub gpu: bool,
    pub work_dir: PathBuf,
    pub payer_key_path: PathBuf,
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,
    #[serde(default = "default_compute_unit_limit")]
    pub compute_unit_limit: u32,
    /// Derived at preflight from the live settlement `ProgramData` — `None` until the
    /// poster computes it; not a fixed config default.
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
    /// The follower loop: consecutive prove attempts for the SAME batch — each
    /// with a freshly wiped work directory — before it halts with `ProveAttemptsExhausted { attempts }`
    /// rather than retrying forever. `LocalCargoZisk::prove` itself reads none of this — it makes
    /// exactly one subprocess invocation per call; the retry-with-wipe loop is `follower`'s own.
    #[serde(default = "default_max_prove_attempts")]
    pub max_prove_attempts: u32,
    #[serde(default = "default_prove_timeout_secs")]
    pub prove_timeout_secs: u64,
    #[serde(default = "default_finalize_walk")]
    pub finalize_walk: u64,
    /// The follower loop: consecutive `VerifierBehind` retries for the SAME
    /// job before it halts with `FollowerError::VerifierBehindAlarm` — a reth-verifier that never
    /// catches up must not retry forever.
    #[serde(default = "default_verifier_behind_alarm_polls")]
    pub verifier_behind_alarm_polls: u32,
    /// The follower loop: consecutive `StaleAnchor` retries for the SAME
    /// posted candidate before it halts with `FollowerError::StaleAnchorAlarm` — a FINALIZED read that
    /// never catches up to a landed send must not retry forever.
    #[serde(default = "default_stale_anchor_alarm_polls")]
    pub stale_anchor_alarm_polls: u32,
    /// The follower loop: consecutive `TransientFetchError` retries before it halts
    /// with `FollowerError::FetchAlarm` — a read failure this persistent is no longer "about to
    /// recover."
    #[serde(default = "default_fetch_alarm_polls")]
    pub fetch_alarm_polls: u32,
    /// How many of the most recent `work_dir/<batch>` directories to keep once a
    /// batch reaches a terminal outcome — `0` (default) removes each one immediately.
    #[serde(default)]
    pub keep_work_dirs: u32,
    /// Retention gate for `close_pending_ix` — `None` (default) keeps every pending PDA readable
    /// by `RootView` forever; stays `None` on Tiber until a per-chain policy exists.
    #[serde(default)]
    pub close_pending_after_batches: Option<u64>,
    #[serde(default = "default_metrics_addr")]
    pub metrics_addr: String,
    /// Reporting knob only for the cost metric — the bill is the truth.
    #[serde(default)]
    pub gpu_hourly_usd: Option<f64>,
    #[serde(default)]
    pub database_url: Option<String>,
}

/// The vkey-of-record fixture (`fixtures/vkeys/<chain>-layout1.json`): `programVK`,
/// `rootCVadcopFinal`, `elf_sha256`, `chain_id`, `layout_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VkeyOfRecord {
    pub program_vk: [u8; 32],
    pub root_c: [u8; 32],
    pub elf_sha256: [u8; 32],
    pub chain_id: u64,
    pub layout_id: u8,
}

#[derive(Debug, Deserialize)]
struct VkeyJson {
    #[serde(rename = "programVK")]
    program_vk: String,
    #[serde(rename = "rootCVadcopFinal")]
    root_c_vadcop_final: String,
    elf_sha256: String,
    chain_id: u64,
    layout_id: u8,
}

/// Every refusal is by name — never a silently-accepted mismatch.
#[derive(Debug, thiserror::Error)]
pub enum VkeyError {
    #[error("read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("{field} in {path}: expected 32 bytes of hex, got {source}")]
    BadHex {
        path: PathBuf,
        field: &'static str,
        #[source]
        source: hex::FromHexError,
    },
    #[error("{field} in {path}: expected 32 bytes, got {got}")]
    BadHexLen {
        path: PathBuf,
        field: &'static str,
        got: usize,
    },
    /// Covers both named refusals the plan groups under one error: the vkey json's own
    /// `layout_id != 1`, or (once checked against a `Config`) its `chain_id` disagreeing with the
    /// config's.
    #[error("vkey json mismatch: {0}")]
    VkeyJsonMismatch(String),
    #[error("elf mismatch: {elf_path} hashes to {got}, vkey json {vkey_path} expects {expected}")]
    ElfMismatch {
        elf_path: PathBuf,
        vkey_path: PathBuf,
        expected: String,
        got: String,
    },
    #[error("read elf {path}: {source}")]
    ReadElf {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

fn hex32(path: &Path, field: &'static str, s: &str) -> Result<[u8; 32], VkeyError> {
    let bytes = hex::decode(s.trim_start_matches("0x")).map_err(|source| VkeyError::BadHex {
        path: path.to_path_buf(),
        field,
        source,
    })?;
    bytes.try_into().map_err(|b: Vec<u8>| VkeyError::BadHexLen {
        path: path.to_path_buf(),
        field,
        got: b.len(),
    })
}

impl VkeyOfRecord {
    /// Deserializes the vkey JSON at `path` and refuses (`VkeyJsonMismatch`) any `layout_id`
    /// other than 1 — the only layout this prover ever proves against. Does not know the running
    /// chain id; see [`Config::load_vkey_of_record`] for the chain-id cross-check.
    pub fn load(path: &Path) -> Result<Self, VkeyError> {
        let s = std::fs::read_to_string(path).map_err(|source| VkeyError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let j: VkeyJson = serde_json::from_str(&s).map_err(|source| VkeyError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        if j.layout_id != 1 {
            return Err(VkeyError::VkeyJsonMismatch(format!(
                "{}: layout_id must be 1, got {}",
                path.display(),
                j.layout_id
            )));
        }
        Ok(VkeyOfRecord {
            program_vk: hex32(path, "programVK", &j.program_vk)?,
            root_c: hex32(path, "rootCVadcopFinal", &j.root_c_vadcop_final)?,
            elf_sha256: hex32(path, "elf_sha256", &j.elf_sha256)?,
            chain_id: j.chain_id,
            layout_id: j.layout_id,
        })
    }
}

fn sha256_file(path: &Path) -> Result<[u8; 32], VkeyError> {
    let bytes = std::fs::read(path).map_err(|source| VkeyError::ReadElf {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(hasher.finalize().into())
}

/// Refused by name — a TOML file that fails to read, fails to parse, or (via
/// `#[serde(deny_unknown_fields)]`) names a key `Config` does not have is a refusal to start,
/// never a silently-ignored knob.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

impl Config {
    /// The [`rome_zk_solana_sender::SendTuning`] the prover's sends use, taken from this config. Its loaded-accounts limit is 0 on purpose: the run loop re-derives it per anchor from `loaded_accounts_data_size_limit` (see `RunConfig`'s own doc).
    pub fn send_tuning(&self) -> rome_zk_solana_sender::SendTuning {
        rome_zk_solana_sender::SendTuning {
            compute_unit_limit: self.compute_unit_limit,
            loaded_accounts_data_size_limit: 0,
            priority_fee_micro_lamports: self.priority_fee_micro_lamports,
            max_priority_fee_micro_lamports: self.max_priority_fee_micro_lamports,
            confirm_timeout: std::time::Duration::from_secs(self.confirm_timeout_secs),
            confirm_commitment: self.confirm_commitment,
            status_poll_interval: std::time::Duration::from_millis(self.confirm_poll_interval_ms),
        }
    }

    /// Reads and deserializes the TOML config at `path`. An unknown key is a named parse
    /// refusal (`ConfigError::Parse`, whose message names the field `serde`/`toml` rejected it
    /// by) — never a silently-ignored typo or stale knob.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let s = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&s).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Loads the vkey of record (`self.vkey_json`) and refuses by name: `VkeyJsonMismatch` if its
    /// `layout_id != 1` or its `chain_id` disagrees with `self.chain_id`; `ElfMismatch` if the
    /// sha256 of the file at `self.elf_path` disagrees with the vkey json's `elf_sha256`.
    pub fn load_vkey_of_record(&self) -> Result<VkeyOfRecord, VkeyError> {
        let vkey = VkeyOfRecord::load(&self.vkey_json)?;
        if vkey.chain_id != self.chain_id {
            return Err(VkeyError::VkeyJsonMismatch(format!(
                "{}: chain_id {} != config chain_id {}",
                self.vkey_json.display(),
                vkey.chain_id,
                self.chain_id
            )));
        }
        let got = sha256_file(&self.elf_path)?;
        if got != vkey.elf_sha256 {
            return Err(VkeyError::ElfMismatch {
                elf_path: self.elf_path.clone(),
                vkey_path: self.vkey_json.clone(),
                expected: hex::encode(vkey.elf_sha256),
                got: hex::encode(got),
            });
        }
        Ok(vkey)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn repo_vkey_path() -> PathBuf {
        PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/vkeys/tiber-200101-layout1.json"
        ))
    }

    fn base_config(elf_path: PathBuf) -> Config {
        Config {
            chain_id: 200101,
            inbox_program_id: "11111111111111111111111111111111111111111".into(),
            settlement_program_id: "11111111111111111111111111111111111111111".into(),
            solana_rpc_url: "http://127.0.0.1:8899".into(),
            verifier_rpc_url: "http://127.0.0.1:8547".into(),
            genesis_path: PathBuf::from("genesis.json"),
            elf_path,
            vkey_json: repo_vkey_path(),
            zisk_home: PathBuf::from("/nonexistent/zisk"),
            gpu: true,
            work_dir: PathBuf::from("/tmp/rome-zk-prover-work"),
            payer_key_path: PathBuf::from("/nonexistent/payer.json"),
            poll_interval_ms: default_poll_interval_ms(),
            compute_unit_limit: default_compute_unit_limit(),
            loaded_accounts_data_size_limit: None,
            priority_fee_micro_lamports: default_priority_fee_micro_lamports(),
            max_priority_fee_micro_lamports: default_max_priority_fee_micro_lamports(),
            confirm_timeout_secs: default_confirm_timeout_secs(),
            confirm_commitment: Default::default(),
            confirm_poll_interval_ms: default_confirm_poll_interval_ms(),
            max_prove_attempts: default_max_prove_attempts(),
            prove_timeout_secs: default_prove_timeout_secs(),
            finalize_walk: default_finalize_walk(),
            verifier_behind_alarm_polls: default_verifier_behind_alarm_polls(),
            stale_anchor_alarm_polls: default_stale_anchor_alarm_polls(),
            fetch_alarm_polls: default_fetch_alarm_polls(),
            keep_work_dirs: 0,
            close_pending_after_batches: None,
            metrics_addr: default_metrics_addr(),
            gpu_hourly_usd: None,
            database_url: None,
        }
    }

    #[test]
    fn loads_the_real_vkey_of_record() {
        let vkey = VkeyOfRecord::load(&repo_vkey_path()).expect("load");
        assert_eq!(vkey.chain_id, 200101);
        assert_eq!(vkey.layout_id, 1);
        assert_eq!(
            hex::encode(vkey.program_vk),
            "e5ea5c144f19aba3e8a72f897dcb565c1b18bc335b54fd93e06b06689c53cb03"
        );
    }

    #[test]
    fn non_hex_characters_in_a_vkey_field_are_refused_by_name() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(
            f,
            r#"{{"programVK":"0xnot-hex-at-all","rootCVadcopFinal":"0x{}","elf_sha256":"0x{}","chain_id":200101,"layout_id":1}}"#,
            "22".repeat(32),
            "33".repeat(32),
        )
        .unwrap();
        let err = VkeyOfRecord::load(f.path()).unwrap_err();
        assert!(
            matches!(
                err,
                VkeyError::BadHex {
                    field: "programVK",
                    ..
                }
            ),
            "expected BadHex naming programVK, got {err:?}"
        );
    }

    #[test]
    fn a_vkey_field_of_the_wrong_hex_length_is_refused_by_name() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(
            f,
            r#"{{"programVK":"0x{}","rootCVadcopFinal":"0x{}","elf_sha256":"0x{}","chain_id":200101,"layout_id":1}}"#,
            "11".repeat(31), // 31 bytes, not the required 32
            "22".repeat(32),
            "33".repeat(32),
        )
        .unwrap();
        let err = VkeyOfRecord::load(f.path()).unwrap_err();
        assert!(
            matches!(
                err,
                VkeyError::BadHexLen {
                    field: "programVK",
                    got: 31,
                    ..
                }
            ),
            "expected BadHexLen naming programVK with got=31, got {err:?}"
        );
    }

    #[test]
    fn layout_id_2_is_a_vkey_json_mismatch() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(
            f,
            r#"{{"programVK":"0x{}","rootCVadcopFinal":"0x{}","elf_sha256":"0x{}","chain_id":200101,"layout_id":2}}"#,
            "11".repeat(32),
            "22".repeat(32),
            "33".repeat(32),
        )
        .unwrap();
        let err = VkeyOfRecord::load(f.path()).unwrap_err();
        assert!(
            matches!(err, VkeyError::VkeyJsonMismatch(ref m) if m.contains("layout_id")),
            "expected VkeyJsonMismatch naming layout_id, got {err:?}"
        );
    }

    #[test]
    fn chain_id_disagreeing_with_config_is_a_vkey_json_mismatch() {
        let cfg = base_config(PathBuf::from("/nonexistent/elf"));
        let mut cfg = cfg;
        cfg.chain_id = 999; // real vkey json says 200101
        let err = cfg.load_vkey_of_record().unwrap_err();
        assert!(
            matches!(err, VkeyError::VkeyJsonMismatch(ref m) if m.contains("chain_id")),
            "expected VkeyJsonMismatch naming chain_id, got {err:?}"
        );
    }

    #[test]
    fn elf_sha_mismatch_is_refused_by_name() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"not the real elf").unwrap();
        let cfg = base_config(f.path().to_path_buf());
        let err = cfg.load_vkey_of_record().unwrap_err();
        assert!(
            matches!(err, VkeyError::ElfMismatch { .. }),
            "expected ElfMismatch, got {err:?}"
        );
    }

    #[test]
    fn a_matching_elf_hash_loads_clean() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"stand-in elf bytes").unwrap();
        f.flush().unwrap();
        let got = sha256_file(f.path()).unwrap();

        let mut vkey_file = tempfile::NamedTempFile::new().unwrap();
        write!(
            vkey_file,
            r#"{{"programVK":"0x{}","rootCVadcopFinal":"0x{}","elf_sha256":"0x{}","chain_id":7,"layout_id":1}}"#,
            "44".repeat(32),
            "55".repeat(32),
            hex::encode(got),
        )
        .unwrap();

        let mut cfg = base_config(f.path().to_path_buf());
        cfg.chain_id = 7;
        cfg.vkey_json = vkey_file.path().to_path_buf();
        let vkey = cfg.load_vkey_of_record().expect("should load clean");
        assert_eq!(vkey.elf_sha256, got);
    }

    const MINIMAL_VALID_TOML: &str = r#"
        chain_id = 200101
        inbox_program_id = "11111111111111111111111111111111111111111"
        settlement_program_id = "11111111111111111111111111111111111111111"
        solana_rpc_url = "http://127.0.0.1:8899"
        verifier_rpc_url = "http://127.0.0.1:8547"
        genesis_path = "genesis.json"
        elf_path = "elf"
        vkey_json = "vkey.json"
        zisk_home = "/nonexistent/zisk"
        work_dir = "/tmp/rome-zk-prover-work"
        payer_key_path = "/nonexistent/payer.json"
    "#;

    #[test]
    fn a_minimal_valid_toml_loads_clean() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(MINIMAL_VALID_TOML.as_bytes()).unwrap();
        let cfg = Config::load(f.path()).expect("should load clean");
        assert_eq!(cfg.chain_id, 200101);
        assert_eq!(cfg.max_prove_attempts, default_max_prove_attempts());
        assert_eq!(
            cfg.stale_anchor_alarm_polls,
            default_stale_anchor_alarm_polls()
        );
        assert_eq!(cfg.fetch_alarm_polls, default_fetch_alarm_polls());
    }

    /// The `/metrics` responder binds to loopback only by
    /// default — a devnet compose overrides this to `0.0.0.0` for its scrape sidecar, but the config's
    /// OWN default must never expose this off-box.
    #[test]
    fn metrics_addr_defaults_to_loopback_not_all_interfaces() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(MINIMAL_VALID_TOML.as_bytes()).unwrap();
        let cfg = Config::load(f.path()).expect("should load clean");
        assert_eq!(cfg.metrics_addr, "127.0.0.1:9003");
    }

    #[test]
    fn an_unknown_config_key_is_refused_by_name() {
        // `prove_timeout_sec` (missing the trailing `s`) — a plausible typo of the real
        // `prove_timeout_secs` knob, never silently ignored.
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(MINIMAL_VALID_TOML.as_bytes()).unwrap();
        writeln!(f, "prove_timeout_sec = 42").unwrap();
        let err = Config::load(f.path()).unwrap_err();
        let msg = err.to_string();
        assert!(
            matches!(err, ConfigError::Parse { .. }) && msg.contains("prove_timeout_sec"),
            "expected a Parse error naming prove_timeout_sec, got {err:?} ({msg})"
        );
    }

    /// The fast-finality defaults reach the prover's config, and `confirmed` stays selectable.
    #[test]
    fn confirm_defaults_are_finalized_and_15s_and_confirmed_is_selectable() {
        let load = |extra: &str| {
            let mut f = tempfile::NamedTempFile::new().unwrap();
            f.write_all(format!("{MINIMAL_VALID_TOML}\n{extra}\n").as_bytes())
                .unwrap();
            Config::load(f.path())
        };
        let cfg = load("").expect("should load clean");
        assert_eq!(cfg.confirm_timeout_secs, 15);
        assert_eq!(cfg.confirm_poll_interval_ms, 500);
        assert_eq!(
            cfg.confirm_commitment,
            rome_zk_solana_sender::ConfirmCommitment::Finalized
        );
        let cfg = load("confirm_commitment = \"confirmed\"").expect("confirmed is selectable");
        assert_eq!(
            cfg.confirm_commitment,
            rome_zk_solana_sender::ConfirmCommitment::Confirmed
        );
        assert!(load("confirm_commitment = \"processed\"").is_err());
    }

    /// The confirm settings reach the `SendTuning` the binary hands the sender.
    #[test]
    fn send_tuning_carries_the_confirm_settings_from_config() {
        use rome_zk_solana_sender::ConfirmCommitment;
        let load = |extra: &str| {
            let mut f = tempfile::NamedTempFile::new().unwrap();
            f.write_all(format!("{MINIMAL_VALID_TOML}\n{extra}\n").as_bytes())
                .unwrap();
            Config::load(f.path()).unwrap()
        };
        let t = load("").send_tuning();
        assert_eq!(t.confirm_commitment, ConfirmCommitment::Finalized);
        assert_eq!(t.confirm_timeout, std::time::Duration::from_secs(15));
        assert_eq!(
            t.status_poll_interval,
            std::time::Duration::from_millis(500)
        );
        let cfg = load(
            "confirm_commitment = \"confirmed\"\nconfirm_timeout_secs = 7\nconfirm_poll_interval_ms = 123",
        );
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
