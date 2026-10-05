# rome-zk-prover

The always-on proving core. Proves every finalized inbox batch, in order, on one GPU machine, and hands the
finished proof off to whatever posts it to the settlement program.

**Built so far:** config with a fail-closed TOML loader and the vkey-of-record loader, the `Prover`
trait and its `cargo-zisk` subprocess implementation (process-group timeout kill, a bounded output
drain, an output-file existence check), a Rust port of the ZisK PLONK calldata decoder with a
structural (compile-time) gate against the vkey of record, the one-`getMultipleAccounts`-call chain
anchor (carrying the settlement program's own account + `ProgramData` and `global_config`, and every
loaded account's live length) with its named refusals, the poster (chain/continuity/local-verify
guards, `PublicValues` derived from the checked proof itself, a live loaded-accounts preflight that
refuses a too-low configured limit, refusal classification of a failed send —
`AlreadyPosted`/`HeadAhead`/`StaleAnchor`/`PayerLow`/named — via the sender crate's own
`sender_error_transaction_error`), the finalize sweep (a best-effort stop at a non-`Final` successor)
and a status-checked retention-gated close — and now **the follower loop**: an always-on state
machine (`follower` module) that proves every finalized inbox batch in order over `anchor`/`poster`/
`finality`'s own steps, idles with zero `Prover::prove` calls while nothing is behind, resumes safely
across a restart via a preemption-safe work directory keyed on the ELF's own sha256, and reports
Prometheus metrics (`metrics` module) on `/metrics`. **History** (`store` module): every transition
is recorded through a `Store` seam into Postgres when `database_url` is configured (`NoopStore`
otherwise) — the chain stays the loop's only cursor; the store is never read back, and a store
failure never stops a batch from proving. The CLI (`bin/rome-zk-prover.rs`) runs either
`--once --batch N` (one job, the follower's own degenerate single-iteration case) or `--follow`
(forever, until SIGTERM/SIGINT or `--iterations N`); `--dry-run` never sends a real transaction in
either mode.

## Its own workspace

Like [`rome-zk-prover-input`](../rome-zk-prover-input), this crate is **excluded** from the rome-zk
root workspace (root `Cargo.toml`'s `exclude`), with its own `Cargo.lock`: it path-depends on
`rome-zk-prover-input` (for the follower's own input-building step), inheriting that crate's exact
`alloy-consensus`/`alloy-genesis`/`alloy-rpc-types-debug`/`alloy-eips` = 2.0.5 pins, which conflict
with `rome-zk-batcher`'s `alloy = "^2.4"` in one shared lockfile.

```sh
cd crates/rome-zk-prover
cargo test --locked
```

Never `-p rome-zk-prover` from the repo root (it is not a member there). Verified: one
`Cargo.lock` resolves and compiles `alloy` 2.0.5 (from `rome-zk-prover-input`), `solana-program` 4.1.0
/`solana-client` 4.3.0/`solana-sdk` 4.1.0 (the same versions the workspace pins, which
`zk-settlement-client`/`zk-inbox-client` build on), and the Solana crates `rome-zk-solana-sender`
sends transactions with (`solana-message` 4.6.0, `solana-transaction` 4.3.0, etc.) together — the
prior conflict is specifically prover-input's `alloy` pins vs the batcher's range, which never
applies here since this crate does not depend on the batcher.

## Config + vkey of record (`config` module)

`Config` is the TOML-rendered runtime configuration: chain ids, program ids, RPC URLs,
the ELF and vkey-json paths, the ZisK home, a persistent work directory, the payer key **path**
(never the key bytes — those are never read into this struct), and tuning knobs with the design's
own defaults (poll interval, compute-unit limit, priority fee, confirm timeout, prove attempts and
timeout, the finalize walk, metrics address). Chain facts (`max_drift_secs`, registry entries,
`head_*`, `proving_policy`, the real block range) are read live from chain — never
configured here.

`Config::load` reads the TOML file and refuses (`ConfigError::Parse`, naming the field) any key
the struct does not have (`#[serde(deny_unknown_fields)]`) — a typo'd or stale knob is a refusal to
start, never a silently-ignored setting. `max_prove_attempts` is read here but used by the follower
loop; this crate's own `Prover` implementation never retries.

`VkeyOfRecord::load` deserializes the vkey-of-record JSON fixture
(`fixtures/vkeys/<chain>-layout1.zisk-<release>.json` — the one file that owns `programVK`, `rootCVadcopFinal`,
`elf_sha256`, `chain_id`, `layout_id` and the ZisK release the guest was built and proved under; this module never
invents a literal for any of them) and refuses, by name, any `layout_id` other than 1 or a hex field that fails to
decode or is the wrong length (`BadHex`/`BadHexLen`), a record with no `zisk` field (`ZiskVersionMissing`), a release
the verifier table does not know (`ZiskVersionUnknown`) or has withdrawn (`ZiskVersionWithdrawn`), a
`rootCVadcopFinal` that is not the release's pinned recursion root (`RootCNotOfVersion`), and a `scheme` that is not
the release's own. `Config::load_vkey_of_record` additionally refuses
(`VkeyJsonMismatch`) a vkey json whose `chain_id` disagrees with the running config, and
(`ElfMismatch`) an ELF file whose sha256 disagrees with the vkey json's `elf_sha256`. A registry
rotation the operator has not yet pointed this config at is a refusal to start, not a silent proof of
the wrong chain or the wrong ELF.

## The `Prover` trait and `LocalCargoZisk` (`prover` module)

```rust
pub trait Prover {
    fn prove(&self, elf: &Path, input_bin: &Path, out_file: &Path) -> Result<ProofFile, ProveError>;
}
```

`LocalCargoZisk` runs `<zisk_home>/bin/cargo-zisk prove -e <elf> -i <input> --plonk -y [-g] -o <out_file>`
as a subprocess, spawned as the leader of its own new process group, with `ZISK_HOME=<zisk_home>` in its environment so
the binary, the proving key and the cache come from one install. Written against `cargo-zisk` 1.3.1-alpha: `-o <path>`
produces the proof at that exact file path (not a directory); `--plonk` runs the STARK step and the SNARK wrap in one
invocation.

Release. Before every proof the `--version` line is read, and the release in it has to be the one the vkey of record
names, or the prover refuses by name (`ZiskVersionMismatch`) before proving anything. At start-up the binary also checks
that the install's proving key is that release's key set: the root of `provingKey/zisk/vadcop_final/vadcop_final.verkey.json`
has to be the release's pinned recursion root (`ProvingKeyMismatch`). The local check of each finished proof, and the
post, use the release the chain's active registry entry for the programVK names, the same release whose key the program
checks under (`anchor` refuses an entry whose release is not the record's, `ReleaseNotOfRecord`).

GPU. The GPU build of `cargo-zisk` has a runtime `-g` flag on `prove`, and without it that build proves on the CPU.
The CPU-only build has no such flag at all. The two builds tell themselves apart in the `--version` line, `[gpu]` or
`[cpu]`. So `gpu` in the config is a required setting with no default: `gpu = true` passes `-g` and makes the prover
refuse to start, by name (`GpuBuildRequired`), when the `cargo-zisk` it finds is not the GPU build (the binary checks at
start-up, `prove()` checks again before every run); `gpu = false` proves on the CPU on purpose. A config that leaves
`gpu` out is refused, so no node ends up on the CPU by accident. Per-stage walls
(`GENERATING_WRAPPER_SNARK_PROOF`'s duration, the STARK proof's own wall and step count, whether a
"verified" line was ever printed) are parsed straight from the subprocess's own stdout/stderr -
never invented when a line does not appear.

A timeout kills the WHOLE process group (`killpg`), not only the direct child pid — a tool that
forks a helper of its own (even something as simple as backgrounding a second process) would
otherwise leave that helper running after `prove()` returns. Draining the subprocess's stdout/stderr
is itself bounded (a channel with `recv_timeout`, not an unconditional thread join): a helper that
somehow escapes the process group entirely (its own session, via `setsid`) can still delay `prove()`
by holding a pipe open, but can never hang it — the call always returns within roughly `1.5x` the
configured timeout.

Refuses by name: `ProveFailed` (a non-zero exit), `ProveTimeout` (no exit within the configured
timeout — the process group is killed), `NotVerified` (a clean exit that never printed a verified
line), `OutputMissing` (a clean, verified exit that never actually wrote the output file, or wrote
it empty), `Spawn` (the binary itself could not be started, including a `zisk_home` with nothing
installed at `bin/cargo-zisk`). Exercised entirely against a fake `cargo-zisk` binary under
[`tests/fake-cargo-zisk/`](tests/fake-cargo-zisk) — no real ZisK toolchain is touched by the crate's
own test suite. Each fake script is installed once for the whole test binary (a shared, lazily-built
table of pre-installed homes), not once per test — `prove()` itself makes exactly one spawn attempt.

## The calldata decoder (`calldata` module)

A Rust port of the earlier Python decoder (`zisk_calldata.py`): a hand-rolled bincode-2 (varint) decoder for the
`Proof` file `cargo-zisk prove --plonk` writes, producing the four ABI fields
`zk_settlement_client::layout1_proof_abi` assembles (`program_vk`, `root_c`, the 512-byte public
values, the 768-byte proof bytes) plus the derived `public_signal` — computed via
`veritas::zisk_public_signal`, never reimplemented here, so the reduction mod the BN254
scalar field is the exact one the on-chain verifier runs.

Decodes [`fixtures/s10/block14.zisk.bin`](../../fixtures/s10/block14.zisk.bin) byte-for-byte against the already-committed
[`fixtures/s10/block14.calldata.json`](../../fixtures/s10/block14.calldata.json), and the ABI it
assembles verifies on the host via `veritas::verify_zisk`. Refuses by name: `NotPlonk` (the
`Proof` enum's discriminant was not 1), `TrailingOrShort` (the file ran out of bytes mid-decode, or
had bytes left over after the last field), `ProgramVkMismatch` (the packaged public-output vkey word
disagreed with the proof's own `program_vk` field), `Malformed` (a wrong `proof_bytes` length or an
unexpected word count once the flag is stripped, or one of the two varint tag bytes bincode 2
reserves for a u128 tail and this wire never uses), `BadVadcopFlag` (the leading vadcop_final flag
word, when present, was not `1` — stricter than the Python reference decoder, which strips whatever
value is there; no soundness impact, since this file never reaches the on-chain verifier).

`check_against_record(&Calldata, &VkeyOfRecord) -> Result<RecordChecked, CalldataError>` compares a
decoded proof's own `program_vk` and `root_c` against the vkey of record and, only on success, wraps
the calldata as a `RecordChecked` — the ONLY type `abi::layout1_from` accepts. A raw `Calldata` cannot reach the ABI-building path at all; the crate's own
`compile_fail` doctest on `abi::layout1_from` pins this as a compile-time property, not a convention
a caller has to remember. A proof made against a different (unregistered) ELF is refused by name
(`ProgramVkNotOfRecord`) rather than silently posted; a `root_c` disagreement is `RootCNotOfRecord`.
Comparing the vkey of record itself against the registry's ACTIVE entry (is it still registered,
not retired, already activated) is the anchor module's `VkeyNotActive` refusal, below.

## Building `PostRootFields` (`publics` module)

`to_post_root_fields(pv: &PublicValues, anchor: Anchor) -> PostRootFields` builds a
`PostRootProved` send's arguments from a decoded guest `PublicValues` (via
`rome_zk_layouts::public_values::unpack_zisk_outputs` + `read` on the calldata decoder's public
values) and the poster's own chain anchor (`batch`, `prev_batch`, `pre_state_root`) — this small
`publics::Anchor` (not to be confused with `anchor::Anchor`, the chain snapshot below) is just the
three facts the chain — never the proof — supplies. `block_roots_merkle` is always sent as zero (a
poster claim the proof does not bind).

Tested against the first real proof of the reset chain's batch 1, committed as
`fixtures/prover-input/txv1-dev-reset6-batch-1.{json,bin}` and its ZisK 1.3.1 proof
`txv1-dev-reset6-batch-1.zisk-1.3.1.plonk.bin`: that real PLONK proof decodes to the registered vkey of record
(`programVK 0x77c143cf…`), the four fields it decodes to are the ones ZisK's own export
(`…zisk-1.3.1.calldata.json`) holds for it, and its committed public values cover
batch 1, blocks 1..=60, matching the sidecar's own recorded parent hash, state root, and both
commitments.

## The chain anchor (`anchor` module)

`anchor(fetch, settlement_program, inbox_program, chain_id, candidate_batch, vkey) -> Result<Anchor,
AnchorError>` is ONE `getMultipleAccounts` call (via the `SnapshotFetch` trait — a real
`RpcClient::get_multiple_accounts_with_commitment` at FINALIZED for the CLI, a counting fake in this
crate's own tests) for `[root, registry, chain_config, batch_cursor, predecessor pending,
candidate_batch's inbox batch, global_config, the settlement program's own account, its
ProgramData]` — nine accounts, every fact the poster's next decision (and the loaded-accounts
preflight below) depends on, at one consistent slot (the prover's own read client, never
the sender's CONFIRMED one). `candidate_batch` is supplied by the caller (`root.head_pending_batch +
1` from a prior anchor in a follower loop, or `--batch N` on the CLI) rather than rediscovered, so
the predecessor's PDA is known up front and the whole snapshot fits one call.

The settlement program's own account is decoded (not just fetched): a loader-v3 "Program" record is
always exactly 36 bytes (a 4-byte little-endian enum tag of `2` plus the 32-byte `ProgramData`
address), and its embedded `ProgramData` address must agree with the PDA this crate derives via
`bpf_loader_upgradeable::get_program_data_address` — `SettlementProgramNotUpgradeable`/
`SettlementProgramDataAddressMismatch` refuse otherwise. `Anchor::account_data_lens` then carries
every one of those nine accounts' own live data length, in `post_root_proved_ix`'s own account order
(the not-yet-created candidate pending PDA and the predecessor at the genesis sentinel are `0`;
`system_program`'s own live length is read from the same snapshot — it is a real 21-byte account on
the cluster, not empty; the fee payer and treasury are assumed `0` — plain system wallets on every
chain so far. A treasury that holds account data is legal for the program and would be under-counted;
the ≥ 11 KB of slack in the 32 KiB page rounding is what absorbs that today — a bound, not a
guarantee) — the input
to the loaded-accounts preflight below. `Anchor::global_config` (treasury, registry authority) is
read here too, so the CLI needs no second round trip for the treasury address any more.

Refuses by name: `HeadAhead` (`candidate_batch != head_pending_batch + 1` — the chain
head moved, re-anchor), `VkeyNotActive` (no registry entry `(BN254, PLONK, vkey.program_vk)` under
layout 1 with `activation_slot <= slot` and not retired — covers a never-registered vkey, a retired
one, and one that activates in the future, each with its own reason string), `InboxNotFinalizedYet`
(the batch PDA exists but is not yet finalized, or is absent while the cursor has not yet passed it —
retry either way), `AbandonedInboxBatch` (the batch PDA is absent AND `cursor.next_batch >
candidate_batch` in the SAME snapshot — someone ran `AbandonBatch` for it (the batcher never does), this id
will never be finalized; alarm and stop, never a chain write), and the
settlement-program/`ProgramData`/`global_config` missing/mismatch refusals above.

## The poster (`poster` module)

`derive_public_values(&RecordChecked) -> Result<PublicValues, PreSendRefusal>` decodes the checked
proof's own packaged public outputs (`unpack_zisk_outputs` + `read`) — the ONLY source of
`PublicValues` this crate ever builds a send from; `PostParams` carries no separately-supplied `pv`
of its own, so a caller cannot post a proof against values that disagree with what the proof itself
packaged.

`build_post_ix(&PostParams) -> Result<Instruction, PreSendRefusal>` derives `pv` this way, then
refuses BEFORE any network call: `ChainMismatch` (the proof's own `chain_id` disagrees with this
anchor's chain), **`InboxCommitmentMismatch`/`OpenTsMismatch`** (the proof's own packaged
`inbox_commitment`/`open_unix_ts` disagree with the candidate batch's real inbox account — the local
mirror of `settle.rs`'s own `AccMismatch`/`OpenTsMismatch`, so a stale artefact carried over a chain
reset, or posted by hand against the wrong chain, is refused at zero fee rather than sent and refused
on chain), `ContinuityMismatch` (the proof's own `first_number` disagrees with the predecessor's
`last_block + 1`), then `LocalVerifyFailed` via `verify_checked(&RecordChecked, &VkeyOfRecord)` — the
one shared pairing check (`veritas::verify_zisk` on the assembled ABI, plus the ABI's own
embedded `program_vk` bytes agreeing with the vkey of record) both this function and the follower's
own resume gate (`follower::try_resume`) call, never a duplicated implementation. Only on success does
`build_post_ix` build the real `post_root_proved_ix`.

`preflight_loaded_accounts_data_size_limit(&[usize]) -> u32` rounds
`rome_zk_solana_sender::required_loaded_accounts_bytes` (the shared SIMD-0186 `Σ(64 + len)` formula)
up to the next 32 KiB page, from the anchor's own `account_data_lens`.
`resolve_loaded_accounts_data_size_limit(configured, &account_data_lens) -> Result<u32,
LoadedAccountsLimitTooLow>` is what the CLI actually calls, for BOTH the dry-run header and the real
send: `configured == None` derives (this is the normal case); a configured value below the derived
requirement is refused by name (`LoadedAccountsLimitTooLow { configured, required }`) — once right after
the first anchor, before any input is built or a proof is run, and again against the fresh anchor before
the send, since live lengths can grow — rather than silently raised or sent anyway; a configured value
at or above it is used as given. The 256 KiB literal this replaces no longer appears on the production
path (two test rigs still pin it as the old, too-small value).

`classify_send_failure(custom_code, insufficient_funds, head_pending_batch_after_reanchor, our_batch)
-> PostRefusal` names a failed send by the on-chain program's own error: `AlreadyPosted`
(`BadBatchSequence`, and a re-anchor shows the head EXACTLY at our batch — this batch is already
posted), `HeadAhead` (`BadBatchSequence`, and the head has moved STRICTLY PAST our batch — someone
else's batch landed ahead of ours; re-anchor and pick a fresh candidate), `StaleAnchor`
(`BadBatchSequence`, and the re-anchor STILL shows the head behind our batch — the re-anchor's own
FINALIZED read is stale relative to whatever rejected our send; retry the re-anchor), `PayerLow` (a
transaction-level, not program, failure), else `PostFailed { name }` from a table pinning every
`zk_settlement::errors::SettleError` discriminant to its name (this crate does not depend on the
program crate itself, so the table is the one place the mapping is asserted — and tested against real
on-chain `StateRootMismatch`/`GasInBatchMismatch`/`BadBatchSequence` failures, not just by hand).

Real BPF (`rome-zk-testkit`, `prefer_bpf`, the settlement program loaded via the upgradeable loader
so `anchor()`'s own Program/`ProgramData` reads see a genuine account pair): the committed gate
fixture's real proof posts the reset chain's batch 1 end to end — born `Final` immediately
(`head_final_batch` advances in the same call), `root.number == 60`, `root.block_hash ==
pv.last_block_hash`; CU measured ≈ 572–576k (same ballpark as the earlier 575,461 CU figure); the signed
V1 transaction is ≤ 4,096 B; the derived loaded-accounts limit covers the real, live `ProgramData`
length (well past the old 262,144-byte default). Adversarial, also real BPF: a tampered
`state_root`/`gas_in_batch` is refused on chain and classified by name; a second post of the same
batch classifies `AlreadyPosted`; a predecessor `last_block` off by one, a `chain_id` mismatch, a
tampered inbox batch `acc`/`open_unix_ts`, and a flipped proof byte all refuse before any network call
is attempted (a counting fake `Sender` proves zero calls).

## Finalize sweep + retention (`finality` module)

`plan_finalize(head_pending_batch, head_final_batch, finalize_walk) -> Option<FinalizePlan>`: while
behind, the next in-order batch plus every already-`Final` successor to walk past in the SAME
`FinalizeBatch` call, capped by `finalize_walk` (`MAX_FINALITY_WALK` on chain) and by
`head_pending_batch`. Needed whenever a post landed with `advances_head == false` (an out-of-order
proved batch, or a skip). The walk is best-effort: if it meets a batch that is
not (yet) `Final` — a challenge-window post still `Pending`, never an outcome the proved path alone
produces — the transaction still succeeds and `head_final_batch` stops at the last genuinely `Final`
successor, rather than the whole call failing.

`should_close(head_pending_batch, batch, status, close_pending_after_batches)` is the retention gate:
`None` (Tiber's default) never closes anything, so `RootView` can read any batch's
history forever; `Some(k)` proposes closing a batch once it is BOTH strictly behind
`head_pending_batch - k` AND its own `status` (read from the same snapshot the caller already has,
never assumed) is `STATUS_FINAL` — a `Pending` batch is never proposed for close, since the program
would refuse it (`NotFinal`) and this gate stops that doomed send before it is ever built.

Real BPF: a sweep over three out-of-order-`Final` pending PDAs advances `head_final_batch` from 0 to
3 in one `FinalizeBatch(1, walk=[2,3])` call (~12.5–17k CU — a pure head-advance walk, no pairing);
a walk that instead meets a batch still `Pending` still succeeds, stopping `head_final_batch` at the
last `Final` one; `RootView(1)` still succeeds with no retention gate; with `Some(1)`, only batch 1
is closed and `RootView(1)` is then refused (the PDA no longer exists), `RootView(2)` still succeeds.

## The follower loop (`follower` module)

The state machine: `Queued` (anchor taken; `HeadAhead` here means an exact restart-after-
`Relayed` re-discovery, classified `AlreadyPosted`/`Superseded` rather than an error) → `InputBuilt`
→ `Proving` → `Decoded` → `LocallyVerified` → `Posting` → `Relayed` → `Finalized`; terminal `Idle`
(`batches_behind == 0` — `cursor_next_batch - 1 - head_pending_batch` — zero `Prover::prove` calls)
and `Superseded` (a re-anchor after proving shows the chain moved PAST our candidate); halting
`AbandonedInboxBatch` (no skip instruction exists — alarm and stop),
`VerifierBehindAlarm` (a `VerifierBehind` streak that never clears after
`verifier_behind_alarm_polls` consecutive retries), `StaleAnchorAlarm` (a `StaleAnchor` retry streak
that never clears after `stale_anchor_alarm_polls` consecutive retries), and
`ProveAttemptsExhausted` (every prove attempt for a batch failed `max_prove_attempts` times running),
and `FetchAlarm` (a `TransientFetchError` streak that never clears after `fetch_alarm_polls`
consecutive retries — a transport failure this persistent is no longer "about to recover").
A post refused `StaleAnchor` — the FINALIZED read genuinely lagging a send that already landed — is
`Outcome::Retry` for the SAME candidate, never a halt; it does NOT resend on the next retry either,
unless a fresh re-anchor shows the head has actually moved (`RunConfig::stale_send_pending`
remembers the candidate) — a FINALIZED read can lag several poll intervals behind a send that already
confirmed, and resending into that lag pays a real fee for a transaction that may already have landed.

`run_one(fetch, prover, sender, verifier, cfg, candidate_batch, metrics) -> Result<Outcome,
FollowerError>` runs the whole pipeline once — `--once`'s entire job, and the follower loop's own
per-iteration call. It splits into two phases: `prepare_checked_proof` (steps 1–5 — anchor,
resume-or-rebuild, prove, decode, record-check) and `post_checked` (steps 6–8 — re-anchor, build +
send, finalize sweep, retention-gated close), so a caller (the `--dry-run` live gate) can stop after
the first phase without ever sending. `run(deps, cfg, until, stop, metrics, sleep)` is the loop
itself: each iteration tries `run_one` against its own best guess at the next candidate batch
(self-correcting from `HeadAhead`/`InboxNotFinalizedYet`, so a fresh call converges within a few
iterations rather than needing external bookkeeping), sleeping on `Idle`/`Retry`, stopping when
`stop.should_stop()` (checked once per iteration, never mid-post) or `RunUntil::Iterations(N)` is
reached.

**Artefact binding + a resume gate that re-proves, never halts:** every job's artefacts live under
`work_dir/<batch>/{input.bin, sidecar.json, proof}`, and `sidecar.json` is the FULL
`rome-zk-prover-input::build::Sidecar` (expected publics + provenance), written once, from the
DA-derived expectation, BEFORE `Prover::prove` ever runs — a kill mid-prove leaves a sidecar plus a
partial-or-absent proof file, which the checks below simply fail to resume, wiping and rebuilding.
Entering a job, `try_resume` resumes at `Decoded` (no `fetch_and_verify_batch`/`build_batch_input`/
`Prover::prove` at all) only when EVERY one of these holds: `input.bin` and `proof` both exist as
real files; the sidecar decodes; its own `provenance.elf_sha256` equals the vkey of record's; its own
`chain_id`/`inbox_commitment`/`open_unix_ts` equal the FRESH anchor's own inbox batch (never the ELF
sha alone — a chain reset that keeps the same `work_dir`, or a hand `--once` against another chain,
would satisfy that while still being a stale artefact); the resumed `proof` itself decodes, checks
against the record, and reproduces the sidecar's own recorded publics; AND it passes the real BN254
pairing (`poster::verify_checked`, the SAME check `build_post_ix` runs before every send) — a proof
whose bytes were corrupted after the fact can decode clean and match the record's vkey words while no
longer satisfying the pairing. ANY failure wipes the whole directory and proves again — bounded by
`Config::max_prove_attempts` (each attempt gets a freshly wiped directory) — never halts on a corrupt
or cryptographically-broken cache; only a run that exhausts every attempt halts, with
`ProveAttemptsExhausted { attempts }`. A restart after `Relayed` needs no special case: the next
`anchor()` for that same batch id sees `HeadAhead` (the chain head already advanced) and
`prepare_checked_proof` returns `Outcome::AlreadyPosted` before the work directory is ever touched.
`poster::build_post_ix` mirrors this same binding one layer down, locally, before every send (see
"The poster" above) — `verify_checked` lives in `poster` and is the one place both callers run the
pairing, never a duplicated implementation.

`Deps<F, P, S, V>` bundles the four traits a real run needs: `F: SnapshotFetch +
rome_zk_prover_input::inbox::AccountFetch` (the FINALIZED chain/inbox reads — one real RPC client
implements both; every method on both traits returns a `Result` — a genuinely missing account is
`Ok(None)`, a transport failure is a distinct, named `FetchError`, never a panic and never conflated
with the account simply not existing). A transient read failure retries the same candidate rather
than halting wherever it can occur: the chunk/batch-account read (`fetch_and_verify_batch`), the
post-time re-anchor, and the finalize/close sweep (there, the job's own successful post still stands
— the sweep is simply deferred to the next one); a streak of these that never clears halts with
`FetchAlarm { polls }` after `Config::fetch_alarm_polls` (default 30) consecutive retries, the same
shape as the verifier-behind and stale-anchor alarms. A transport-level failure on the reth-verifier
read (the connection, a timeout, a torn response body) retries under the same bound; a JSON-RPC
error, a decode failure or a missing block is a fact about the data, not the wire, and halts by name. `P: Prover`, `S: Sender`,
`V: rome_zk_prover_input::verifier::VerifierFetch` (`head`/`header`/`block`/`witness` —
`RemoteVerifier` wraps the existing reth-verifier free functions unchanged for production; the
follower's own tests drive the whole input-building step against a fake implementation instead, never
a live reth node). Every follower test is fake-driven — in-order batches 1..5 (the fake sender's own recorded
`PostRootFields.batch` sequence, every batch bound to the SAME canned proof's own committed inbox
commitment), an abandoned id halting after the ones before it, idle with zero prove calls
(mutation-tested: dropping the idle check turns it red; the idle path makes exactly one snapshot read
per iteration), `HeadAhead` between verify and post classifying `Superseded` with zero sends for that
job, `VerifierBehind` retried then proceeding once the fake head advances (and alarming after the
configured consecutive-poll count), the `batches_behind` gauge matching
`cursor_next_batch - 1 - head_pending_batch` in a scripted scenario, the resume rule itself (a restart
between `LocallyVerified` and `Posting` proves once and posts once; a restart after `Relayed` posts no
second time; a corrupt resumed proof file is wiped and re-proved then posts once; a resumed proof that
decodes and checks clean but fails only the pairing is wiped and re-proved then posts once; a fresh
proof that always fails only the pairing exhausts `max_prove_attempts` with zero sends; a sidecar
bound to another chain's inbox commitment is wiped and re-proved; the FRESH anchor's own identity
changing under a cached proof — not merely a hand-edited sidecar file — is caught the same way;
`ProveAttemptsExhausted` after the configured bound — each mutation-tested), a `StaleAnchor` retry
converging to `AlreadyPosted` with zero further sends even across a streak of several consecutive
stale re-anchor reads (never resending into the lag — mutation-tested: dropping the memory of an
in-flight stale send lets it resend on every retry), a streak that never clears alarming after the
configured poll count, a transient fetch error on the chunk/batch-account read, the post-time
re-anchor, or a full snapshot read each retrying rather than halting or panicking (and a streak that
never clears alarming with `FetchAlarm`), a genuine failed send halting by name, `close_pending_after_
batches = Some(k)` closing the batch strictly behind the threshold end to end through the loop, the
payer-lamports gauge written every iteration, `batch_gas_used`/`lag_seconds` independently
distinguishable, and (with the default `keep_work_dirs = 0`) every posted batch's own `work_dir`
directory gone once the run finishes.

## Metrics (`metrics` module)

A `prometheus::Registry` with every metric the follower loop reports — `rome_zk_prover_batches_behind`,
`rome_zk_prover_head{kind}`, `rome_zk_prover_lag_seconds` (`now - the posted batch's own
open_unix_ts`, written at `Relayed` from the proof's own packaged public values — no extra fetch),
`rome_zk_prover_stage_wall_seconds{stage}` (a histogram over `input`/`stark`/`plonk`/`verify`/`post`/
`finalize`), `rome_zk_prover_batch_gas_used` (written at `Relayed`, same source),
`rome_zk_prover_batch_cost_usd` (`gpu_hourly_usd × (stark + plonk wall seconds) / 3600` — reporting
only, `gpu_hourly_usd` defaults to 0), `rome_zk_prover_posts_total{result}` (`relayed`/
`already_posted`/`superseded`/`stale_anchor_retry`/`failed`), `rome_zk_prover_prove_attempts_total`,
`rome_zk_prover_payer_lamports` (a plain `getBalance` on the fee payer, sampled once per loop
iteration regardless of outcome — reporting only, deliberately a separate read from the one FINALIZED
decision snapshot: a stale balance changes no refusal this crate makes), `rome_zk_prover_state{state}`
(exactly one state reads 1 at a time — set to `failed` on every halting `Err` the loop returns),
`rome_zk_prover_abandoned_batch_alarm`, `rome_zk_prover_stale_anchor_alarm`, and
`rome_zk_prover_fetch_alarm` — served on `/metrics` by `rome-zk-metrics-http`, the same `Registry` +
`TextEncoder` wiring `rome-zk-batcher`/`rome-zk-sequencer` already use. `metrics_addr` defaults to
`127.0.0.1:9003` (loopback only) — a devnet compose overrides this to `0.0.0.0` for its own scrape
sidecar to reach it.

## The CLI (`bin/rome-zk-prover.rs`)

```sh
rome-zk-prover --config prover.toml --once --batch N [--dry-run]
rome-zk-prover --config prover.toml --follow [--iterations N] [--dry-run]
```

`--once --batch N` calls `follower::run_one` directly and exits — the follower's own degenerate
single-iteration case. `--follow` runs `follower::run` until SIGTERM/SIGINT (graceful: the current
stage's send always finishes, never killed mid-post) or `--iterations N` iterations have run.
`--dry-run` means something different in each mode, and never sends a real transaction either way:
under `--once`, it drives the full pipeline for real — anchor, resume-or-rebuild, a genuine
`Prover::prove` call, decode, record-check — then stops after the local re-verify and prints the
built `PostRootFields`, the signed V1 tx's serialized size, and a read-only `simulateTransaction`
result (`sig_verify: false, replace_recent_blockhash: true`); under `--follow`, it is a cheaper,
**anchors-only** live gate (`dry_run_follow_loop`, a separate print loop from the real state
machine) — every iteration prints the anchor's own
`head_pending_batch`/`head_final_batch`/`cursor_next_batch`/`batches_behind` and the candidate batch
it would attempt next, self-correcting from `HeadAhead`/`InboxNotFinalizedYet` exactly like the real
loop's own bookkeeping, but never fetching an inbox batch, building input or calling `Prover::prove`
at all (`--follow --dry-run` requires `--iterations N`, since a read-only gate must be bounded). For
a full-pipeline dry run of a specific batch (including a real prove), use `--once --batch N
--dry-run` instead.

A failed send is classified, never just propagated: `sender_error_transaction_error` (in
`rome-zk-solana-sender`) extracts the on-chain `TransactionError` from whichever `SenderError`
variant carries it, mapped to `(custom_code, insufficient_funds)` and fed to
`classify_send_failure` against ONE fresh root re-read (not a full re-anchor — the failure is
already narrowed to a batch-sequence or funding question). `AlreadyPosted` counts as success; every
other refusal is a named halt.

The read client for anchoring/input-fetching is always FINALIZED and always distinct from the
sender's CONFIRMED client; the payer key is read from a file path
(`Config::payer_key_path`, never printed or logged) and used both as the authority
pubkey every instruction builder needs and, converted once at the sender boundary, as the
signing key `rome-zk-solana-sender` sends with. The treasury address comes from the
fresh anchor's own `global_config` — no second round trip. The `/metrics` HTTP responder is bound
before anything else runs (a bad or busy address is a named fatal error, never a silently dead
endpoint) and served on a spawned task for the whole run's lifetime, in every mode.

## History (`store` module — Postgres job history)

Every follower transition (`Queued → InputBuilt → Proving → Proved → Verified → Posted →
AlreadyPosted/Superseded → Finalized`, or `Failed`) is recorded through a `Store` seam — the
follower's own fifth dependency alongside `fetch`/`prover`/`sender`/`verifier`. The chain stays the
loop's ONLY cursor: `Store` has no `get`/`query` method at all, so nothing in the follower can read
history back even by accident, and a store failure is logged + counted
(`rome_zk_prover_store_errors_total`) but never stops a batch from proving.

**Two implementations.** `NoopStore` — `Config.database_url` unset, the default — drops every event
(one log line at startup, "prover history disabled"). `PgStore` — `database_url` set — writes to
Postgres via `sqlx`, applying `migrations/prover/0001_proof_jobs.sql` once at connect time. Connecting
or migrating failing at START is a named, fail-closed refusal (`HistoryUnreachable`): the operator
asked for history and the process refuses to start silently believing it has none. Once running,
every `record` failure is fail-open — proving is this crate's whole reason to exist, bookkeeping is
not allowed to interrupt it. The URL is never logged unredacted (`store::redact_url`,
`postgres://user:***@host/db`).

**Schema** (`migrations/prover/0001_proof_jobs.sql`). The design's generic name for this database is
`rome_zk_prover`; a deployment may name its own instance differently (its rendered `database_url` is the
source of truth):

- `proof_jobs` — one row per `(chain_id, batch, attempt)` (`UNIQUE`, the idempotency key an UPSERT
  targets: `ON CONFLICT ... DO UPDATE`, so replaying the same attempt's events twice leaves one row,
  byte-identical, and a genuinely new prove attempt for the same batch gets its own row rather than
  overwriting the failed attempt's own history). `status` is a text enum matching the transition
  names above (`queued, input_built, proving, proved, verified, posted, already_posted, superseded,
  failed, finalized`) and always reflects the most recent transition recorded. Every other column is
  filled progressively, one transition at a time (`NULL` until the transition that knows that fact
  runs): `first_block`/`last_block` (Queued), `input_bytes`/`wall_input_ms` (InputBuilt), `backend`
  (Proving), `program_vk`/`wall_stark_ms`/`wall_plonk_ms` (Proved), `wall_verify_ms` (Verified),
  `sig`/`wall_post_ms`/`gas_used`/`cost_usd` (Posted), `finalize_sig` (Finalized).
- `proofs` — one row per `proof_jobs.id` that reached `Verified`: the 768-byte ZisK PLONK proof
  (`proof_abi`) and the 512-byte packaged public values (`publics`) — never the larger 1,344-byte
  on-chain ABI, which is reconstructible from these two fields plus the vkey of record at read time.

**Why `Posted` can mean "already final."** `PostRootProved` writes its own pending PDA `Final`
immediately and advances `head_final_batch` in the SAME instruction whenever a batch posts in strict
order (`programs/zk-settlement/src/settle.rs`, "posted+proved (final immediately)") — the only path
this crate's product shape takes today (one proof per batch, no challenge window). So the common case
never sends a separate `FinalizeBatch` at all: the follower's own finalize sweep, run right after every
post, simply finds the batch already final and attributes the `Finalized` row to the post's own
signature. A `FinalizeBatch` is sent (and its own signature recorded) only in the rarer case where a
prior post did NOT advance the head in the same instruction.

**A restart can discover a batch is final with no local record of the signature.** If the process is
preempted between the on-chain send confirming and this crate's own finalize-sweep bookkeeping
completing, the very next `--follow` (or `--once`) run for that candidate anchors straight into
`AlreadyPosted`, on the chain's own FINALIZED read, before ever touching a work directory. When that same
read also shows `candidate_batch <= head_final_batch`, the batch IS final — the follower records
`Finalized` with `finalize_sig = NULL` rather than leaving the row stuck at `posted` forever: the chain is
the source of the fact, the signature that made it so is not locally known. This write is not tied to one
specific prove attempt (a restart may have no on-disk artefact for the candidate at all) — see the
attempt-0 sentinel below.

**Attempt `0` is a sentinel, never a real prove attempt.** The follower's own prove-attempt loop numbers
its attempts from 1. A job transition NOT tied to one specific attempt — the loop's own halting `Failed`
(a store failure never masks the real halting error; recorded by `run_one` itself, so `--once`'s direct
call gets it too, not only `--follow`'s loop) and the restart-onto-already-final `Finalized` above — is
recorded under `attempt = 0` instead. A read path must not treat `attempt DESC` as "most recent": a
batch's real attempts restart from 1 on every process restart (a resumed job's own attempt, from its
sidecar, is the exception — it continues the SAME attempt the artefact was produced under), so a stale
higher-numbered attempt from a PREVIOUS life can outrank a lower-numbered, genuinely later one. Order by
`updated_at DESC` instead (below) — the true recency signal regardless of attempt numbering or a `0`
sentinel.

**Row contract for the explorer API**: the queries a read-path service runs against this
schema — never write access, and no ORM abstraction over the shape below.

```sql
-- Latest job per batch (one row per batch id, the most recently updated row — never `attempt DESC`,
-- which a restart's attempt-numbering reset or a sentinel-attempt write can outrank incorrectly)
SELECT DISTINCT ON (chain_id, batch) *
FROM proof_jobs
WHERE chain_id = $1
ORDER BY chain_id, batch, updated_at DESC;

-- Proof bytes by batch (the winning attempt's own proof + publics)
SELECT p.proof_abi, p.publics
FROM proofs p
JOIN proof_jobs j ON j.id = p.job_id
WHERE j.chain_id = $1 AND j.batch = $2
ORDER BY j.attempt DESC
LIMIT 1;

-- Wall times per stage, across every job for a chain
SELECT batch, attempt, wall_input_ms, wall_stark_ms, wall_plonk_ms, wall_verify_ms, wall_post_ms
FROM proof_jobs
WHERE chain_id = $1
ORDER BY batch, attempt;
```

The deploy config already renders `database_url` (the deploy's own Postgres) — the CLI (`bin/rome-zk-prover.rs`) builds `PgStore`/`NoopStore` from
`Config.database_url` at start and hands it into `Deps`/`run_one` alongside the other four
dependencies; nothing in the deploy config changes for this.

## Not yet built

- Broadcasting a signed transaction anywhere other than a real (non-dry-run) `--once`/`--follow` run
  against a live cluster — the crate's own test suite never sends to a real cluster; every real-BPF
  test runs against `solana-program-test`, and every follower test runs against an in-memory fake
  chain.

## Deployment

This crate's binary is what the operator's deployed runtime image runs (`--config /data/prover.toml
--follow`). The deployment itself covers provisioning, config rendering, key management, and the operator
health check.

## Depends on

`rome-zk-prover-input` (lib), `zk-settlement-client`, `zk-inbox-client`, `rome-zk-layouts`,
`veritas` (`no-entrypoint`), `rome-zk-solana-sender`, `rome-zk-metrics-http`,
`prometheus` (the `metrics` module's registry), `sqlx` (`postgres`/`migrate`/`runtime-tokio-rustls`
— the `store` module's `PgStore`, against `migrations/prover/`), `nix` (`signal`/`process` features — `killpg` on the
timeout path); `solana-program`/`solana-client`/`solana-sdk` (every reader in
this workspace, the anchor module's own FINALIZED client) and, for the CLI's own payer key +
`--dry-run` `simulateTransaction` call,
`solana-keypair`/`solana-hash`/`solana-signer`/`solana-commitment-config`/`solana-client` (aliased
`solana-client-v1`) + `wincode`, matching `rome-zk-solana-sender`'s exact pins; `tokio`'s `signal`
feature for the CLI's own SIGTERM/SIGINT handling under `--follow`. Dev-only: `rome-zk-testkit`
(real-BPF poster/finality tests), `solana-program-test`, `solana-signature`, `tempfile`, and — for
the follower's own fake-driven test harness, which builds real channel/chunk bytes and a real
reth-verifier `Header`/`Block`/`ExecutionWitness` — `rome-zk-channel`, `rome-zk-merkle`,
`alloy-consensus`, `alloy-rpc-types-debug`, `reth-ethereum-primitives`, pinned to EXACTLY
`rome-zk-prover-input`'s own versions.
