//! The ordered-log record format, segments, torn-tail recovery and tail-follow reader live in the standalone
//! `rome-zk-log` crate: the sequencer writes it, the batcher/derive/indexer read it — one format, shared
//! rather than reimplemented. Re-exported under this module's name so every existing call site in this crate
//! — and every external reader reaching it as `rome_zk_sequencer::log::*` — keeps compiling unchanged.
//!
//! `log_numbering_origin` is not re-exported bare here: `crate::recovery` wraps it,
//! mapping `rome_zk_log::LogNumberingError` onto this crate's own `RecoveryError::LogNumbering` so every
//! existing caller of `rome_zk_sequencer::recovery::log_numbering_origin` sees the same error type it
//! always has.
pub use rome_zk_log::{
    replay, LogError, LogReader, LogWriter, SubBlockRecord, TornTail, MAX_FRAME_LEN,
};
