# rome-zk-prover-input-cross-repo-wire

Proves `rome-zk-prover-input`'s `wire::RomePublicInput`/`RomeWitnessInput` encode byte-for-byte
compatibly with the guest's own `guest_rome::input::{RomePublicInput,
RomeWitnessInput}` (wire v3; `rome-protocol/zisk-eth-client`, branch `deposits-guest`,
`crates/clients/rome/guest/src/input.rs`) — in both directions. This is the correctness gap
`rome-zk-prover-input/README.md`'s "Open questions" named as missing.

## Why this is its own crate, not a dev-dependency of `rome-zk-prover-input` itself

`guest-rome`'s own `Cargo.toml` enables `alloy-primitives`'s `native-keccak` feature (the ZisK
guest's accelerated keccak hook). Cargo unifies that feature across every crate sharing
one `Cargo.lock`'s resolution of `alloy-primitives`. Declaring `guest-rome` as an optional, test-only
dependency directly inside `rome-zk-prover-input/Cargo.toml` was tried first — confirmed, in CI
and locally, to poison `rome-zk-prover-input`'s own normal build: `alloy_primitives::keccak256` (called
from `rome_zk_prover_input::header_hash`, real production code, never test-only) then needs a
`native_keccak256` symbol that exists only inside a ZisK guest runtime, and the CLI **bin** target fails
to *link* even though nothing in its own code path touches `guest-rome`.

This crate is its own standalone workspace (own `Cargo.lock`, own `[workspace]` table, excluded from both
the root workspace and `rome-zk-prover-input`'s own) so that poisoning stays contained here. It supplies
its own `native_keccak256` stub (`src/lib.rs`) — a real, correct Keccak-256 (via `sha3`), never actually
exercised by this proof, so the test binary links on a native host.

## Depends on a `.fork/` checkout — never committed

`Cargo.toml` path-depends on `../../.fork/crates/clients/rome/guest` — two levels up from this crate
(`crates/` → the rome-zk repo root → `.fork/`). Clone it once, as a sibling of `crates/` under the repo
root:

```sh
git clone --branch deposits-guest https://github.com/rome-protocol/zisk-eth-client <repo-root>/.fork
cd <repo-root>/.fork
git submodule update --init third_party/ziskethone
```

The submodule step is required even though this proof never touches `third_party/ziskethone` itself —
`third_party/ziskethone` is a `path` dependency of another member of the fork's own root workspace
(`crates/clients/*/*` — a glob that matches `guest-rome`'s siblings too), so Cargo refuses to resolve ANY
manifest under that workspace, `guest-rome` included, until the submodule is present.

`.fork/` is in `.gitignore` — it must never be committed into rome-zk. A Cargo path dependency cannot be
made conditional at the manifest level (if the path does not exist, `cargo` refuses to resolve the
manifest at all, with a raw filesystem error) — `run.sh` is the "skip with a clear message" wrapper this
crate uses instead of letting that raw error stand as the only signal:

```sh
./run.sh              # skips loud if .fork/ is absent; runs `cargo test` otherwise
```

## What the tests prove

- `forward_ours_encode_guest_decode` — encode with `rome_zk_prover_input::wire`, decode with
  `guest_rome::input`, assert every field (the v3 deposit range included, with and without deposits).
- `reverse_guest_encode_ours_decode` — the opposite direction.
- `both_sides_encode_the_same_bytes` — one value, both encoders, identical bytes.
- `witness_input_round_trips_both_directions` — `RomeWitnessInput`, both ways (a second, separately-read
  bincode frame in the guest's own two-read contract).

## Mutation verification (not a standing test)

bincode's wire format carries no field names or tags, so reordering two fields on one side is invisible
to the *decode* step itself (it never errors) — it silently reassigns values to the wrong field instead.
That is why every assertion above is a per-field `assert_eq!` against the encoded value, not merely
"decode succeeded": a reorder mutation is caught by a field disagreeing, not a decode failure.

Verified live once (not committed as a standing test — it requires temporarily editing a
struct's field declaration order, which would otherwise break the real contract on every CI run):
swapping `batch`/`open_slot`'s declaration order in `rome-zk-prover-input/src/wire.rs`'s
`RomePublicInput` and rerunning `cargo test` here turned both round-trip tests red —

```
thread 'tests::forward_ours_encode_guest_decode' panicked at src/lib.rs:104:9:
assertion `left == right` failed
  left: 182706
 right: 3930
```

(`182706`/`3930` are `open_slot`/`batch` from the test's own sample input — decoded back swapped, exactly
the silent-corruption failure mode this proof exists to catch.) The edit was reverted with `git checkout
--` immediately after capturing this output, and the rerun after the revert was green again.

## Wire v3 re-check, and the lockfile

Re-run for wire v3 (the deposit range after `blocks`): swapping the declarations of `settlement_program` and
`deposit_hash_from` in `rome-zk-prover-input/src/wire.rs` (same type, so decode never errors) turned
`forward_ours_encode_guest_decode`, `reverse_guest_encode_ours_decode` and `both_sides_encode_the_same_bytes`
red (`left: [68, 68, ...]`, `right: [51, 51, ...]`), and restoring the order turned them green again.

`Cargo.lock` here was regenerated against the guest-v3 checkout, seeded from `rome-zk-prover-input/Cargo.lock`:
the old lock predated that crate's Solana 4.3 bump, and a fresh resolve (no seed) picks a
`solana-signature`/`solana-keypair` pair that does not compile. If the lock goes stale again, seed it the same way.
