//! The cross-repo wire proof this repo's own README (rome-zk-prover-input, "Open
//! questions") flagged as missing — `rome_zk_prover_input::wire::RomePublicInput`/`RomeWitnessInput`
//! encode byte-for-byte compatibly with the fork's own `guest_rome::input::{RomePublicInput,
//! RomeWitnessInput}` (`rome-protocol/zisk-eth-client`, branch `rome-guest`), in both directions.
//!
//! **Why this is its own crate, not a dev-dependency of `rome-zk-prover-input` itself:** `guest-rome`'s own
//! `Cargo.toml` enables `alloy-primitives`'s `native-keccak` feature (the ZisK guest's accelerated keccak hook). Cargo
//! unifies that feature across every crate sharing one `Cargo.lock`'s resolution of `alloy-primitives`; declaring
//! `guest-rome` as an (optional, test-only) dependency directly inside `rome-zk-prover-input/Cargo.toml` was tried
//! first and confirmed, locally and in CI, to poison `rome-zk-prover-input`'s own normal build —
//! `alloy_primitives::keccak256` (called from `rome_zk_prover_input::header_hash`, real production code, not test-only)
//! then needs a `native_keccak256` symbol on a native host. This crate is its own standalone workspace (own
//! `Cargo.lock`, own `[workspace]` table, excluded from the root and from `rome-zk-prover-input`'s own workspace) so
//! that poisoning stays contained here — `rome-zk-prover-input`'s own `cargo test --locked` is never touched by any of
//! this.
//!
//! **No `native_keccak256` stub of its own:** an earlier revision of
//! this crate defined that `extern "C"` symbol itself (a real, if never-exercised, Keccak-256 via `sha3`)
//! to satisfy the link. `ziskos` 1.2.0-alpha already exports `native_keccak256` for non-`hints` HOST
//! builds (`src/zisklib/lib/keccak256.rs:135`) — since `guest-rome` depends on `ziskos` directly, this
//! crate's own link already resolves the symbol from there; a second definition of the same
//! `#[no_mangle] extern "C"` function is a duplicate-symbol link error waiting to happen the moment
//! `ziskos` is linked into the same binary (as it always is here, via `guest-rome`), not a defence — it
//! was untested until this fix removed it and confirmed the four tests below still pass. `sha3` is no
//! longer a dependency of this crate either.
//!
//! **Depends on a checkout of the fork at `../../.fork`** (two levels up from this crate: `crates/` →
//! the rome-zk repo root → `.fork/`) — `git clone https://github.com/rome-protocol/zisk-eth-client
//! <repo-root>/.fork && cd <repo-root>/.fork && git checkout rome-guest && git submodule update --init
//! third_party/ziskethone`. If that checkout is absent, Cargo itself refuses to resolve this crate's
//! manifest (a hard requirement — no path dependency can be made conditional at the Cargo.toml level);
//! the wrapper script this crate's README documents (`crates/rome-zk-prover-input-cross-repo-wire/run.sh`)
//! checks for the checkout FIRST and prints a named skip line rather than letting that raw Cargo error
//! stand as the only signal — see that script.

#[cfg(test)]
mod tests {
    use rome_zk_prover_input::wire as ours;

    fn sample_header(number: u64) -> alloy_consensus::Header {
        alloy_consensus::Header {
            number,
            gas_used: 21_000,
            timestamp: 1_789_337_436 + number,
            ..Default::default()
        }
    }

    fn ours_public() -> ours::RomePublicInput {
        ours::RomePublicInput {
            chain_id: 200101,
            batch: 3930,
            open_slot: 182706,
            open_unix_ts: 1_789_356_237,
            max_drift_secs: 60,
            expected_count: 1,
            chunk_bodies: vec![vec![1, 2, 3, 4, 5]],
            parent_header: sample_header(39180),
            blocks: vec![],
        }
    }

    fn guest_public() -> guest_rome::input::RomePublicInput {
        guest_rome::input::RomePublicInput {
            chain_id: 200101,
            batch: 3930,
            open_slot: 182706,
            open_unix_ts: 1_789_356_237,
            max_drift_secs: 60,
            expected_count: 1,
            chunk_bodies: vec![vec![1, 2, 3, 4, 5]],
            parent_header: sample_header(39180),
            blocks: vec![],
        }
    }

    /// Forward direction: encode with `rome_zk_prover_input::wire`, decode
    /// with `guest_rome::input` — byte for byte, field by field. This is the correctness gap the
    /// `rome-zk-prover-input` README's "Open questions" named as missing.
    #[test]
    fn forward_ours_encode_guest_decode() {
        let ours = ours_public();
        let bytes = ours.serialize();
        let decoded = guest_rome::input::RomePublicInput::deserialize(&bytes);
        assert_eq!(decoded.chain_id, ours.chain_id);
        assert_eq!(decoded.batch, ours.batch);
        assert_eq!(decoded.open_slot, ours.open_slot);
        assert_eq!(decoded.open_unix_ts, ours.open_unix_ts);
        assert_eq!(decoded.max_drift_secs, ours.max_drift_secs);
        assert_eq!(decoded.expected_count, ours.expected_count);
        assert_eq!(decoded.chunk_bodies, ours.chunk_bodies);
        assert_eq!(decoded.parent_header, ours.parent_header);
        assert_eq!(decoded.blocks.len(), ours.blocks.len());
    }

    /// Reverse direction: encode with `guest_rome::input`, decode with `rome_zk_prover_input::wire`.
    #[test]
    fn reverse_guest_encode_ours_decode() {
        let guest = guest_public();
        let bytes = guest.serialize();
        let (decoded, _): (ours::RomePublicInput, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        assert_eq!(decoded.chain_id, guest.chain_id);
        assert_eq!(decoded.batch, guest.batch);
        assert_eq!(decoded.open_slot, guest.open_slot);
        assert_eq!(decoded.open_unix_ts, guest.open_unix_ts);
        assert_eq!(decoded.max_drift_secs, guest.max_drift_secs);
        assert_eq!(decoded.expected_count, guest.expected_count);
        assert_eq!(decoded.chunk_bodies, guest.chunk_bodies);
        assert_eq!(decoded.parent_header, guest.parent_header);
    }

    /// `RomeWitnessInput`, both directions — a plain `Vec<ExecutionWitness>`, but both sides must still
    /// agree since this is a second, separately-read bincode frame (the guest's own two-read contract).
    #[test]
    fn witness_input_round_trips_both_directions() {
        let ours = ours::RomeWitnessInput {
            witnesses: vec![alloy_rpc_types_debug::ExecutionWitness::default()],
        };
        let bytes = ours.serialize();
        let decoded = guest_rome::input::RomeWitnessInput::deserialize(&bytes);
        assert_eq!(decoded.witnesses.len(), ours.witnesses.len());

        let guest = guest_rome::input::RomeWitnessInput {
            witnesses: vec![alloy_rpc_types_debug::ExecutionWitness::default()],
        };
        let bytes = guest.serialize();
        let (decoded, _): (ours::RomeWitnessInput, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        assert_eq!(decoded.witnesses.len(), guest.witnesses.len());
    }

    /// Documents the mutation check ("reorder two fields in one side's struct →
    /// red"): bincode's wire format carries no field names or tags, so a field reorder on one side is
    /// invisible to the DECODE step itself (it never errors) — it silently reassigns values to the wrong
    /// field instead. That is exactly why every assertion above is a per-field `assert_eq!` against the
    /// ENCODED value, not merely "decode succeeded": a reorder mutation is caught by a field disagreeing,
    /// not by a decode failure. Verified live (not committed as a standing test, since it requires
    /// temporarily editing a struct's field declaration order): swapping the declaration order of
    /// `batch`/`open_slot` in `rome-zk-prover-input/src/wire.rs`'s `RomePublicInput` turns
    /// `forward_ours_encode_guest_decode` red (`decoded.batch`/`decoded.open_slot` come back swapped
    /// against the values this test encoded) — see this crate's README "Mutation verification" for the
    /// exact commands and the captured RED output.
    #[test]
    fn mutation_methodology_is_documented_not_a_standing_assertion() {
        // No-op: the real mutation is a source edit + a live rerun, recorded in the README
        // rather than left as a permanent test that would otherwise require breaking the real contract
        // on every CI run.
    }
}
