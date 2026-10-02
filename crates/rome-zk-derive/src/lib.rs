//! `rome-zk-derive` — derivation node: kona-shaped stages over the Solana inbox, committed block env,
//! replay gate, strict policy.
//!
//! ## Pipeline
//!
//! ```text
//! SolanaTraversal -> InboxRetrieval -> FrameQueue -> ChannelBank -> BatchQueue -> AttributesQueue -> EngineController
//! ```
//!
//! Pull-based, exactly like kona's op-node-derivation-pipeline shape: each stage is a small struct that
//! pulls from the one before it and hands a narrower type to the one after it. The seams that an
//! extension can substitute are the two that talk to something *outside* this process — [`AccountReader`]
//! (Solana account reads: real RPC in production, `solana-program-test`'s `BanksClient` in tests) and
//! [`engine::EngineApi`] (the Engine API: a real JWT-authed reth in production, a `MockEngineApi` in
//! tests) — everything in between (frame parsing, channel reassembly, batch decode + strict validity,
//! block-env computation) is pure, deterministic, single-implementation logic with no seam to abstract.
//!
//! This node only **reads** Solana ("V1-free read paths ... no transactions") — it never
//! builds or signs a Solana transaction, so none of the SIMD-0385 V1 tx-format
//! constraints apply to it.
#![forbid(unsafe_code)]

pub mod attributes;
pub mod batch_queue;
pub mod chain_bound;
pub mod channel_bank;
pub mod config;
pub mod engine;
pub mod frame_queue;
pub mod inbox;
pub mod metrics;
pub mod pipeline;
pub mod reader;
pub mod resume;
pub mod testutil;
pub mod traversal;

/// The two outcomes a pipeline stage can produce. Named after kona's own
/// `PipelineErrorKind` (`op-alloy`/kona `derive::errors`), which itself named three: **Temporary**
/// ("nothing new yet, back off and retry — not a fault"), **Critical** ("the strict validity
/// policy is violated — stop, post nothing to the engine, and surface this loudly"), and
/// **Reset** ("the pipeline's local state has drifted — walk the cursor back and rebuild it").
///
/// **`Reset` is deliberately absent from this crate.** With finalized-only Solana
/// reads and sequential, never-reused batch ids there is no reorg for [`traversal::SolanaTraversal`] to
/// recover from — an earlier change already deleted its one production trigger (an `open_slot`-gap
/// heuristic that livelocked: any batcher pause longer than one timeout wedged the node forever, five
/// consecutive `Reset`s and no engine call) and reassigned `Reset` to mean only
/// "the engine's head disagrees with the derived head" — but nothing in this crate ever actually raised
/// it for that case either: a genuine identity mismatch is, and always was, [`PipelineError::Critical`]
/// ([`engine::EngineController::advance`]'s consolidation and build-identity checks) — the correct
/// outcome for an equivocation or operator error, never a recoverable drift to walk back from. Removing
/// the now-provably-dead variant removes its one caller too
/// ([`pipeline::DerivePipeline::step`]'s former `Reset` arm, which rewound
/// [`traversal::SolanaTraversal`] and re-seeded the engine in-process). A restart still re-seeds —
/// from the settlement-root anchor (`resume`) or the engine's own real genesis
/// ([`engine::EngineController::from_engine_head`]) — the same way it always did; nothing is lost by not
/// having an in-process reaction to a condition that never fired.
#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    /// Nothing new yet (the next batch isn't finalized, an RPC call timed out, the engine is busy) —
    /// the caller should back off and retry; this is not a fault in anything the pipeline has seen.
    #[error("temporary: {0}")]
    Temporary(String),
    /// The strict validity policy was violated: a chunk that does not decode, a channel that
    /// does not reassemble, a tx that does not decode/chain-id-match, a sealed block whose committed
    /// identity diverges from what the engine actually built or already holds, or a batch's declared
    /// timestamps breaching the drift bound. Never a skip — the pipeline stops and
    /// nothing is posted to the engine.
    #[error("critical: {0}")]
    Critical(String),
}

// Kona's pipeline also has `Signal::{Reset, FlushChannel}`, its mechanism for a downstream stage to tell an
// upstream one to reset or drop a channel. Not a separate type here: [`InboxRetrieval`] reads every sealed
// chunk of a batch in one call (a finalized batch's chunks are all already on Solana), so
// [`channel_bank::ChannelBank`] never accumulates a channel across more than one
// [`pipeline::DerivePipeline::step`] call — there is no partial, abandoned-mid-stream channel for a
// `FlushChannel` signal to act on; its bounded, oldest-first eviction (this struct's own doc) already covers
// the one case that matters (a channel that will never complete). `Signal::Reset` has no analogue here either
// (this enum's own doc): a restart re-seeds from the settlement-root anchor or the engine's own genesis
// directly, never a signal a stage reacts to mid-run.
