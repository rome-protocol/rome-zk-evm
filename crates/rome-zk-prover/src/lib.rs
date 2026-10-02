//! `rome-zk-prover` — the always-on proving core.
//!
//! **Single-batch building blocks:** config and the vkey-of-record loader ([`config`]); the `Prover` trait
//! and its `cargo-zisk` subprocess implementation ([`prover`]); the ZisK PLONK calldata decoder
//! ([`calldata`]); building a `PostRootProved` send's arguments from a decoded proof's public values
//! ([`publics`]).
//!
//! **The rest of the single-batch path:** the structural [`calldata::RecordChecked`] gate on
//! the ABI-building path ([`abi`]); the one-`getMultipleAccounts`-call chain snapshot and its named
//! refusals ([`anchor`]); the poster (continuity + local-verify guards, refusal classification,
//! [`poster`]); the finalize sweep and retention-gated close ([`finality`]).
//!
//! **The always-on follower loop** ([`follower`]): the state machine that proves every finalized inbox
//! batch in order, idles with zero `Prover::prove` calls while nothing is behind, resumes safely across
//! a restart, and reports Prometheus metrics ([`metrics`]).
//! `bin/rome-zk-prover.rs`'s `--once --batch N [--dry-run]` runs `follower::run_one` for exactly one
//! batch; `--follow [--dry-run] [--iterations N]` runs `follower::run` until stopped.
//!
//! **Postgres history** ([`store`]): every follower transition is recorded through a `Store` seam —
//! `NoopStore` when `database_url` is unset, `PgStore` (Postgres) otherwise. The chain stays the only
//! cursor the loop itself ever reads; a store failure at run time is logged and counted, never allowed
//! to stop proving.
#![forbid(unsafe_code)]

pub mod abi;
pub mod anchor;
pub mod calldata;
pub mod config;
pub mod finality;
pub mod follower;
pub mod metrics;
pub mod poster;
pub mod prover;
pub mod publics;
pub mod store;
