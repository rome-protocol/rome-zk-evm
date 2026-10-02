# rome-zk-executor-api

The `Executor` trait and its associated types — the boundary between the sequencer's admission, log and
sealing logic and whatever engine actually executes EVM transactions and builds blocks. This crate exists
purely because Cargo forbids a dependency cycle: [`rome-zk-executor-reth`](../rome-zk-executor-reth) (the
in-process execution engine) must depend on the trait it implements, and
[`rome-zk-sequencer`](../rome-zk-sequencer) must depend on both the trait and (optionally) the
implementation. Factoring the trait into its own crate lets both sides depend on it without depending on
each other. `rome-zk-sequencer` re-exports everything here as `rome_zk_sequencer::executor::*`, so from a
caller's point of view this is not a new public surface — it is the same seam, moved one level down.

## What it guarantees

- **The block environment is committed before execution.** `open_block(BlockEnv)` fixes a block's number,
  timestamp, gas limit, coinbase and `prevRandao` before that block's first sub-block executes. Every
  sub-block's execution and the block's own sealing both run under exactly that environment — an executor
  that lets any of them diverge (by still deriving its own timestamp at seal time, for instance) breaks
  the guarantee that a signed pre-confirmation can never turn out to disagree with the block it was sealed
  into.
- **`prev_randao` is deterministic and sequencer-predictable, never entropy.** It is
  `keccak256(chain_id ‖ number ‖ channel_id)`, where `channel_id` is derived from the chain id and the
  block's epoch id (`number / PREV_RANDAO_EPOCH_BLOCKS`) — pinned by a golden test (independently
  cross-checked against two separate keccak256 implementations) so a future change to the formula is
  deliberate, not silent. `PREV_RANDAO_EPOCH_BLOCKS` (10) is a **fixed constant**, never a chain profile
  value: `prev_randao(chain_id, number)` takes no profile parameter, by construction
  — there is nothing a chain's `[profile].blocks_per_batch` (the batcher's own posting cadence, a
  different quantity that happens to share today's value) could thread into it even if a caller tried.
  Contracts that need real randomness use an oracle, as on any single-sequencer rollup.
- **A per-sub-block execution budget is enforced by the caller, not assumed by the executor.**
  `execute_sub_block` takes a gas limit and a wall-clock deadline; an executor must stop reaching new
  transactions once either binds, and must return everything it did not reach, in order, as
  `SubBlockOutcome::not_executed` — never block past the deadline, and never let one sub-block's gas run
  unbounded.
- **Execution never reorders.** Arrival order is execution order; an executor's job is to run exactly the
  ordered list it is handed and report what happened to each entry (included, rejected with a reason, or
  not reached) — never to reorder for its own convenience.
- **A rejection carries the sender.** `RejectedTx` on execution-time rejection (nonce, funds, intrinsic
  gas, or an engine-specific error) is not just a hash — it names the sender so the caller can reconcile
  its own admission-side nonce cache to the executor's real value, not just drop the transaction.

## Config

None — this is a pure trait-and-types crate with no runtime configuration of its own. Configuration for
which executor actually runs lives in [`rome-zk-sequencer`](../rome-zk-sequencer)'s own config (see that
crate's README).

## How to test

```sh
cargo test -p rome-zk-executor-api
```

The tests here are the pure, deterministic ones: the `prev_randao` golden vector, and that it actually
varies with chain id, block number, and batch boundary (never a constant). Behavioral tests of an actual
`Executor` implementation live in [`rome-zk-executor-reth`](../rome-zk-executor-reth) and
[`rome-zk-sequencer`](../rome-zk-sequencer).

## Security notes

The `SubBlockLimits::unbounded()` constructor exists solely for
`rome_zk_sequencer::recovery`'s replay path, which re-executes a log record's transactions exactly as
originally attempted — a record already holds only the transactions that were actually attempted live, so
reapplying a fresh (non-reproducible, wall-clock-relative) deadline or gas cutoff a second time would be
wrong, not merely redundant. Any other caller reaching for `unbounded()` should be treated as a bug: normal
execution always bounds a sub-block.

## Design notes

This crate defines the `Executor` boundary, including the rule that the block
environment is published, not inferred, and the `prevRandao` formula. Shaped after reth's own `PayloadJob`
contract (`best_payload()` must always be valid, never an error for "nothing built yet") so that an
in-process reth implementation can sit behind this trait without changing anything above it.

## Depends on

Nothing beyond `alloy-primitives` (for `Address`, `B256`, `Bytes`, `TxHash`, `keccak256`) and `thiserror`.
Implemented by [`rome-zk-executor-reth`](../rome-zk-executor-reth); consumed by
[`rome-zk-sequencer`](../rome-zk-sequencer).
