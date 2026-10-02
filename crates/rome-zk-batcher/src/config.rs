//! TOML config (+ env overrides left to the binary, following `rome-zk-sequencer::config`'s pattern):
//! chain id, inbox/settlement program ids, log dir, batching cadence, sender tuning, cluster.
//!
//! The payer key is never in this file or the repo — only a **path**, resolved at startup (mirrors
//! `rome-zk-sequencer::config::Config::sequencer_key_path`).

use serde::Deserialize;
use solana_program::pubkey::Pubkey;
use std::net::SocketAddr;
use std::path::PathBuf;

use crate::channel::DEFAULT_MAX_FRAME_BODY_LEN;

/// The design's own default (`channel::DEFAULT_MAX_FRAMES_PER_CHANNEL`) — restored now that `GrowBatch`
/// lets `OpenBatch` reach any `expected_count`, not only the ≤312-leaf
/// ceiling a single `create_account` CPI can create in one shot (the
/// batcher now issues `OpenBatch` + however many `GrowBatch` calls are needed in one transaction — see
/// `pipeline::open_and_grow_batch`).
fn default_max_frames_per_batch() -> usize {
    crate::channel::DEFAULT_MAX_FRAMES_PER_CHANNEL
}
fn default_max_frame_body_len() -> usize {
    DEFAULT_MAX_FRAME_BODY_LEN
}
/// `in_flight` is a bound on outstanding **transactions**, and every real frame is now
/// exactly one transaction (`Open`+`Write`+`Seal`+`SealLeaf` in a single V1 tx, no split) — so this is
/// directly a bound on outstanding frames. 240 is carried over from the original headroom
/// target ("≈ 240 frames in flight"); nothing about V1 lowers that target, it simply no longer needs
/// multiplying by a per-frame stage width (that width was always 1 already for the one-transaction case,
/// and every frame is now that case).
const TARGET_FRAMES_IN_FLIGHT: usize = 240;

/// = [`TARGET_FRAMES_IN_FLIGHT`] (one V1 transaction per frame, so "outstanding
/// transactions" and "outstanding frames" are the same count — the old per-frame stage-width multiplier,
/// `design_frame_max_stage_width`, is withdrawn along with the split plan it measured).
fn default_in_flight() -> usize {
    TARGET_FRAMES_IN_FLIGHT
}

/// Solana's own binding ceiling on chunk-lane throughput: the runtime's cost accounting charges every writable account
/// the transaction's *requested* compute limit, not what it actually consumes (`cost_model.rs:211` in upstream
/// `agave`'s `solana-cost-model`), and caps each writable account at `MAX_WRITABLE_ACCOUNT_UNITS` CU per ~0.4 s block
/// (`block_cost_limits.rs:48`) — **which generation of `agave` a cluster runs decides that cap**: 12,000,000 through
/// 2.1.6/3.x, raised to 24,000,000 on 4.x (devnet has been observed on `4.3.0-rc.0`; see [`AgaveGeneration`]). Every
/// chunk-lane transaction — one V1 tx per frame, and `OpenBatch`+`GrowBatch` — write-locks the fee payer, so the payer
/// account itself is this ceiling's bottleneck: at most `max_writable_account_units / tx_cost_units(..)` such
/// transactions land in any one block, independent of `in_flight`. Returns the resulting frames/s bound at
/// `txs_per_frame` chunk-lane transactions per frame (always 1 with one V1 tx per frame, kept as a parameter so the
/// formula stays honest about what it assumes rather than hard-coding the current shape into its own name).
pub fn payer_cap_frames_per_sec(
    generation: AgaveGeneration,
    compute_unit_limit: u32,
    writable_accounts: u32,
    instruction_data_bytes: u16,
    loaded_accounts_data_size_limit: u32,
    txs_per_frame: u32,
) -> f64 {
    const BLOCK_TIME_SECS: f64 = 0.4;
    let cost = tx_cost_units(
        compute_unit_limit,
        writable_accounts,
        instruction_data_bytes,
        loaded_accounts_data_size_limit,
    ) as f64;
    let txs_per_block = generation.max_writable_account_units() as f64 / cost;
    let tx_per_sec = txs_per_block / BLOCK_TIME_SECS;
    tx_per_sec / txs_per_frame as f64
}

/// Which generation of `agave` the target cluster runs — settles `MAX_WRITABLE_ACCOUNT_UNITS`. This is a fact about
/// the cluster's own validator software, not something this crate can derive from a config value, so it is named
/// explicitly rather than silently assumed; [`TARGET_AGAVE_GENERATION`] is the conservative default this crate's own
/// tests and docs use until a gate run's own `txs/block` measurement settles which one devnet actually enforces (the
/// gate run records txs/block to settle which limit devnet applies).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgaveGeneration {
    /// `solana-program`/`agave` 2.1.6 through 3.x — `MAX_WRITABLE_ACCOUNT_UNITS` = 12,000,000.
    Pre4x,
    /// `agave` 4.x (devnet observed on `4.3.0-rc.0`) — raised to 24,000,000.
    Agave4x,
}

impl AgaveGeneration {
    pub const fn max_writable_account_units(self) -> u64 {
        match self {
            AgaveGeneration::Pre4x => 12_000_000,
            AgaveGeneration::Agave4x => 24_000_000,
        }
    }
}

/// The conservative default until a gate run settles which generation devnet is actually enforcing
/// (see [`AgaveGeneration`]'s own doc) — the lower, pre-4.x cap, so a capacity estimate built from this
/// default under-promises rather than over-promises throughput.
pub const TARGET_AGAVE_GENERATION: AgaveGeneration = AgaveGeneration::Pre4x;

/// Solana's real per-transaction cost-model terms, summed into the same CU-denominated units
/// `MAX_WRITABLE_ACCOUNT_UNITS` caps: `compute_unit_limit` (the transaction's own requested limit, charged in full
/// whatever it actually consumes), plus 720 CU per signature (this crate's transactions always carry exactly one, the
/// fee payer's), plus 300 CU per write-locked account, plus the *instruction*-data term (see `instruction_data_bytes`'s
/// own doc below — **not** a function of the transaction's serialized wire size, `agave`'s `solana-cost-model` never
/// charges that), plus `8 * ceil(loaded_accounts_data_size_limit / 32 KiB)` for the accounts it loads. Previously
/// `payer_cap_frames_per_sec` divided only by `compute_unit_limit`, ignoring every one of these other cost terms
/// `solana-cost-model` actually charges a transaction — pinned by this crate's own tests against the design frame
/// (`config.rs` tests) rather than re-derived by eye each time a term changes.
///
/// `instruction_data_bytes`: the u16 **saturating sum of every instruction's own `data.len()`** in the
/// transaction — `solana-cost-model` 4.2.1 `src/cost_model.rs:183-185`
/// (`get_instructions_data_cost`: `transaction.instruction_data_len() / (INSTRUCTION_DATA_BYTES_COST as
/// u16)`) and `solana-runtime-transaction` 4.2.1 `src/instruction_data_len.rs:9-11`
/// (`InstructionDataLenBuilder::process_instruction`: `self.value = self.value.saturating_add
/// (instruction.data.len() as u16)`). This term is **never** the transaction's serialized wire size —
/// accounts, signatures, and the message's own Compact-Array framing are not part of it — and the earlier
/// version of this function charged exactly that wrong quantity (the design frame's ~4,052-B wire size)
/// divided by 140, rather than the ~3,776 B of real instruction-data payload divided by 4.
pub fn tx_cost_units(
    compute_unit_limit: u32,
    writable_accounts: u32,
    instruction_data_bytes: u16,
    loaded_accounts_data_size_limit: u32,
) -> u64 {
    const SIGNATURE_COST_UNITS: u64 = 720;
    const WRITE_LOCK_COST_UNITS: u64 = 300;
    // solana-cost-model 4.2.1 src/block_cost_limits.rs:7 (`COMPUTE_UNIT_TO_US_RATIO: u64 = 30`) and :20
    // (`INSTRUCTION_DATA_BYTES_COST: u64 = 140 /*bytes per us*/ / COMPUTE_UNIT_TO_US_RATIO`) — this divides
    // out to 4 by ordinary `u64` integer division (140 / 30 = 4, truncated), and `get_instructions_data_cost`
    // (src/cost_model.rs:183-185) applies it to `instruction_data_len()` with the same truncating division,
    // never `div_ceil`.
    const INSTRUCTION_DATA_BYTES_COST: u64 = 140 / 30;
    const LOADED_ACCOUNTS_PAGE_BYTES: u64 = 32 * 1024;
    const LOADED_ACCOUNTS_PAGE_COST_UNITS: u64 = 8;

    let data_cost = (instruction_data_bytes as u64) / INSTRUCTION_DATA_BYTES_COST;
    let loaded_pages =
        (loaded_accounts_data_size_limit as u64).div_ceil(LOADED_ACCOUNTS_PAGE_BYTES);

    compute_unit_limit as u64
        + SIGNATURE_COST_UNITS
        + WRITE_LOCK_COST_UNITS * writable_accounts as u64
        + data_cost
        + LOADED_ACCOUNTS_PAGE_COST_UNITS * loaded_pages
}

/// The design frame's own real instruction-data byte sum —
/// **not** the transaction's serialized wire size (`sender.rs`'s
/// `design_frame_v1_tx_fits_4096_and_carries_both_header_limits` test proves that separately, against the
/// 4,096-B envelope limit; a different quantity from what `agave`'s cost model charges). Builds the real
/// `Open`+`Write`+`Seal`+`SealLeaf` plan (`pipeline::plan_chunk`) for one frame at the design's own
/// `channel::DEFAULT_MAX_FRAME_BODY_LEN`, then sums each instruction's own `data.len()` exactly as agave's
/// `InstructionDataLenBuilder` does (see `tx_cost_units`'s own doc for the source cite). A signed/
/// serialized V1 tx's instruction data is byte-identical whether reached through `plan_chunk`'s legacy
/// `Instruction`s or the V1 conversion (`sender::compat::to_v1_instruction` copies `data` verbatim), so
/// summing over the legacy list here is exactly the real term — never hand-computed or re-derived by eye.
#[cfg(test)]
fn design_frame_instruction_data_bytes() -> u16 {
    let placeholder = Pubkey::new_from_array([1u8; 32]);
    let payload =
        vec![0u8; crate::channel::FRAME_HEADER_LEN + crate::channel::DEFAULT_MAX_FRAME_BODY_LEN];
    let ixs = crate::pipeline::plan_chunk(&placeholder, &placeholder, 200_101, 0, 0, &payload);
    ixs.iter()
        .fold(0u16, |acc, ix| acc.saturating_add(ix.data.len() as u16))
}
/// Every chunk-lane transaction write-locks exactly these three accounts (the four-
/// instruction `Open`+`Write`+`Seal`+`SealLeaf` plan, `pipeline::plan_chunk`): the fee payer, the chunk
/// PDA, and the batch account (writable only because `SealLeaf` writes it — `Open`/`Write`/`Seal` only
/// ever read it). `system_program` stays read-only throughout.
#[cfg(test)]
const DESIGN_FRAME_WRITABLE_ACCOUNTS: u32 = 3;

/// `getSignatureStatuses` calls per outstanding-signature batch — capped at the RPC node's own hard cap
/// ([`crate::sender::MAX_SIGNATURE_STATUSES_PER_CALL`]) by `sender::run_send_and_confirm_many` regardless
/// of this value, so a config authoring a larger number is clamped, never rejected.
fn default_signature_status_batch_size() -> usize {
    crate::sender::MAX_SIGNATURE_STATUSES_PER_CALL
}
fn default_priority_fee_micro_lamports() -> u64 {
    // Starting point; bounded growth on resubmit (see `sender.rs`).
    1_000
}
fn default_max_priority_fee_micro_lamports() -> u64 {
    200_000
}
fn default_compute_unit_limit() -> u32 {
    200_000
}
/// Every chunk-lane transaction (`Open`+`Write`+`Seal`+`SealLeaf` in one V1 tx, and `OpenBatch`+`GrowBatch`)
/// write-locks the fee payer, and Solana's cost accounting charges each writable account the tx's own *requested* limit
/// plus its signature/write-lock/data/loaded-accounts terms (`tx_cost_units` above), capped at
/// `MAX_WRITABLE_ACCOUNT_UNITS` CU per block (12,000,000 through agave 3.x, 24,000,000 on 4.x — see
/// [`AgaveGeneration`]). Measured chunk-lane CU (combined 4-ix `Open`+`Write`+`Seal`+`SealLeaf`): 15.7k;
/// `OpenBatch`+`GrowBatch(900)` 20.2k — 40,000 (≈2x the largest measured) at the design frame's own instruction-data
/// size and write-lock count (`payer_cap_frames_per_sec(TARGET_AGAVE_GENERATION, 40_000,
/// DESIGN_FRAME_WRITABLE_ACCOUNTS, design_frame_instruction_data_bytes(), 262_144, 1)`) clears **≈704 frames/s at
/// [`AgaveGeneration::Pre4x`]'s 12,000,000 cap, ≈1,408 at [`AgaveGeneration::Agave4x`]'s 24,000,000** (corrected from
/// an earlier ≈719/≈1,438 that charged the transaction's wire size instead of its instruction-data size) — this crate's
/// own tests pin both numbers (see `payer_cap_frames_per_sec_pins_the_design_frames_cost_at_both_agave_generations`).
/// `FinalizeBatch` keeps its own separate, much higher limit (`finalize_compute_unit_limit` below) — it is not part of
/// the chunk lane and is sent once per batch, not once per frame.
fn default_chunk_compute_unit_limit() -> u32 {
    40_000
}
/// `FinalizeBatch` alone needs its own, much higher compute-unit limit than every other instruction this batcher sends
/// (`OpenBatch`/`GrowBatch`/chunk `Open`+`Write`+`Seal`/`SealLeaf`, all cheap and covered by `compute_unit_limit`
/// above): measured 334,459 CU to finalize a 900-leaf batch on SBPF v3 (~370 CU/leaf; 372,951 CU before the dependency
/// bump), so `compute_unit_limit`'s 200,000 default fails every batch above ~530 frames outright (every one of the 120
/// confirm-polls in `finalize_and_verify` would see the same CU-exceeded failure, surfacing as `LeavesNeverComplete` —
/// a stuck, sealed-but-never-finalized batch). 600,000 covers the design's own 900-frame default with headroom; a chain
/// configured for a larger `max_frames_per_batch` must raise this too (see `pipeline::finalize_and_verify`'s call site
/// in the binary, which uses this — not `compute_unit_limit` — for the `FinalizeBatch` send only).
fn default_finalize_compute_unit_limit() -> u32 {
    600_000
}
/// 15 s (was 60), the fast-finality bound — see `rome_zk_solana_sender::DEFAULT_CONFIRM_TIMEOUT_SECS`.
fn default_confirm_timeout_secs() -> u64 {
    rome_zk_solana_sender::DEFAULT_CONFIRM_TIMEOUT_SECS
}
/// `--follow`/`--once` post up to this many batches concurrently — `OpenBatch(N+1)` is sent once `OpenBatch(N)` has
/// confirmed (the on-chain `batch_cursor`'s own sequential order), the chunk lanes of different batches overlap freely,
/// and `FinalizeBatch(N+1)` is sent only after `FinalizeBatch(N)` has confirmed. 2 is the starting point from an
/// earlier measurement: work per batch (~3.5 s) plus the cluster's own inclusion tail (8 of 26 sends took 10.2-11.5 s
/// to confirm) pushed a strictly-sequential pipeline to ~12.8 s/batch against 10 s of production — a window of 2
/// overlaps exactly the inclusion tail's own latency with the next batch's work instead of stacking it serially.
fn default_batches_in_flight() -> usize {
    2
}
fn default_confirm_poll_interval_ms() -> u64 {
    // One batched `getSignatureStatuses` call covers every outstanding chunk tx per tick
    // (`sender::RpcSender::send_and_confirm_many`) — 400 ms keeps the poll rate well under public RPC
    // rate limits while still resolving most confirmations within a couple of ticks.
    400
}
/// The V1 header config mask's `loaded_accounts_data_size_limit` field — the runtime's ceiling on the total bytes of
/// every account a transaction loads.
///
/// The previous 131,072 (128 KiB)
/// default was computed from only the batch account (`rome_zk_layouts::batch::account_len`, up to
/// 29,123 B at the design's own 900-leaf default) plus headroom — it ignored SIMD-0186's actual
/// accounting, under which **every loaded account costs 64 B + its own data length, and a loader-v3
/// program's `ProgramData` account is counted even though it is never one of the transaction's own
/// `AccountMeta`s.** Tiber's inbox `ProgramData` is 133,077 B, so a chunk-lane transaction really loads
/// ≈143–163 KB depending on `max_frames_per_batch` — well over 128 KiB. Live-verified by
/// `simulateTransaction` against devnet (`tests/devnet_probe.rs`'s
/// `design_frame_v1_tx_simulates_clean_on_target_cluster`): 131,072 →
/// `MaxLoadedAccountsDataSizeExceeded`; **262,144 (256 KiB) → `err: None`.** 256 KiB is the value an
/// earlier measurement harness (`inbox_writer`) used, restored here — it is not merely "enough headroom
/// over a guess", it is verified sufficient against the live target. [`crate::loaded_accounts::run`] derives the real
/// requirement from the live inbox program at process start and refuses to run if a config sets this
/// field any lower, so this default is a starting point, never the sole guard. One value shared by every
/// tuning this binary builds (`tuning`/`chunk_tuning`/`finalize_tuning`) — unlike `compute_unit_limit`,
/// this is a data-size ceiling, not a per-lane cost knob, so there is no chunk-vs-finalize split to make
/// here.
pub fn default_loaded_accounts_data_size_limit() -> u32 {
    262_144
}

/// This crate's `/metrics` HTTP responder (`rome_zk_metrics_http::serve`), mirroring
/// `rome-zk-sequencer::config::Config::metrics_addr`'s own default shape (a required field there; a
/// defaulted one here since most existing deploys never declared this key at all).
/// `9002` is next to the sequencer's `9945` and reth's `9001` — none of the three collide.
fn default_metrics_addr() -> SocketAddr {
    "127.0.0.1:9002"
        .parse()
        .expect("default_metrics_addr: literal must parse")
}

/// Sample only every Nth finalized batch (CU sampling
/// is informational only, but two `getTransaction` reads *every* batch — 3-13 s of them observed on a
/// degraded cluster in an earlier measurement — is needless noise at any real posting rate). 50 is
/// the design's own starting point: frequent enough to keep the CU figure honest as the workload shifts,
/// rare enough that a busy chain doesn't spend a `getTransaction` round trip on nearly every batch.
fn default_cu_sample_every() -> u32 {
    50
}

/// Tiber's own default — a partial group closes by age after 60 s of holding
/// its first block, the batcher's own receipt clock (see [`crate::grouping::SizeCappedGrouper::close_if_stale`]).
pub fn default_batch_close_after_secs() -> u64 {
    60
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub chain_id: u64,
    #[serde(with = "pubkey_string")]
    pub inbox_program_id: Pubkey,
    #[serde(with = "pubkey_string")]
    pub settlement_program_id: Pubkey,
    pub log_dir: PathBuf,
    #[serde(default = "default_max_frames_per_batch")]
    pub max_frames_per_batch: usize,
    #[serde(default = "default_max_frame_body_len")]
    pub max_frame_body_len: usize,
    /// Bound on outstanding **transactions** (with one V1 transaction per frame this is
    /// directly a bound on outstanding frames; see `sender::run_send_and_confirm_many`'s own module doc),
    /// sized by `default_in_flight` so ≈ `TARGET_FRAMES_IN_FLIGHT` frames can really be concurrently
    /// mid-flight.
    #[serde(default = "default_in_flight")]
    pub in_flight: usize,
    /// Signatures per `getSignatureStatuses` call; clamped to
    /// `sender::MAX_SIGNATURE_STATUSES_PER_CALL` regardless of what a config sets here.
    #[serde(default = "default_signature_status_batch_size")]
    pub signature_status_batch_size: usize,
    #[serde(default = "default_priority_fee_micro_lamports")]
    pub priority_fee_micro_lamports: u64,
    #[serde(default = "default_max_priority_fee_micro_lamports")]
    pub max_priority_fee_micro_lamports: u64,
    #[serde(default = "default_compute_unit_limit")]
    pub compute_unit_limit: u32,
    /// The compute-unit limit for every chunk-lane transaction — one V1 tx per frame
    /// (`Open`+`Write`+`Seal`+`SealLeaf`) and `OpenBatch`+`GrowBatch` — as opposed to
    /// `compute_unit_limit` above (kept for anything else) and `finalize_compute_unit_limit` below
    /// (`FinalizeBatch` only). See `default_chunk_compute_unit_limit`'s own doc for the payer-cost-cap
    /// derivation.
    #[serde(default = "default_chunk_compute_unit_limit")]
    pub chunk_compute_unit_limit: u32,
    #[serde(default = "default_finalize_compute_unit_limit")]
    pub finalize_compute_unit_limit: u32,
    /// The V1 header config mask's `loaded_accounts_data_size_limit`, shared by every tuning this binary
    /// builds — see `default_loaded_accounts_data_size_limit`'s own doc.
    #[serde(default = "default_loaded_accounts_data_size_limit")]
    pub loaded_accounts_data_size_limit: u32,
    #[serde(default = "default_confirm_timeout_secs")]
    pub confirm_timeout_secs: u64,
    /// The level a send must reach before it counts as sent: `"finalized"` (the default, so a
    /// sent batch is a final one) or `"confirmed"`. See `rome_zk_solana_sender::SendTuning`.
    #[serde(default)]
    pub confirm_commitment: rome_zk_solana_sender::ConfirmCommitment,
    /// The status-poll cadence: the batched path's `getSignatureStatuses` tick, and the single-shot
    /// path's pause between checks.
    #[serde(default = "default_confirm_poll_interval_ms")]
    pub confirm_poll_interval_ms: u64,
    /// The bounded posting window — see `default_batches_in_flight`'s own doc.
    #[serde(default = "default_batches_in_flight")]
    pub batches_in_flight: usize,
    /// Where this process serves `GET /metrics` (`rome_zk_metrics_http::serve`), in
    /// both `--once` and `--follow`. Env override `ROME_ZK_BATCHER_METRICS_ADDR` (see
    /// `Config::apply_env_overrides`), mirroring `rome-zk-sequencer`'s own
    /// `ROME_ZK_SEQUENCER_METRICS_ADDR` pattern. Tiber's rendered config sets this to `0.0.0.0:9002`
    /// (compose-network only, never a public `ports:` mapping).
    #[serde(default = "default_metrics_addr")]
    pub metrics_addr: SocketAddr,
    /// Sample the CU-sample lookups only every Nth finalized batch — see
    /// `default_cu_sample_every`'s own doc. Zero is refused at load (`ConfigError::ZeroCuSampleEvery`):
    /// unconstructable, not merely undesirable, since the pipeline's own gate would otherwise divide by
    /// it deciding whose turn it is.
    #[serde(default = "default_cu_sample_every")]
    pub cu_sample_every: u32,
    /// A group in progress closes by age once it has held its
    /// first block for this many seconds, measured on the batcher's own monotonic receipt clock — never
    /// `Block.timestamp` vs wall clock (see [`crate::grouping::SizeCappedGrouper::close_if_stale`]'s own
    /// doc for why). Refused at `0` (`ConfigError::BatchCloseAfterZero`) and, once the sequencer's
    /// `profile.json` is read, below this chain's own block time (`ConfigError::BatchCloseAfterBelowBlockTime`
    /// — see [`validate_batch_close_after`]): a value below one block time would close every group at one
    /// block, faster than the chain could ever fill it.
    #[serde(default = "default_batch_close_after_secs")]
    pub batch_close_after_secs: u64,
    pub rpc_url: String,
    pub payer_key_path: PathBuf,
    /// Free-form label surfaced in logs/metrics only (e.g. "devnet", "mainnet") — no behavior keys off it
    /// except that a `cluster` value is required so a config can never accidentally omit stating which
    /// cluster it targets.
    pub cluster: String,
}

mod pubkey_string {
    use serde::{Deserialize, Deserializer};
    use solana_program::pubkey::Pubkey;
    use std::str::FromStr;

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Pubkey, D::Error> {
        let s = String::deserialize(d)?;
        Pubkey::from_str(&s).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error(
        "max_frames_per_batch={max_frames_per_batch}'s OpenBatch+GrowBatch plan ({grow_calls} GrowBatch \
         calls) is {size} bytes, over Solana's {limit}-byte V1 one-transaction wire limit — lower \
         max_frames_per_batch"
    )]
    GrowPlanTooLarge {
        max_frames_per_batch: usize,
        grow_calls: usize,
        size: usize,
        limit: usize,
    },
    /// The sequencer's `profile.json` exists but predates `blocks_per_batch` — a file
    /// written before that field joined the persisted identity (`rome_zk_sequencer::recovery`'s own
    /// old-format migration path). This batcher has no local `blocks_per_batch` of its own any more (it
    /// reads the grouper cap from this file), so there is nothing to fall back to — restart
    /// the sequencer once with `--write-profile-json` to add the field, then rerun this batcher.
    #[error(
        "{path:?} predates blocks_per_batch — restart the sequencer once with \
         --write-profile-json to add it, then rerun this batcher"
    )]
    ProfileJsonMissingBlocksPerBatch { path: PathBuf },
    /// The sequencer's `profile.json` predates `first_block` — written by an
    /// older sequencer whose ordered log is numbered from block 0, not 1. Not migratable: bring up a
    /// fresh chain (a chain reset, not an in-place migration), never restart
    /// the old sequencer's log in place with `--write-profile-json`.
    #[error(
        "{path:?} predates first_block — this log was written under 0-based block \
         numbering; bring up a fresh chain (fresh genesis, fresh log, fresh profile.json) rather than \
         rerunning this batcher against it"
    )]
    ProfileJsonMissingFirstBlock { path: PathBuf },
    /// The sequencer refuses a stored `first_block` that disagrees with `rome_zk_sequencer::profile::FIRST_BLOCK` by
    /// VALUE (`ProfileIdentity::first_mismatch`); this batcher previously only checked the field's presence (see
    /// [`read_profile_identity`]) and would accept any value a hand-edited file happened to carry — two readers of
    /// one file with different rules. Named distinctly from [`ConfigError::ProfileJsonMissingFirstBlock`] (that one
    /// is "absent"; this one is "present but wrong").
    #[error(
        "{path:?}: first_block is {stored}, expected {expected} (rome_zk_sequencer::profile::FIRST_BLOCK) \
         — a 0-based (or otherwise wrong) log cannot be posted by this batcher"
    )]
    ProfileJsonFirstBlockNotOne {
        path: PathBuf,
        stored: u64,
        expected: u64,
    },
    /// `sub_blocks_per_block = 0` (read from the
    /// sequencer's `profile.json`, see [`read_profile_identity`]) would never let
    /// [`crate::source::BlockSource::next_block`] complete a group — it would eventually fail with a
    /// misleading `NonContiguousIndex`/"no complete blocks" error rather than this named one, up front.
    #[error("{path:?}: sub_blocks_per_block must be nonzero")]
    ZeroSubBlocksPerBlock { path: PathBuf },
    #[error("reading profile identity at {path:?}: {source}")]
    ProfileJsonIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing profile identity at {path:?}: {source}")]
    ProfileJsonParse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    /// `cu_sample_every = 0` is unconstructable, not merely a bad tuning
    /// value — `pipeline::settle_one_batch`'s own gate (`finalized_so_far % cu_sample_every`) would
    /// divide by it on the very first finalized batch. Refused by name at config load, before that gate
    /// is ever reached, rather than a runtime panic mid-run. Sampling every batch is `cu_sample_every =
    /// 1`, never `0`.
    #[error(
        "cu_sample_every must be nonzero (got 0) — sampling every batch is cu_sample_every = 1, not 0"
    )]
    ZeroCuSampleEvery,
    /// Mirrors `rome-zk-sequencer::config::ConfigError::EnvOverride`: an env override
    /// (currently only `ROME_ZK_BATCHER_METRICS_ADDR`) whose value fails to parse into its field's type.
    #[error("invalid env override {var}={value:?}: {reason}")]
    EnvOverride {
        var: &'static str,
        value: String,
        reason: String,
    },
    /// `batch_close_after_secs = 0` is unconstructable, not merely undesirable — a
    /// group would close by age the instant it received its first block, indistinguishable from never
    /// accumulating anything at all. Checked at config load, before any profile is read.
    #[error("batch_close_after_secs must be nonzero (got 0)")]
    BatchCloseAfterZero,
    /// `batch_close_after_secs` below this chain's own block time
    /// (`sub_block_ms * sub_blocks_per_block / 1000`, read from the sequencer's `profile.json` —
    /// [`read_profile_identity`]) would close every group by age before a single block time even elapses,
    /// faster than the chain could ever fill one — never a legal cadence. Checked by
    /// [`validate_batch_close_after`] once the profile identity is available, distinctly from
    /// [`ConfigError::BatchCloseAfterZero`] (that one needs no profile at all).
    #[error(
        "batch_close_after_secs {got} must be >= this chain's own block time ({block_time_secs}s)"
    )]
    BatchCloseAfterBelowBlockTime { got: u64, block_time_secs: u64 },
}

/// `batch_close_after_secs` must be at least this chain's own block
/// time (`sub_block_ms * sub_blocks_per_block / 1000`, in `u128` so a large `sub_block_ms` at many
/// `sub_blocks_per_block` cannot silently wrap) — a value below it would close every group by age at one
/// block, faster than the chain could ever fill it. Called once `profile_identity` is available (the
/// binary, right after [`read_profile_identity`]); `batch_close_after_secs == 0` is refused earlier, at
/// TOML parse time ([`Config::from_toml_str`]), before any profile is read.
pub fn validate_batch_close_after(
    batch_close_after_secs: u64,
    profile_identity: &rome_zk_profile::ProfileIdentity,
) -> Result<(), ConfigError> {
    let block_time_ms =
        profile_identity.sub_block_ms as u128 * profile_identity.sub_blocks_per_block as u128;
    // Compare in milliseconds: a 1.5 s block time must refuse `1`, which whole-second division would
    // have waved through. The refusal names the block time rounded UP to whole seconds — the smallest
    // legal value.
    if (batch_close_after_secs as u128) * 1_000 < block_time_ms {
        return Err(ConfigError::BatchCloseAfterBelowBlockTime {
            got: batch_close_after_secs,
            block_time_secs: block_time_ms.div_ceil(1_000) as u64,
        });
    }
    Ok(())
}

/// The batcher reads the sequencer's own `profile.json` — written beside the ordered
/// log by `rome_zk_sequencer::recovery::reconcile_profile_identity` when the log is created — for
/// `sub_blocks_per_block` and `block_gas_limit`, instead of owning parallel config fields with their own
/// (previously divergent: 20 vs the sequencer's `[profile]`, and a 2,000,000,000 gas-limit default vs
/// the sequencer's 100,000,000) defaults. `log_dir` is the directory actually being read (the binary's
/// `--once <log_dir>` argument, not necessarily `Config::log_dir` — see that field's own doc).
pub fn read_profile_identity(
    log_dir: &std::path::Path,
) -> Result<rome_zk_profile::ProfileIdentity, ConfigError> {
    let path = log_dir.join(rome_zk_profile::PROFILE_JSON_FILENAME);
    match rome_zk_profile::read_profile_json(log_dir) {
        // A missing `profile.json` is a named io error, not a panic or a silent default — this batcher
        // (unlike the sequencer) has no "fresh log, write one" branch of its own, so `Missing` always
        // means "nothing for this batcher to read".
        Ok(rome_zk_profile::StoredProfileJson::Missing) => Err(ConfigError::ProfileJsonIo {
            path,
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no profile.json beside the log yet — the sequencer writes it when its ordered \
                 log is created",
            ),
        }),
        // A `profile.json` written before `blocks_per_batch` joined the persisted
        // identity is missing the key entirely — named and pointed at the sequencer's migration flag,
        // rather than falling through to a generic (and confusing) "missing field" parse error.
        Ok(rome_zk_profile::StoredProfileJson::V0(_)) => {
            Err(ConfigError::ProfileJsonMissingBlocksPerBatch { path })
        }
        // A `profile.json` written before `first_block` joined the persisted
        // identity predates the numbering-from-1 sequencer entirely — named here rather than falling
        // through to a generic parse error.
        Ok(rome_zk_profile::StoredProfileJson::V1(_)) => {
            Err(ConfigError::ProfileJsonMissingFirstBlock { path })
        }
        Ok(rome_zk_profile::StoredProfileJson::Current(identity)) => {
            // Refuse a stored `first_block` by VALUE, not just presence — the
            // sequencer's own `first_mismatch` already does this (`profile.rs`'s `first_block`
            // comparison); this batcher must apply the identical rule to the identical file, or a
            // hand-edited value the sequencer would refuse is silently accepted here.
            if identity.first_block != rome_zk_profile::FIRST_BLOCK {
                return Err(ConfigError::ProfileJsonFirstBlockNotOne {
                    path,
                    stored: identity.first_block,
                    expected: rome_zk_profile::FIRST_BLOCK,
                });
            }
            if identity.sub_blocks_per_block == 0 {
                return Err(ConfigError::ZeroSubBlocksPerBlock { path });
            }
            Ok(identity)
        }
        Err(rome_zk_profile::ProfileJsonError::Io { path, source }) => {
            Err(ConfigError::ProfileJsonIo { path, source })
        }
        Err(rome_zk_profile::ProfileJsonError::Parse { path, source }) => {
            Err(ConfigError::ProfileJsonParse { path, source })
        }
    }
}

/// `zk_inbox_client::open_and_grow_batch_ixs`'s whole OpenBatch+GrowBatch plan is sent as **one** V1
/// transaction — measured here (not estimated), at config-parse time, rather
/// than discovered mid-run against a real cluster. Builds the real plan (real instruction count/shape for
/// the configured `max_frames_per_batch`) into a real, signed V1 transaction exactly as
/// `sender::build_tx` would (no `ComputeBudget` instructions — limits live in the V1 header config mask —
/// and a real `Keypair`, not just a placeholder pubkey, since a V1 message must actually be signed to
/// measure its true wire size) and compares the real wire size (`wincode::serialize`, the actual on-wire
/// encoding, matching `sender.rs`) against 4,096 B.
fn assert_open_and_grow_plan_fits_one_transaction(
    max_frames_per_batch: usize,
) -> Result<(), ConfigError> {
    let payer = solana_keypair::Keypair::new();
    let payer_legacy = crate::sender::compat::from_v1_pubkey(&{
        use solana_signer::Signer;
        payer.pubkey()
    });
    let placeholder = Pubkey::new_from_array([1u8; 32]);
    let grow_ixs = zk_inbox_client::open_and_grow_batch_ixs(
        &placeholder,
        &payer_legacy,
        0,
        0,
        max_frames_per_batch as u32,
        &placeholder,
    );
    let grow_calls = grow_ixs.len().saturating_sub(1); // minus OpenBatch itself
    let v1_ixs: Vec<solana_instruction::Instruction> = grow_ixs
        .iter()
        .map(crate::sender::compat::to_v1_instruction)
        .collect();
    let config = solana_message::v1::TransactionConfig::empty()
        .with_compute_unit_limit(0)
        .with_loaded_accounts_data_size_limit(0);
    let message = {
        use solana_signer::Signer;
        solana_message::v1::Message::try_compile_with_config(
            &payer.pubkey(),
            &v1_ixs,
            solana_hash::Hash::default(),
            config,
        )
    }
    .unwrap_or_else(|e| {
        panic!(
            "max_frames_per_batch={max_frames_per_batch}'s OpenBatch+GrowBatch plan failed to compile \
             into a V1 transaction message: {e}"
        )
    });
    let tx = solana_transaction::versioned::VersionedTransaction::try_new(
        solana_message::VersionedMessage::V1(message),
        &[&payer],
    )
    .expect("signing with a fresh keypair for its own fee payer cannot fail");
    let size = wincode::serialize(&tx)
        .expect("serializing a signed V1 transaction cannot fail")
        .len();
    let limit = 4_096usize;
    if size > limit {
        return Err(ConfigError::GrowPlanTooLarge {
            max_frames_per_batch,
            grow_calls,
            size,
            limit,
        });
    }
    Ok(())
}

impl Config {
    /// The base [`rome_zk_solana_sender::SendTuning`] every send of this process starts from: compute limit, fee bounds
    /// and the confirm settings, all from this config. The binary derives its chunk-lane and finalize variants by
    /// overriding only the compute limit.
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

    pub fn from_toml_str(s: &str) -> Result<Self, ConfigError> {
        let cfg: Self = toml::from_str(s)?;
        assert_open_and_grow_plan_fits_one_transaction(cfg.max_frames_per_batch)?;
        if cfg.cu_sample_every == 0 {
            return Err(ConfigError::ZeroCuSampleEvery);
        }
        if cfg.batch_close_after_secs == 0 {
            return Err(ConfigError::BatchCloseAfterZero);
        }
        Ok(cfg)
    }

    pub fn load(path: impl AsRef<std::path::Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let s = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let mut cfg = Self::from_toml_str(&s)?;
        cfg.apply_env_overrides(|k| std::env::var(k).ok())?;
        Ok(cfg)
    }

    /// Env-override logic, parameterized over the env lookup so it is testable without
    /// mutating the real process environment — mirrors
    /// `rome_zk_sequencer::config::Config::apply_overrides_from`'s own shape. Currently one variable:
    /// `ROME_ZK_BATCHER_METRICS_ADDR`.
    fn apply_env_overrides(
        &mut self,
        get: impl Fn(&str) -> Option<String>,
    ) -> Result<(), ConfigError> {
        if let Some(v) = get("ROME_ZK_BATCHER_METRICS_ADDR") {
            self.metrics_addr = v.parse().map_err(|e| ConfigError::EnvOverride {
                var: "ROME_ZK_BATCHER_METRICS_ADDR",
                value: v.clone(),
                reason: format!("{e}"),
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
chain_id = 200198
inbox_program_id = "11111111111111111111111111111111"
settlement_program_id = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
log_dir = "/var/lib/rome-zk/log"
rpc_url = "https://api.devnet.solana.com"
payer_key_path = "/home/rome/.config/solana/batcher.json"
cluster = "devnet"
"#;

    #[test]
    fn example_config_parses_with_only_required_fields_and_documented_defaults() {
        let cfg = Config::from_toml_str(EXAMPLE).unwrap();
        assert_eq!(cfg.chain_id, 200_198);
        assert_eq!(
            cfg.max_frames_per_batch,
            crate::channel::DEFAULT_MAX_FRAMES_PER_CHANNEL
        );
        assert_eq!(cfg.max_frame_body_len, DEFAULT_MAX_FRAME_BODY_LEN);
        assert_eq!(cfg.in_flight, default_in_flight());
        assert_eq!(
            cfg.signature_status_batch_size,
            crate::sender::MAX_SIGNATURE_STATUSES_PER_CALL
        );
        assert_eq!(cfg.cluster, "devnet"); // the code default when the key is omitted — a label only
        assert_eq!(cfg.confirm_poll_interval_ms, 400);
        assert_eq!(cfg.compute_unit_limit, 200_000);
        assert_eq!(cfg.chunk_compute_unit_limit, 40_000);
        assert_eq!(cfg.finalize_compute_unit_limit, 600_000);
        assert_eq!(cfg.loaded_accounts_data_size_limit, 262_144);
        assert_eq!(cfg.batches_in_flight, 2);
        assert_eq!(cfg.metrics_addr, default_metrics_addr());
        assert_eq!(cfg.cu_sample_every, 50);
        assert_eq!(cfg.batch_close_after_secs, 60);
    }

    /// `batch_close_after_secs = 0` is refused at config load —
    /// before any profile is even read (unconstructable regardless of the chain's block time). A value
    /// below the chain's own block time is a separate refusal, checked once `profile_identity` is
    /// available ([`validate_batch_close_after`]) — this test proves both by name.
    #[test]
    fn zero_or_sub_block_time_close_after_is_refused_by_name() {
        let toml = format!("{EXAMPLE}\nbatch_close_after_secs = 0\n");
        let err = Config::from_toml_str(&toml).unwrap_err();
        assert!(
            matches!(err, ConfigError::BatchCloseAfterZero),
            "must be the named refusal: {err}"
        );

        // A profile whose own block time (2s: sub_block_ms=100 * sub_blocks_per_block=20 / 1000) exceeds a
        // configured, nonzero batch_close_after_secs (1s) is refused too, naming both values.
        let identity = rome_zk_sequencer::profile::ProfileIdentity {
            chain_id: 200_198,
            sub_block_ms: 100,
            sub_blocks_per_block: 20,
            sub_block_gas_limit: 5_000_000,
            block_gas_limit: 100_000_000,
            blocks_per_batch: 10,
            first_block: rome_zk_sequencer::profile::FIRST_BLOCK,
        };
        let err = validate_batch_close_after(1, &identity).unwrap_err();
        assert_eq!(
            err.to_string(),
            "batch_close_after_secs 1 must be >= this chain's own block time (2s)"
        );
        assert!(matches!(
            err,
            ConfigError::BatchCloseAfterBelowBlockTime {
                got: 1,
                block_time_secs: 2
            }
        ));

        // Exactly the block time, and anything above it, is legal.
        validate_batch_close_after(2, &identity).expect("equal to block time must be accepted");
        validate_batch_close_after(60, &identity).expect("above block time must be accepted");

        // A block time that is not a whole number of seconds (75 ms x 20 = 1.5 s) must still refuse a
        // value below it — whole-second division would have accepted 1 here.
        let identity_1500ms = rome_zk_sequencer::profile::ProfileIdentity {
            sub_block_ms: 75,
            ..identity
        };
        let err = validate_batch_close_after(1, &identity_1500ms).unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::BatchCloseAfterBelowBlockTime {
                    got: 1,
                    block_time_secs: 2
                }
            ),
            "1 s is below a 1.5 s block time; the refusal names the smallest legal value (2 s), got {err:?}"
        );
        validate_batch_close_after(2, &identity_1500ms)
            .expect("the smallest whole second at or above the block time must be accepted");
    }

    /// An omitted `metrics_addr` defaults to `127.0.0.1:9002` — next
    /// to the sequencer's `9945` and reth's `9001`, none colliding.
    #[test]
    fn default_metrics_addr_is_127_0_0_1_9002() {
        assert_eq!(
            default_metrics_addr(),
            "127.0.0.1:9002".parse::<std::net::SocketAddr>().unwrap()
        );
    }

    /// `ROME_ZK_BATCHER_METRICS_ADDR` overrides the config file's own
    /// `metrics_addr`, mirroring the sequencer's `ROME_ZK_SEQUENCER_METRICS_ADDR`. Parameterized over the
    /// env lookup (never the real process environment — tests must not mutate `std::env` for each other).
    #[test]
    fn metrics_addr_env_override_replaces_the_configured_value() {
        let mut cfg = Config::from_toml_str(EXAMPLE).unwrap();
        assert_eq!(cfg.metrics_addr, default_metrics_addr());

        cfg.apply_env_overrides(|k| {
            (k == "ROME_ZK_BATCHER_METRICS_ADDR").then(|| "127.0.0.1:9111".to_string())
        })
        .unwrap();

        assert_eq!(
            cfg.metrics_addr,
            "127.0.0.1:9111".parse::<std::net::SocketAddr>().unwrap()
        );
    }

    /// An env override that fails to parse is refused by name, not silently ignored or
    /// panicking.
    #[test]
    fn metrics_addr_env_override_that_fails_to_parse_is_refused_by_name() {
        let mut cfg = Config::from_toml_str(EXAMPLE).unwrap();
        let err = cfg
            .apply_env_overrides(|k| {
                (k == "ROME_ZK_BATCHER_METRICS_ADDR").then(|| "not-an-addr".to_string())
            })
            .unwrap_err();
        assert!(
            matches!(err, ConfigError::EnvOverride { var, .. } if var == "ROME_ZK_BATCHER_METRICS_ADDR"),
            "must be the named refusal: {err}"
        );
    }

    /// Dropping this check (accepting 0) would let
    /// `pipeline::settle_one_batch`'s modulo gate divide by zero on the very first finalized batch.
    #[test]
    fn cu_sample_every_zero_is_refused_at_config_load() {
        let toml = format!("{EXAMPLE}\ncu_sample_every = 0\n");
        let err = Config::from_toml_str(&toml).unwrap_err();
        assert!(
            matches!(err, ConfigError::ZeroCuSampleEvery),
            "must be the named refusal: {err}"
        );
    }

    #[test]
    fn cu_sample_every_nonzero_is_accepted() {
        let toml = format!("{EXAMPLE}\ncu_sample_every = 3\n");
        let cfg = Config::from_toml_str(&toml).unwrap();
        assert_eq!(cfg.cu_sample_every, 3);
    }

    /// The default posting window is two batches.
    #[test]
    fn default_batches_in_flight_is_two() {
        assert_eq!(default_batches_in_flight(), 2);
    }

    /// `in_flight`'s default is now directly `TARGET_FRAMES_IN_FLIGHT` — one V1
    /// transaction per frame means "outstanding transactions" and "outstanding frames" are the same
    /// count, so there is no per-frame stage-width multiplier left to derive.
    #[test]
    fn default_in_flight_equals_the_target_frame_count_directly() {
        assert_eq!(default_in_flight(), TARGET_FRAMES_IN_FLIGHT);
        assert_eq!(default_in_flight(), 240);
    }

    /// The payer-cap model must charge the transaction's real cost terms (signature, write-lock, instruction-data,
    /// loaded-accounts — `tx_cost_units`), parameterized by which `agave` generation's `MAX_WRITABLE_ACCOUNT_UNITS`
    /// applies, not just divide by `compute_unit_limit` alone — and the data term itself must be Σ instruction data ÷ 4
    /// (agave's real divisor), never wire-size ÷ 140. Pinned against the design frame (40,000 CU, 3 write-locked
    /// accounts, its own real instruction-data byte sum from `plan_chunk`, the corrected 262,144-B loaded-accounts
    /// limit): the re-derived figures are "≈704 frames/s at 12,000,000" (pre-4.x) and "≈1,408 at 24,000,000" (agave
    /// 4.x) — this test pins the formula's real output, not rounded prose (verify against the formula, never re-derive
    /// by eye).
    #[test]
    fn payer_cap_frames_per_sec_pins_the_design_frames_cost_at_both_agave_generations() {
        const TXS_PER_FRAME: u32 = 1; // One V1 tx per frame, no split.
        let loaded_limit = default_loaded_accounts_data_size_limit();
        let instruction_data_bytes = design_frame_instruction_data_bytes();

        let pre4x_frames_per_sec = payer_cap_frames_per_sec(
            AgaveGeneration::Pre4x,
            default_chunk_compute_unit_limit(),
            DESIGN_FRAME_WRITABLE_ACCOUNTS,
            instruction_data_bytes,
            loaded_limit,
            TXS_PER_FRAME,
        );
        assert!(
            (700.0..710.0).contains(&pre4x_frames_per_sec),
            "expected ≈704 frames/s at the pre-4.x 12,000,000 cap, got {pre4x_frames_per_sec}"
        );

        let agave4x_frames_per_sec = payer_cap_frames_per_sec(
            AgaveGeneration::Agave4x,
            default_chunk_compute_unit_limit(),
            DESIGN_FRAME_WRITABLE_ACCOUNTS,
            instruction_data_bytes,
            loaded_limit,
            TXS_PER_FRAME,
        );
        assert!(
            (1400.0..1415.0).contains(&agave4x_frames_per_sec),
            "expected ≈1,408 frames/s at the agave-4.x 24,000,000 cap, got {agave4x_frames_per_sec}"
        );
        // Agave 4.x exactly doubles the cap, so the derived rate must double too (same per-tx cost either
        // way — only the block-wide budget changed).
        assert!((agave4x_frames_per_sec - 2.0 * pre4x_frames_per_sec).abs() < 0.01);
        assert_eq!(TARGET_AGAVE_GENERATION, AgaveGeneration::Pre4x);
    }

    /// Pins `tx_cost_units` itself against the design frame's own numbers (recomputed for
    /// `Seal { len, body_hash }`): 40,000 (CU) +
    /// 720 (one signature) + 300*3 (three write-locked accounts) + (3,776 instruction-data bytes / 4)
    /// (real Σ ix data from `plan_chunk` — Open 25 B + Write (9 + 3,700-B payload) + Seal 37 B (5 B
    /// `len` + 32 B `body_hash`) + SealLeaf 5 B = 3,776 B, 944 CU) + 8*ceil(262,144/32 KiB)
    /// (loaded-accounts pages) = 42,628.
    #[test]
    fn tx_cost_units_pins_the_design_frames_terms() {
        let instruction_data_bytes = design_frame_instruction_data_bytes();
        assert_eq!(
            instruction_data_bytes, 3_776,
            "the design frame's real Open+Write+Seal+SealLeaf instruction-data sum must be 3,776 B \
             (drifted — recompute the terms in this test's own doc comment)"
        );
        let cost = tx_cost_units(
            default_chunk_compute_unit_limit(),
            DESIGN_FRAME_WRITABLE_ACCOUNTS,
            instruction_data_bytes,
            default_loaded_accounts_data_size_limit(),
        );
        assert_eq!(cost, 42_628);
    }

    /// `FinalizeBatch` needs its own, higher CU limit than every
    /// other instruction — 600,000 covers the measured 334,459 CU a 900-leaf `FinalizeBatch` costs, with
    /// headroom; `compute_unit_limit` (200,000) stays as-is for every other, cheap instruction.
    #[test]
    fn default_finalize_compute_unit_limit_covers_a_900_leaf_batch_with_headroom() {
        // Re-measured on real SBPF v3 (zk-inbox `accumulator.rs`, 900 leaves, one call): 334,459 CU.
        // The value before the dependency bump was 372_951.
        const MEASURED_900_LEAF_FINALIZE_CU: u32 = 334_459;
        assert!(default_finalize_compute_unit_limit() > MEASURED_900_LEAF_FINALIZE_CU);
        assert!(
            default_finalize_compute_unit_limit() > default_compute_unit_limit(),
            "finalize must use a strictly higher limit than the general per-instruction default"
        );
    }

    /// `GrowBatch` removed the ≤312-leaf `OpenBatch` ceiling, so the
    /// default is once again the design's own 900-frame channel default, not a CPI-derived cap.
    #[test]
    fn default_max_frames_per_batch_is_the_design_default() {
        assert_eq!(
            default_max_frames_per_batch(),
            crate::channel::DEFAULT_MAX_FRAMES_PER_CHANNEL
        );
    }

    #[test]
    fn every_documented_knob_can_be_overridden() {
        // `blocks_per_batch` is no longer a config field of this crate at all: the
        // grouper cap is read from the sequencer's own `profile.json` — see
        // `blocks_per_batch_comes_from_the_profile_not_config` below.
        let s = format!(
            "{EXAMPLE}\nmax_frames_per_batch = 100\nmax_frame_body_len = 900\n\
             in_flight = 16\nsignature_status_batch_size = 32\n\
             priority_fee_micro_lamports = 5000\nmax_priority_fee_micro_lamports = 50000\n\
             compute_unit_limit = 400000\nchunk_compute_unit_limit = 60000\n\
             finalize_compute_unit_limit = 700000\nloaded_accounts_data_size_limit = 262144\n\
             confirm_timeout_secs = 30\n\
             confirm_poll_interval_ms = 250\nbatches_in_flight = 4\n"
        );
        let cfg = Config::from_toml_str(&s).unwrap();
        assert_eq!(cfg.max_frames_per_batch, 100);
        assert_eq!(cfg.max_frame_body_len, 900);
        assert_eq!(cfg.in_flight, 16);
        assert_eq!(cfg.signature_status_batch_size, 32);
        assert_eq!(cfg.priority_fee_micro_lamports, 5000);
        assert_eq!(cfg.max_priority_fee_micro_lamports, 50000);
        assert_eq!(cfg.compute_unit_limit, 400000);
        assert_eq!(cfg.chunk_compute_unit_limit, 60000);
        assert_eq!(cfg.finalize_compute_unit_limit, 700000);
        assert_eq!(cfg.loaded_accounts_data_size_limit, 262144);
        assert_eq!(cfg.confirm_timeout_secs, 30);
        assert_eq!(cfg.confirm_poll_interval_ms, 250);
        assert_eq!(cfg.batches_in_flight, 4);
    }

    /// The batcher
    /// has no `blocks_per_batch` config field any more — the grouper cap comes entirely from the
    /// sequencer's own `profile.json`. A profile declaring 5 must be read as 5, not refused.
    #[test]
    fn blocks_per_batch_comes_from_the_profile_not_config() {
        let dir = tempfile::tempdir().unwrap();
        let profile = rome_zk_sequencer::profile::Profile {
            blocks_per_batch: 5,
            ..rome_zk_sequencer::profile::Profile::default()
        };
        let identity = rome_zk_sequencer::profile::ProfileIdentity::new(200_198, &profile);
        rome_zk_sequencer::recovery::reconcile_profile_identity(dir.path(), identity, false)
            .unwrap();

        let read = read_profile_identity(dir.path()).unwrap();
        assert_eq!(read.blocks_per_batch, 5);
    }

    /// End-to-end: a `profile.json`
    /// declaring 5 used to be irrelevant, since `bin/rome-zk-batcher.rs` fed `SizeCappedGrouper` from
    /// `Config::blocks_per_batch`, refused at load unless it was exactly 10. This test wires
    /// `read_profile_identity`'s own output straight into `SizeCappedGrouper::new` — exactly what
    /// `bin/rome-zk-batcher.rs`'s `run_once`/`run_follow` do — and proves a profile declaring 5 actually
    /// caps a group at 5 blocks, not the old shared default of 10.
    #[test]
    fn profile_json_blocks_per_batch_of_5_actually_caps_grouping_at_5_not_10() {
        let dir = tempfile::tempdir().unwrap();
        let profile = rome_zk_sequencer::profile::Profile {
            blocks_per_batch: 5,
            ..rome_zk_sequencer::profile::Profile::default()
        };
        let identity = rome_zk_sequencer::profile::ProfileIdentity::new(200_198, &profile);
        rome_zk_sequencer::recovery::reconcile_profile_identity(dir.path(), identity, false)
            .unwrap();
        let read = read_profile_identity(dir.path()).unwrap();

        let mut grouper = crate::grouping::SizeCappedGrouper::new(
            read.blocks_per_batch,
            crate::channel::DEFAULT_MAX_FRAMES_PER_CHANNEL,
            DEFAULT_MAX_FRAME_BODY_LEN,
            None,
        );
        let block = |n: u64| crate::channel::Block {
            number: n,
            timestamp: 1_757_000_000 + n,
            gas_limit: 5_000_000,
            txs: vec![],
        };
        let mut closed_at = None;
        let now = std::time::Instant::now();
        for n in 0..7u64 {
            match grouper.push(block(n), now).unwrap() {
                crate::grouping::PushOutcome::Accepted => {}
                crate::grouping::PushOutcome::Closed { reason, .. } => {
                    closed_at = Some((n, reason));
                    break;
                }
            }
        }
        let (closed_at_block, reason) = closed_at.expect("a 5-cap must close within 7 pushes");
        assert_eq!(
            closed_at_block, 4,
            "blocks 0..=4 is 5 blocks — the cap must close on the 5th push (block number 4), not the \
             old shared default of 10"
        );
        assert!(
            matches!(reason, crate::grouping::CloseReason::Cap),
            "must close on the blocks_per_batch cap, not a size close, at this tiny block size"
        );
        assert_eq!(grouper.take_group().len(), 5);
    }

    /// `Config` itself no longer parses `blocks_per_batch` at all — an old config file that still sets
    /// it is silently ignored by `toml`'s own unknown-field behavior (this crate makes no claim about
    /// that field any more), proving the field is genuinely gone rather than merely renamed.
    #[test]
    fn config_no_longer_has_a_blocks_per_batch_field() {
        let s = format!("{EXAMPLE}\nblocks_per_batch = 11\n");
        // Must still load cleanly — this crate does not even look at the key any more.
        Config::from_toml_str(&s).unwrap();
    }

    /// A `profile.json`
    /// declaring `sub_blocks_per_block = 0` is refused by [`read_profile_identity`], naming the file,
    /// rather than silently handed to `BlockSource` (which would never complete a group and eventually
    /// fail with a misleading `NonContiguousIndex`/"no complete blocks" error instead).
    #[test]
    fn zero_sub_blocks_per_block_in_profile_json_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join(rome_zk_sequencer::recovery::PROFILE_JSON_FILENAME);
        let identity = rome_zk_sequencer::profile::ProfileIdentity {
            chain_id: 200_198,
            sub_block_ms: 50,
            sub_blocks_per_block: 0,
            sub_block_gas_limit: 5_000_000,
            block_gas_limit: 100_000_000,
            blocks_per_batch: 10,
            first_block: rome_zk_sequencer::profile::FIRST_BLOCK,
        };
        std::fs::write(&path, serde_json::to_string(&identity).unwrap()).unwrap();

        let err = read_profile_identity(dir.path()).unwrap_err();
        assert!(
            matches!(err, ConfigError::ZeroSubBlocksPerBlock { .. }),
            "{err:?}"
        );
    }

    /// A `profile.json` written before
    /// `blocks_per_batch` joined the persisted identity (every other field present, this one missing) is
    /// refused by [`read_profile_identity`], naming the file and pointing at the sequencer's migration
    /// flag — never silently defaulted to some assumed cap.
    #[test]
    fn profile_json_without_blocks_per_batch_is_refused_naming_the_migration_flag() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join(rome_zk_sequencer::recovery::PROFILE_JSON_FILENAME);
        let legacy = serde_json::json!({
            "chain_id": 200_198,
            "sub_block_ms": 50,
            "sub_blocks_per_block": 20,
            "sub_block_gas_limit": 5_000_000,
            "block_gas_limit": 100_000_000,
        });
        std::fs::write(&path, serde_json::to_string(&legacy).unwrap()).unwrap();

        let err = read_profile_identity(dir.path()).unwrap_err();
        match &err {
            ConfigError::ProfileJsonMissingBlocksPerBatch { path: p } => assert_eq!(p, &path),
            other => panic!("expected ProfileJsonMissingBlocksPerBatch, got {other:?}"),
        }
        assert!(err.to_string().contains("--write-profile-json"));
    }

    /// A `profile.json` written before `first_block` joined the persisted
    /// identity (`blocks_per_batch` present, this one missing) is refused by
    /// [`read_profile_identity`], naming the file, never silently assumed 1-based.
    #[test]
    fn profile_json_without_first_block_is_refused_naming_the_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join(rome_zk_sequencer::recovery::PROFILE_JSON_FILENAME);
        let legacy = serde_json::json!({
            "chain_id": 200_198,
            "sub_block_ms": 50,
            "sub_blocks_per_block": 20,
            "sub_block_gas_limit": 5_000_000,
            "block_gas_limit": 100_000_000,
            "blocks_per_batch": 10,
        });
        std::fs::write(&path, serde_json::to_string(&legacy).unwrap()).unwrap();

        let err = read_profile_identity(dir.path()).unwrap_err();
        match &err {
            ConfigError::ProfileJsonMissingFirstBlock { path: p } => assert_eq!(p, &path),
            other => panic!("expected ProfileJsonMissingFirstBlock, got {other:?}"),
        }
    }

    /// A `profile.json` whose `first_block` is
    /// PRESENT but wrong (a hand-edited `0`, say) must be refused by VALUE, not silently accepted
    /// because the field merely exists — the sequencer's own `first_mismatch` already refuses this same
    /// disagreement (`profile.rs`'s `a_changed_first_block_is_named`); this batcher must apply the
    /// identical rule to the identical file. With the value check disabled
    /// this test must go red.
    #[test]
    fn profile_json_with_first_block_zero_is_refused_by_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join(rome_zk_sequencer::recovery::PROFILE_JSON_FILENAME);
        let identity = rome_zk_sequencer::profile::ProfileIdentity {
            chain_id: 200_198,
            sub_block_ms: 50,
            sub_blocks_per_block: 20,
            sub_block_gas_limit: 5_000_000,
            block_gas_limit: 100_000_000,
            blocks_per_batch: 10,
            first_block: 0,
        };
        std::fs::write(&path, serde_json::to_string(&identity).unwrap()).unwrap();

        let err = read_profile_identity(dir.path()).unwrap_err();
        match &err {
            ConfigError::ProfileJsonFirstBlockNotOne {
                path: p,
                stored,
                expected,
            } => {
                assert_eq!(p, &path);
                assert_eq!(*stored, 0);
                assert_eq!(*expected, rome_zk_sequencer::profile::FIRST_BLOCK);
            }
            other => panic!("expected ProfileJsonFirstBlockNotOne, got {other:?}"),
        }
    }

    /// [`read_profile_identity`] reads exactly what
    /// `rome_zk_sequencer::recovery::reconcile_profile_identity` writes — the same file, the same shape
    /// — so the batcher and the sequencer never carry parallel, potentially divergent, ideas of the
    /// chain's block shape.
    #[test]
    fn read_profile_identity_reads_what_the_sequencer_writes() {
        let dir = tempfile::tempdir().unwrap();
        let written = rome_zk_sequencer::profile::ProfileIdentity::new(
            200_198,
            &rome_zk_sequencer::profile::Profile::default(),
        );
        rome_zk_sequencer::recovery::reconcile_profile_identity(dir.path(), written, false)
            .unwrap();

        let read = read_profile_identity(dir.path()).unwrap();
        assert_eq!(read, written);
    }

    /// A missing `profile.json` (a log the sequencer never wrote an identity for) is a named io error, not a panic or
    /// a silent default. The message must say WHY the file is missing (the sequencer writes it when the log is
    /// created) rather than a raw, opaque OS error ("entity not found") — this is the most common operator mistake
    /// (batcher started before the sequencer).
    #[test]
    fn missing_profile_json_is_a_named_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_profile_identity(dir.path()).unwrap_err();
        assert!(matches!(err, ConfigError::ProfileJsonIo { .. }), "{err:?}");
        assert!(
            err.to_string()
                .contains("no profile.json beside the log yet"),
            "{err}"
        );
    }

    #[test]
    fn bad_program_id_string_fails_to_parse() {
        let s = EXAMPLE.replace("11111111111111111111111111111111", "not-a-valid-pubkey");
        assert!(Config::from_toml_str(&s).is_err());
    }

    /// A `max_frames_per_batch` whose OpenBatch+GrowBatch plan needs
    /// enough `GrowBatch` calls to overflow one transaction must be refused at config-load time, not
    /// discovered mid-run against a real cluster.
    #[test]
    fn max_frames_per_batch_that_overflows_one_transaction_is_rejected_at_load_time() {
        let s = format!("{EXAMPLE}\nmax_frames_per_batch = 50000\n");
        let err = Config::from_toml_str(&s).unwrap_err();
        assert!(
            matches!(err, ConfigError::GrowPlanTooLarge { .. }),
            "expected GrowPlanTooLarge, got {err:?}"
        );
    }

    /// The design's own 900-frame default (and Tiber's configured value) must fit comfortably — this is
    /// the boundary the guard must never falsely reject.
    #[test]
    fn the_design_default_of_900_frames_fits_comfortably() {
        assert_open_and_grow_plan_fits_one_transaction(
            crate::channel::DEFAULT_MAX_FRAMES_PER_CHANNEL,
        )
        .expect("900 frames must fit in one OpenBatch+GrowBatch transaction");
    }

    /// The fast-finality defaults reach the batcher's config, and `confirmed` stays selectable.
    #[test]
    fn confirm_defaults_are_finalized_and_15s_and_confirmed_is_selectable() {
        let cfg = Config::from_toml_str(EXAMPLE).unwrap();
        assert_eq!(cfg.confirm_timeout_secs, 15);
        assert_eq!(
            cfg.confirm_commitment,
            rome_zk_solana_sender::ConfirmCommitment::Finalized
        );
        let cfg =
            Config::from_toml_str(&format!("{EXAMPLE}\nconfirm_commitment = \"confirmed\"\n"))
                .unwrap();
        assert_eq!(
            cfg.confirm_commitment,
            rome_zk_solana_sender::ConfirmCommitment::Confirmed
        );
        assert!(
            Config::from_toml_str(&format!("{EXAMPLE}\nconfirm_commitment = \"processed\"\n"))
                .is_err()
        );
    }

    /// The confirm settings reach the `SendTuning` the binary hands the sender, so a
    /// binary hard-wiring `confirmed` (or dropping the timeout/poll interval) fails here.
    #[test]
    fn send_tuning_carries_the_confirm_settings_from_config() {
        use rome_zk_solana_sender::ConfirmCommitment;
        let t = Config::from_toml_str(EXAMPLE).unwrap().send_tuning();
        assert_eq!(t.confirm_commitment, ConfirmCommitment::Finalized);
        assert_eq!(t.confirm_timeout, std::time::Duration::from_secs(15));
        assert_eq!(
            t.status_poll_interval,
            std::time::Duration::from_millis(400)
        );
        let cfg = Config::from_toml_str(&format!(
            "{EXAMPLE}\nconfirm_commitment = \"confirmed\"\nconfirm_timeout_secs = 7\nconfirm_poll_interval_ms = 123\n"
        ))
        .unwrap();
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
