# rome-zk-exit-prover

The off-chain follower for exits: watches the L2 exit portal for `ExitInitiated`, proves each message
against a FINAL batch's `state_root` via `eth_getProof`, verifies the proof LOCALLY before ever sending a
transaction, and sends `ProveExit` (28) the same way `rome-zk-prover` sends `PostRootProved` — through
`rome-zk-solana-sender`'s V1 `Sender`.

## The loop: the chain is the state, this process is a cache

**The follower's own state is the chain** — specifically the `exit_nullifier` bit
`ProveExit` sets for a proved message. Everything [`follower::Follower`] keeps in memory (which messages
are pending, on what they're waiting, which are stuck) is a cache a restart rebuilds for free from
`eth_getLogs(portal, portal_from_block)` plus one nullifier-page read per message — never a disk cursor,
never a database. This replaces an earlier version of this binary whose one `from_block` cursor advanced
for every log BEFORE the attempt's own outcome was known, silently dropping a message that came back
`StateUnavailableAtRoot`/`QueuedForWindow` (it would never be seen again once the cursor passed its
block) — and which re-sent every already-proved exit at a fresh fee on any restart from genesis, since it
never read the nullifier bit at all.

The bin is wire-only, one pass = `--once`, and the whole poll body is [`run::poll_once`] (not `main`
itself — this is what makes it testable over fakes rather than a live RPC):

```
verifier.eth_get_logs(portal, follower.scan_from_block)   // an RPC failure here counts a metric, skips this poll
    → follower.ingest(logs)                                 // advances the cursor, decodes, dedupes
    → settlement.read_slot()                                // a failure here ALSO skips this poll, never a fabricated slot
    → follower.due(now)                                     // which pending/stuck hashes to try now
    → attempt_exit(...)  for each                           // core.rs, unchanged shape below
    → follower.apply(hash, now, outcome)                    // routes the outcome back into state
```

`attempt_exit` itself, per message:

1. Read the settlement `root` and `exit_config` accounts. `root.head_final_batch` is the newest Final
   batch; `exit_config.exit_portal` is the ONLY source of the portal address this crate ever proves
   against — never the log, never a caller argument (the message could name any address; the on-chain
   `ProveExit` itself binds the portal from `exit_config`, and this crate mirrors that exactly).
2. **Read the `exit_nullifier` page for this message's `nonce >> 13` and check its bit** (an absent page
   account means the bit is clear — no exit on that page has ever been proved). A SET bit returns
   `Outcome::AlreadyProved` right here, before `read_pending`, `eth_getProof`, or the `Sender` are ever
   touched — this is what makes a restart-from-genesis re-scan of every historical `ExitInitiated` log
   cost zero fees for anything already done.
3. **Cap pre-checks, still before any fetch or the `Sender`**: (a) this message's own `cap_units(amount)`
   against the WHOLE `root.exit_cap_per_window` — a message too big for the cap under ANY window refuses
   `ExceedsWindowCap` right here; (b) a `read_exit_window(window_index)` read (absent account = `0`
   spent) — if this window has already spent enough that the message would overflow it, queue to the
   next window (`Outcome::QueuedForWindow`) without ever building or sending anything. The on-chain
   `ExitCapExceeded` (68) classification (below) still catches the race where two followers, or two polls
   of one, both pass this read before either sends.
4. Read that batch's `pending` account for its `state_root` and `last_block`.
5. `eth_getProof(exit_config.exit_portal, [message.storage_slot()], pending.last_block)` against the L2
   verifier node.
6. Measure the projected signed V1 `ProveExit` transaction's size (a throwaway keypair — no real key
   material needed, since every `Pubkey`/`Signature` is a fixed byte length regardless of value).
7. Verify the proof LOCALLY: `rome_zk_mpt::verify_account(pending.state_root, exit_config.exit_portal,
   proof.account_nodes)` → `storage_root` → `verify_storage(storage_root, slot, proof.storage_nodes) ==
   Present(1)`. This is the exact check `programs/zk-settlement::exit::prove_exit` performs on chain —
   run here, for free, before any fee is ever spent on a proof that would fail.
8. Build `ProveExit` and send it through `rome-zk-solana-sender::Sender` (V1), then classify the result.

Every step above except the last refuses (or returns `AlreadyProved`/`QueuedForWindow`) locally before
the `Sender` is ever touched — the order matches the on-chain instruction's own cheapest-first checks
(bounds before hashing). **A read failure (`Err(CoreError::{Read,Verifier,Build})`, never reaching the
`Sender`) is distinct from a send failure**: the follower treats it as free (see below), never counting
it toward the same budget as an actual `SendFailed` attempt.

## The follower's retry/stuck state machine

`follower::Follower` keeps two maps, keyed by message hash: `pending` (not yet done) and `stuck`
(terminal, but not abandoned). `follower::Follower::apply` matches every `Refusal` variant BY NAME (no
catch-all `_` arm — a newly-added refusal fails to compile here until it is deliberately classified) and
routes every `attempt_exit` outcome:

| outcome | routing |
|---|---|
| `Sent { window_index, units }` / `AlreadyProved` | done — removed from both maps. `Sent` also adds `units` to `Follower::sent_units[window_index]` (pruned to the current and previous window): our OWN confirmed sends, which the `finalized` settlement reader cannot see until the cluster has finalized them (the confirmed-to-finalized gap: 0-1 slot on devnet, about 31 on a TowerBFT cluster); `run::poll_once` passes `sent_units_in(window)` into `AttemptParams::local_spent_units` and the pre-send window check takes `max(chain, local)` |
| `QueuedForWindow { retry_slot, .. }` | `pending`, `Wait::Slot(retry_slot)` — due again once the current slot reaches it. `Pending.window_requeues` counts these; past `max_window_requeues` (default `3`), `stuck`/`MaxWindowRequeues{seen_head_final}` — a bound that ALARMS (`rome_zk_exit_stuck{reason="max_window_requeues"}`), never abandons: re-tried, fee-free, once `head_final_batch` advances |
| `Refused(ProofTooLarge { .. })` | straight to `stuck` — only a staged-proof path can truly fix this, but it is re-tried the moment `head_final_batch` advances (a later/shrunk trie may fit), never on every poll in between |
| `Refused(ExceedsWindowCap { .. })` | straight to `stuck`/`ExceedsWindowCap` — no window under the CURRENT cap could ever admit this message; re-tried once `head_final_batch` advances, since the cap is governance-changeable |
| `Refused(Verify(RootMismatch \| ProofInvalid(_)))` | straight to `stuck`/`ProofInvalid` — a failed proof verify against the CURRENT root is worth surfacing on its own; re-tried once `head_final_batch` advances |
| `Refused(Verify(ExitNotSent))` | `pending`, `Wait::NextFinal{seen_head_final}` — TRANSIENT: the exclusion proof every fresh exit gets at a Final root older than the message is the ORDINARY first outcome (the `Verify(Absent)` case), never "stuck" |
| `Refused(UnsupportedAsset)` | `stuck`/`UnsupportedAsset`, TERMINAL — v1 is native-asset only (the on-chain gate's local mirror); nothing on chain can ever change this message |
| `Refused(StateUnavailableAtRoot \| NoFinalBatch \| ExitConfigUnset \| ExitCapUnset \| ChallengeWindowZero)` | `pending`, `Wait::NextFinal{seen_head_final}` — TRANSIENT, due again once a LATER Final root exists (a message initiated after the current Final batch's own last block is legitimately unavailable there — not a bug) |
| `SendFailed(_)` | `send_attempts += 1`, stays `pending`/`Wait::Now` until `max_send_attempts` (default `5`), then `stuck`/`MaxSendAttempts` (a fee may have been spent already; bounded so a persistent failure does not retry forever) |
| `Err(_)` (`CoreError::Read`/`Verifier`/`Build` — the `Sender` was NEVER touched) | stays `pending`/`Wait::Now`, `send_attempts` UNTOUCHED — a read failure is free, never conflated with an actual send attempt; counted separately on `rome_zk_exit_read_errors_total{kind}` |

An undecodable log is counted (`IngestReport::decode_errors`) and skipped — the loop survives a
malformed or foreign log, never aborts on the first one. A message seen across two overlapping polls
(`ingest` called twice with an overlapping `fromBlock` range) is deduped by hash and attempted once.
`run::poll_once`'s own three RPC calls fail the same way: an `eth_getLogs` error counts
`rome_zk_exit_rpc_errors_total{method="eth_getLogs"}` and skips the poll; a `read_slot` (get_slot) error
counts the same counter with `method="get_slot"` and skips the whole attempt round — never falls back to
slot `0`, which would send a `ProveExit` tagged for the WRONG challenge window at a real fee; a `read_root`
error does the same with `method="read_root"` — never a fabricated `head_final_batch = 0`, which would
make every message parked this poll due again on the next one. A message that returns from `stuck` keeps
its `send_attempts`/`window_requeues` — the budget is never reset per Final root.

## The named refusals (never touch the `Sender`)

Three pre-MPT gates mirror `programs/zk-settlement::exit::prove_exit`'s own root/config checks, in the
same order, before the batch is even read — none of them ever fetches a proof or touches the `Sender`:

- **`ExitConfigUnset`** — `exit_config.exit_portal` is unset (all-zero): exits are not configured for
  this chain yet.
- **`ExitCapUnset`** — `root.exit_cap_per_window == 0` (mirrors the on-chain error 64): `0`
  disables exits fail-closed. Governance can activate `exit_portal` alone (an independent `pending_mask`
  bit from the cap), so `(exit_portal set, exit_cap_per_window == 0)` is reachable on any chain — this
  gate is what stops that window from producing a doomed `ProveExit` send.
- **`ChallengeWindowZero`** — `root.challenge_window_slots == 0`: cannot compute a window index.
- **`NoFinalBatch`** — `root.head_final_batch == 0`: nothing is Final yet.

Then, right after the pre-send nullifier check and still before `read_pending`/`eth_getProof`, two cap
pre-checks:

- **`ExceedsWindowCap { units, cap }`** — this message's own `cap_units(amount)` exceeds the WHOLE
  `root.exit_cap_per_window` — no window under the current cap could ever admit it. Distinct from
  `QueuedForWindow` below: this is "never, under this cap", not "try the next window".
- (not a `Refusal` — a distinct `Outcome`) a spent-window read that would overflow the window this
  attempt targets returns `Outcome::QueuedForWindow` directly, the same outcome the on-chain
  `ExitCapExceeded` send classification produces (below) — queuing here is just cheaper, since no fetch or
  send ever happens first.

Once past those, three more refusals can fire after the batch's `pending` account and the `eth_getProof`
proof are read, still before the `Sender` is ever touched:

- **`StateUnavailableAtRoot { number }`** — the verifier node's `eth_getProof` refused because the
  requested block is outside its own `--rpc.eth-proof-window`. The message persists in L2 storage
  (portal storage is append-only); a later poll, once `root.head_final_batch` has advanced to a batch
  whose block the verifier still serves, retries and proves cleanly. This crate never retries against an
  OLDER batch — only the newest Final root is ever tried.
- **`ProofTooLarge { len, max }`** — the measured signed V1 transaction exceeds the configured byte
  budget (`--max-proof-bytes`, capped at the 4,096 B V1 envelope regardless of config). No staged-proof
  fallback exists in this crate — that is a later, contingent design (only built if measurement on a
  real, larger trie shows it is needed).
- **`Verify(RootMismatch)`** — the `eth_getProof` account proof does not hash to `pending.state_root`
  (the proof was fetched against the wrong root, or the state moved between the read and the fetch).
- **`Verify(ProofInvalid(_))`** — the account or storage proof failed to verify for any other reason,
  including a malformed proof-node hex string in the `eth_getProof` response (a torn/garbled verifier
  reply): decoding refuses cleanly here rather than panicking the follow loop.
- **`Verify(ExitNotSent)`** — the storage slot proves absent, or present with a value other than `1`: the
  message was never recorded as sent at this root.

## Send classification

A `ProveExit` send can come back:

- **Confirmed** → `Outcome::Sent`.
- **`ExitAlreadyProved` (65)** → `Outcome::AlreadyProved` — the nullifier bit is already set (by this
  process on an earlier run, or by any other prover); not a failure, advance past this message. This is
  the RACE classification — the bit was set by someone else BETWEEN this attempt's own pre-send read
  (above) and its send landing. The pre-send read is added, not a replacement: it catches the far more
  common case (a restart re-scanning history) cheaply, before ever building or sending anything; this
  classification stays for the much rarer in-flight race.
- **`ExitCapExceeded` (68)** → `Outcome::QueuedForWindow { next_index, retry_slot }` — the per-window cap
  refused this exit; re-queue to the next challenge window and retry there. **Never a
  halt.**
- Anything else → `Outcome::SendFailed(message)` — the caller's own retry/halt policy decides.

## What this crate does NOT do

- No staged proof path (`StageExitProof`) — a later, contingent design item.
- No REPLAY/CAP bookkeeping of its own — those are enforced ONLY on chain; this crate classifies what a
  send comes back with (`ExitAlreadyProved`/`ExitCapExceeded`), and separately reads the nullifier bit
  pre-send (above), but never decides either itself.
- No disk cursor or database — `follower::Follower`'s state is an in-process cache only, rebuilt from
  chain on every restart (a persisted cursor was the rejected alternative).
- No chain writes in its own test suite: every test runs against `fixtures/exit/*.json` and small,
  scripted, refusing fakes — never a live cluster or verifier node.

## Running it

```
rome-zk-exit-prover --config exit-prover.toml [--once]
```

`Config` (TOML, `deny_unknown_fields`): `chain_id`, `settlement_program_id`, `settlement_rpc` (Solana),
`verifier_rpc` (the L2 reth/derive node the portal lives on), `payer_key_path`, plus tuning knobs
(`poll_interval_ms`, `compute_unit_limit`, `loaded_accounts_data_size_limit`, `priority_fee_micro_lamports`,
`max_priority_fee_micro_lamports`, `confirm_timeout_secs` (default `15`), `confirm_commitment` (`finalized`, the default, or `confirmed`), `confirm_poll_interval_ms` (default `500`), `max_proof_bytes`, `metrics_addr`,
`portal_from_block` — default `0`; Tiber's own deployed portal starts at `208399`
(set in the operator's deploy config), `max_send_attempts` — default `5` — and `max_window_requeues` —
default `3`, bounding `Pending.window_requeues` the same way `max_send_attempts` bounds `send_attempts`).
The exit portal ADDRESS is never configured — it is read live from `exit_config` on every restart;
`portal_from_block` only bounds how far back a fresh follower's `eth_getLogs` scan starts.

The real end-to-end run against a live Tiber Final root needs the verifier's own
`--rpc.eth-proof-window` set wide enough to cover the gap between an exit's send block and the batch that
finalizes it — a devnet compose flag set at deployment (this crate's own `StateUnavailableAtRoot`
retry is the mechanism; setting the flag is deployment, not code).

## Metrics (`/metrics`, Prometheus text format)

`rome_zk_exit_prove_attempts_total{result}`, `rome_zk_exit_queued_total`, `rome_zk_exit_proof_bytes`
(histogram), `rome_zk_exit_latency_secs`, `rome_zk_exits_proved_total`, `rome_zk_exit_pending` (gauge —
`follower.pending.len()`), `rome_zk_exit_stuck{reason}` (gauge —
`max_send_attempts`/`proof_too_large`/`max_window_requeues`/`exceeds_window_cap`/`proof_invalid`/`unsupported_asset`),
`rome_zk_exit_scan_from_block` (gauge), `rome_zk_exit_log_decode_errors_total` (counter),
`rome_zk_exit_read_errors_total{kind}` (counter — `settlement`/`verifier`/`build`; `attempt_exit`
returning `Err(_)`, the `Sender` never touched), `rome_zk_exit_rpc_errors_total{method}` (counter —
`eth_getLogs`/`get_slot`/`read_root`; an RPC call `run::poll_once` itself made failing outright).

## The release tool

Releasing a proved exit from `programs/zk-bridge`'s vault (`ReleaseExit`) is a SEPARATE, operator-run
tool — [`zk-bridge-client`](../zk-bridge-client)'s `release-exit` example — not part of this crate's own
loop. See that crate's README.

## Testing

```
cargo test -p rome-zk-exit-prover
```

Every test runs against `fixtures/exit/*.json` (the anvil scenario) and in-process fakes for
`SettlementReader`, `VerifierRpc` and `Sender` — no network, no cluster, no `build-sbf` (this is a plain
off-chain binary). `src/follower.rs`'s own `#[cfg(test)]` module drives the retry/stuck state machine
directly (no `attempt_exit`, hand-built outcomes); `tests/attempt_exit.rs` covers `attempt_exit`'s own
refusals and cap pre-checks; `tests/follower.rs` covers the two cases that need a real `attempt_exit` call
(the pre-send nullifier check, and the full restart-from-genesis wire loop); `tests/run.rs` covers
`run::poll_once`'s own two RPC-failure paths (an `eth_getLogs` error, a `get_slot` error) against fakes
that panic if the `Sender` (or any further settlement read) is ever reached.
