//! `rome-zk-exit-prover` — the off-chain follower: watches the L2 exit portal for `ExitInitiated`, proves each
//! message against a FINAL batch's `state_root` via `eth_getProof`, verifies the proof LOCALLY before ever sending
//! (a doomed proof must never spend a fee), and sends `ProveExit` (28) the same way `rome-zk-prover` sends
//! `PostRootProved` — through [`rome_zk_solana_sender`]'s V1 [`rome_zk_solana_sender::Sender`].
//!
//! ## What this crate does NOT do
//! - No chain writes anywhere in its own test suite: every test here runs against fixtures
//!   (`fixtures/exit/*.json`) and refusing fakes, never a live cluster or verifier.
//! - No staged proof path: [`core::Refusal::ProofTooLarge`] is a hard pre-send refusal; a staged
//!   `StageExitProof` path is a LATER contingency, not built here.
//! - No replay/cap bookkeeping of its own: those are enforced ONLY on chain
//!   (`programs/zk-settlement::exit::prove_exit`). This crate DOES read the on-chain nullifier bit
//!   pre-send (the source of truth for whether an exit is already done, [`core::attempt_exit`]),
//!   but never decides replay/cap itself; it only classifies what a send comes back with
//!   (`ExitAlreadyProved`/`ExitCapExceeded`) or reads the bit ahead of one.
//! - No disk cursor or database: [`follower::Follower`]'s state is an in-process cache only, rebuilt from
//!   `eth_getLogs(portal, portal_from_block)` on every restart (a persisted cursor was the rejected alternative).
//!
//! ## Modules
//! - [`rpc`]: `eth_getLogs`/`eth_getProof`/`eth_getBlockByNumber` over `ureq` (mirrors
//!   `rome-zk-prover-input::verifier::rpc_call`'s shape).
//! - [`logs`]: decodes one `ExitInitiated` log entry into an `ExitMessage`.
//! - [`proof`]: `eth_getProof` -> `rome_zk_mpt::ExitProof`, and the local pre-send MPT re-verify.
//! - [`settlement`]: reads `root`/`pending(batch)`/`exit_config`/`exit_nullifier(page)` (the
//!   [`settlement::SettlementReader`] trait + its real `solana-client`-backed implementation).
//! - [`core`]: [`core::attempt_exit`] — the one-message attempt: refuse-before-send (including the
//!   pre-send nullifier check), then send-and-classify.
//! - [`follower`]: [`follower::Follower`] — the stateful, chain-derived retry/stuck cache the bin drives
//!   (`ingest` → `due` → `attempt_exit` → `apply`).
//! - [`config`]: the TOML-rendered runtime `Config`.
//! - [`metrics`]: Prometheus metrics served on `/metrics`.
//! - [`run`]: [`run::poll_once`] — the whole poll (`eth_getLogs` → `ingest` → `attempt_exit` on every due
//!   message → `apply`), lifted out of the bin so it is testable.

#![forbid(unsafe_code)]

pub mod config;
pub mod core;
pub mod follower;
pub mod logs;
pub mod metrics;
pub mod proof;
pub mod rpc;
pub mod run;
pub mod settlement;
