//! The sub-block header (canonical encoding, hash, domain-separated signing hash) now lives in the
//! standalone `rome-zk-log` crate — the record is the header plus its payload.
//! Re-exported under this module's name so every existing call site in this crate
//! keeps compiling unchanged.
pub use rome_zk_log::{SubBlockHeader, SIGNING_DOMAIN};
