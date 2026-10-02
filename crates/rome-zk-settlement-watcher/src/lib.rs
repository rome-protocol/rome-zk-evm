//! `rome-zk-settlement-watcher` — Solana signatures -> Postgres. Two independent passes: [`ingest`] (one
//! row per settlement transaction, plus one per-program attribution row carrying that row's decoded
//! lifecycle events as JSON) and [`lifecycle`] (a separate pass, reading only the database — never
//! Solana — deriving `inbox_chunk`/`batch` lifecycle state from those rows in true chronological order,
//! gated on ingest feed completeness).
//!
//! This crate covers two ingest paths, the settlement data model, read-time per-block/per-batch status, the
//! batch/chunk/root account layouts and instruction shapes (what the signatures on Solana actually carry),
//! finality, and the Postgres database `rome_zk_explorer`, shared with the indexer.
//!
//! This crate never writes to Solana and never decides finality — it reports what Solana's own transaction history says
//! ([`finality::track_finality`] advances `settlement_tx.status` toward `finalized`/`dropped` by re-checking Solana
//! itself; the FINALIZED discipline governs `rome-zk-derive`'s DA reads, not this crate's tx-history feed). Layouts,
//! PDA derivations, instruction shapes and account-position lookups come from `zk-inbox-client`/`zk-settlement-client`
//! — never re-implemented here.
#![forbid(unsafe_code)]

pub mod cursor;
pub mod decode;
pub mod finality;
pub mod ingest;
pub mod lifecycle;
pub mod migrate;
pub mod rpc;
pub mod status;

pub use cursor::{CursorState, ProgramKind};
pub use decode::{BatchEvent, ChunkEvent, ChunkEventKind, DecodedTx, DerivedEvents, ExitEvent};
pub use ingest::{run_once, PageOutcome, WatcherConfig};
pub use lifecycle::{derive_exit_once, derive_once, DeriveOutcome};
pub use migrate::migrate;
pub use rpc::{RawTx, RpcSource, SignatureInfo, Source, SourceError};
pub use status::block_status;
