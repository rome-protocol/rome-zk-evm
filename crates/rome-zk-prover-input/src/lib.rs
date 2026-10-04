//! Host-only input generator for the batch guest: reads a
//! finalized batch's inbox chunk bodies + batch account (Solana, `zk-inbox-client`'s own read path),
//! fetches each block + its execution witness from a reth verifier, and writes the guest's two bincode
//! inputs. Never generates a proof and never writes to any cluster.

pub mod build;
pub mod genesis;
pub mod inbox;
pub mod verifier;
pub mod wire;

pub use inbox::{decode_block_range, fetch_and_verify_batch, BatchAccount, InboxError};
pub use wire::{write_slice_frame, DepositInput, RomePublicInput, RomeWitnessInput};

/// `keccak(rlp(header))` — used to anchor `parent_hash` in [`build::ExpectedPublicValues`], the same hash
/// `guest_rome::chain::header_hash` computes in-guest (kept as a tiny, independently-written duplicate
/// rather than a cross-repo dependency — see `src/wire.rs`'s module doc for why).
pub fn header_hash(header: &alloy_consensus::Header) -> alloy_primitives::B256 {
    use alloy_rlp::Encodable;
    let mut buf = Vec::with_capacity(header.length());
    header.encode(&mut buf);
    alloy_primitives::keccak256(&buf)
}
