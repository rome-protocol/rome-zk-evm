# rome-zk-solana-sender

The Solana RPC send/confirm machinery: builds and signs a **V1** (SIMD-0385) transaction, submits it, and
tracks it to confirmation — resubmitting with a fresh blockhash and a bumped (bounded) priority fee on
expiry. Ships only the RPC implementation (`RpcSender`); a TPU/QUIC client is a follow-up implementing the
same `Sender` trait.

## Why this crate exists

Before it did, this lived inside `rome-zk-batcher` (`src/sender.rs`) as the one piece of the batcher every
other Solana-writing service would eventually need again: the prover orchestrator's `PostRootProved`
poster, the challenger, and governance tooling all have to build, send and confirm V1 transactions the
same way, none of them needing the batcher's own channel codec, grouping, or chain-anchor resolution to do
it. Splitting it out gives that machinery one home instead of a second copy per consumer.

## What it owns

- **The V1-only transaction build** — every transaction is `VersionedMessage::V1`: a
  4,096-byte envelope with the CU limit and loaded-accounts-data-size limit carried in the message
  header's own config mask, never as `ComputeBudget` instructions (an unset V1 config field is zero,
  fail-closed, so `build_tx` sets both explicitly on every transaction).
- **The one crate-generation conversion boundary** (`compat` module) — `zk-inbox-client` and
  `zk-settlement-client` build their instructions with the `solana-program` facade crate on purpose
  (their output also feeds `solana-program-test` call sites unconverted); `compat::to_v1_instruction` (and
  the matching `Pubkey`/`Signature` conversions) is the single place a `solana_program::instruction::
  Instruction` becomes the V1-generation `solana_instruction::Instruction` a V1 message actually compiles
  from. Since the Agave crate bump both sides draw their
  `Pubkey`/`Instruction` from the same Agave 4.3.0-line `solana-pubkey`/`solana-instruction` crates, so this
  boundary is now a naming/ownership seam (which crate builds an instruction vs. which crate sends it),
  not a bridge between two incompatible Rust types the way it was before the bump — kept as a real conversion
  function rather than collapsed, since the two call sites still own genuinely different concerns.
- **`FramePlan`/`Stage`** — an explicit DAG (an ordered list of stages, each a list of mutually independent
  transactions) a caller builds and this crate's `RpcSender::send_and_confirm_many` drives concurrently,
  bounded by an in-flight count of outstanding transactions.
- **`RpcOps`** and **`RpcSender`** — the four network calls (`sendTransaction`, batched
  `getSignatureStatuses`, `getLatestBlockhash`, `getBlockHeight`) as a trait, implemented for real against
  `solana-client`'s V1-generation nonblocking RPC client, with retry (`with_retry`, exponential backoff)
  around every one of them.
- **`bumped_priority_fee`/`priority_lamports`** — the priority-fee doubling-with-cap schedule and the
  V1 priority-fee formula, `ceil(cu * price / 1e6)` lamports for a µL/CU price, independently tested.
- **Co-signers** — `TxSigner` is what signs a transaction: a `Keypair` alone, or `PayerAndCosigners`, a fee
  payer plus further required signers. `RpcSender::with_cosigners` adds keys that sign after the payer; a
  required signer missing from the set fails the build with `SenderError::MissingSigner` before anything is
  sent. Long-running services sign with the payer alone; the operator CLI uses co-signers, for a reserved
  registration for example.
- **Compute retry** — `Sender::send_and_confirm_many_retrying_compute` resends a transaction once, unchanged
  except for a higher compute-unit limit, when it failed on chain for running out of compute units
  (`is_compute_exceeded`, `sender_error_is_compute_exceeded`). A retry limit not above the base limit turns
  the retry off. `RpcSender` does the retry inside its batched confirm loop; the batcher's chunk lane uses it.
- **`build_v1_tx`** builds a V1 transaction exactly as a real send does, for a caller that only measures its
  wire size. **`required_loaded_accounts_bytes`** is the loaded-accounts size formula, `Σ(64 + len)` over every
  account a transaction loads. **`RPC_REQUEST_TIMEOUT`** (20 s) bounds each HTTP request to the RPC node.
- **`SendTuning`** — per-send tuning. `confirm_commitment` (`ConfirmCommitment::Finalized` by default, so
  "sent" means final; `Confirmed` is selectable) is the level a send must reach; `confirm_timeout` (default
  15 s, `DEFAULT_CONFIRM_TIMEOUT_SECS`) sets the overall ceiling (ten times it) after which a send stops resubmitting
  and starts no queued frame or later stage; it keeps waiting on a transaction that has landed or whose
  latest blockhash is still valid, and gives up only once none can still land, or once the cluster's block
  height has not advanced for twenty times it (a stall, not wall time: a slow cluster that keeps advancing
  never fails a send). A send that had landed when the stall ended it gets its own error
  (`LandedNotFinal` / `BatchLandedNotFinal`): it executed and may still finalize. It
  does not trigger a resubmit: a send is resubmitted only once its blockhash has expired, and never while an
  earlier signature of it is already `confirmed`;
  `status_poll_interval` (default 500 ms) paces the single-send path's status checks. `SendTuning::default()`
  carries these defaults; the compute and fee fields are always set by the caller.
- **`ConfirmedFrame`/`BatchSendOutcome`** — per-frame and whole-batch confirmation results (signature,
  latency, resubmit count) a caller reports or measures against.
- **`describe_rpc_error`** — redacts a request URL (which reqwest's own `Display` appends as
  `" for url (<URL>)"`, and which can carry a secret in its query string) out of an RPC client error before
  it ever reaches a log line. `SenderError::Rpc`'s own `message` field is computed through it at
  construction time; `rome-zk-batcher`'s own RPC-error-carrying error variants (`resolve.rs`, `preflight.rs`,
  `loaded_accounts.rs`, `pipeline.rs`'s CU-sample logging) use it the same way, since this crate is already
  a dependency of that one.

Its production code never depends on `rome-zk-batcher` or `rome-zk-channel` — `FramePlan`/`Stage` are built
from the `solana_program::instruction::Instruction` type `zk-inbox-client`/`zk-settlement-client` already
produce, not from `channel::Frame`. Its own test suite depends on both as dev-dependencies only (to build a real
chunk-lane plan through the batcher's own `pipeline::plan_chunk`, and to size a fixture frame body against
the real codec constants), the same accepted dev-only cycle already in use between `rome-zk-batcher` and
`rome-zk-derive`.

## Wire facts

- V1 envelope: 4,096 bytes. The design-max chunk-lane frame (`Open` + one `Write` of the whole 3,681-byte
  body + `Seal{len, body_hash}` + `SealLeaf`), signed as one transaction, is **4,084 bytes** — 12 bytes of
  headroom (`design_frame_v1_tx_fits_4096_and_carries_both_header_limits`, this crate's own test).
- `getSignatureStatuses`/`getMultipleAccounts` per-call caps (`MAX_SIGNATURE_STATUSES_PER_CALL`,
  `MAX_MULTIPLE_ACCOUNTS`) are protocol-level constants, identical across every Solana client crate
  generation this workspace has used — re-exported here so every caller chunks its own polling/scanning
  the same way.

## Consumers

- **`rome-zk-batcher`** re-exports this crate's whole public surface under its historical
  `rome_zk_batcher::sender` module path (`sender.rs` is now a one-line `pub use`), so no existing call site
  or test in that crate changed. `pipeline.rs` builds `FramePlan`s from grouped batches; `resolve.rs` and
  `config.rs` use the re-exported per-call caps and the `compat` conversions directly.
- **`rome-zk-prover`** (its `PostRootProved` poster), **`rome-zk-exit-prover`** (`ProveExit`) and
  **`rome-zk-ops`** (every operator command) use it directly. None of them touches the batcher's own
  channel, grouping or anchor logic to reach it. `programs/zk-settlement`'s tests use its `compat`
  conversions to measure real V1 transaction sizes.
