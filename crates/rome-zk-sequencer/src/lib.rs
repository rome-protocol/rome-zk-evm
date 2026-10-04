//! `rome-zk-sequencer` — single-node sequencer: admission queue, signed 50 ms sub-blocks / 1 s blocks,
//! an fsynced ordered log, and a WebSocket pre-confirmation feed.
//!
//! A signed pre-confirmation absent from the finalized inbox is slashable.
//!
//! Reference designs adopted (cited again at point of use): Nitro's sequencer queue
//! (`offchainlabs/nitro` `execution/gethexec/sequencer.go` — bounded queue 1024, 12 s queue timeout, a
//! per-sender nonce cache and a nonce-failure/parking cache), OP conductor/sequencer's interrupt-driven
//! sealing timer (`op-node/rollup/sequencing/sequencer.go` — `nextAction = payload_time - sealingDuration`,
//! default sealing duration 50 ms), RISE shreds / MegaETH mini-blocks (a sub-block is ordered txs +
//! receipts root with **no state root**; the state root is computed once per block), and reth's
//! `PayloadJob` contract (`best_payload()` is always valid) — used only to shape the `Executor` trait so
//! an in-process reth could drop in behind it (it now does, via `rome-zk-executor-reth`).
//!
//! ## Scope
//! Single node, no raft. Execution runs through the [`executor::Executor`] trait: [`executor::MockExecutor`]
//! (deterministic, 21 000 gas per tx, nonce checked) and, behind the default `reth` feature, the in-process
//! `rome_zk_executor_reth::RethExecutor`. The trait is the seam a reth `PayloadJob` drops into without
//! changing anything else.
//!
//! ## No fee ordering
//! Admission is a bounded FIFO queue, not a mempool: arrival order is sealing order, always. The design
//! explicitly forbids building sub-blocks from a fee-ordered pool — pre-
//! confirmation latency is the product, and a priority auction on top of it would let a later, higher-fee
//! tx jump a pre-confirmed one, which is exactly the equivocation that is slashable. Congestion pricing,
//! if ever added, is a separate lever (admission shedding) applied to the whole queue, never a reorder key.
//!
//! ## Module map
//! - [`header`] — `SubBlockHeader`, its canonical RLP encoding and hash.
//! - [`signing`] — secp256k1 signing/recovery over a header hash; `Preconfirmation`.
//! - [`executor`] — the `Executor` trait (the seam for a real EVM) and `MockExecutor`.
//! - [`admission`] — bounded queue, per-sender nonce cache, nonce-gap parking.
//! - [`log`] — append-only fsynced ordered log, segment files, replay with torn-tail recovery.
//! - [`merkle`] — binary Merkle tree (`tx_root`, inclusion proofs).
//! - [`sealer`] — the 50 ms / 1 s timer-driven sealing loop.
//! - [`preconf`] — `SubBlockSink` (the batcher hand-off seam, **best-effort only** — see its module doc)
//!   and the preconfirmation feed event shape.
//! - [`metrics`] — Prometheus counters/histograms.
//! - [`config`] — TOML + env config, including the sequencer signing key path. Every field this crate
//!   exposes — including `admission.max_parked_per_sender`, `admission.max_next_nonce_entries`, `rpc.*`,
//!   and `sub_block_gas_limit` — is documented in
//!   `config.example.toml` at this crate's root, which `config::tests::
//!   example_config_parses_with_every_documented_knob` parses and asserts against directly; keep the two
//!   in sync.
//! - [`rpc`] — jsonrpsee JSON-RPC + WebSocket server (`eth_sendRawTransaction`, `eth_chainId`,
//!   `rome_sendRawTransaction`, `rome_getPreconfirmation`, `rome_subscribe`).
//! - [`sequencer`] — wires the above into one running node, including startup log replay.
#![forbid(unsafe_code)]

pub mod admission;
pub mod config;
pub mod deposits;
pub mod executor;
pub mod header;
pub mod log;
pub mod merkle;
pub mod metrics;
/// The standalone reth JSON-RPC node (`--executor reth`'s RPC surface). Gated behind
/// the `reth` feature — same reasoning as `rome-zk-executor-reth` itself (dependency weight).
#[cfg(feature = "reth")]
pub mod node;
pub mod preconf;
pub mod profile;
pub mod recovery;
pub mod rpc;
pub mod sealer;
pub mod sequencer;
pub mod signing;
pub mod testutil;
pub mod tx;
