# rome-zk-profile

The one home for a chain's cadence/cap profile, the identity persisted beside its ordered log, and the
`profile.json` file format. Depends only on `serde`, `serde_json` and `thiserror` — no Solana, reth, alloy
or tokio — so every reader (the sequencer, the batcher, the derivation node) can depend on it directly
without pulling in a heavier dependency graph than it actually needs.

## Why this crate exists

Before it did, three crates each carried their own idea of a chain's shape:

- `rome-zk-sequencer::profile` declared the `[profile]` config table (`Profile`) and the identity
  persisted beside the log (`ProfileIdentity`), but its own `DEFAULT_SUB_BLOCKS_PER_BLOCK` and
  `DEFAULT_SUB_BLOCK_GAS_LIMIT` were themselves re-reads of constants that actually lived on
  `rome-zk-sequencer::sealer` and `rome-zk-sequencer::executor` — the numeric home and the "profile" home
  were two different modules pointing at each other.
- `rome-zk-batcher::config` read the sequencer's `profile.json` back in, re-implementing the same
  legacy-format detection (`blocks_per_batch` missing, `first_block` missing) as a second copy of the
  classification logic.
- `rome-zk-derive::config` pinned its own `DEFAULT_BLOCKS_PER_BATCH = 10` constant, independently of
  either of the above, with a comment promising it would eventually read from a shared place.

This crate is that shared place. Every `DEFAULT_*` constant, the `Profile`/`ProfileIdentity` types, and
the pure file layer that reads and classifies a `profile.json` on disk now live here once; the other three
crates depend on this one instead of on each other.

## What it owns

- **`Profile`** — the `[profile]` config table: `sub_block_ms`, `sub_blocks_per_block`,
  `blocks_per_batch`, `sub_block_gas_limit`, `block_gas_limit`, `admission_shed_pct`, `da_bytes_per_sec`,
  `prover_gas_per_sec` — with `Default` and `validate()` enforcing every cross-field invariant (a whole
  number of seconds per block; an explicit `block_gas_limit` that agrees with the product; an implied
  gas/s or DA bytes/s that does not exceed its own declared budget).
- **Every `DEFAULT_*` constant** the design fixes: 50 ms sub-blocks, 20 sub-blocks/block, 10 blocks/batch,
  5,000,000 sub-block gas, 70% admission headroom, the derived DA/prover throughput budgets, and the
  timestamp drift bound's `DEFAULT_MAX_DRIFT_SECS` (60 s) — the value the operator's chain-registration
  script passes to `InitChainV2`; derive reads the chain's own `chain_config` instead.
- **`ProfileIdentity`** — the subset of a chain's identity persisted beside its ordered log as
  `profile.json`: `chain_id`, cadence, gas shape, `blocks_per_batch`, and `first_block` (the ordered log's
  fixed numbering origin, always 1). `first_mismatch` reports the first field (in a fixed order) that
  disagrees between a stored and a configured identity.
- **The `profile.json` file layer** — `read_profile_json` (`src/storage.rs`) classifies whatever is on
  disk into `StoredProfileJson`: `Missing` (no file), `V0` (a file predating `blocks_per_batch`), `V1` (a
  file with `blocks_per_batch` but predating `first_block`), or `Current` (every field present).
  `write_profile_identity` (`src/storage.rs`) writes a fresh identity, creating the log directory if it
  does not exist yet.

## What it deliberately does not own

This crate knows nothing about the ordered log's own record format, the sequencer's migration flag
(`--write-profile-json`), or the numbering-origin check that reads a log's first record
(`rome_zk_sequencer::recovery::log_numbering_origin`). Those stay sequencer-side orchestration, built on
top of this crate's pure read/write primitives — see `rome-zk-sequencer`'s own README and
`recovery::reconcile_profile_identity`'s doc comment for the full migration state machine. Keeping this
crate reth-free is exactly what lets `rome-zk-batcher` and `rome-zk-derive` depend on it without pulling in
`rome-zk-sequencer`'s heavy `reth` feature.

## Consumers

- **`rome-zk-sequencer`** re-exports this crate's whole public surface under its historical
  `rome_zk_sequencer::profile` module path (so no external caller's import path changed), and its
  `sealer::SUB_BLOCKS_PER_BLOCK` / `executor::DEFAULT_SUB_BLOCK_GAS_LIMIT` constants now re-export this
  crate's `DEFAULT_SUB_BLOCKS_PER_BLOCK` / `DEFAULT_SUB_BLOCK_GAS_LIMIT` (the numeric home moved here; it
  used to be the other way around). `recovery::reconcile_profile_identity` is a thin orchestration layer
  over this crate's `read_profile_json`/`write_profile_identity`, plus the sequencer-owned migration-flag
  and numbering-origin checks.
- **`rome-zk-batcher`** calls `rome_zk_profile::read_profile_json` directly and maps its classification
  onto its own `ConfigError` variants (same names, same variants as before this crate existed — message
  text can differ, e.g. the missing-file case now names why the file is absent instead of surfacing the
  raw OS error). `grouping::grouper_from_profile` is the one place a `ProfileIdentity`'s `blocks_per_batch`
  becomes a `SizeCappedGrouper`'s cap, shared by both the `--once` and `--follow` binary paths.
- **`rome-zk-derive`** re-exports `DEFAULT_BLOCKS_PER_BATCH` from this crate instead of pinning its own
  copy of the number 10.

## Testing

```
cargo test -p rome-zk-profile
```

No external services, no Solana cluster, no reth — every test is a pure in-memory or tempdir-backed unit
test. `cargo tree -p rome-zk-profile` shows the crate's own dependency graph is exactly `serde`,
`serde_json`, `thiserror` (plus their transitive deps) — no `solana-*`, no `reth-*`, no `alloy*`, no
`tokio`.
