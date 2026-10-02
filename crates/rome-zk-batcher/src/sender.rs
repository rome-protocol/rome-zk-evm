//! The Solana sender (V1 transaction build, in-flight bound, status batching, block-height resubmit, the
//! retry counter) lives in the standalone `rome-zk-solana-sender` crate. It is the same send/confirm machinery,
//! not something specific to this crate's own batch pipeline: the prover (`rome-zk-prover`) and the exit prover
//! use it too. Re-exported under this module's name so every existing call site in this crate — and every
//! external reader reaching it as `rome_zk_batcher::sender::*` — keeps compiling unchanged.
pub use rome_zk_solana_sender::*;
