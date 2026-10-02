# rome-zk-executor-reth

An in-process [Reth](https://github.com/paradigmxyz/reth) implementation of the
[`Executor`](../rome-zk-executor-api) trait: the default execution engine
[`rome-zk-sequencer`](../rome-zk-sequencer) drives. It also carries (behind the sequencer's own feature
flag) a standalone `eth_*`/`net`/`web3`/`debug`/`trace` JSON-RPC surface reading directly off this
executor's own database, so a node running this executor can be queried like any other Ethereum node.

## What it guarantees

- **The committed block environment is never re-derived mid-block.** `open_block` fixes the block's
  number, timestamp (the later of "one second after the parent" and "the first sub-block's own second"),
  gas limit, coinbase and `prevRandao` before the first sub-block executes; `seal_block` asserts the
  environment it is given still matches, and fails loudly (`EnvMismatch`) rather than silently proceeding
  if it does not.
- **Sub-block execution runs against a layered, in-memory view**, not against the database directly:
  transactions execute against the persisted state plus an overlay of every block that has been sealed but
  not yet durably written (normally exactly one block). Later sub-blocks within the same block see
  earlier sub-blocks' effects, exactly as the design's execution model requires.
- **Block sealing re-executes the block's already-recovered transactions once**, through Reth's own block
  builder, to compute the real state root and header — there is no second signature-recovery pass, only a
  second execution pass over the same ordered list sub-block execution already validated.
- **Persistence is asynchronous and strictly ordered.** A sealed block is handed to a background task that
  waits for the previous block's write before writing its own — `open_block` for the next block never
  waits on this task. A block stays in the in-memory overlay until its write is confirmed, so a reader
  never sees a "half persisted" state.
- **A crash loses at most the still-unpersisted block(s) in the overlay**, never more: the sequencer's
  ordered log is the durable source of truth, and on restart this crate reports the highest block number
  its own database already reflects, so the sequencer's replay path re-executes only the log's tail beyond
  that point rather than the whole history.
- **The block number this crate is told (`BlockEnv::number`) is the same number Reth's own header ends up
  with — enforced, not merely arranged.** Reth still assigns real header numbers itself (genesis is real
  height 0, every later block is parent + 1) — this crate never reads `BlockEnv.number` to decide what
  height to build — but the sequencer numbers its first sealed block 1, so the two coincide for every
  block a healthy chain ever produces; `last_persisted_block()` (what a restart reads back) reports that
  same, single number. `open_block` refuses (`ExecutorError::EnvMismatch`) unless `env.number` is exactly
  the real canonical parent's height + 1, and `seal_block` refuses unless the header it just built carries
  the same number the caller's `BlockSealInputs::block` claims — a numbering drift becomes unconstructable
  at this boundary rather than merely asserted by callers that happen to agree with each other.

## Config

`RethConfig` (constructed by the caller — normally
[`rome-zk-sequencer`](../rome-zk-sequencer)'s own config loader) — a data directory path and a genesis
specification path; no other runtime configuration. See `rome-zk-sequencer`'s own README and
`config.example.toml` for how these paths are supplied end to end.

## How to test

```sh
cargo test -p rome-zk-executor-reth
```

This pulls Reth's dependency graph (pinned to the exact upstream release tag the workspace targets — see
this crate's `Cargo.toml`), so a cold build is heavier than most crates in this workspace; expect a longer
first compile.

## Reading a datadir

Reth keeps headers/transactions/receipts in static files (NippyJar) and indices/checkpoints in MDBX; a
crash between the two commits (see "Recovering from a torn persist" below) can leave them disagreeing
about the persisted head. `examples/inspect_datadir.rs` opens a datadir the same way `RethExecutor::new`
does and prints, without exercising the healer or writing anything:

```sh
cargo run -p rome-zk-executor-reth --example inspect_datadir -- <datadir> <genesis.json>
```

- the highest committed block per static file segment (headers, transactions, receipts, senders,
  account/storage changesets)
- MDBX's own `best_block_number` (the `StageId::Finish` checkpoint) and `last_block_number` (the
  static-file frontier)
- every stage checkpoint
- header presence (both `sealed_header` and the lower-level `header_by_number`) for `best-2..=best+1`

It never writes: opening a datadir only runs the idempotent `init_genesis` (a no-op once genesis is
already there), and this tool deliberately never calls `ProviderFactory::check_consistency` — that call
is documented to potentially heal (write to) the static file segments, and the point of this tool is to
show the datadir's state *before* any healer touches it, for manual confirmation against a real torn
datadir.

### Recovering from a torn persist

`DatabaseProvider::commit` runs, in one thread: static data+offsets `sync_all` → the `.conf` written to a
`.tmp`, fsynced, renamed into place, parent directory fsynced → RocksDB → MDBX `tx.commit()`. A kill can
therefore leave exactly two shapes:

- **(A) One header row durable in data+offsets, `.conf` and MDBX both at the previous block** — the kill
  landed after the static fsync and before the rename. This is the common shape and the one behind the
  incident on the Tiber devnet. Because NippyJar's cursor reads the last configured row's hash column up to
  the end of the data file, the dangling row corrupts that column: `sealed_header(n)` decodes to `None`
  while `header_by_number(n)` still decodes — the old "canonical head header missing" refusal. At open,
  `static_heal::detect_and_verify_forward_heal` inspects the row first and refuses it by name (MDBX's
  `HeaderNumbers` has no entry for its hash), then Reth's `ProviderFactory::check_consistency` truncates
  it; the executor opens at the previous block and the ordered log replays the torn block.
- **(B) `.conf` one block ahead of MDBX** — the kill landed after the rename and before the MDBX commit.
  `check_consistency`'s checkpoint-vs-static invariant prunes the extra static row; the executor opens at
  MDBX's checkpoint and the log replays the rest. No named log line.

A third shape — the `.conf` lagging a **committed** MDBX — is not producible by a kill under this order.
The forward heal covers it anyway, as defence in depth: before `check_consistency` can discard the row it
verifies four facts (the row's block number, `keccak256(rlp(header))` against its hash column, its parent
hash against the config's last row, and MDBX `HeaderNumbers[hash]`), and only when all four hold does it
re-append the row after the truncation and require the datadir consistent afterwards. Any other outcome
(more than one dangling row, or a failed check) is logged by name and left to Reth's own heal.

In every shape the sequencer then resumes: `crates/rome-zk-sequencer/tests/reth_torn_persist_resume.rs`
drives heal → tail replay → sealing more blocks → a clean reopen, for both shapes and for blocks with and
without transactions, and compares every block hash to a run that was never torn.

**What `RethExecutor::new` writes at open:** every open runs `ProviderFactory::check_consistency`, which is
documented to potentially rewrite ANY static file segment's `.conf`/offsets/data and write MDBX/RocksDB
metadata — this happens on every restart, torn or not (`NippyJarWriter::new` commits any pending heal
unconditionally). A forward heal additionally appends one header row and commits it. **Copy the datadir
before the first restart if you want forensics on a torn shape** — the second restart, even a clean one,
may have already rewritten the evidence.

`examples/inspect_datadir` refuses to run on a directory that has no `db/mdbx.dat` (it never creates a
datadir on a typo — see its own doc) and writes nothing to an existing datadir's chain data.

## Security notes

The block-environment mismatch check (`ExecutorError::EnvMismatch`) is an internal consistency check on
this crate's own state, not expected to fire when the sealer above it calls `open_block` /
`execute_sub_block` / `seal_block` in the order the [`Executor`](../rome-zk-executor-api) trait requires —
its purpose is to fail loudly rather than silently diverge if that calling discipline is ever violated.
Persistence durability itself stays on: this crate keeps its database in durable-commit mode rather than
relaxing it for speed, since replaying the ordered log's tail after a crash is a cheap, well-tested
recovery path and the fsync cost measured against it is small relative to the pre-confirmation budget (see
below).

## Design notes

This executor is one stage of the sequencer's execution pipeline ("execute N+1 while merkleizing N"), and
block sealing must never block the next block's execution. The end-to-end pre-confirmation budget this
executor is one link in was measured on Tiber devnet at 96% of pre-confirmations landing within 100 ms over
241,000 transactions at 3,600 TPS; it is a property of the whole sequencer, not of this crate alone. This
crate's own module documentation (`src/lib.rs`) cites the upstream Reth source files its block-building and
persistence code is built against, file by file, for anyone reviewing it against a specific Reth release.

## Depends on

[`rome-zk-executor-api`](../rome-zk-executor-api) for the trait it implements. Pulls the pinned Reth crate
set directly (git dependency at the workspace's exact upstream tag — Reth publishes no released crates to
crates.io for its internal components, so a tag-pinned git dependency is Reth's own documented consumption
model).
