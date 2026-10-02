//! The channel/frame stream codec now lives in the standalone `rome-zk-channel` crate: one codec,
//! shared byte-for-byte between this crate (encode) and `rome-zk-derive` (decode), rather than
//! reimplemented on either side. Re-exported under this module's name so every existing call site in
//! this crate — and every test that reaches it as `rome_zk_batcher::channel::*` — keeps compiling
//! unchanged.
pub use rome_zk_channel::*;
