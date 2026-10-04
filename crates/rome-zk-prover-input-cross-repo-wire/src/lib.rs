//! The cross-repo wire proof this repo's own README (rome-zk-prover-input, "Open
//! questions") flagged as missing — `rome_zk_prover_input::wire::RomePublicInput`/`RomeWitnessInput`
//! encode byte-for-byte compatibly with the fork's own `guest_rome::input::{RomePublicInput,
//! RomeWitnessInput}` (`rome-protocol/zisk-eth-client`, branch `deposits-guest`: wire v3), in both
//! directions.
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
//! the rome-zk repo root → `.fork/`) — `git clone --branch deposits-guest
//! https://github.com/rome-protocol/zisk-eth-client <repo-root>/.fork && cd <repo-root>/.fork && git
//! submodule update --init third_party/ziskethone` (the fork is private: a box clones it from a git
//! bundle). If that checkout is absent, Cargo itself refuses to resolve this crate's
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

    const SETTLEMENT_PROGRAM: [u8; 32] = [0x33; 32];
    const DEPOSIT_HASH_FROM: [u8; 32] = [0x44; 32];

    /// The deposit records both sides encode: distinct values in every field, so a swapped or
    /// mis-sized field shows up.
    fn deposit_values() -> [([u8; 32], [u8; 20], u64); 2] {
        [
            ([0x11; 32], [0x22; 20], 1_000_000_000),
            ([0x55; 32], [0x66; 20], 2),
        ]
    }

    fn ours_public(with_deposits: bool) -> ours::RomePublicInput {
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
            settlement_program: SETTLEMENT_PROGRAM,
            deposit_from: if with_deposits { 5 } else { 0 },
            deposit_hash_from: DEPOSIT_HASH_FROM,
            deposits: if with_deposits {
                deposit_values()
                    .into_iter()
                    .map(|(sender, recipient, amount_gwei)| ours::DepositInput {
                        sender,
                        recipient,
                        amount_gwei,
                    })
                    .collect()
            } else {
                vec![]
            },
        }
    }

    fn guest_public(with_deposits: bool) -> guest_rome::input::RomePublicInput {
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
            settlement_program: SETTLEMENT_PROGRAM,
            deposit_from: if with_deposits { 5 } else { 0 },
            deposit_hash_from: DEPOSIT_HASH_FROM,
            deposits: if with_deposits {
                deposit_values()
                    .into_iter()
                    .map(
                        |(sender, recipient, amount_gwei)| guest_rome::input::DepositInput {
                            sender,
                            recipient,
                            amount_gwei,
                        },
                    )
                    .collect()
            } else {
                vec![]
            },
        }
    }

    /// Every field of the v3 public input, one side's value against the other's, deposits included.
    macro_rules! assert_same_public {
        ($decoded:expr, $expected:expr) => {{
            let (d, e) = (&$decoded, &$expected);
            assert_eq!(d.chain_id, e.chain_id);
            assert_eq!(d.batch, e.batch);
            assert_eq!(d.open_slot, e.open_slot);
            assert_eq!(d.open_unix_ts, e.open_unix_ts);
            assert_eq!(d.max_drift_secs, e.max_drift_secs);
            assert_eq!(d.expected_count, e.expected_count);
            assert_eq!(d.chunk_bodies, e.chunk_bodies);
            assert_eq!(d.parent_header, e.parent_header);
            assert_eq!(d.blocks.len(), e.blocks.len());
            assert_eq!(d.settlement_program, e.settlement_program);
            assert_eq!(d.deposit_from, e.deposit_from);
            assert_eq!(d.deposit_hash_from, e.deposit_hash_from);
            assert_eq!(d.deposits.len(), e.deposits.len());
            for (x, y) in d.deposits.iter().zip(e.deposits.iter()) {
                assert_eq!(x.sender, y.sender);
                assert_eq!(x.recipient, y.recipient);
                assert_eq!(x.amount_gwei, y.amount_gwei);
            }
        }};
    }

    /// Forward direction: encode with `rome_zk_prover_input::wire`, decode
    /// with `guest_rome::input` — byte for byte, field by field, with and without deposits. This is the
    /// correctness gap the `rome-zk-prover-input` README's "Open questions" named as missing.
    #[test]
    fn forward_ours_encode_guest_decode() {
        for with_deposits in [false, true] {
            let ours = ours_public(with_deposits);
            let bytes = ours.serialize();
            let decoded = guest_rome::input::RomePublicInput::deserialize(&bytes);
            assert_same_public!(decoded, ours);
        }
    }

    /// Reverse direction: encode with `guest_rome::input`, decode with `rome_zk_prover_input::wire`.
    #[test]
    fn reverse_guest_encode_ours_decode() {
        for with_deposits in [false, true] {
            let guest = guest_public(with_deposits);
            let bytes = guest.serialize();
            let (decoded, used): (ours::RomePublicInput, usize) =
                bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
            assert_eq!(used, bytes.len(), "trailing bytes after the v3 input");
            assert_same_public!(decoded, guest);
        }
    }

    /// The same value encodes to the same bytes on both sides.
    #[test]
    fn both_sides_encode_the_same_bytes() {
        for with_deposits in [false, true] {
            assert_eq!(
                ours_public(with_deposits).serialize(),
                guest_public(with_deposits).serialize()
            );
        }
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
