# rome-zk-batcher

Reads the sequencer's ordered log tail, packages blocks into a compressed byte stream, cuts that stream
into fixed-size frames, and posts each frame to the inbox program as one Solana transaction. Also drives
the inbox program's batch accumulator — opening a batch, sealing each chunk into it, and finalizing it once
every chunk has landed — and hands the finalized batch off to whatever posts roots to the settlement
program.

## What it guarantees

- **Stateless — one on-chain chain anchor, both modes.** This process keeps no durable state of its own
  (no local resume file): on start, it re-derives its resume point purely from the inbox program's
  on-chain batch accounts — the last finalized batch's own posted blocks, decoded from its still-intact
  sealed chunks (or, once those are rent-recycled, from the settlement root plus an exact replay of the
  ordered log) — see `anchor::resolve_anchor`. Every not-finalized batch found in the pending window
  `[root.head_final_batch, cursor.next_batch)` at startup is `AbandonBatch`'d and its chunk PDAs
  closed before anything is posted (`resume`'s own module doc explains why a stateless resume can never
  safely guess "still live" instead) — unless a finalized batch sits ABOVE an open one in that window, in
  which case the sweep refuses by name (`PipelineError::FinalizedAboveOpenBatch`) and abandons nothing: defense
  in depth now that `FinalizeBatch` is authority-gated on chain.
- **Channel = batch, frame = chunk.** One batch (at most `blocks_per_batch` blocks, read from the
  sequencer's own `profile.json` — 10 by default and 60 on Tiber; `rome-zk-derive` does not read
  `profile.json` and carries the same value in its own config) is RLP-encoded and compressed as a
  single stream; the compressed bytes are cut into frames of at most 3,681 bytes each, one frame per
  inbox chunk, emitted as soon as enough compressed bytes are ready rather than waiting for the whole
  batch to finish compressing. A shadow compressor tracks the true compressed size so output never
  overshoots the chunk size limit.
- **A batch never holds more than `blocks_per_batch` consecutive blocks, and continuity is enforced across
  every group boundary, restart included.** `grouping::BlockGrouper` enforces all of: no group ever grows
  past the cap, every block pushed must continue immediately from the last one *including the first push
  after a group is taken*, and a gap in the log (a missing block) is a named error with nothing from that
  run posted. `grouping::SizeCappedGrouper` — the **one grouping path both `--once` and `--follow` drive
  from the chain anchor** — additionally closes a group early, before the
  `blocks_per_batch` cap, if the next block would push the channel's compressed encoding over
  `max_frames_per_batch`; `--once` needs this exactly as much as `--follow` does (a full cap-sized group at
  the design gas ceiling would otherwise stall every rerun identically). These are exactly the invariants
  `rome-zk-derive`'s `batch_queue::decode_batch` checks on the read side; a dev-dependency test
  (`tests/derive_decode_batch_compat.rs`) proves the real function accepts every shape this crate's own
  grouping produces, size-closed groups included.
- **`BlockSource` refuses rather than truncate.** If the log genuinely holds more sub-blocks for a block
  than this process is configured for, `BlockSource` peeks one record ahead before ever handing back a
  "complete" block and refuses (`SourceError::ProfileMismatch`) — a misconfigured `sub_blocks_per_block`
  is caught before anything half-formed is posted, not one record later.
- **Seals are fully parallel; finalization is the one ordered step.** Because the accumulator's leaves are
  order-independent, every chunk's `Open`+`Write`+`Seal` sequence can be in flight concurrently; only
  `FinalizeBatch` — the Merkle reduction over the whole batch — is a single ordered step per batch.
- **A bounded posting window overlaps up to `batches_in_flight` batches (default 2).** `OpenBatch(N+1)` is
  sent only once `OpenBatch(N)` has confirmed, chunk lanes of different batches overlap freely, and
  `FinalizeBatch(N+1)` is sent only after `FinalizeBatch(N)` has confirmed and its hand-off is done —
  overlapping the cluster's own inclusion-tail latency with the next batch's work instead of paying it
  serially, without ever reordering finalization (`rome-zk-derive` depends on batches finalizing in id
  order). A failure in any in-flight batch stops the whole run: no further `OpenBatch` is ever sent — the
  shared failure flag is checked both at the top of `submit_group` and again right before `OpenBatch`
  itself (after `resolve_batch_id`'s own account reads, which are real await points a sibling can fail
  during) — and `--follow`'s own idle tail-wait polls for that failure on every tick
  (`WindowedPoster::poll_failure`) — it is never learned about only on the next group submission, which on
  a quiet chain could be a whole batch period away.
- **`lag_blocks` is read at each batch's own hand-off time, from a live counter, not captured when the
  batch was submitted.** The main loop keeps a shared newest-block counter, updated every time the ordered
  log yields a new block (`--once` and `--follow` alike); each batch's own settle task reads it only once
  it actually finalizes and hands off — so the gauge reflects how far the log has run ahead of settlement
  by then, not a snapshot from whenever this batch happened to be opened.
- **CU sampling never sits on the posting path.** The two `getTransaction` reads a finalized batch's own
  CU figures come from are spawned as a single budget-bounded (5 s total) background task; their outcome
  is a log line only and can never delay the next batch's own progress.
- **At startup, every open-not-finalized batch in the pending window is abandoned, not just the newest**
  (except when a finalized batch sits above an open one — then the sweep refuses and abandons nothing; defense
  in depth now that `FinalizeBatch` is authority-gated on chain).
  A bounded posting window can leave more than one batch open-not-finalized after a crash (e.g. the older
  one still mid-chunk while a newer one had already been opened) — every one of them, from the settlement
  root's own `head_final_batch` up to the cursor's `next_batch`, is swept before anything is posted. That
  window is probed with paged `getMultipleAccounts` calls (100 ids per page — the same page size the
  chain-anchor walk uses), never one `get_account` per id: while no batch has settled, `head_final_batch`
  stays 0, so this window can span every batch the chain has ever opened.
- **Re-derives before spending a fee.** Before committing to a channel's contents, this process decodes
  its own encoded stream back through the same stages a derivation node would use and compares the result
  to the sequencer's own blocks — a self-check that the compressed bytes it is about to pay to post
  actually decode to what the sequencer sequenced.
- **A startup preflight refuses to run silently misconfigured.** Before this process sends any
  fee-spending transaction, it confirms two facts against the live chain: that its own payer is the
  chain's registered authority (otherwise every `OpenBatch`/chunk `Open` it attempts would be rejected
  on-chain), and that the inbox program id it is configured with matches what the settlement program's
  registry actually names for this chain. A mismatch on either fails the process at startup with a named
  error, not after paying to find out on-chain.
- **Back-pressure flows toward the sequencer, never toward silently dropping data.** When unposted bytes
  build up beyond a threshold, this process signals the sequencer to lower admission — it never drops
  transactions to keep up.
- **Rent recycling waits for finality.** A chunk is only closed once the batch's root is final (per the
  settlement watcher's status), never before — closing early would delete data availability the
  settlement layer might still need.
- **A partial group also closes on its own after `batch_close_after_secs` — measured on this process's own
  receipt clock, never a block's own timestamp.** A group that never fills (the chain runs below its
  block-count cap, or produces nothing for a while) would otherwise sit unposted forever; once it has held
  its first block for `batch_close_after_secs` (default 60 s), it posts through the exact same
  `WindowedPoster::submit_group` call a full or size-closed group uses — no separate posting path.
  Measuring from this process's own clock, rather than a block's own on-chain timestamp, matters for two
  reasons — at Tiber's profile (60 blocks / 60 s / 1 s blocks) chain-time age reaches 60 s while the grouper
  holds 59 blocks (block N+59's last sub-block seals at ≈ T+59.95, read ≤ 400 ms later), so every loaded batch
  would close `Age` at 59 and never `Cap`; and a batcher wall clock ahead of the sequencer's would shrink
  groups toward 1 block (60× the 28 s fixed proof cost, 60× the 0.001 SOL fee). Receipt time involves one
  host's clock only: both failure modes are unconstructable, not guarded. An empty group never closes
  this way, however long it has been since the last one did — there is nothing to measure the age of.
  `--follow` checks this on every loop tick, whether a block just arrived or the log went quiet, so a
  trickle of blocks slower than the cap still closes on schedule; `--once` never checks it at all — it
  reads the whole log and posts its own trailing partial group once, explicitly, at the end, exactly as it
  did before this existed. A partial group still sitting unposted at shutdown is dropped exactly as
  before — the next run re-reads those same blocks off the log from the on-chain anchor.

## Config

TOML (`chain_id`, the inbox and settlement program ids, per-instruction compute-unit limits, in-flight and
confirmation tuning, `batches_in_flight` (the bounded posting window, default 2), `batch_close_after_secs`
(a partial group closes on its own after this many seconds on this process's own receipt clock; default
60, refused at 0 and below the chain's own block time once the sequencer's `profile.json` is read),
`metrics_addr` (default `127.0.0.1:9002`, env override `ROME_ZK_BATCHER_METRICS_ADDR`), `cu_sample_every`
(default 50), `rpc_url`, `cluster` — a required free-form label so a config can never omit stating which
cluster it targets). **The payer's signing key is never inlined** — `payer_key_path` names a file path,
resolved at process startup, exactly like the sequencer's own `sequencer_key_path`. See `src/config.rs`
for every field and its default.

## Metrics

Served over `GET /metrics` at `metrics_addr` (TOML config key, default `127.0.0.1:9002`, env override
`ROME_ZK_BATCHER_METRICS_ADDR`) — the exact same reth-free HTTP responder
[`rome-zk-sequencer`](../rome-zk-sequencer) serves, pulled into its own crate,
[`rome-zk-metrics-http`](../rome-zk-metrics-http), so this crate never needs a reth dependency to expose
one endpoint. Spawned once at process startup, in both `--once` and `--follow`. Tiber's rendered config
sets this to `0.0.0.0:9002` and `docker-compose.settlement.yml` exposes it to the compose network only
(never a public `ports:` mapping); the operator's deploy config lists the ports.

Prometheus text exposition (`metrics::Metrics::render`): `rome_zk_batcher_frames_sent_total`,
`_chunk_confirm_latency_seconds`, `_resubmits_total`, `_batches_finalized_total`, `_batches_failed_total`,
`_bytes_per_tx`, `_compression_ratio`, and cadence observability for the bounded posting window —
`_open_confirm_seconds`/`_finalize_confirm_seconds`/`_batch_post_seconds` (histograms, 0.25 s – 60 s
buckets), `_batches_in_flight` (gauge, 0..=`batches_in_flight`), `_lag_blocks` (gauge: newest block
visible in the ordered log minus the last finalized batch's own last block), and
`_cu_samples_triggered_total` (counter: finalized batches whose turn it was to CU-sample under
`cu_sample_every`, bumped whether or not an RPC client is configured — see below).

Age-close observability: `_groups_closed_total{reason=cap|size|age}` (counter — a quiet chain that never
produces anything shows no increments here at all), `_oldest_unposted_block_age_seconds` (gauge: how long
the group in progress has held its first block on this process's own receipt clock, 0 whenever nothing is
unposted), `_block_age_on_arrival_seconds` (histogram: this process's wall clock minus a block's own
timestamp at the moment it was read — observability only, never a close decision; a restart resuming into
a backlog legitimately shows a large value here).

**Solana clock skew:** `_solana_clock_skew_seconds` (histogram, signed, buckets
past ±60 s both sides) observes, once per `OpenBatch`, the just-opened batch account's own committed
`open_unix_ts` minus this process's own unix wall clock captured immediately before the send. A negative
value means this batcher's clock was already ahead of the moment the chain later stamped. Observability
only — never a decision input anywhere in this crate; the actual drift bound this feeds the
confirmation measurement for lives in [`rome-zk-derive`](../rome-zk-derive), checked there against the
same `open_unix_ts`.

**CU sampling is off the posting path, budget-bounded, and rate-limited (`cu_sample_every`, default 50).**
Two `getTransaction` reads per sampled batch (`OpenBatch+Grow`, one representative chunk-lane frame) are
informational only — spawned, never awaited by the batch's own settle task — and only
run on every Nth finalized batch (`cu_sample_every` in config, refused at `0`: sampling every batch is
`cu_sample_every = 1`). A timed-out or unresolved lookup logs at `debug`, never `warn` — routine noise on
a busy or degraded cluster, not an operational alarm.

**The chain's block shape (`sub_blocks_per_block`, `block_gas_limit`, `blocks_per_batch`) is not a config
field of this crate at all.** Every one of those is read at startup from `profile.json`, the file the
sequencer writes beside its own ordered log the moment that log is created
(`rome_zk_sequencer::recovery::reconcile_profile_identity`) —
`config::read_profile_identity(log_dir)` calls [`rome-zk-profile`](../rome-zk-profile)'s own
`read_profile_json` (the same classifier the sequencer's `reconcile_profile_identity` is built on) and
maps its classification onto this crate's own `ConfigError` variants, so this crate can never carry a
`blocks_per_batch` that disagrees with whatever the sequencer actually declared. A `profile.json` that
predates `blocks_per_batch` (every other field present, this one missing) is refused, naming
the file and pointing at the sequencer's `--write-profile-json` migration flag, never silently assumed to
be the design's shared default (10). A `profile.json` that predates `first_block` (a
0-based log, from before block numbering started at 1) is refused the same way (`ProfileJsonMissingFirstBlock`) — not migratable; bring
up a fresh chain rather than rerunning this batcher against it. A `first_block` that is *present* but
wrong (a hand-edited `0`, say) is refused by value too (`ProfileJsonFirstBlockNotOne`)
— the same rule the sequencer's own identity check applies to the identical file.

**`grouping::grouper_from_profile` is the one place a `ProfileIdentity` becomes a `SizeCappedGrouper`.**
Both the `--once` and `--follow` binary paths call this one library function (`profile_identity`,
`max_frames_per_batch`, `max_frame_body_len`, the chain-anchor-seeded `last_taken`) rather than each
inlining `SizeCappedGrouper::new(blocks_per_batch, ..)` against a local variable that could silently drift
from what `profile.json` actually says — a chain's `blocks_per_batch` is read from the profile identity
every time, never a hardcoded literal.

**The numbering-origin check does not depend on `profile.json` at all.**
`anchor::resolve_anchor` calls [`rome-zk-log`](../rome-zk-log)'s own `log_numbering_origin` on the log
directory directly — before any on-chain state is read, on every code path (fresh chain, finalized-batch
decode, settlement-root fallback) — and refuses by name (`AnchorError::LogNumbering`) unless the log's own
first record is `(block 1, index 0)`. This is deliberate independence, not redundancy: `profile.json` is a
config file the sequencer writes about itself, and this batcher never trusts a config file's claim about
the log's origin when it can read the log itself.

## Running it

```sh
rome-zk-batcher --config batcher.toml --once <log_dir>     # read the whole log, post what's new, exit
rome-zk-batcher --config batcher.toml --follow <log_dir>   # tail the log, post a batch every time one closes
```

Both modes resume from the same on-chain **chain anchor** (`anchor::resolve_anchor`) — there is no
local resume file. On every start, before resolving the anchor or posting anything, every
not-finalized batch found in the pending window `[root.head_final_batch, cursor.next_batch)` is
`AbandonBatch`'d and its chunk PDAs closed — a prior instance's crash mid-post (possibly leaving more
than one batch open under a bounded posting window) is never guessed at as "maybe still live". One
exception, defense in depth now that `FinalizeBatch` is authority-gated on chain: if a finalized batch
sits above an open one in that window (for example a batch finalized under an older program version,
or by another process holding this chain's authority key), the sweep refuses by name
(`PipelineError::FinalizedAboveOpenBatch`) and abandons nothing — abandoning the open batch would
strand its blocks; the process exits and stays halted on that chain until the open batch is finalized.
This run then reads `batch_cursor.next_batch` once more as its own `expected_next_batch`: if the live
cursor ever disagrees with it later, another writer holding this chain's authority key posted in
between, and this process refuses by name rather than resolving a fresh id under a moved cursor —
exactly one batcher process per chain authority is enforced, not merely assumed.

**One grouping path from the anchor, both modes** (`--once` is `--follow` without the
tail wait): both open the log AT the anchor and verify the very first block the log hands back matches it
exactly (a named `Gap` refusal otherwise — a gap in the log, or the log and chain have diverged), then
accumulate into one `grouping::SizeCappedGrouper` seeded with `anchor.from_block - 1`.

- **`--once <log_dir>`**: posts every group that closes as the log is read, then — once the log is
  exhausted (no tail-follow wait) — posts whatever partial group is left, and exits. A rerun over an
  unchanged (or grown) log therefore posts only whatever is genuinely new, size-closed groups and all;
  there is no from-block-0 regrouping to misalign against a previous run's partial tail group.
- **`--follow <log_dir>`**: continuously tails the log's live tail, posting a group the moment it closes —
  on the `blocks_per_batch` cap, earlier if the next block would push the channel over
  `max_frames_per_batch`, or once a partial group has held its first block for `batch_close_after_secs` on
  this process's own receipt clock — until interrupted (Ctrl-C, registered at process start so a signal
  before the first post still gets tokio's clean-shutdown handling rather than the OS default).
  `--from-block <N>` is a **verified** cross-check only: it must equal the resolved anchor exactly, or this
  refuses rather than trusting an operator's guess over the on-chain fact.

## How to test

```sh
cargo test -p rome-zk-batcher
```

Building the inbox program first (`cargo build-sbf --manifest-path programs/zk-inbox/Cargo.toml
--sbf-out-dir target/deploy`) is required — this crate's integration tests load the real compiled program
into `solana-program-test` so their behavior (and any compute-unit assertion) reflects real BPF execution.
A few tests are gated behind `--ignored` because they read a live devnet RPC (no transaction sent, no
value spent — see each test's own doc comment for exactly what it checks and why it cannot run any other
way); run those explicitly with `cargo test -p rome-zk-batcher -- --ignored --nocapture` when you need
them.

## Security notes

The re-derive-before-send check exists specifically so a bug in this process's own encoding cannot
silently post data that would fail to decode back into the sequencer's blocks — catching that here, before
a fee is paid, is strictly cheaper than catching it downstream in a derivation node that would otherwise
have to reject the whole batch. The preflight check closes a narrower but sharper failure mode: without
it, a batcher pointed at the wrong inbox program id (a copy-paste config error, or a stale config after a
program migration) would spend fees on every attempt and fail every one on-chain, with no signal until
those failures are noticed downstream.

## Throughput limit

One payer key's frames-per-second limit is set by its per-block compute-unit budget, not by the transport.

## Depends on

[`rome-zk-channel`](../rome-zk-channel) for the channel/frame stream codec (re-exported here as `channel`
so every existing call site is unchanged) and [`rome-zk-solana-sender`](../rome-zk-solana-sender) for the
Solana send/confirm machinery (re-exported as `sender`) — both used to live in this crate's own `src/`,
split out so the derivation node and, eventually, the prover orchestrator/challenger can depend on just the
piece they need. [`rome-zk-log`](../rome-zk-log) for the ordered-log reader (`source.rs`'s `LogReader`,
`anchor.rs`'s `log_numbering_origin`) and [`rome-zk-profile`](../rome-zk-profile) for the chain's cadence
defaults (`sealer::SUB_BLOCKS_PER_BLOCK`'s replacement, `ProfileIdentity`, `FIRST_BLOCK`) — both direct
dependencies now, not reached through the sequencer. **`rome-zk-sequencer` is a dev-dependency only**: the
cross-crate tests seal a real `SealerState` end to end rather than reimplementing the sealer as a fixture,
with its `reth` feature turned off since none of that needs the execution engine.
[`zk-inbox-client`](../zk-inbox-client) and [`zk-settlement-client`](../zk-settlement-client) for
instruction building and account decoding against both on-chain programs;
[`rome-zk-merkle`](../rome-zk-merkle) and [`rome-zk-layouts`](../rome-zk-layouts) for the accumulator
construction and account layouts.
