# rome-zk-sequencer

The single-node sequencer: accepts raw EVM transactions, admits and orders them, executes them through the
[`Executor`](../rome-zk-executor-api) trait, signs a pre-confirmation as soon as a sub-block is durably
logged, and appends every sub-block to an append-only ordered log. The ordered log is this system's single
source of truth for transaction order — everything downstream (the batcher, derivation, the guest that
will eventually prove blocks) reads it, nothing downstream can reorder what it recorded.

## What it guarantees

- **Admission is a bounded queue, not a mempool.** Arrival order is sealing order, always — there is no
  fee-ordered pool, and no priority auction can let a later, higher-fee transaction jump one that already
  has a signed pre-confirmation. A per-sender nonce cache with a bounded nonce-gap "parking" area lets an
  out-of-order nonce wait for its predecessor without blocking every other sender.
- **Sign and durably log before acknowledging.** A sub-block is executed, its header (chain, block number,
  sub-block index, microsecond timestamp, transaction root, receipts root, gas used, previous hash) is
  signed, and the record is appended and fsynced to the ordered log — only then is the pre-confirmation
  returned to the sender and published on the pre-confirmation WebSocket feed. A pre-confirmation the
  sequencer has not yet durably recorded is never handed out.
- **Sub-blocks seal every 50 ms; blocks (20 sub-blocks) seal every 1 s — the chain's own `[profile]`
  defaults, not hardcoded constants.** A sub-block carries ordered transactions, a transaction root and a
  receipts root but no state root — the state root is computed once per block, on a timer-driven schedule
  with a hard deadline (interrupt-driven, not polled: a late sub-block seals immediately rather than
  waiting out the rest of its window). A chain may declare a different cadence (e.g. 25 ms sub-blocks, 40
  per block) as long as the product is still a whole number of seconds — see [Config](#config) below.
- **Blocks are numbered from 1.** A fresh chain's genesis is design/EVM height 0 (the sequencer never
  seals it); the first sub-block the sequencer ever seals opens block 1, and every later block increments
  from there — so `BlockEnv.number` and the underlying EVM header's real block number are the same value
  for every block a healthy chain ever produces. This coincidence is by construction, not something any
  consumer re-derives.
- **The log record is the executed block, not the attempted one.** A sub-block's record holds exactly the
  transactions the executor *included* — a transaction rejected at execution time (bad nonce,
  insufficient funds, intrinsic-gas floor) is never written to the log and never reaches data
  availability; a transaction the executor did not reach before its budget ran out is carried forward to
  the next sub-block, not recorded as attempted-and-failed. This makes replay unambiguous: a logged
  transaction that fails on re-execution is a genuine divergence, never an expected skip.
- **An idle chain seals nothing.** A tick with no ready transactions writes no record and never calls the
  executor, unless a block is already open (an opened block always finishes its full sub-block count,
  empty tail sub-blocks included). By default a chain never manufactures an empty block just because a
  timer fired; a chain that wants a regular timestamp even while quiet can opt into sealing an empty block
  at most every N seconds instead — see `empty_block_interval_secs` under [Config](#config). Either way,
  the first real transaction after any idle gap opens its block at the wall clock, never at a stale
  "one second after the last block" value carried over from before the gap.
- **Recovery replays from the log, not from wall-clock assumptions.** On restart, the sequencer asks its
  executor for the highest block it already has durably persisted (if any) and replays only the log's tail
  beyond that point — see [`rome-zk-executor-reth`](../rome-zk-executor-reth) for what "durably persisted"
  means for the default executor.
- **What it never does:** build sub-blocks from a fee-ordered pool, answer a sender before the log append
  is fsynced, hold an unbounded admission queue, or persist a transaction the executor rejected.

## Config

TOML file plus environment-variable overrides (`ROME_ZK_SEQUENCER_CHAIN_ID`, `_RPC_ADDR`, `_METRICS_ADDR`,
`_LOG_DIR`, `_KEY_PATH`, `_SUB_BLOCK_GAS_LIMIT`). **The `GET /metrics` responder itself now lives in
[`rome-zk-metrics-http`](../rome-zk-metrics-http)** — a small, reth-free crate the
batcher depends on too — so `metrics::serve_metrics` here is a thin call into it; this crate's own
behaviour at `metrics_addr` is unchanged. Every field the config loader accepts is documented, with
its default, in [`config.example.toml`](config.example.toml) at this crate's root — that file is parsed
directly by this crate's own test suite, so it cannot silently drift out of sync with what the loader
actually accepts. In particular: **the sequencer's signing key is never inlined in configuration** —
`sequencer_key_path` names a file path, resolved at startup; the key itself never appears in this
repository or in any config file committed to it.

**`[profile]`: the chain's cadence and cap, declared and validated together.** `Profile`, `ProfileIdentity`,
every `DEFAULT_*` constant, and the `profile.json` file layer now live in the standalone
[`rome-zk-profile`](../rome-zk-profile) crate — this crate's own `src/profile.rs` is a thin re-export
under the historical `crate::profile` module path, so nothing importing `rome_zk_sequencer::profile::*`
had to change. `src/sealer.rs`'s `SUB_BLOCKS_PER_BLOCK` and `src/executor.rs`'s
`DEFAULT_SUB_BLOCK_GAS_LIMIT` re-export `rome-zk-profile`'s own constants — the numeric home moved there;
it used to be the other way around (this crate's `sealer`/`executor` modules were the source of truth and
`profile.rs` merely re-read them). Every
field is optional and defaults to the design's own values (50 ms sub-blocks, 20/block, 10 blocks/batch,
5,000,000 sub-block gas limit, 70% admission headroom) — a config that omits the table entirely behaves
exactly as it did before `[profile]` existed. Loading rejects an internally inconsistent profile at
startup, naming the bound: a sub-block period × count that isn't a whole number of seconds (EVM
timestamps are integer seconds), an explicit `block_gas_limit` that disagrees with
`sub_block_gas_limit × sub_blocks_per_block`, or an implied gas/s or DA bytes/s that exceeds the profile's
own declared `prover_gas_per_sec`/`da_bytes_per_sec` budget. `sub_block_gas_limit` and `block_gas_limit`
used to be top-level `Config` fields — they now live under `[profile]` only; a config that still carries
either at the top level is refused at load time naming the stray key, not silently started on
`[profile]`'s defaults. On restart, replay of the ordered log is checked against the configured
`sub_blocks_per_block`: a log whose actual block/sub-block shape doesn't fit it (e.g. a log sealed at
20/block replayed under a profile declaring 40/block) is refused, naming the configured value, rather than
silently replaying to a divergent head. **`empty_block_interval_secs` (default 0) controls whether an
idle chain ever seals an empty block at all.** 0 means never: a tick with nothing to execute writes no
record and never touches the executor — a chain sits quiet indefinitely, and the first real transaction
after any gap opens its block at the wall clock, not at a value derived from how long the chain was idle.
A nonzero value N means "seal an empty block at most every N seconds" (useful for a chain whose consumers
want a regular heartbeat timestamp even when quiet); loading refuses a value below the profile's own block
time, since sealing an empty block faster than a real one could ever close is never a legal cadence. Once
a block has actually opened (its first sub-block sealed, with or without transactions), every later
sub-block in it seals regardless of content — the idle gate only ever decides whether a NEW block opens.
On a brand-new chain (no log yet) a nonzero N seals empty block 1 on the very first tick, because there is
no previous block timestamp to measure N from; a restarted chain measures N from the replayed last block.

**The chain's profile identity is persisted beside the ordered log, as `profile.json`.** Reading and
classifying that file (a fresh log with none yet, a current file, or one of the two legacy shapes below)
is [`rome-zk-profile`](../rome-zk-profile)'s own `read_profile_json`/`write_profile_identity` — this
crate's `reconcile_profile_identity` is the sequencer-specific orchestration around those two calls: the
migration flag, and the ordered-log numbering-origin check described below. The first time a log
directory is opened, `profile.json` is written next to it: `chain_id`, `sub_block_ms`,
`sub_blocks_per_block`, `sub_block_gas_limit`, the effective `block_gas_limit`, `blocks_per_batch`, and
`first_block`. Every later start compares the stored identity against the configured one — after every
env-var override, so `ROME_ZK_SEQUENCER_SUB_BLOCK_GAS_LIMIT` is checked too — *before* any replay work
touches the log; any disagreement on any field, chain id included, refuses to start naming the field, the
stored value, and the configured value. An existing log directory that has records but no `profile.json`
(written before this check existed — Tiber's, for instance) refuses to start; `--write-profile-json`
performs the one-time migration, writing today's config as that log's identity. A `profile.json` that
*has* every other field but predates `blocks_per_batch` specifically is refused the same way, distinctly
named, and upgraded by the same flag (rewriting the file with `blocks_per_batch` added from today's
config) rather than defaulted or assumed to match. `rome-zk-batcher` reads this same file for the
chain's block shape — including `blocks_per_batch`, its own grouper cap — rather than owning a parallel
config field; see its own README.

**`first_block` (the ordered log's numbering origin — always 1) is not migratable
the way `blocks_per_batch` is.** A `profile.json` missing it is refused (`ProfileJsonMissingFirstBlock`),
and `--write-profile-json` only ever adds it to a log directory with nothing sealed under it yet — never
over a log that already holds real records, since those records are genuinely 0-based (from before
block numbering started at 1) and no flag can retroactively make them 1-based. **One check, every caller
that could otherwise stamp or trust a wrong origin:** `recovery::log_numbering_origin` reads the log's own first
record and is called before every `profile.json` write over a directory with existing records — every
migration path that could otherwise write `first_block: 1` over real 0-based history, not just the one
that already had its own has-records gate — and again in `replay_into_executor` before any record is
replayed, so the premise holds even if `profile.json` was somehow bypassed. `rome-zk-batcher` calls the
same function directly on the log it is pointed at, independent of `profile.json`, before resolving its
own chain anchor (see its README) — so a 0-based log is unconstructable as either service's input, not
just the sequencer's. A 0-based log (from before block numbering started at 1) is not migratable at all:
bring up a fresh chain instead (on Tiber, a chain reset); never restart the old log in place.

## How to test

```sh
cargo test -p rome-zk-sequencer
```

The default build includes the `reth` feature (the in-process Reth executor is on by default, matching how
the binary actually ships), so this pulls Reth's dependency graph and its end-to-end tests (starting the
real binary on ephemeral ports and driving it over real sockets). To test only the mock-executor path,
without touching Reth's build graph at all:

```sh
cargo test -p rome-zk-sequencer --no-default-features
```

## Security notes

The "no fee-ordered pool" rule is a security property, not only a performance one: a signed
pre-confirmation is a claim about inclusion order, and the design's slashing argument (a pre-confirmation
absent from, or reordered relative to, the finalized inbox is attributable equivocation) only holds if
admission order really is sealing order. A future change that introduces any reordering between admission
and sealing — a priority lane, a fee-based reshuffle — would break that guarantee at the core, not merely
degrade it, and needs to go back through the design's security model before landing.

## Design references

The design covers the sequencer itself (admission, sub-block/block cadence, recovery), the sequencing and
data-availability flow, and the pre-confirmation latency budget — measured on Tiber devnet at 96% of
pre-confirmations within 100 ms over 241,000 transactions at 3,600 TPS; see this crate's own module
documentation in `src/lib.rs` for the reference systems this design borrows shape from). The design's
security model covers the pre-confirmation-equivocation property above.

## Depends on

[`rome-zk-log`](../rome-zk-log) for the sub-block header and the ordered-log record format itself
(re-exported here as `log`/`header` — both moved out of this crate's own `src/`, so every existing call
site is unchanged); [`rome-zk-executor-api`](../rome-zk-executor-api) for the execution
boundary; [`rome-zk-executor-reth`](../rome-zk-executor-reth) (optional, default-on via the `reth`
feature) for the default execution engine. Consumed by [`rome-zk-batcher`](../rome-zk-batcher), which
still depends on this crate directly (for `sealer`, `preconf`, `recovery`'s profile-identity helpers and
`metrics` — unrelated to the log-format move) as well as reading the ordered-log format through it.
