//! `rome-zk-profile` — one home for a chain's cadence/cap profile, its persisted identity, and the
//! `profile.json` file format.
//!
//! Before this crate existed, three different crates each carried their own idea of a chain's
//! `blocks_per_batch` default (`rome-zk-sequencer`, `rome-zk-batcher`, `rome-zk-derive`), and the
//! `sub_blocks_per_block`/`sub_block_gas_limit` design defaults lived on `rome-zk-sequencer`'s `sealer`
//! and `executor` modules with `rome-zk-sequencer::profile` merely re-reading them. This crate inverts
//! that: every `DEFAULT_*` numeric constant, the `[profile]` config shape ([`Profile`]), the identity
//! persisted beside a chain's ordered log ([`ProfileIdentity`]), and the pure file layer that reads and
//! writes `profile.json` all live here, with zero dependency on Solana, reth, alloy or tokio — just
//! `serde`, `serde_json` and `thiserror`. `rome-zk-sequencer`, `rome-zk-batcher` and `rome-zk-derive` all
//! depend on this crate instead of on each other for these values.
//!
//! This crate deliberately does **not** know about the ordered log's own format, migration flags
//! (`--write-profile-json`), or the numbering-origin check (`log_numbering_origin`) — those stay
//! sequencer-side orchestration concerns (`rome_zk_sequencer::recovery`) built on top of the pure
//! read/write primitives here ([`read_profile_json`], [`write_profile_identity`]). Keeping this crate
//! reth-free is what lets `rome-zk-derive` and `rome-zk-batcher` depend on it directly without pulling in
//! `rome-zk-sequencer`'s heavy `reth` feature.

mod profile;
mod storage;

pub use profile::{
    Profile, ProfileError, ProfileIdentity, DEFAULT_ADMISSION_SHED_PCT, DEFAULT_BLOCKS_PER_BATCH,
    DEFAULT_DA_BYTES_PER_SEC, DEFAULT_EMPTY_BLOCK_INTERVAL_SECS, DEFAULT_MAX_DRIFT_SECS,
    DEFAULT_PROVER_GAS_PER_SEC, DEFAULT_SUB_BLOCKS_PER_BLOCK, DEFAULT_SUB_BLOCK_GAS_LIMIT,
    DEFAULT_SUB_BLOCK_MS, FIRST_BLOCK,
};
pub use storage::{
    read_profile_json, write_profile_identity, LegacyProfileIdentityV0, LegacyProfileIdentityV1,
    ProfileJsonError, StoredProfileJson, PROFILE_JSON_FILENAME,
};
