# rome-zk-settlement-watcher

Solana signatures → Postgres. The service that tracks the inbox and settlement programs' own transaction
history and turns it into a small, queryable set of tables (`rome_zk_explorer`) so nothing else in the
system — the batcher's rent-recycling decision, an explorer UI, alerting — ever has to re-derive that
state from raw account reads.

This crate never writes to Solana, and it never *decides* finality. It reports what Solana's own
transaction history and confirmation status say; the batch/root-posting programs are the actual source of
truth.

## Two passes, not one

- **Ingest** (`ingest::run_once`) walks `getSignaturesForAddress` backward and writes exactly two tables:
  `settlement_tx` (one row per signature: slot, block time, signer, status) and `settlement_tx_program`
  (one row per `(signature, program)` this crate ingested it under — `kind`, `ix_kind`, `chain_id`,
  `batch_id`; a transaction carrying instructions for two different programs gets one row
  from each ingester rather than one clobbering the other). Nothing here derives lifecycle state, and
  nothing here needs its rows in true chronological order — a cold backward walk sees newest signatures
  first, and that is fine.
- **Derive** (`lifecycle::derive_once`) is a *separate* pass over already-ingested `settlement_tx`/
  `settlement_tx_program` rows, read back in true `(slot, id)` order (never ingest's own arrival order),
  applying their lifecycle effects into `inbox_chunk`/`batch`. Splitting this out is what makes ingest
  order-independent: applying a `Seal` before its `Open`, or a later partial `FinalizeBatch` before an
  earlier one, would corrupt derived state if it happened during a backward walk where newer pages commit
  before older ones. **Reads only the database, never Solana**: ingest already decoded and persisted each
  row's `ChunkEvent`/`BatchEvent` list as JSON (`settlement_tx_program.events`), so derive never re-fetches
  or re-decodes a transaction body — removing both the doubled RPC load and the exposure where a
  transient Bigtable `null` on a second fetch could silently lose a row's lifecycle event forever.
- **Derive is gated on feed completeness.** `derive_once`'s row-fetch query joins `settlement_cursor` and
  only returns rows while no ingest walk is in progress for that kind (`backfill_head_sig IS NULL`) —
  reported as `DeriveOutcome::IngestWalkInProgress` when gated. An ingest walk that fails partway through
  (an RPC timeout on an older page during a cold backfill is the ordinary case) must never let this pass
  advance past rows a not-yet-ingested older page will still supply; the walk-in-progress marker is
  written in the same DB transaction as the first row a walk ever commits, so the gate and the marker
  share one truth with no race window. `ingest::run_once` also never lets one RPC page's boundary split a
  Solana slot across two committed units (a trailing run of signatures sharing the oldest slot seen so far
  is buffered in memory and merged into the next page fetch instead) — otherwise the newer half of a
  same-slot pair could land in an earlier-committed batch and get the lower `settlement_tx.id`, inverting
  `(slot, id)` order for exactly that pair.

Both passes have their own durable cursor (`settlement_cursor`, `derive_cursor`) and are safe to call on a
timer independently; `derive_once` naturally lags a little behind `ingest::run_once`, and refuses outright
while an ingest walk is unfinished.

A third, structurally identical pass, `lifecycle::derive_exit_once`, applies `exit` row
mutations from the SAME settlement-program ingest kind (`ProveExit`/`ConsumeExit` are instructions of the
same on-chain program `PostRoot`/`FinalizeBatch`/etc. already ingest under `kind = 'root'` — never a
separate ingest source) — its own `derive_cursor` row (`kind = 'root'`), entirely independent of
`derive_once`'s `'inbox'`-kind cursor, so the two passes never interfere.

## What it does

- **A two-phase cursor for the cold walk** (`cursor.rs`): the very first walk against a
  program with a multi-million-signature backlog (Tiber's inbox program today) commits every fetched page
  as soon as it is decoded — never buffering more than one page in memory — and records a durable
  `backfill_before` marker after every commit. A crash mid-walk resumes from that marker (at most one
  page's worth of redundant, idempotent re-work) instead of restarting from the tip; `last_sig`/`last_slot`
  (the steady-state checkpoint) only advance once the whole walk reaches its end.
- **Paged `getSignaturesForAddress` with a per-request timeout and two-endpoint failover** (`rpc.rs`):
  a degraded RPC endpoint cannot silently stop this service from making progress.
- **Rows and the cursor share one commit** (`ingest.rs`): a fault partway through a chunk's DB transaction
  (a constraint violation, a dropped connection) commits neither the rows nor the cursor advance for that
  chunk, so the durable cursor never names a signature that is not actually in `settlement_tx`.
- **`settlement_tx`**: one row per signature this crate cares about — slot, block time, signer, and a
  `status` (`confirmed` → `finalized`, or the terminal `dropped`) this crate's own
  `finality::track_finality` pass advances by re-checking Solana. A signature `getSignatureStatuses`
  reports no status at all for becomes `dropped` once it has been behind the node's own finalized slot for
  `finality::DROPPED_HORIZON_SLOTS` (150) slots — a terminal state, so it is never re-checked forever.
- **`settlement_tx_program`**: one row per `(settlement_tx_id, kind)` — `ix_kind` (the instruction name(s)
  that program's ingester decoded from this signature — a bundle like `Open+Write+Seal+SealLeaf` for the
  design's one-V1-tx-per-frame shape, or a single lifecycle name), `chain_id`/`batch_id` (nullable — `NULL`
  means "not attributable", never a misleading `0`).
- **`inbox_chunk`**, keyed by `chunk_pda` (the chunk PDA account's own address) as well as the
  human-readable `chunk_id`: `Open` inserts a row; `Seal` (and, if it ever needs to, `SealLeaf`) resolves
  purely by the chunk PDA account named in the instruction and only ever `UPDATE`s an existing row — a
  `Seal`-only transaction with no prior `Open` (the batcher's own resubmit path) is a genuine no-op, never
  a fabricated row. `byte_len`/`body_hash` come from `Seal`'s own fields (the authoritative sealed length),
  never from `Open`'s allocation size or `Write`'s payload (this crate never reads chunk body bytes at all
  — that is `rome-zk-derive`'s job).
- **`batch`**, from `OpenBatch`/`FinalizeBatch`/`AbandonBatch`/`CloseBatch`/`GrowBatch`. Those last four
  instructions carry no `chain_id`/`batch` in their own data (the target account is named in the accounts
  list, not the data) — `batch.batch_pda` (set the moment `OpenBatch` is first observed) is what lets the
  derive pass resolve that account address back to `(chain_id, batch_id)` without ever reading account
  state. `batch.finalize_cursor` mirrors `programs/zk-inbox/src/batch.rs::finalize_batch_inner` exactly:
  `FinalizeBatch` is permissionless and may be called with a partial `step`, and `status` only becomes
  `'finalized'` once the cursor reaches `expected_count` — never on an earlier partial call. Applying the
  same `(batch_pda, settlement_tx_id)` step twice (a `derive_cursor` reset for repair) is idempotent via
  the `batch_finalize_step` ledger. `batch.status` stays in `{open, finalized, abandoned}` forever —
  `CloseBatch` (rent reclaimed) and `AbandonBatch` are recorded on the orthogonal `closed_tx`/
  `abandoned_tx` columns, never a fourth status value and never sharing `finalized_tx`.
- **`inbox_chunk.closed_tx`**: set from `Close` (rent reclaimed) — the row is never deleted, so a reclaimed
  chunk's DA history still shows as posted rather than silently vanishing.
- **`block_status`**, a SQL view (`status.rs::block_status`) deriving `sequenced | data posted (confirmed)
  | data posted (dropped) | data posted | root posted | final | abandoned (confirmed) | abandoned
  (dropped) | abandoned` at read time from `batch`/`settlement_tx`/`root_post` — nothing is stored on a
  batch row that would need rewriting at each transition. The `(confirmed)`/`(dropped)` suffix reflects
  the *finalizing* (or *abandoning*) transaction's own status (`confirmed` is
  provisional, `finalized` is the only safe read) — it becomes plain `data posted`/`abandoned` once that
  transaction itself reports `finalized`, or the `(dropped)` variant if it instead reaches the terminal
  `dropped`. The suffix applies uniformly to both arms: a `dropped` `AbandonBatch` must not
  read identically to a `finalized` one. This is a documented bound, not something the view can close on
  its own — once a row is derived from a `confirmed` transaction, it is never un-applied later if that
  transaction turns out `dropped`; the repair is an operator re-deriving from a reset `derive_cursor` once
  the real on-chain outcome is known. The schema has no per-L2-block table yet (a
  `block_batch`/`l2_block_settlement` mapping is the natural follow-on), so the view's grain is
  `(chain_id, batch_id)`, the coarsest level this crate's own data actually supports.
- **`root_post`, `proof`, `challenge`** are reserved tables for root posting, the
  prover and the challenger; this crate does not populate them yet. `batch.root`/`batch.acc` are likewise reserved (`NULL` for now): both
  are computed on-chain and stored in the batch *account*, never passed as a `FinalizeBatch` instruction
  argument, so a pure transaction-history watcher cannot populate them without an added `getAccountInfo`
  read this crate does not perform.
- **`exit`** (keyed by `message_hash`, globally unique): `ProveExit` (28) inserts a row
  (`status = 'proved'`, `proved_sig` = that transaction's own signature); `ConsumeExit` (29) is the only
  thing that ever moves it to `'released'` (`released_sig` set), resolved purely by `message_hash` — never
  a fabricated row for a `ConsumeExit` with nothing to update (the same "no matching row = genuine no-op"
  shape `Seal`-before-`Open` gets on `inbox_chunk`). `window_index` stays `NULL`: it is a runtime
  `Clock::slot`-derived on-chain value (`window = slot / root.challenge_window_slots`), not an instruction
  argument, and this watcher never reads Solana account state — the same documented bound as
  `batch.root`/`batch.acc` above. `amount` is decimal TEXT (Postgres has no native 128-bit integer type).

- **`settlement_tx_program.events_version`**: the `events` payload's own schema
  version, `1` today. The compatibility rule is additive-only — a new field on an existing
  `ChunkEvent`/`BatchEvent`/`ExitEvent` variant is `#[serde(default)]` and does not bump this column
  (`DerivedEvents::exit_events` itself is one such field, added later: `#[serde(default)]` so an
  already-ingested row's `events` JSON, which predates it, still decodes as an empty `Vec`); a
  renamed/removed variant or a changed field type does. `lifecycle::derive_once` never lets a decode
  failure abort the pass: an undecodable row (the batch-terminal-state repair path re-derives the OLDEST rows with
  the NEWEST decoder, exactly where a real break would surface) is a named `IngestError::UndecodableEvents`
  that is logged and skipped for that one row, never a panic. Repair: re-ingest the row (`ON CONFLICT
  (settlement_tx_id, kind) DO UPDATE SET events, events_version`), then reset `derive_cursor` to re-derive
  it.
- **Account resolution never shifts**: `decode.rs` resolves an instruction's accounts
  into one slot per index (`Vec<Option<&str>>`), never compacted — an index that does not resolve against
  the transaction's own static `account_keys` (an address-lookup-table-loaded key this crate does not
  decode, or a malformed message) yields `None` for that slot only, never a left-shift that mis-attributes
  a later, real account to an earlier one. ALT-bearing and CPI-nested inbox instructions are out of scope
  for this watcher for now (client transactions carry no ALTs today).
- **RPC failover on a null transaction body**: a null `getTransaction` response from
  one endpoint means "try the next", not "this signature does not exist" — Agave maps a transient
  Bigtable read error to a bare null, so `rpc::RpcSource`'s two-endpoint failover only
  resolves to `Ok(None)` once every configured endpoint agrees; an endpoint that errors instead of
  agreeing null propagates that error rather than silently resolving to `None`.

Instruction decoding is never re-implemented here: `zk_inbox_client::decode_instruction` and
`zk_settlement_client::decode_instruction` are the single place each
program's borsh instruction enum is parsed, and `zk_inbox_client::{chunk_account_index,
batch_account_index}` are the single place that says which position in an instruction's own account list
carries the chunk/batch PDA — this crate never hard-codes those positions itself. An instruction this
crate cannot decode (a future variant, or — confirmed against real Tiber history, see below — an *older*
wire shape a live program predates) is skipped, not fatal: this crate keeps making progress on what it
does understand rather than wedge the whole page on one instruction it does not.

## What the replay fixture shows

Tiber's **live** inbox program still predates this repo's `Seal { len: u32, body_hash: [u8; 32] }` shape.
Every real `Seal` instruction in the recorded fixture — including the most recent
batch, 4005 — is the older 5-byte `{ len: u32 }` payload (verified byte-for-byte:
`disc=2, len=5` on every chunk-bundle transaction sampled). `InboxIx::Seal` correctly fails to borsh-decode
a 5-byte buffer into its 37-byte variant, so this crate never sets `sealed`/`body_hash` from Tiber's real
history today — the chunk row still exists (from `Open`, whose shape is unchanged), just with
`sealed = false`. The settlement program's own history shows the same kind of skew already happened once
and was reconciled (`MigrateChain` brought forward a chain that predated the newer layout); the analogous
fix here is a `zk-inbox` redeploy to Tiber.

## Config

`ingest::WatcherConfig { rpc_page_size, commit_batch_size }` — how many signatures one
`getSignaturesForAddress` call requests (bounded by Solana's own 1,000/call ceiling) and how many
signatures' worth of writes land in one DB transaction before the cursor advances. `rpc::RpcSource::new`
takes RPC endpoint URLs in priority order (first to answer within `RPC_REQUEST_TIMEOUT` wins).

## Running the service

`cargo run -p rome-zk-settlement-watcher --bin rome-zk-settlement-watcher` — applies
`migrations/explorer/*.sql` (`migrate::migrate`) then loops ingest (both programs) + both derive passes
(inbox lifecycle, exit lifecycle) + the finality pass on a fixed interval, forever. Config is
environment variables only, named and fail-closed
(an absent required one is a startup error naming exactly which): `ROME_ZK_WATCHER_DATABASE_URL`,
`ROME_ZK_WATCHER_RPC_PRIMARY`, `ROME_ZK_WATCHER_INBOX_PROGRAM`, `ROME_ZK_WATCHER_SETTLEMENT_PROGRAM`
required; `ROME_ZK_WATCHER_RPC_SECONDARY` (failover endpoint) and
`ROME_ZK_WATCHER_POLL_INTERVAL_SECS` (default 5) optional. See
`src/bin/rome-zk-settlement-watcher.rs` for the full list and its doc comment.

## How to test

Every integration test needs Docker (a throwaway `postgres:16` container per test, self-destructing —
`--label rome-zk-test=1` plus `exec timeout 600` (≤ 10 min) in its entrypoint, so a killed test process
cannot leave it running forever; the CI job also reaps every `rome-zk-test`-labelled container with
`if: always()` regardless of why the job ended). **Running Docker-backed tests requires the invoking user
to be in the `docker` group**; `TestPg::start` fails fast with a clear panic naming the missing permission
otherwise.

```sh
cargo test -p rome-zk-settlement-watcher --lib                # decode.rs unit tests, no DB
cargo test -p rome-zk-settlement-watcher --test replay         # the Tiber devnet replay (below)
cargo test -p rome-zk-settlement-watcher --test finality       # confirmed -> finalized / dropped
cargo test -p rome-zk-settlement-watcher --test exit_lifecycle # ProveExit/ConsumeExit decode + exit table
cargo test -p rome-zk-settlement-watcher --test devnet_probe -- --ignored --nocapture   # real, read-only RPC
docker ps -aq --filter label=rome-zk-test | xargs -r docker rm -f   # force-remove any leaked rome-zk-test containers
```

`tests/replay.rs` replays `fixtures/settlement-watcher/tiber-devnet-batches-4003-4005.json` — 2,794 real
signatures from Tiber devnet's own inbox program (batches 4003 finalized/290 chunks, 4004 abandoned/799
chunks landed of 800 submitted — one failed on-chain with `ComputationalBudgetExceeded`, correctly
producing no chunk row, 4005 finalized/899 chunks) plus 11 from the settlement program — through
`tests/support::FixtureSource`, a `Source` implementing the exact same `before`/`until`/`limit` paging
contract a real RPC node has. `tests/support::FailAfter` injects a mid-page failure to prove a watcher
killed partway through a backlog resumes with no gap or duplicate; `tests/support::ScriptedSource` is a
small, fully hand-built `Source` for cases the fixture's own real history cannot exercise (a partial
`FinalizeBatch`, a `Seal`-only transaction, a DB fault mid-chunk, a same-slot pair split across a page
boundary, a derive pass gated on an unfinished ingest walk, `CloseBatch`/`AbandonBatch` recorded on their
own columns, a `FinalizeBatch` step re-derived after a cursor reset); `tests/support::NullBodyOnce` proves
a transient Bigtable-null `getTransaction` body is retried rather than silently skipped.

`tests/exit_lifecycle.rs` uses its own small hand-built `Source` (not the Tiber fixture — Tiber
has never posted `ProveExit`/`ConsumeExit` yet, so there is no real history to replay) serving two real,
builder-compiled instructions (`zk_settlement_client::prove_exit_ix`/`consume_exit_ix`) through the real
`ingest::run_once` + `lifecycle::derive_exit_once`, proving both the by-name decode and the `exit` row's
`proved` → `released` transition end to end against a real Postgres container.

## Security notes

Read path only. Nothing here can influence sequencing, settlement, or finality — it reports on Solana's
own transaction history, it does not decide it. The database is not authoritative for anything on-chain;
losing or corrupting it never risks funds, only this crate's own read views (rebuildable from Solana at
any time by resetting `settlement_cursor`/`derive_cursor`).

## Design notes

This crate covers the explorer's Solana ingest: the batch/chunk/root layouts and instruction shapes (what the
signatures actually carry), finality, and the Postgres topology. It follows three choices: a minimal
settlement data model, UNNEST-batched inserts, and a "feed, not shadow table" approach.

## Depends on

[`zk-inbox-client`](../zk-inbox-client) and [`zk-settlement-client`](../zk-settlement-client) for PDA
derivation, instruction decoding, and account-position lookup; [`rome-zk-layouts`](../rome-zk-layouts) to
re-derive a decoded `ProveExit`'s `message_hash()` — the single owner of that arithmetic, never
re-implemented here. Shares its Postgres database (`rome_zk_explorer`, `migrations/explorer/`) with
[`rome-zk-indexer`](../rome-zk-indexer) and [`rome-zk-explorer-api`](../rome-zk-explorer-api) once those
land.
