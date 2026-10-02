//! Chain cadence + cap profile: the single place a chain's timing and throughput caps are declared and validated
//! together. Every value here used to be an independent hardcoded constant scattered across `rome-zk-sequencer` and
//! `rome-zk-executor-reth` (50 ms sub-block period, 20 sub-blocks/block, 10 blocks/batch, 5,000,000 sub-block gas
//! limit, 70% admission headroom); every one of those numeric homes now lives in this crate so `rome-zk-sequencer`,
//! `rome-zk-batcher` and `rome-zk-derive` read one shared definition instead of three independently-drifting
//! copies.
//!
//! Design basis: block time (1 s EVM blocks; 50 ms signed sub-blocks... EVM timestamps are integer
//! seconds) and sustained admission (70M gas/s = 70% of the 100M gas/s executor cap, the sequencer
//! sheds above it); sub-blocks every 50 ms, EVM blocks every 1 s (20 sub-blocks per block), batches
//! every 10 s. DA-per-gas ratio: 74 B per transfer (zstd-19, signatures kept), measured for a 21,000-gas
//! transfer — used here as bytes-per-gas, not a fresh premise.

use serde::{Deserialize, Serialize};

/// Design default: 50 ms sub-block period.
pub const DEFAULT_SUB_BLOCK_MS: u64 = 50;
/// Design default: 20 sub-blocks per block, i.e. 1 s EVM blocks at the default period. Owned here —
/// `rome-zk-sequencer::sealer::SUB_BLOCKS_PER_BLOCK` re-exports this value; it is no longer the other
/// way around.
pub const DEFAULT_SUB_BLOCKS_PER_BLOCK: u16 = 20;
/// Design default: 10 blocks per batch = one root post every 10 s. The design pins this value;
/// changing it is a deliberate design change, not a silent default drift.
pub const DEFAULT_BLOCKS_PER_BATCH: u32 = 10;
/// Design default: 5,000,000 gas per sub-block = 100M gas/s executor cap at the default 50 ms period.
/// Owned here — `rome-zk-sequencer::executor::DEFAULT_SUB_BLOCK_GAS_LIMIT` re-exports this value; it is
/// no longer the other way around.
pub const DEFAULT_SUB_BLOCK_GAS_LIMIT: u64 = 5_000_000;
/// Design default: sustained admission is 70% of the executor cap; the sequencer sheds above it.
pub const DEFAULT_ADMISSION_SHED_PCT: u8 = 70;
/// Design default: the one-sided timestamp drift bound (`block.timestamp <= open_unix_ts + max_drift_secs`) — Clock
/// sysvar deviation + posting latency headroom. The one home for this number: `rome-zk-derive::config::Config`
/// re-exports it as its own `max_drift_secs` default, the same way `DEFAULT_BLOCKS_PER_BATCH` is re-exported. The
/// design pins this value; changing it is a deliberate design change.
pub const DEFAULT_MAX_DRIFT_SECS: u64 = 60;

/// Measured DA bytes for a 21,000-gas (flat) transfer, zstd-19 over the whole batch stream, with signatures kept.
/// Used as a bytes-per-gas ratio to turn a profile's declared gas/s into an expected DA bytes/s so the two budgets
/// (`da_bytes_per_sec`, `prover_gas_per_sec`) can be validated against one declared cap.
const DA_BYTES_PER_TRANSFER: u128 = 74;
const GAS_PER_TRANSFER: u128 = 21_000;

/// Default `prover_gas_per_sec`: exactly the default profile's own implied gas/s (100M) — the **measured executor
/// ceiling** ("~107 A100 workers at cap"), NOT the `always` provisioning point. `always` is sized to 40M gas/s per
/// chain; a chain that wants that lower, cheaper profile declares its own
/// `sub_block_gas_limit`/`prover_gas_per_sec`/`da_bytes_per_sec` explicitly (as
/// `crates/rome-zk-sequencer/config.example.toml`, and therefore Tiber, do) rather than relying on this bare
/// default. A chain that raises its cap above whatever it declares must raise this budget too (see
/// [`Profile::validate`]).
pub const DEFAULT_PROVER_GAS_PER_SEC: u64 = 100_000_000;

/// Design default `da_bytes_per_sec`: the default profile's own implied gas/s (100M) converted through
/// [`DA_BYTES_PER_TRANSFER`]/[`GAS_PER_TRANSFER`] — the DA rate the default cap actually produces, so
/// the default profile validates with no slack (a chain raising its cap must raise this too).
pub const DEFAULT_DA_BYTES_PER_SEC: u64 =
    ((DEFAULT_PROVER_GAS_PER_SEC as u128 * DA_BYTES_PER_TRANSFER) / GAS_PER_TRANSFER) as u64;

/// 0 = never seal a block with no transactions (Tiber's own default) — sub-blocks keep ticking off-chain, but
/// nothing is written to the log and the executor is never called while a chain is idle. A chain that wants a
/// regular timestamp even when idle declares a nonzero value (seconds) instead; [`Profile::validate`] refuses
/// anything below this profile's own block time.
pub const DEFAULT_EMPTY_BLOCK_INTERVAL_SECS: u64 = 0;

fn default_sub_block_ms() -> u64 {
    DEFAULT_SUB_BLOCK_MS
}
fn default_sub_blocks_per_block() -> u16 {
    DEFAULT_SUB_BLOCKS_PER_BLOCK
}
fn default_blocks_per_batch() -> u32 {
    DEFAULT_BLOCKS_PER_BATCH
}
fn default_sub_block_gas_limit() -> u64 {
    DEFAULT_SUB_BLOCK_GAS_LIMIT
}
fn default_admission_shed_pct() -> u8 {
    DEFAULT_ADMISSION_SHED_PCT
}
fn default_da_bytes_per_sec() -> u64 {
    DEFAULT_DA_BYTES_PER_SEC
}
fn default_prover_gas_per_sec() -> u64 {
    DEFAULT_PROVER_GAS_PER_SEC
}

/// The `[profile]` table: one chain's cadence and cap, declared and validated together. Every field that used to be
/// an independent hardcoded constant now lives here; a config that omits the table entirely gets the design's own
/// defaults (`Profile::default()` — the values above), so an existing deploy that has never heard of `[profile]`
/// keeps behaving exactly as it does today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct Profile {
    /// Sub-block signing period, in milliseconds (every 50 ms by default).
    #[serde(default = "default_sub_block_ms")]
    pub sub_block_ms: u64,
    /// Sub-blocks per EVM block (20 by default). `sub_block_ms * sub_blocks_per_block` MUST be a whole number of
    /// seconds — EVM timestamps are integer seconds — checked by [`Self::validate`].
    #[serde(default = "default_sub_blocks_per_block")]
    pub sub_blocks_per_block: u16,
    /// EVM blocks per batch = one root post every `blocks_per_batch * sub_block_ms *
    /// sub_blocks_per_block` (by default every 10 s: 10 EVM blocks).
    #[serde(default = "default_blocks_per_batch")]
    pub blocks_per_batch: u32,
    /// Gas budget per sub-block (the executor cap).
    #[serde(default = "default_sub_block_gas_limit")]
    pub sub_block_gas_limit: u64,
    /// This chain's block gas limit, published into every block's `BlockEnv`. `None` (the common case)
    /// derives it as `sub_block_gas_limit * sub_blocks_per_block`; `Some` must equal that product
    /// exactly or [`Self::validate`] rejects the profile as inconsistent.
    #[serde(default)]
    pub block_gas_limit: Option<u64>,
    /// Percentage of the executor cap the sequencer sustains before shedding admission
    /// (sustained admission; the sequencer sheds above it; default 70). Declared and validated here;
    /// enforcement in `Admission` is separate and still open.
    #[serde(default = "default_admission_shed_pct")]
    pub admission_shed_pct: u8,
    /// Declared DA throughput budget in bytes/s. A profile whose implied DA rate (gas/s converted
    /// through the measured 74 B/21,000-gas ratio) exceeds this is rejected at load
    /// ([`Self::validate`]).
    #[serde(default = "default_da_bytes_per_sec")]
    pub da_bytes_per_sec: u64,
    /// Declared prover throughput budget in gas/s. A profile whose implied gas/s exceeds this is
    /// rejected at load ([`Self::validate`]) — this is the "1G gas/s is not a knob" guard ("the cap is set by
    /// prover + DA economics").
    #[serde(default = "default_prover_gas_per_sec")]
    pub prover_gas_per_sec: u64,
    /// 0 (default) = never seal a block with no transactions; N = seal an empty block at most every N seconds for a
    /// chain that wants regular timestamps even while idle. [`Self::validate`] refuses `0 < N < block_time_secs`.
    /// **Deliberately not part of [`ProfileIdentity`]** — this is the sequencer's own `[profile]` knob, not a value
    /// the batcher or derive need to agree on.
    #[serde(default)]
    pub empty_block_interval_secs: u64,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            sub_block_ms: DEFAULT_SUB_BLOCK_MS,
            sub_blocks_per_block: DEFAULT_SUB_BLOCKS_PER_BLOCK,
            blocks_per_batch: DEFAULT_BLOCKS_PER_BATCH,
            sub_block_gas_limit: DEFAULT_SUB_BLOCK_GAS_LIMIT,
            block_gas_limit: None,
            admission_shed_pct: DEFAULT_ADMISSION_SHED_PCT,
            da_bytes_per_sec: DEFAULT_DA_BYTES_PER_SEC,
            prover_gas_per_sec: DEFAULT_PROVER_GAS_PER_SEC,
            empty_block_interval_secs: DEFAULT_EMPTY_BLOCK_INTERVAL_SECS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProfileError {
    #[error("profile: sub_block_ms must be nonzero")]
    ZeroSubBlockMs,
    #[error("profile: sub_blocks_per_block must be nonzero")]
    ZeroSubBlocksPerBlock,
    #[error("profile: blocks_per_batch must be nonzero")]
    ZeroBlocksPerBatch,
    #[error("profile: sub_block_gas_limit must be nonzero")]
    ZeroSubBlockGasLimit,
    #[error("profile: admission_shed_pct must be in 1..=100, got {got}")]
    InvalidShedPct { got: u8 },
    /// EVM timestamps are integer seconds — a block time that is not a whole number of seconds cannot be published
    /// as a valid EVM block timestamp step.
    #[error(
        "profile: block time {block_time_ms} ms ({sub_block_ms} ms x {sub_blocks_per_block}) is not \
         a whole number of seconds — EVM timestamps are integer seconds"
    )]
    NotWholeSeconds {
        sub_block_ms: u64,
        sub_blocks_per_block: u16,
        block_time_ms: u128,
    },
    /// `block_gas_limit` was declared explicitly and does not equal `sub_block_gas_limit *
    /// sub_blocks_per_block`.
    #[error(
        "profile: block_gas_limit {declared} != sub_block_gas_limit ({sub_block_gas_limit}) * \
         sub_blocks_per_block ({sub_blocks_per_block}) = {expected}"
    )]
    InconsistentBlockGasLimit {
        declared: u64,
        sub_block_gas_limit: u64,
        sub_blocks_per_block: u16,
        expected: u64,
    },
    /// The profile's implied gas/s exceeds its own declared prover budget — "1G gas/s is not a knob"
    /// without a matching budget raise.
    #[error("{gas_per_sec} gas/s needs prover_gas_per_sec >= {gas_per_sec}; declared {prover_gas_per_sec}")]
    ExceedsProverBudget {
        gas_per_sec: u128,
        prover_gas_per_sec: u64,
    },
    /// The profile's implied DA rate (gas/s through the measured bytes/gas ratio) exceeds its own
    /// declared DA budget.
    #[error(
        "{gas_per_sec} gas/s needs da_bytes_per_sec >= {da_bytes_per_sec_needed}; declared \
         {da_bytes_per_sec}"
    )]
    ExceedsDaBudget {
        gas_per_sec: u128,
        da_bytes_per_sec_needed: u128,
        da_bytes_per_sec: u64,
    },
    /// A nonzero `empty_block_interval_secs` below this profile's own block time would seal empty blocks FASTER
    /// than real ones ever could — never a legal cadence. 0 (never seal empty) is always legal regardless of block
    /// time.
    #[error(
        "profile: empty_block_interval_secs {got} must be 0 or >= this profile's block time \
         ({block_time_secs}s)"
    )]
    EmptyBlockIntervalBelowBlockTime { got: u64, block_time_secs: u64 },
}

impl Profile {
    /// This profile's implied sustained gas/s: `sub_block_gas_limit` sub-blocks executed
    /// `1000 / sub_block_ms` times a second, computed in `u128` so a large `sub_block_gas_limit` at a
    /// short `sub_block_ms` cannot silently wrap.
    pub fn gas_per_sec(&self) -> u128 {
        (self.sub_block_gas_limit as u128 * 1_000) / self.sub_block_ms.max(1) as u128
    }

    /// This profile's implied DA bytes/s at [`Self::gas_per_sec`], via the measured 74 B per
    /// 21,000-gas transfer ratio (see the module doc).
    pub fn da_bytes_per_sec_needed(&self) -> u128 {
        (self.gas_per_sec() * DA_BYTES_PER_TRANSFER) / GAS_PER_TRANSFER
    }

    /// This profile's block time in milliseconds: `sub_block_ms * sub_blocks_per_block`.
    pub fn block_time_ms(&self) -> u128 {
        self.sub_block_ms as u128 * self.sub_blocks_per_block as u128
    }

    /// `block_gas_limit` after resolving the `None` (derive-from-product) case — only meaningful once
    /// [`Self::validate`] has already accepted the profile (an inconsistent explicit value is rejected
    /// there, not silently overridden here).
    pub fn effective_block_gas_limit(&self) -> u64 {
        self.block_gas_limit.unwrap_or_else(|| {
            self.sub_block_gas_limit
                .saturating_mul(self.sub_blocks_per_block as u64)
        })
    }

    /// Validate every cross-field invariant this profile must satisfy before a chain can load it.
    /// Every rejection names the offending bound, never a bare "invalid config".
    pub fn validate(&self) -> Result<(), ProfileError> {
        if self.sub_block_ms == 0 {
            return Err(ProfileError::ZeroSubBlockMs);
        }
        if self.sub_blocks_per_block == 0 {
            return Err(ProfileError::ZeroSubBlocksPerBlock);
        }
        if self.blocks_per_batch == 0 {
            return Err(ProfileError::ZeroBlocksPerBatch);
        }
        if self.sub_block_gas_limit == 0 {
            return Err(ProfileError::ZeroSubBlockGasLimit);
        }
        if self.admission_shed_pct == 0 || self.admission_shed_pct > 100 {
            return Err(ProfileError::InvalidShedPct {
                got: self.admission_shed_pct,
            });
        }

        let block_time_ms = self.block_time_ms();
        if !block_time_ms.is_multiple_of(1_000) {
            return Err(ProfileError::NotWholeSeconds {
                sub_block_ms: self.sub_block_ms,
                sub_blocks_per_block: self.sub_blocks_per_block,
                block_time_ms,
            });
        }
        let block_time_secs = (block_time_ms / 1_000) as u64;

        if self.empty_block_interval_secs != 0 && self.empty_block_interval_secs < block_time_secs {
            return Err(ProfileError::EmptyBlockIntervalBelowBlockTime {
                got: self.empty_block_interval_secs,
                block_time_secs,
            });
        }

        if let Some(declared) = self.block_gas_limit {
            let expected = self
                .sub_block_gas_limit
                .saturating_mul(self.sub_blocks_per_block as u64);
            if declared != expected {
                return Err(ProfileError::InconsistentBlockGasLimit {
                    declared,
                    sub_block_gas_limit: self.sub_block_gas_limit,
                    sub_blocks_per_block: self.sub_blocks_per_block,
                    expected,
                });
            }
        }

        let gas_per_sec = self.gas_per_sec();
        if gas_per_sec > self.prover_gas_per_sec as u128 {
            return Err(ProfileError::ExceedsProverBudget {
                gas_per_sec,
                prover_gas_per_sec: self.prover_gas_per_sec,
            });
        }

        let da_bytes_per_sec_needed = self.da_bytes_per_sec_needed();
        if da_bytes_per_sec_needed > self.da_bytes_per_sec as u128 {
            return Err(ProfileError::ExceedsDaBudget {
                gas_per_sec,
                da_bytes_per_sec_needed,
                da_bytes_per_sec: self.da_bytes_per_sec,
            });
        }

        Ok(())
    }
}

/// The subset of a chain's identity persisted beside the ordered log as `profile.json`: everything a restart must
/// agree with before touching the log at all. `chain_id` lives on the sequencer's own `Config`, not [`Profile`]
/// itself, so it is threaded in separately by [`Self::new`] rather than duplicated onto `Profile`.
///
/// Persisted as plain JSON (not TOML — this file is written by the process, never hand-edited) beside
/// the log directory's own segment files. A restart compares the stored identity against the
/// **configured** one (i.e. after every env-var override the caller's own config loader already
/// applied) — a config whose profile changed since the log was created must be refused before any
/// replay work runs, not discovered mid-replay as a divergent head. See
/// `rome_zk_sequencer::recovery::reconcile_profile_identity`, which owns the sequencer-specific
/// orchestration (migration flags, the ordered-log numbering-origin check) around the pure read/write
/// primitives this crate provides ([`crate::read_profile_json`], [`crate::write_profile_identity`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileIdentity {
    pub chain_id: u64,
    pub sub_block_ms: u64,
    pub sub_blocks_per_block: u16,
    pub sub_block_gas_limit: u64,
    /// [`Profile::effective_block_gas_limit`] — the resolved value, never the raw `Option`, so an
    /// operator relying on the derive-from-product default (`block_gas_limit` omitted in TOML) still
    /// gets a real number to compare against on the next restart.
    pub block_gas_limit: u64,
    /// `blocks_per_batch` joins the persisted identity — the batcher reads this field
    /// (`rome_zk_batcher::config::read_profile_identity`) instead of owning its own copy that could
    /// silently drift from the sequencer's `[profile]` table. A `profile.json` written before this field
    /// existed lacks this key entirely; that is handled explicitly by the file layer in this crate
    /// (never defaulted here — deliberately no `#[serde(default)]`: a missing field must surface as a
    /// named migration refusal, not a silent 0 or the configured value slipping in unheard).
    pub blocks_per_batch: u64,
    /// The ordered log's numbering origin — always `1` (genesis, block 0, is never sealed). Joins the persisted
    /// identity for the same reason `blocks_per_batch` did: a `profile.json` written by an older binary (0-based
    /// numbering) lacks this key entirely, and that must surface as a named migration refusal — the cheapest place
    /// to catch an old log, before any replay work touches it — rather than being silently accepted and replayed
    /// 0-based forever. Deliberately no `#[serde(default)]`, for the same reason `blocks_per_batch` has none.
    pub first_block: u64,
}

/// The sequencer's first-ever sealed block, for every chain — never a per-profile knob (there is exactly
/// one legal numbering origin, unlike `blocks_per_batch`), but persisted anyway so an old (0-based) log's
/// `profile.json` is missing it and can be refused by name.
pub const FIRST_BLOCK: u64 = 1;

impl ProfileIdentity {
    pub fn new(chain_id: u64, profile: &Profile) -> Self {
        Self {
            chain_id,
            sub_block_ms: profile.sub_block_ms,
            sub_blocks_per_block: profile.sub_blocks_per_block,
            sub_block_gas_limit: profile.sub_block_gas_limit,
            block_gas_limit: profile.effective_block_gas_limit(),
            blocks_per_batch: profile.blocks_per_batch as u64,
            first_block: FIRST_BLOCK,
        }
    }

    /// The first field (in a fixed, documented order) that differs between `self` (the value stored in
    /// `profile.json`) and `configured` (today's config, after env overrides) — `None` if every field
    /// agrees. Named-field-first order (`chain_id` checked before the cadence/gas fields) means a log
    /// pointed at from the wrong chain's config is reported as a chain_id mismatch, never masked by an
    /// incidental gas-limit difference between two unrelated chains' defaults.
    pub fn first_mismatch(&self, configured: &Self) -> Option<(&'static str, String, String)> {
        if self.chain_id != configured.chain_id {
            return Some((
                "chain_id",
                self.chain_id.to_string(),
                configured.chain_id.to_string(),
            ));
        }
        if self.sub_block_ms != configured.sub_block_ms {
            return Some((
                "sub_block_ms",
                self.sub_block_ms.to_string(),
                configured.sub_block_ms.to_string(),
            ));
        }
        if self.sub_blocks_per_block != configured.sub_blocks_per_block {
            return Some((
                "sub_blocks_per_block",
                self.sub_blocks_per_block.to_string(),
                configured.sub_blocks_per_block.to_string(),
            ));
        }
        if self.sub_block_gas_limit != configured.sub_block_gas_limit {
            return Some((
                "sub_block_gas_limit",
                self.sub_block_gas_limit.to_string(),
                configured.sub_block_gas_limit.to_string(),
            ));
        }
        if self.block_gas_limit != configured.block_gas_limit {
            return Some((
                "block_gas_limit",
                self.block_gas_limit.to_string(),
                configured.block_gas_limit.to_string(),
            ));
        }
        if self.blocks_per_batch != configured.blocks_per_batch {
            return Some((
                "blocks_per_batch",
                self.blocks_per_batch.to_string(),
                configured.blocks_per_batch.to_string(),
            ));
        }
        if self.first_block != configured.first_block {
            return Some((
                "first_block",
                self.first_block.to_string(),
                configured.first_block.to_string(),
            ));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_profile_validates() {
        Profile::default().validate().unwrap();
    }

    /// This crate is the numeric home, so the pin is against its own literal defaults, not
    /// `rome-zk-sequencer::sealer`/`::executor` (those crates re-export these values — see their own doc comments
    /// and `rome-zk-sequencer`'s `default_profile_matches_the_sealer_and_executor_re_exports` regression test for
    /// the cross-check the other direction).
    #[test]
    fn default_profile_matches_the_pinned_design_defaults() {
        let p = Profile::default();
        assert_eq!(p.sub_block_ms, 50);
        assert_eq!(p.sub_blocks_per_block, DEFAULT_SUB_BLOCKS_PER_BLOCK);
        assert_eq!(p.sub_block_gas_limit, DEFAULT_SUB_BLOCK_GAS_LIMIT);
        assert_eq!(
            p.effective_block_gas_limit(),
            DEFAULT_SUB_BLOCK_GAS_LIMIT * DEFAULT_SUB_BLOCKS_PER_BLOCK as u64
        );
    }

    /// The design pins the timestamp drift bound at 60 s — changing it is a deliberate design change, not a
    /// silent default drift.
    #[test]
    fn default_max_drift_secs_matches_the_pinned_value() {
        assert_eq!(DEFAULT_MAX_DRIFT_SECS, 60);
    }

    /// Tiber's own default is 0 (never seal a block with no transactions) — a config that omits `[profile]`
    /// entirely, or omits just this field, must load with the idle gate OFF, not some nonzero cadence nobody asked
    /// for.
    #[test]
    fn default_profile_never_seals_an_empty_block() {
        assert_eq!(Profile::default().empty_block_interval_secs, 0);
        assert_eq!(DEFAULT_EMPTY_BLOCK_INTERVAL_SECS, 0);
    }

    /// An interval below this profile's own block time is refused, naming the offending bound — sealing an empty
    /// block FASTER than a real one ever could is never legal.
    #[test]
    fn profile_refuses_interval_below_block_time() {
        let p = Profile {
            empty_block_interval_secs: 1,
            sub_block_ms: 50,
            sub_blocks_per_block: 40, // block_time_secs = 2
            ..Profile::default()
        };
        let err = p.validate().unwrap_err();
        assert_eq!(
            err,
            ProfileError::EmptyBlockIntervalBelowBlockTime {
                got: 1,
                block_time_secs: 2,
            }
        );
    }

    /// An interval exactly equal to the block time is the minimum legal nonzero value.
    #[test]
    fn an_interval_equal_to_block_time_validates() {
        let p = Profile {
            empty_block_interval_secs: 1, // the default profile's own block time is 1s
            ..Profile::default()
        };
        p.validate().unwrap();
    }

    /// 0 is always legal, regardless of block time — "never seal empty" is not a cadence to compare
    /// against the block time at all.
    #[test]
    fn zero_interval_is_always_valid_regardless_of_block_time() {
        let p = Profile {
            empty_block_interval_secs: 0,
            sub_block_ms: 500,
            sub_blocks_per_block: 4, // block_time_secs = 2s — 0 would still be legal at 1..1 too
            ..Profile::default()
        };
        p.validate().unwrap();
    }

    /// `ProfileIdentity`'s persisted JSON shape must stay exactly the seven fields it always had —
    /// `empty_block_interval_secs` is a `Profile`-only knob, not a value the batcher/derive restart check needs to
    /// agree on. **Mutation** (add the field to `ProfileIdentity`, temporarily, to prove this catches it): the key
    /// set gains an eighth entry and this assertion goes red.
    #[test]
    fn profile_identity_shape_excludes_the_idle_knob() {
        let profile = Profile {
            empty_block_interval_secs: 5,
            ..Profile::default()
        };
        let identity = ProfileIdentity::new(200_101, &profile);
        let value = serde_json::to_value(identity).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "block_gas_limit",
                "blocks_per_batch",
                "chain_id",
                "first_block",
                "sub_block_gas_limit",
                "sub_block_ms",
                "sub_blocks_per_block",
            ],
            "ProfileIdentity's persisted shape must not gain empty_block_interval_secs"
        );
    }

    /// A profile with 30 ms x 20 (0.6 s block) is rejected with the whole-seconds error.
    #[test]
    fn thirty_ms_times_twenty_is_rejected_not_a_whole_second() {
        let p = Profile {
            sub_block_ms: 30,
            sub_blocks_per_block: 20,
            ..Profile::default()
        };
        let err = p.validate().unwrap_err();
        assert_eq!(
            err,
            ProfileError::NotWholeSeconds {
                sub_block_ms: 30,
                sub_blocks_per_block: 20,
                block_time_ms: 600,
            }
        );
    }

    /// 25 ms x 40 = 1,000 ms — a whole second — must validate. At the default 5,000,000 sub_block_gas_limit this
    /// doubles gas/s to 200M, so the budgets must rise to match (this test's own choice, not a design premise) —
    /// proving the whole-seconds check is independent of the throughput checks.
    #[test]
    fn twenty_five_ms_times_forty_is_a_whole_second_and_validates() {
        let p = Profile {
            sub_block_ms: 25,
            sub_blocks_per_block: 40,
            prover_gas_per_sec: 200_000_000,
            da_bytes_per_sec: ((200_000_000u128 * 74) / 21_000) as u64,
            ..Profile::default()
        };
        assert_eq!(p.block_time_ms(), 1_000);
        assert_eq!(p.gas_per_sec(), 200_000_000);
        p.validate().unwrap();
    }

    /// A 1G gas/s profile without matching budgets is rejected naming the bound;
    /// with budgets declared it loads.
    #[test]
    fn one_giga_gas_per_sec_is_rejected_unless_the_prover_budget_matches() {
        // 100,000,000 gas per sub-block at the default 50ms period = 2,000,000,000 gas/s... use exactly
        // 1,000,000,000 gas/s as the example: sub_block_gas_limit = 50,000,000 at 50ms.
        let p = Profile {
            sub_block_gas_limit: 50_000_000,
            ..Profile::default()
        };
        assert_eq!(p.gas_per_sec(), 1_000_000_000);
        let err = p.validate().unwrap_err();
        assert_eq!(
            err,
            ProfileError::ExceedsProverBudget {
                gas_per_sec: 1_000_000_000,
                prover_gas_per_sec: DEFAULT_PROVER_GAS_PER_SEC,
            }
        );
        assert_eq!(
            err.to_string(),
            "1000000000 gas/s needs prover_gas_per_sec >= 1000000000; declared 100000000"
        );

        // Raising the declared prover (and DA) budgets to match must let it load.
        let p_with_budget = Profile {
            sub_block_gas_limit: 50_000_000,
            prover_gas_per_sec: 1_000_000_000,
            da_bytes_per_sec: p_with_budget_da(),
            ..Profile::default()
        };
        p_with_budget.validate().unwrap();
    }

    fn p_with_budget_da() -> u64 {
        // 1,000,000,000 gas/s * 74 / 21,000 rounded down.
        ((1_000_000_000u128 * 74) / 21_000) as u64
    }

    /// A profile that raises the prover budget but not the DA budget is still rejected — naming the DA
    /// bound specifically, not the prover one.
    #[test]
    fn exceeding_only_the_da_budget_is_rejected_naming_da() {
        let p = Profile {
            sub_block_gas_limit: 50_000_000,
            prover_gas_per_sec: 1_000_000_000,
            // da_bytes_per_sec left at the (too-small) default.
            ..Profile::default()
        };
        let err = p.validate().unwrap_err();
        assert!(
            matches!(err, ProfileError::ExceedsDaBudget { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn explicit_block_gas_limit_matching_the_product_validates() {
        let p = Profile {
            block_gas_limit: Some(
                DEFAULT_SUB_BLOCK_GAS_LIMIT * DEFAULT_SUB_BLOCKS_PER_BLOCK as u64,
            ),
            ..Profile::default()
        };
        p.validate().unwrap();
    }

    #[test]
    fn explicit_block_gas_limit_not_matching_the_product_is_rejected() {
        let p = Profile {
            block_gas_limit: Some(1),
            ..Profile::default()
        };
        let err = p.validate().unwrap_err();
        assert!(
            matches!(err, ProfileError::InconsistentBlockGasLimit { .. }),
            "{err:?}"
        );
    }

    /// Profile round-trips through TOML.
    #[test]
    fn profile_round_trips_through_toml() {
        let toml_str = r#"
            sub_block_ms = 25
            sub_blocks_per_block = 40
            blocks_per_batch = 10
            sub_block_gas_limit = 5000000
            admission_shed_pct = 70
            da_bytes_per_sec = 1000000
            prover_gas_per_sec = 200000000
        "#;
        let p: Profile = toml::from_str(toml_str).unwrap();
        assert_eq!(p.sub_block_ms, 25);
        assert_eq!(p.sub_blocks_per_block, 40);
        p.validate().unwrap();
    }

    #[test]
    fn empty_profile_table_takes_every_default() {
        let p: Profile = toml::from_str("").unwrap();
        assert_eq!(p, Profile::default());
    }

    /// The operator's deploy script substitutes `[profile].empty_block_interval_secs` from
    /// `EMPTY_BLOCK_INTERVAL_SECS` (the deploy environment's own default, 0) into the real, committed
    /// `crates/rome-zk-sequencer/config.example.toml` — this proves that substitution's own output (the exact
    /// `[profile]` table Tiber ships) parses and validates through this crate's `Profile`, the same struct
    /// `Config::load` uses. A rename or shape change to the `[profile]` table that the script's sed line no
    /// longer targets breaks this test, not only the deploy scripts' own shell-level render check.
    #[test]
    fn tiber_rendered_profile_table_validates() {
        let example_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../rome-zk-sequencer/config.example.toml");
        let example = std::fs::read_to_string(&example_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", example_path.display()));
        assert!(
            example.contains("empty_block_interval_secs = 0"),
            "config.example.toml must still carry the literal line the deploy script's sed \
             (`s|^empty_block_interval_secs = .*|empty_block_interval_secs = ${{val}}|`) targets"
        );
        // The deploy script's own sed substitutes EMPTY_BLOCK_INTERVAL_SECS from the deploy environment
        // file — whose default (0) is exactly what config.example.toml's own literal already reads, so this
        // render is a no-op on the value; the assert above pins the literal the sed targets.
        // Split on the real table HEADERS (each on its own line), not a mention of "[profile]" inside
        // this same file's own header comment ("the [profile], [admission] and [rpc] tables are...").
        let profile_table = example
            .split_once("\n[profile]\n")
            .expect("config.example.toml must have a [profile] table header")
            .1
            .split_once("\n[admission]\n")
            .expect("config.example.toml must have an [admission] table header after [profile]")
            .0;
        let p: Profile = toml::from_str(profile_table)
            .unwrap_or_else(|e| panic!("rendered [profile] table must parse: {e}"));
        assert_eq!(p.empty_block_interval_secs, 0);
        assert_eq!(p.blocks_per_batch, 60);
        p.validate()
            .expect("Tiber's rendered profile must validate");
    }

    /// The test above renders `EMPTY_BLOCK_INTERVAL_SECS`'s own DEFAULT (0) into
    /// `config.example.toml` — a no-op on the value, so it would still pass even if
    /// the deploy script's own sed substitution were deleted entirely (the literal in the committed
    /// file is already 0). This test mirrors that SAME sed substitution
    /// (`s|^empty_block_interval_secs = .*|empty_block_interval_secs = ${val}|`) with a NON-default
    /// value, so dropping the substitution is what turns it red.
    #[test]
    fn render_config_substitution_lands_a_non_default_empty_block_interval() {
        let example_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../rome-zk-sequencer/config.example.toml");
        let example = std::fs::read_to_string(&example_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", example_path.display()));
        let profile_table = example
            .split_once("\n[profile]\n")
            .expect("config.example.toml must have a [profile] table header")
            .1
            .split_once("\n[admission]\n")
            .expect("config.example.toml must have an [admission] table header after [profile]")
            .0;
        // The deploy script's own line, applied here with val=5 (never the example's own default of 0,
        // so a dropped substitution is visible): `s|^empty_block_interval_secs = .*|empty_block_interval_secs = ${val}|`
        let rendered = profile_table
            .lines()
            .map(|line| {
                if line.starts_with("empty_block_interval_secs = ") {
                    "empty_block_interval_secs = 5".to_string()
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("empty_block_interval_secs = 5"),
            "test setup: the substitution above must have found a line to replace"
        );
        let p: Profile = toml::from_str(&rendered)
            .unwrap_or_else(|e| panic!("rendered [profile] table must parse: {e}"));
        assert_eq!(p.empty_block_interval_secs, 5);
        p.validate()
            .expect("a non-default empty_block_interval_secs (5, well above the 1s default block time) must still validate");
    }

    /// Same substitution, at a value BELOW a 2s block time (`sub_block_ms` 100 x `sub_blocks_per_block`
    /// 20) — refused by name, not silently accepted.
    #[test]
    fn render_config_substitution_below_block_time_is_refused_by_name() {
        let example_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../rome-zk-sequencer/config.example.toml");
        let example = std::fs::read_to_string(&example_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", example_path.display()));
        let profile_table = example
            .split_once("\n[profile]\n")
            .expect("config.example.toml must have a [profile] table header")
            .1
            .split_once("\n[admission]\n")
            .expect("config.example.toml must have an [admission] table header after [profile]")
            .0;
        let rendered = profile_table
            .lines()
            .map(|line| {
                if line.starts_with("empty_block_interval_secs = ") {
                    "empty_block_interval_secs = 1".to_string()
                } else if line.starts_with("sub_block_ms = ") {
                    "sub_block_ms = 100".to_string()
                } else if line.starts_with("sub_blocks_per_block = ") {
                    "sub_blocks_per_block = 20".to_string()
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let p: Profile = toml::from_str(&rendered)
            .unwrap_or_else(|e| panic!("rendered [profile] table must parse: {e}"));
        assert_eq!(p.sub_block_ms, 100);
        assert_eq!(p.sub_blocks_per_block, 20); // block time = 100ms x 20 = 2s
        assert_eq!(
            p.validate(),
            Err(ProfileError::EmptyBlockIntervalBelowBlockTime {
                got: 1,
                block_time_secs: 2,
            })
        );
    }

    #[test]
    fn profile_identity_new_reads_effective_block_gas_limit_not_the_raw_option() {
        let profile = Profile {
            block_gas_limit: None,
            ..Profile::default()
        };
        let identity = ProfileIdentity::new(200_101, &profile);
        assert_eq!(identity.chain_id, 200_101);
        assert_eq!(
            identity.block_gas_limit,
            profile.effective_block_gas_limit()
        );
    }

    #[test]
    fn identical_identities_have_no_mismatch() {
        let identity = ProfileIdentity::new(200_101, &Profile::default());
        assert_eq!(identity.first_mismatch(&identity), None);
    }

    #[test]
    fn a_different_chain_id_is_reported_first_even_alongside_other_differences() {
        let stored = ProfileIdentity::new(1, &Profile::default());
        let configured = ProfileIdentity {
            chain_id: 2,
            sub_block_gas_limit: stored.sub_block_gas_limit * 2,
            ..stored
        };
        let (field, s, c) = stored.first_mismatch(&configured).unwrap();
        assert_eq!(field, "chain_id");
        assert_eq!(s, "1");
        assert_eq!(c, "2");
    }

    #[test]
    fn a_changed_gas_limit_is_named() {
        let stored = ProfileIdentity::new(200_101, &Profile::default());
        let configured = ProfileIdentity {
            sub_block_gas_limit: 2_000_000,
            block_gas_limit: 40_000_000,
            ..stored
        };
        let (field, stored_v, configured_v) = stored.first_mismatch(&configured).unwrap();
        assert_eq!(field, "sub_block_gas_limit");
        assert_eq!(stored_v, "5000000");
        assert_eq!(configured_v, "2000000");
    }

    /// `ProfileIdentity::new` reads `blocks_per_batch` from the profile, not a bare 0/default that would happen to
    /// match every other profile's field. Changing `DEFAULT_BLOCKS_PER_BATCH` from 10 to some other value would
    /// turn this test red only if it stopped asserting a hardcoded literal — this test intentionally passes its own
    /// explicit `7`, independent of the default, to isolate the read path; the default itself is pinned by
    /// `a_changed_blocks_per_batch_is_named` below and by every downstream crate's own pin test.
    #[test]
    fn profile_identity_new_reads_blocks_per_batch_from_the_profile() {
        let profile = Profile {
            blocks_per_batch: 7,
            ..Profile::default()
        };
        let identity = ProfileIdentity::new(200_101, &profile);
        assert_eq!(identity.blocks_per_batch, 7);
    }

    /// A restart whose configured `blocks_per_batch` disagrees with the stored identity is refused, naming
    /// `blocks_per_batch` specifically — not masked by every other field agreeing. Also pins the default to 10
    /// (bumping `DEFAULT_BLOCKS_PER_BATCH` to 11 turns this assertion red).
    #[test]
    fn a_changed_blocks_per_batch_is_named() {
        let stored = ProfileIdentity::new(200_101, &Profile::default());
        assert_eq!(stored.blocks_per_batch, 10);
        let configured = ProfileIdentity {
            blocks_per_batch: 5,
            ..stored
        };
        let (field, stored_v, configured_v) = stored.first_mismatch(&configured).unwrap();
        assert_eq!(field, "blocks_per_batch");
        assert_eq!(stored_v, "10");
        assert_eq!(configured_v, "5");
    }

    /// `ProfileIdentity::new` always records the fixed numbering origin (`FIRST_BLOCK` = 1) — there is exactly one
    /// legal value, unlike `blocks_per_batch`, but it must still be present on the struct so a `profile.json`
    /// written before that field existed (missing it entirely) can be told apart from one that has it.
    #[test]
    fn profile_identity_new_always_records_first_block_as_one() {
        let identity = ProfileIdentity::new(200_101, &Profile::default());
        assert_eq!(identity.first_block, FIRST_BLOCK);
        assert_eq!(FIRST_BLOCK, 1);
    }

    /// A stored identity whose `first_block` disagrees with configured is refused, naming `first_block`
    /// specifically — not masked by every other field agreeing. Skipping the `first_block` branch in
    /// `first_mismatch` turns this test red.
    #[test]
    fn a_changed_first_block_is_named() {
        let stored = ProfileIdentity::new(200_101, &Profile::default());
        let configured = ProfileIdentity {
            first_block: 0,
            ..stored
        };
        let (field, stored_v, configured_v) = stored.first_mismatch(&configured).unwrap();
        assert_eq!(field, "first_block");
        assert_eq!(stored_v, "1");
        assert_eq!(configured_v, "0");
    }
}
