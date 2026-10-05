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
  `[root.head_final_batch, cursor.next_batch)` at startup is **finished under its own id** before
  anything new is posted (see the next bullet). A batch id is never given up: settlement posts exactly the
  next id and needs that id's batch finalized, and an id the inbox cursor has passed can never be opened
  again. A finalized batch sitting ABOVE an open one in that window is refused by name
  (`PipelineError::FinalizedAboveOpenBatch`): defense in depth now that `FinalizeBatch` is authority-gated
  on chain.
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
- **The deposit cursor rides in the stream as the optional fifth block field.** `BlockSource` reads each block's
  withdrawal indices off the ordered log (it never counts them): a block's `deposits_end` is its last
  withdrawal's index plus one, and a block without withdrawals keeps the previous value. Indices must run on
  without a gap or a repeat, and the first index after the resume point must equal the deposit cursor's
  `deposit_next` (set with `BlockSource::with_deposit_start`, default 0) or the previous batch's end; anything
  else stops the batcher with `FirstDepositIndexMismatch` or `DepositIndexGap`. `SizeCappedGrouper` keeps the
  running value (`with_deposits(from, cap)`) and writes the field through the channel's `set_deposits_end`, so it
  appears exactly where the value changes. When a `DepositCap` is set it closes a group before a block that
  would take it over the queue's `max_per_batch`, using the stricter of the active and pending values, and the
  next group starts where that one ended (`CloseReason::Deposits`). A log without deposits gives the same
  stream and frame bytes as before (`tests/deposits_stream.rs`).
- **`FinalizeBatchV2` is sent with a range the batcher works out itself, from the batch's own posted stream.**
  No caller passes a range. `finalize_and_verify` decodes the posted frames and `deposits::plan_range` takes
  `from` from the live cursor (`deposit_next`, read once every earlier batch is finalized or abandoned; 0 while
  the cursor is still v1) and `to` from `resolve_deposits_end` over the decoded stream. The normal path and the
  restart path (`resume_open_batches`) both go through it, so a restart cannot send a range the stream does not
  carry; a finalized batch with the wrong range cannot be proved and `AbandonBatch` refuses it. Before any
  send the posted stream must decode to exactly the blocks the frames were cut from, the fifth field included
  (the re-derive comparison covers `deposits_end`), and a stream that takes deposits on a chain with no bridge
  is refused by name. The bridge program named in the instruction is read from the chain's `exit_config` at its
  PDA under the settlement program; a chain with no exit config finalizes an empty range. After the finalize,
  `verify_acc` recomputes `acc` from the chunks and the header's range, checks that the header's range ends
  where the stream's does, and the batcher checks that the header's range is the one it sent.
- **A batch the inbox would refuse is not opened.** The inbox measures a deposit's age at the batch's own open
  time, so whether a batch can finalize is settled when it opens, and a batch that leaves out an overdue deposit
  could afterwards only be abandoned. Before `OpenBatch`, `WindowedPoster::submit_group` works out the batch's
  range (it starts where the previously opened batch's range ends, or at the cursor for the first one), and
  `deposits::check_deadline_at_open` applies the inbox's rule with the batcher's clock as the open time, 300 s
  early (the host clock and the cluster clock differ, and `OpenBatch` lands after the check), and with the
  shorter of the active and a pending inclusion deadline. A batch that fails it is not opened and the batcher stops with a named error, so the batch id is not used up; the
  sequencer has to put the overdue deposit into the stream. A chain with no bridge or no queue is not checked.
- **A v1 cursor is topped up once before the chain's first V2.** The instruction that grows the cursor to its
  69-byte layout has no payer, so the batcher sends one plain system transfer from its payer for the missing
  rent first. It reads the cursor and its balance before sending, so a restart or a second pass never pays
  twice. At startup the batcher also reads the cursor's `deposit_next` and the deposit queue's active and
  pending `max_per_batch` and hands them to `BlockSource::with_deposit_start` and `SizeCappedGrouper`'s
  `DepositCap`. To abandon batches by hand (`examples/abandon_batches.rs`), list the ids oldest first.
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
- **At startup, every open-not-finalized batch in the pending window is finished, never abandoned.**
  A crash between `OpenBatch` and `FinalizeBatch` leaves a batch that settlement still needs. On the next
  start the batcher takes each such batch (lowest id first) and works out which blocks it was cut from.
  The chain still holds how many frames the batch expects, when it was opened, and a hash for every frame
  already sealed; the frames that never landed exist only in the ordered log. So the batcher tries each
  possible last block, from the block after the previous batch up to `blocks_per_batch` blocks long,
  smallest first, and keeps the first one where (1) the log cuts into exactly the expected number of
  frames, (2) every frame already on chain is byte-for-byte what this grouping would have produced, and
  (3) every block is within the allowed time drift of the batch's open time. It then sends the missing
  frames, finalizes (picking up from the finalize cursor if finalize had begun), checks `acc` and hands the
  batch off like any other. If every frame is already on chain there is nothing to search: the frames are
  read back from the chain. With more than one batch open, they are resumed in order, each starting where
  the one before ends. Nothing is sent until the whole plan exists. If no grouping fits (the compressor
  version, frame size, `blocks_per_batch` or the log changed since the crash) the batcher stops with the
  named error `PipelineError::ResumeImpossible { batch, leaves_present, expected_count }`, sends nothing,
  and leaves the batch open: rerun with the build and config that opened it. The window is probed with
  paged `getMultipleAccounts` calls (100 ids per page — the same page size the chain-anchor walk uses),
  never one `get_account` per id: while no batch has settled, `head_final_batch` stays 0, so this window
  can span every batch the chain has ever opened.
- **Never run `AbandonBatch` by hand on a batch settlement still needs: it halts the chain.** Settlement
  posts exactly the next batch id, that id's batch must be finalized, and an abandoned id can never be
  reopened or skipped, so the chain stops for good at that id. The `abandon_batches` example is only for
  ids that nothing needs any more (for example leftovers from a measurement run on a throwaway chain).
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
- **Posted inbox rent stays locked until the accounts are closed.** Closing a posted batch or chunk
  needs a final root covering that batch; closing early would delete data the settlement layer may
  still need. This batcher only closes chunks during startup cleanup of abandoned batches. It does
  not close posted inbox accounts after finality, so plan for their rent to keep accumulating.
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

Age-close observability: `_groups_closed_total{reason=cap|size|deposits|age}` (counter — a quiet chain that never
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
finished under its own id (`recover.rs`) — a prior instance's crash mid-post (possibly leaving more
than one batch open under a bounded posting window) is repaired, not guessed at as "maybe still live" and
not given up. If the log no longer matches what is on chain, the process stops with `ResumeImpossible`
and sends nothing. One
more refusal, defense in depth now that `FinalizeBatch` is authority-gated on chain: if a finalized batch
sits above an open one in that window (for example a batch finalized under an older program version,
or by another process holding this chain's authority key), startup refuses by name
(`PipelineError::FinalizedAboveOpenBatch`) and sends nothing; the process exits and stays halted on that
chain until the open batch is finalized. **`AbandonBatch` sent by hand halts the chain**: settlement still
needs that id and an abandoned id can never be reopened, so do not run it for a batch that has not settled.
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

## Compute-unit limits

Three limits cover the transactions that derive program addresses with a bump search. The search costs compute
units for every extra attempt, and the attempts are fixed per address, so the cost varies with the batch id and the
chunk slot. The batch cursor cannot skip an id, so a batch whose transaction does not fit its limit stops the chain at
that id. An attempt fails with probability one half, so the chance an address needs at least m extra attempts is 2^-m.

| Transaction | Limit (default) | Cost |
|---|---|---|
| Open-and-grow (`OpenBatch` plus the `GrowBatch` instructions a 900-leaf batch needs), once per batch | `open_compute_unit_limit` (400,000) | 23,677 CU, plus 4,500 CU per extra attempt on the batch address |
| A frame's chunk transaction (`Open`, `Write`, `Seal`, `SealLeaf`), once per frame | `chunk_compute_unit_limit` (100,000) | about 16,400 CU, plus 1,500 CU per extra attempt on the batch address and 3,000 CU per extra attempt on the chunk address |
| The same chunk transaction, resent once if it ran out of compute units | `chunk_retry_compute_unit_limit` (400,000) | as above |

`tests/bump_search_cu_limit.rs` runs the compiled inbox program under the devnet inbox and settlement program ids and
scans 256 batch ids for open-and-grow at 900 leaves and 900 chunk slots with a full-size body. The figures are
23,677 CU at best, 28,431 at the median and 55,431 at the worst for open-and-grow, and 26,903 CU at the median and
50,903 at the worst for a chunk transaction. Two batch ids from a wider scan, 274,100 and 1,895,697, cost 109,536 and
132,063 CU in open-and-grow: both are over 100,000, which is why open-and-grow has its own limit, and the test
requires the open limit to leave room for at least 40 more attempts (180,000 CU) above the cheapest id.

A chunk frame that fails on chain for running out of compute units is resent once with the retry limit instead of
failing the batch (`Sender::send_and_confirm_many_retrying_compute`; the runtime reports the overrun as
`ComputationalBudgetExceeded` or as `ProgramFailedToComplete`, and both count). Only that frame pays the higher
limit, once. At the 100,000 base limit a frame needs retrying when its attempts add up to more than 83,600 CU:
28 chunk-address attempts on a batch address that needed none, which is about one frame in 270 million, or 23 when
the batch address needed 10, about one in 8 million. `tests/chunk_compute_retry.rs` forces that path on the real
program with a base limit below any frame's cost, and shows the frame landing on its retry.

What is left after both: an open-and-grow transaction stops the chain only if the batch address needs 84 or more
extra attempts (a chance of 2^-84 per batch id), and a frame fails after its retry only if its attempts add up to more
than 383,600 CU, which is 128 chunk-address attempts on a batch address that needed none and still 87 on one that
needed 83, the most that can open (below 2^-86 per frame). These are very small numbers, not zero: this does not say
no batch can ever stall. The measured figures come from the scans; the 2^-m rule is the model for the rest.

The priority fee is the limit times the price, and it is charged on the limit, not on what the
transaction uses. At 1,000 micro-lamports per CU a 100,000 CU transaction pays 100 lamports; at the
200,000 micro-lamport ceiling it pays 20,000. The block cost cap also charges the limit, so the payer
clears about 292 frames per second at the 12,000,000 cap and about 585 at the 24,000,000 cap. A larger
limit lowers both figures, which is why the chunk limit stays at 100,000 and the rare frame that needs more is retried
instead. Open-and-grow and the retry are sent once per batch and once per rare frame, so their high limits cost
almost nothing against the cap.

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
