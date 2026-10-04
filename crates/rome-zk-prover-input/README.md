# rome-zk-prover-input

Host-only input generator for the batch guest: reads a finalized
batch's inbox chunk bodies + batch account from the Tiber devnet, fetches each block and its execution witness
from a reth verifier, and writes the guest's two bincode inputs — `guest-rome`'s wire format
(wire v3; `rome-protocol/zisk-eth-client`, branch `deposits-guest`, `crates/clients/rome/guest`). Never writes to any
cluster; never generates a proof (proving is a separate step).

Verified end to end against a real batch: see "Real batch, verified" below.

## Its own workspace

This crate is **excluded** from the rome-zk root workspace (`Cargo.toml`'s `exclude`), with its own
`Cargo.lock`: its wire types are pinned exact to the fork's `guest-rome` versions
(`alloy-consensus`/`alloy-genesis`/`alloy-rpc-types-debug`/`alloy-eips` = 2.0.5, `alloy-primitives` =
1.6.0), which conflicts with `rome-zk-batcher`'s `alloy = "^2.4"` in one shared lockfile — verified, not a
style preference; the resolver refuses outright. Build and test it directly:

```sh
cd crates/rome-zk-prover-input
cargo test --locked
```

Never `-p rome-zk-prover-input` from the repo root (it is not a member there). It also declares its own
empty `[workspace]` table — without it, a package excluded from a parent workspace makes cargo keep
walking up the directory tree looking for a different one, which (found the hard way, building from a
nested worktree) can attach it to an unrelated `Cargo.toml` several directories further up.

## Reth-verifier fetch: plain JSON-RPC, not `alloy-provider`

`src/verifier.rs` posts `eth_getBlockByNumber`/`debug_executionWitness` over `ureq` and decodes the
response straight into the pinned `alloy_rpc_types_eth::Block`/`Header` and
`alloy_rpc_types_debug::ExecutionWitness` types (plain serde structs, no dependency on `alloy-provider`
themselves), then applies the same `.into()`/`.into_consensus()` conversion the fork's own
`input-reth::fetch_block` uses via a provider. An earlier draft depended on `alloy-provider` directly —
its generated Multicall3/ArbSys `sol!` bindings need an `alloy-sol-types` version this crate's own small,
standalone dependency graph never resolved consistently alongside `alloy-primitives = 1.6.0`
(`abi_decode_raw_with_config`/`abi_decode_returns_with_config` simply do not exist at that pairing — not a
version *skew* fixable by pinning harder). Removing `alloy-provider`/`alloy-rpc-client`/`alloy-transport*`
entirely (rather than feature-gating them) means this crate builds and tests clean by default — the CLI
binary and the network-touching `build`/`verifier` modules included; the actual RPC calls are exercised
only by `#[ignore]`d tests (against recorded fixtures under `fixtures/prover-input/rpc/`, captured once
over the tunnel) and by running the CLI by hand.

`fetch_block_number` (`eth_blockNumber`) reads the verifier's own current block height — the follower
loop (`rome-zk-prover`) waits for this before proving a batch whose blocks the verifier has
not yet derived. `build_batch_input` (in `src/build.rs`) no longer takes a bare `verifier_rpc: &str`;
it takes `verifier: &mut impl VerifierFetch`, where `VerifierFetch` is this module's own
`head`/`header`/`block`/`witness` trait. `RemoteVerifier` is the real, HTTP-backed implementation —
thin wrappers around this module's own four free functions, unchanged — every production caller
(this crate's own CLI, `rome-zk-prover`) uses; the seam exists so a follower loop's own tests can
drive the whole input-building step against a fake, never a live reth node.

## Why a copy of the guest's wire types, not a dependency on the fork

`src/wire.rs` duplicates `guest-rome::input::{RomePublicInput, RomeWitnessInput}` field for field rather
than depending on the fork crate directly. The fork is a second private repo; this crate's CI (rome-zk's
own `cargo test --workspace`, run from a checkout that does not have the fork cloned anywhere nearby) has
no established credentials for it. Bincode compatibility between the two copies depends on both sides
compiling the *exact same* `alloy_consensus::Header`/`reth_ethereum_primitives::Block`/
`alloy_rpc_types_debug::ExecutionWitness` crate versions — pinned here to match what the fork's own
`bin/guests/stateless-validator-rome` ELF resolves to (a standalone workspace itself, for the same
version-conflict reason). **Wire v2:** `RomePublicInput` no longer carries a
`chain_config` field at all — the chain's rules are baked into the guest ELF at compile time instead
(`guest-rome::chain_config`). `alloy-genesis`'s `serde-bincode-compat` feature stays a dependency only for
the earlier v1→v2 fixture migration (its test is gone now that the fixtures are v3) and `genesis::load_chain_config` (still learns `chain_id` from
`--genesis` for PDA derivation) — no current-shape code needs it. **Proven, both directions, byte for
byte:** see `crates/rome-zk-prover-input-cross-repo-wire` — a sibling crate, its own workspace, that
encodes with this crate's types and decodes with the fork's own, and the reverse.

**Wire v3 (deposits):** `RomePublicInput` appends the deposit range after `blocks` — `settlement_program`,
`deposit_from`, `deposit_hash_from` and `deposits` (`DepositInput { sender, recipient, amount_gwei }`).
`build_batch_input` writes the empty range for now: the batch account's settlement program, `deposit_from` 0,
`deposit_hash_from` = `h_0(settlement_program, chain_id)` (the cursor's value on a chain that never took a
deposit) and no deposits; real ranges arrive with the batch header v3. The two committed fixtures were migrated
from v2 by `wire::tests::migrate_the_committed_fixtures_from_wire_v2_to_v3` (ignored, run once); the standing
test `committed_fixtures_are_wire_v3_with_every_v2_field_unchanged` pins the v2 bytes' hashes and proves
every v2 field and the witness frame are unchanged. **Flag day:** a v2 ELF cannot read v3 input and the
reverse, so this lands only together with the guest release and its new verification keys.

## Command

```sh
rome-zk-prover-input --batch <id> --solana-rpc <url> --inbox <program-id> --settlement <program-id> \
  --verifier-rpc <url> --genesis <genesis.json> --out fixtures/prover-input/txv1-dev-batch-<id>.bin
```

Read-only: `--solana-rpc` at `finalized` commitment (via `zk-inbox-client`'s `devnet-driver` feature),
`--verifier-rpc` expected to be a tunneled loopback address (`http://127.0.0.1:18547`, forwarded to the verifier's
RPC port by an SSH tunnel), never a public endpoint. Writes `<out>` (the two
`ZiskStdin::write_slice`-framed bincode inputs, concatenated) and `<out>` with its extension replaced by
`.json` (the expected public values, printed as hex — `first`, `last`, `gas_used`, `parent_hash`,
`inbox_commitment`, `forced_outcome_commitment` — plus `provenance` and `last_block_hash`/`state_root`,
`None` until a real guest execution fills them in).

### Migrating an existing sidecar to v2

`--migrate-sidecar <existing.json> --genesis <path> --out <new.json> --solana-rpc <label> --verifier-rpc
<label> --fetched-at <unix-secs> --verifier-version <string> --input-bytes <n> [--last-block-hash <hex>]
[--state-root <hex>] [--elf-sha256 <hex>]` upgrades an already-committed, older-shaped sidecar to the v2
[`build::Sidecar`] shape with **no network access at all**: it decodes the existing file's own 9
`ExpectedPublicValues` fields (a batch's public values never change under a sidecar-envelope migration —
only the fixture the sidecar is written alongside decides those) and fills in `provenance` /
`last_block_hash` / `state_root` from the explicit values above, computing `provenance.genesis_sha256`
itself from `--genesis` (a local file hash, still no network). `--solana-rpc`/`--verifier-rpc` here are
provenance LABELS carried forward from the original fetch, not endpoints this mode connects to.

## Real batch, verified

`fixtures/prover-input/txv1-dev-batch-3930.bin` (+ `.json` sidecar) is a real, committed input: Tiber
chain 200101, batch 3930, blocks 39181..=39190 (Tiber is idle — all ten blocks are empty), built by this
crate's own CLI against the live chain and the Tiber verifier over the tunnel, `acc` verified against the
on-chain batch account.

Run through the real `zec-rome` ELF (the fork's `bin/guests/stateless-validator-rome`) under `ziskemu`
(576,056 steps, 0.0131 s): the committed public values matched this crate's own sidecar on
every field it predicts (`chain_id`, `first_number`, `last_number`, `open_unix_ts`, `max_drift_secs`,
`gas_used`, `parent_hash`, `inbox_commitment`, `forced_outcome_commitment`), and the two fields only a real
execution reveals — `last_block_hash`, `state_root` — matched the Tiber verifier's own `hash`/`stateRoot`
for block 39190, fetched independently over `eth_getBlockByNumber`. **`state_root`** is independently
re-confirmed here: every block in this batch has `gas_used == 0` (all ten are empty — Tiber was idle), so
the state trie never changes across the batch, and `fixtures/prover-input/rpc/eth_getBlockByNumber-990d-
full.json`'s own committed `stateRoot` for block 39181 (the first block of the range) is therefore the
same value for every block through 39190 — `3556b5eba070d6f6d4b4f3ca4367c4bbc7a719b5c634625e26a073c10c55a355`,
now recorded in the migrated sidecar. **`last_block_hash`** (block 39190's own hash) depends on that
block's real timestamp and its own EIP-1559 base fee, neither reconstructible from a committed fixture —
the earlier run above confirmed it live but never persisted the value, and it stays `None` in the
migrated sidecar pending a fresh real execution to re-confirm and record it. `provenance.elf_sha256` also
stays `None` here for the same reason: `build-elf.sh` was not re-run against this exact fixture
after landing (recording a stale, non-reproducible hash would be worse than recording none).

This first real run also found a guest bug, fixed in the fork: `ziskos`'s public-output
mechanism is a 64-slot, 32-bit register file (256 raw bytes total), not the 512-byte buffer the guest's
first commit attempt assumed.

### Mutations (real input, real `ziskemu`, none committed)

Four mutations built from the real batch-3930 input, each refused by name under `ziskemu`:

| mutation | refusal |
|---|---|
| a witness for the wrong block substituted | `StatelessValidationFailed: index 0: stateless validation: invalid ancestor chain` |
| one chunk-body byte flipped (inside the compressed payload) | `channel decode: ruzstd decompress or RLP decode failed: Rlp(UnexpectedString)` |
| `max_drift_secs` set to 0 | `MaxDriftSecsZero: max_drift_secs must be non-zero` |
| the parent header replaced (timestamp bumped by 1) | `ParentChainBroken: index 0` |

A fifth probe — lowering `max_drift_secs` to a small nonzero value (1, down from the real 60) — does
**not** refuse: this real, idle batch's blocks all finish sealing *before* the batch is opened
(`open_unix_ts` is 1–10 s *after* the last block's own timestamp), and the guest's drift check is a
one-sided upper bound (`timestamp <= open_unix_ts + max_drift_secs`) — it only ever fires on a block dated
*later* than the batch's own open time, which never happens on honest, already-sealed data regardless of
how small a positive `max_drift_secs` is. Demonstrating `DriftBoundExceeded` itself needs a block whose
timestamp is pushed past `open_unix_ts`, not merely a smaller bound — recorded here as a real observation
about this real batch's shape, not a gap papered over.

## `ZISK_GENESIS_PATH`: not present in the fork at the pinned tree

A `ZISK_GENESIS_PATH` custom-chain patch on `crates/clients/reth/input/src/lib.rs` might be expected.
Checked directly against the fork (zisk-eth-client v0.12.0, the guest's base), it is not present there:
`fetch_chain_config` in that file only recognizes four named chains (Mainnet/Sepolia/Hoodi/Holesky) and
refuses any other chain id — which Tiber (200101) is. `src/genesis.rs` instead reads the chain's genesis
file directly as a standard `alloy_genesis::Genesis` document (the same shape `fetch_chain_config`
ultimately returns a `ChainConfig` from, for its four known chains).

## Tests

```sh
cd crates/rome-zk-prover-input && cargo test --locked
```

Real, load-bearing checks: `inbox::fetch_and_verify_batch` against a batch account + chunk account built
with `rome-zk-layouts`' own writers (never hand-poked bytes), and refused by name
(`InboxError::AccMismatch`) when a chunk body is tampered; `genesis::load_chain_config` refusing a missing or
malformed genesis file by name;
`wire`'s bincode round trip and the stdin frame's length-prefix contract; `verifier`'s decode of RECORDED
`eth_getBlockByNumber`/`debug_executionWitness` responses (`fixtures/prover-input/rpc/`, captured once over
the tunnel) into the pinned alloy types. The live network path (a real Solana node, a real reth verifier)
is `#[ignore]`d (`verifier::tests::live_fetch_block_and_witness_over_the_tunnel`) — run by hand with the
tunnel up.

## Open questions

- Signer recovery cost: this crate's wire format carries no `public_keys` field (unlike `RethInputPublic`),
  so the guest recovers every block's signers itself — see `guest-rome/src/chain.rs`'s module doc for the
  step-count tradeoff this leaves open.
- A loaded (non-idle) batch has not been run end to end yet — Tiber is idle; batch 3930's ten blocks carry
  no transactions. The DA-hash and decode step counts for a loaded batch were already measured separately
  (`guest/rome-zk-bench-decode`); this crate's own real run adds the execution + commit path on
  top, on empty blocks only.
