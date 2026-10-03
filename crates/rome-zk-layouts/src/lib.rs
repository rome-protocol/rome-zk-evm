//! Shared byte layouts for the zk-inbox chunk account (`ZKIB`), batch account (`ZKBT`), per-chain
//! batch-cursor account (`ZKBC`), the zk-settlement root account (`ZKRT`), the per-batch pending account
//! (no magic — see `pending`'s module doc), the verifier registry account (`ZKVR`), and the
//! global-config / chain-config / perm-nonce / reserved-allow accounts — plus the accumulator's `acc`
//! commitment formula and the forced-lane's empty-root domain constant, and the exit account layouts —
//! `exit_config`, `exit_record`, `exit_window`, `exit_nullifier` — plus the exit message's
//! ABI preimage/hash/storage-slot, nullifier-page bit math and cap-unit conversion (`exit` module).
//!
//! **This is also the one home for every account's PDA seeds and derivation**: each module above exports `seeds(...)`
//! (the raw seed components, exactly what an `invoke_signed` seed array needs plus its bump) and `pda(program_id, ...)
//! -> (Pubkey, u8)`. Programs and clients call these instead of redefining the same tag/field-order/endianness
//! combination — `programs/zk-inbox`, `programs/zk-settlement`, `zk-inbox-client` and `zk-settlement-client` all
//! resolve to the same functions here, directly or through a thin same-name wrapper kept for call-site stability.
//!
//! Consumed by `programs/zk-inbox` (on-chain, syscall keccak via `rome_zk_merkle::HashV`) and
//! `crates/zk-inbox-client` (off-chain, software keccak) for the chunk/batch/cursor layouts, and by
//! `programs/zk-settlement` + `crates/zk-settlement-client` for the root/pending/registry/config layouts
//! — all sides read and write the identical byte offsets, field order and endianness.
//!
//! Layout structs expose pubkey fields as raw `[u8; 32]`, not `solana_program::Pubkey` — a caller that
//! wants a typed `Pubkey` wraps the bytes itself (`zk-inbox-client::decode_batch_account` does this).
//! `solana-program` (pinned, SBF-compatible) is an optional dependency behind the default-on `solana`
//! feature — needed only for the `Pubkey` type every `pda`/`seeds` helper uses; every byte-level function
//! is unconditional and pure, so a zkVM guest builds this crate with `default-features = false`.
//!
//! **All integers are little-endian.**

pub mod batch;
pub mod chain_config;
pub mod chainid;
pub mod chunk;
pub mod cursor;
pub mod deposit;
pub mod deposit_queue;
pub mod exit;
pub mod frame;
pub mod global_config;
pub mod pending;
pub mod perm_nonce;
pub mod public_values;
pub mod registry;
pub mod reserved_allow;
pub mod root;

pub use rome_zk_merkle::HashV;

/// Domain-separation string for the forced lane's root while the forced lane is empty (empty in v1:
/// `forced_root = keccak(\"rome-zk/forced/empty/v1\")`). Not the hash of any real forced-lane content —
/// there isn't one yet.
pub const FORCED_EMPTY_DOMAIN: &[u8] = b"rome-zk/forced/empty/v1";

/// `keccak(FORCED_EMPTY_DOMAIN)` — the forced-lane root while the forced lane is empty.
pub fn forced_empty_root(h: &impl HashV) -> [u8; 32] {
    h.hashv(&[FORCED_EMPTY_DOMAIN])
}

/// The accumulator's per-batch commitment: `acc = keccak(chain_id_le[8] ‖ batch_le[8] ‖
/// open_slot_le[8] ‖ expected_count_le[4] ‖ root[32] ‖ forced_root[32])`. All integers little-endian.
/// Hash-agnostic like `rome_zk_merkle`: the on-chain program passes the syscall-backed keccak, off-chain
/// callers a software keccak — both must produce the same bytes for the same inputs.
#[allow(clippy::too_many_arguments)]
pub fn acc(
    h: &impl HashV,
    chain_id: u64,
    batch: u64,
    open_slot: u64,
    expected_count: u32,
    root: &[u8; 32],
    forced_root: &[u8; 32],
) -> [u8; 32] {
    h.hashv(&[
        &chain_id.to_le_bytes(),
        &batch.to_le_bytes(),
        &open_slot.to_le_bytes(),
        &expected_count.to_le_bytes(),
        root,
        forced_root,
    ])
}

/// Layout parse errors: too-short buffer, wrong magic, or (batch account only) wrong version.
#[derive(Debug, PartialEq, Eq)]
pub enum LayoutError {
    TooShort {
        need: usize,
        got: usize,
    },
    BadMagic,
    BadVersion,
    /// A ZisK output word (`public_values::unpack_zisk_outputs`) does not fit in a `u32` — not a
    /// guest-produced buffer.
    BadZiskWord {
        index: usize,
    },
    /// A ZisK output word beyond the 52 the v2 struct uses is non-zero — not a v2 public-values buffer.
    BadZiskTail {
        index: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8; 32]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// Golden test (contract): pins the `acc` formula's field order and endianness against an
    /// independently computed keccak256 (Python pycryptodome, not this crate), so a future field
    /// reorder or endianness slip is caught here — not only via the on-chain/off-chain equivalence
    /// asserted in `programs/zk-inbox/tests/accumulator.rs`.
    #[test]
    fn acc_golden_hex_for_fixed_inputs() {
        let root = [0x11u8; 32];
        let forced_root = [0x22u8; 32];
        let got = acc(
            &(rome_zk_merkle::keccak256 as fn(&[&[u8]]) -> [u8; 32]),
            7,
            3,
            100,
            5,
            &root,
            &forced_root,
        );
        assert_eq!(
            hex(&got),
            "76bff06e13c993d720a8ee60a50bb723078df055e093fd21514bc2b7c72070f4"
        );
    }

    #[test]
    fn forced_empty_root_matches_independently_computed_domain_hash() {
        assert_eq!(
            hex(&forced_empty_root(
                &(rome_zk_merkle::keccak256 as fn(&[&[u8]]) -> [u8; 32])
            )),
            "a939698ea5a2cb2ee272e900f2f3d986294007dcb635b1846b597aec642e938c"
        );
    }
}
