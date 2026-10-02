# rome-zk-derive

The derivation node: a permissionless client that reconstructs the EVM chain purely from what Solana
holds — the inbox program's finalized chunks and the settlement program's finalized root — with no
dependence on the sequencer or any other Rome-operated service. It is the reference anyone can run to
check that a posted root actually matches what the inbox's data decodes and executes to, and the oracle a
future challenger uses to decide whether a posted root is wrong.

## What it guarantees

- **Pull-based pipeline, kona-shaped.** `SolanaTraversal` (walks finalized batches) → `InboxRetrieval`
  (reads a batch's sealed chunks) → `FrameQueue`/`ChannelBank` (reassembles the compressed channel
  stream) → `BatchQueue` (decodes it, strict validity) → `AttributesQueue` (the committed block
  environment) → `EngineController` (drives a real reth's Engine API). Each stage is a small struct that
  pulls from the one before it — see `src/lib.rs`'s module doc for the full stage list and which two
  seams (`AccountReader`, `EngineApi`) a future profile can substitute without touching anything else.
- **Strict validity, never a skip.** A chunk that does not decode, a channel that does not reassemble, a
  transaction that does not decode or names the wrong chain id, or a built/consolidated block whose
  committed identity (parent hash, timestamp, `prevRandao`, gas limit, and the **ordered** transaction
  list) diverges from this batch's frame — any of these is `PipelineError::Critical`: the pipeline stops
  and nothing is posted to the engine. There is no silent-skip path.
- **Read-only, always at FINALIZED commitment.** This node builds and signs no Solana transaction — every
  account read (chunks, the batch cursor, the settlement root) is at
  `CommitmentConfig::finalized()`, the one commitment level under which an account this node has seen
  can never be rolled back out from under it. None of the SIMD-0385 V1 transaction-format constraints
  apply to this crate.
- **Consolidation, not blind rebuilding.** If the engine's own chain already has the block a batch is
  about to derive (a resumed node re-deriving history it already processed), `EngineController` verifies
  the engine's existing block against the batch's committed identity instead of rebuilding it — a cheap
  read + compare, no `build_forced_block`/`new_payload`/`forkchoice_updated`. The engine's own chain is
  the durable truth; a real content mismatch at the right height is a genuine equivocation
  (`PipelineError::Critical`), never a retry.
- **A batch attempt is atomic with respect to the engine's own position.** If a multi-block batch fails
  partway through with a retryable (`Temporary`) error, the engine controller's position is rewound to
  where the attempt started, so the next attempt re-derives the same batch from the same expected height
  — every block that already succeeded consolidates instead of being rebuilt.
- **Resume from the settlement root, not always from genesis.** On start, this node reads the settlement
  program's root account for this chain and asks the engine itself to confirm the block it names; if
  confirmed, traversal resumes right after that root's last finalized batch and the engine seeds directly
  from that block — no need to re-derive already-settled history. Only a genuinely absent root account
  (a fresh chain with no settlement deployment yet) falls back to the always-correct path: start from the
  engine's own real genesis and consolidate forward through whatever the engine already holds. A
  **decoded** root the engine cannot confirm — a block it lacks at the named height, or a hash mismatch
  (equivocation) — is a named refusal instead, never a silent re-derivation from genesis. At the
  settlement's own genesis sentinel there is no continuity to offset from, so a fresh chain can still
  derive its very first batch. See "Resume semantics" below.
- **A one-sided timestamp drift bound on every batch, on by default.** Every block's declared timestamp
  must not exceed `open_unix_ts + max_drift_secs` — a sequencer whose clock has drifted materially ahead,
  or a deliberately future-dated block, is `PipelineError::Critical`, not silently accepted; honest
  posting latency (the batcher posting a batch's DA later than its blocks were sealed) never trips it.
  `open_unix_ts` is the committed Solana clock reading the batch account itself carries (the inbox
  program's `OpenBatch` writes it), read straight off the decoded batch by `SolanaTraversal` into
  `BatchRef::open_unix_ts`, and threaded to `batch_queue::enforce_drift_bound` on every batch this
  pipeline derives; this binary always wires the bound live via `DerivePipeline::with_drift_bound`. See
  `batch_queue::DriftBound` for the exact formula and rationale. **The bound holds across an
  idle gap of any length:** `open_unix_ts` is written fresh when the batcher opens a batch, after that
  batch's own group closes — never against the previous batch's last block — so a block sealed an hour (or
  longer) after the one before it is exactly as valid as one sealed a second later, checked only against
  its own batch's own anchor.
- **`max_drift_secs` comes from the chain, not the TOML.** At startup, `chain_bound::
  chain_drift_bound` reads the chain's own `chain_config` account (over the same `AccountReader` the
  resume anchor uses, at FINALIZED) and refuses by name — never falls back to a default — if the account
  is missing (the chain is not registered under this settlement program), undecodable, names a different
  chain id, is still v1 (`MigrateChainV2` has not run yet for it), or carries `max_drift_secs = 0`
  (unconstructable on chain, refused here too as defense in depth). `derive.toml`'s `max_drift_secs` key
  is now optional: omitted, it defers entirely to the chain's value (the deploy template's committed
  shape); given, it must equal the chain's value exactly or startup refuses by name, naming both numbers
  — a stale or wrong TOML value can no longer silently override what the chain and the guest enforce
  (the settlement program refuses a proof whose committed `max_drift_secs` differs from the chain's).
- **Chunk reads are paged, not one round trip per chunk.** A batch's chunks are all read in one
  `getMultipleAccounts`-backed call (paged at 100 keys per round trip), not one `getAccountInfo` per
  chunk — measured read-only against Tiber devnet (batch 4005, 899 chunks): **336.998 s** with one
  `getAccountInfo` per chunk vs. **2.021 s** paged, a ≈167× reduction.
- **What it never does:** build or sign a Solana transaction, trust a chunk's body without recomputing the
  accumulator's commitment over the bytes actually read, or silently reorder/drop a transaction relative
  to the batch's own frame.

## Config

TOML, loaded from a path given by `--config` (default `config.toml`):

| field | meaning |
|---|---|
| `chain_id` | this chain's EVM chain id |
| `solana_rpc_url` | Solana RPC endpoint this node reads the inbox and settlement root from — always at FINALIZED commitment; read-only, no funded keypair needed |
| `inbox_program_id` | the `zk-inbox` program this chain's chunks/batches live under |
| `settlement_program_id` | the `zk-settlement` program this chain's root account lives under — required, used by the resume anchor at startup |
| `reth.rpc_url` | the reth node's plain (unauthenticated) HTTP endpoint — used only for `testing_buildBlockV1` (see "The two reth endpoints" below) |
| `reth.engine_url` | the reth node's real, JWT-authenticated Engine API endpoint |
| `reth.jwt_secret_path` | path to the JWT secret file both endpoints share |
| `blocks_per_batch` | (default 10) the cap a batch's block count must not exceed — a config/profile value; `config::DEFAULT_BLOCKS_PER_BATCH` re-exports [`rome-zk-profile`](../rome-zk-profile)'s own constant (the one home every reader shares), independent of `rome-zk-executor-api`'s `prevRandao` epoch constant, which is a different quantity |
| `max_open_channels` | (default 16) `ChannelBank`'s bounded, oldest-first eviction limit |
| `max_drift_secs` | optional, no default — how many seconds a block's declared timestamp may exceed its batch's committed `open_unix_ts` anchor by before it is rejected. Omitted: the chain's own `chain_config.max_drift_secs` is used (read at startup). Given: must equal the chain's value or startup refuses by name |
| `metrics_addr` | (default `127.0.0.1:9003`) where this process serves `GET /metrics`; env override `ROME_ZK_DERIVE_METRICS_ADDR` |

## Metrics

Served over `GET /metrics` at `metrics_addr` (default `127.0.0.1:9003`, env override
`ROME_ZK_DERIVE_METRICS_ADDR`) — the same reth-free HTTP responder
[`rome-zk-batcher`](../rome-zk-batcher) and [`rome-zk-sequencer`](../rome-zk-sequencer) serve, pulled from
[`rome-zk-metrics-http`](../rome-zk-metrics-http). Spawned once at process startup, in both `--once` and
follow-forever mode.

Prometheus text exposition (`metrics::Metrics::render`): `rome_zk_derive_batches_derived_total` (counter:
total batches fully derived through the engine), `rome_zk_derive_critical_total` (counter: total
`PipelineError::Critical` raises — the strict-policy stop — regardless of which stage
raised it), `rome_zk_derive_last_batch` (gauge: the most recently derived batch id),
`rome_zk_derive_head_block` (gauge: the most recently derived block's own design number),
`rome_zk_derive_batch_seconds` (histogram: wall-clock seconds to derive one batch). Tiber's own compose
port mapping and Prometheus scrape job are set up outside this crate — it only serves the
endpoint.

## The two reth endpoints, JWT, and `--http.api eth,testing`

This node drives a stock reth purely over JSON-RPC, but needs **two** endpoints on the reth side because
of one real operational dependency: forcing this batch's *exact* ordered transaction list into a built
block (no mempool reordering or omission) uses reth's `testing_buildBlockV1`/`commitBlockV1` mechanism,
which reth serves on its **plain** RPC surface (not the authenticated engine port) and gates behind an
explicit `--http.api testing` flag — reth's own description calls it "highly sensitive: testing-only,
powerful enough to include arbitrary transactions." A production derivation node's reth must therefore run
with `--http.api eth,testing` bound to **localhost only** (never exposed publicly), alongside its normal
authenticated engine port (`--authrpc.*`, JWT-secured, used for `engine_newPayloadV4` /
`engine_forkchoiceUpdatedV3` / `eth_getBlockByNumber`). `reth.rpc_url` in this crate's config points at the
plain port; `reth.engine_url` + `reth.jwt_secret_path` point at the authenticated one.

## Resume semantics

On start (unless `--from-batch` is given):

1. Read the settlement program's root account for this chain (`head_final_batch`, the last finalized
   block's real height and hash).
2. If no root account exists yet (a genuinely fresh chain with no settlement deployment), fall back to
   the always-correct path: start traversal at batch 0 and seed the engine from its own real genesis,
   consolidating forward through whatever the engine already holds.
3. Otherwise ask the engine (`eth_getBlockByNumber` on that real height) whether it already has that
   exact block.
   - If it does, and the hash matches, traversal resumes right after `head_final_batch`, and the engine
     seeds directly from that block — no re-derivation of already-settled history. At the settlement's
     own genesis sentinel (`number == 0` — a chain that has not finalized its first batch yet) there is
     no design block to continue from, so the very first batch is derived starting at design block 1 —
     the same real height 1 the sequencer's first sealed block always is (no offset: `header.number ==
     BlockEnv.number`).
   - If the engine has no block at that height (a fresh derivation node, or one whose database was lost,
     once the batch(es) it would need to re-derive from genesis have had their rent reclaimed), this node
     **refuses to start** with a named error naming the anchor's height, hash and `head_final_batch` —
     restore the engine from a snapshot at or after that height, or run with `--from-batch` while the
     Solana inbox's data is still retained.
   - If the engine has a block there but its hash disagrees with the root's, that is equivocation (a
     stale/foreign root, or a genuine fork) — also a named refusal, never a silent re-derivation from
     genesis.

A confirmable anchor is the only kind ever trusted — the engine's own chain is the one durable truth this
node defers to, and anything it cannot verify against that truth stops the node loudly rather than
silently working around it.

`--from-batch <N>` bypasses the settlement-root anchor entirely and starts traversal at batch `N`, with
the engine seeded from its own genesis — an explicit, engine-verified full re-derivation (useful while the
Solana inbox's data availability is still retained for the batches being re-walked).

## `--once`

By default this binary follows the inbox forever, backing off on idle/temporary conditions and stopping
outright on the first strict-validity failure. `--once` instead derives every batch that is final right
now and exits — useful for scripted verification or catching up a stopped node without leaving a
long-running process behind.

## How to test

```sh
cargo test -p rome-zk-derive
```

Building the inbox and settlement programs first (`cargo build-sbf --manifest-path
programs/zk-inbox/Cargo.toml --sbf-out-dir target/deploy` and the settlement program's equivalent) is
required for the tests that load the real compiled programs into `solana-program-test`. One test is
gated behind `--ignored` because it spins up a real, in-process reth v2.5.2 node over real sockets — run
it explicitly:

```sh
cargo test -p rome-zk-derive --test engine_equivalence -- --ignored --nocapture
```

## Security notes

This node's entire security model rests on never trusting a Rome-operated service for anything it can
instead recompute from Solana's own finalized state: every chunk it reads is re-hashed and reduced
through the same accumulator formula the on-chain program uses, compared against the batch account's own
committed root — a chunk whose bytes were corrupted or swapped after finalization is rejected, not
trusted because it was `sealed`. The `testing_buildBlockV1` dependency above is the one real operational
exception: it is a powerful, testing-labeled reth RPC method this design relies on in production, and must
never be exposed beyond localhost.

## Design references

The design notes cover the derivation node in full (its adopted shape and the pull-based stage traits it
borrows from kona), the committed block environment and the timestamp drift bound, and the derivation flow
with its strict-validity policy. The resume anchor and the drift bound are implemented in `src/resume.rs` and
`batch_queue::enforce_drift_bound`.

## Depends on

[`rome-zk-channel`](../rome-zk-channel) for the channel/frame codec (never reimplemented here — the
decoder must match the batcher's encoder byte for byte; this crate's dependency moved from the whole
`rome-zk-batcher` crate to just the codec, since that was the only piece it ever needed),
[`rome-zk-executor-api`](../rome-zk-executor-api) for the committed `BlockEnv` and `prevRandao` formula,
[`zk-inbox-client`](../zk-inbox-client) and [`zk-settlement-client`](../zk-settlement-client) for account
layouts/PDAs/decoders (never re-derived), and [`rome-zk-layouts`](../rome-zk-layouts) for the chunk
account header (`chunk::read` — `InboxRetrieval::chunks` decodes every chunk header through this one
owner, the same decoder the inbox program and both clients use, rather than a field-by-field re-decode of
raw offsets), and [`rome-zk-metrics-http`](../rome-zk-metrics-http) for the `/metrics` responder (the same
reth-free crate the batcher and sequencer serve from). `rome-zk-batcher` itself is a dev-dependency only, for one integration test
(`tests/real_program_inbox.rs`) that drives the real batcher pipeline end-to-end; `zk-settlement` (the
program crate itself, `no-entrypoint`) is a dev-dependency only, for `tests/chain_config_real_program.rs`,
which drives a real `InitChainV2` to prove `chain_bound::chain_drift_bound` against the real producer.
Consumed by nothing yet inside this workspace — it is the reference client anyone (including a future
challenger) runs
independently.
