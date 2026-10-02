//! `rome-zk-batcher` — channel/frame stream, shadow compressor, parallel seals, FinalizeBatch, TPU sender, re-derive-before-send
//!
//! ## Module map
//! - [`channel`] — the frame/stream codec: `RLP([blocks]) -> zstd-19 -> frames`, the inverse, and the
//!   [`channel::ShadowCompressor`] that decides when a channel is full.
//! - [`source`] — reads the sequencer's ordered log (`rome_zk_log::LogReader`) and groups its
//!   records into [`channel::Block`]s at the chain's own `sub_blocks_per_block` (read from the
//!   sequencer's `profile.json`, [`config::read_profile_identity`]).
//! - [`config`] — TOML config (chain id, program ids, sender tuning, cluster) plus
//!   [`config::read_profile_identity`], which reads the chain's block shape (`sub_blocks_per_block`,
//!   `block_gas_limit`, and `blocks_per_batch`) from the sequencer's own
//!   `profile.json` rather than a parallel config field; this crate owns no `blocks_per_batch` of its
//!   own any more.
//! - [`sender`] — the `Sender` seam (only an RPC implementation for now; TPU/QUIC is a follow-up) that
//!   submits versioned transactions and tracks confirmation.
//! - [`throttle`] — the `Throttle` backpressure seam ("lowers admission, never drops").
//! - [`sink`] — the `PostRootSink` hand-off seam a finalized batch is reported to.
//! - [`resume`] — the stateless-resume decision table: given the inbox's on-chain batch state, decide
//!   whether to post fresh, resume a half-written batch, or abandon-and-repost under a new id.
//! - [`pipeline`] — wires the above into the full per-batch flow: `OpenBatch` -> parallel per-frame
//!   `{Open+Write+Seal}` + `SealLeaf` -> `FinalizeBatch` -> verify `acc` -> hand off to a `PostRootSink`.
//! - [`metrics`] — Prometheus counters/histograms.
//! - [`preflight`] — startup check: the payer must be the chain's root authority, and the configured
//!   inbox program must match the settlement registry's — refuse to run otherwise.
//! - [`loaded_accounts`] — startup check: the configured
//!   `loaded_accounts_data_size_limit` must cover the live inbox program's real SIMD-0186 cost (including
//!   its own `ProgramData` account) at `max_frames_per_batch` — refuse to run otherwise.
//! - [`resolve`] — wires `resume`'s pure decision table to live on-chain reads: which batch id to post
//!   under, including the touched-chunk-range check and the finalized-content re-verification.
//! - [`grouping`] — groups `source`'s blocks into batches of at most `blocks_per_batch` consecutive
//!   blocks each ([`grouping::BlockGrouper`], [`grouping::SizeCappedGrouper`]), shared by `--once`'s
//!   per-run cap and continuous mode's incremental accumulation — the same continuity invariants
//!   `rome-zk-derive`'s `batch_queue::decode_batch` checks on the read side, now carried
//!   across group boundaries too.
//! - [`anchor`] — the **chain anchor**, both modes' one on-chain-verified resume
//!   point — re-derives the last finalized inbox batch's own posted blocks from its sealed chunks (or,
//!   once those are rent-recycled, from the settlement root plus an exact log replay).
//!   Replaces the old local `batcher-cursor.json`.
#![forbid(unsafe_code)]

pub mod anchor;
pub mod channel;
pub mod config;
pub mod grouping;
pub mod loaded_accounts;
pub mod metrics;
pub mod pipeline;
pub mod preflight;
pub mod resolve;
pub mod resume;
pub mod sender;
pub mod sink;
pub mod source;
pub mod throttle;
