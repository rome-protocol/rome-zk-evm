# rome-zk-indexer

**Skeleton only — no behavior yet.** This crate will become the rollup indexer: the service that turns
the execution engine's own notifications (new blocks, reorgs, reverts) into a queryable Postgres schema
covering the full path from a sub-block through to a finalized, posted root.

## What it will guarantee

Once built, this crate is expected to implement:

- An out-of-process ingest path (a reth execution-extension streaming notifications over gRPC to a
  separate writer process), so a slow or stalled database write can never block block production itself.
- Batched inserts per block (rather than one round trip per row), covering blocks, transactions, receipts
  and logs, plus an address-to-transaction index the execution engine's own history API does not provide.
- A per-block "sequenced / data posted / root posted / final / challenged" status, derived at read time by
  joining a block to its chunk, batch and root records — never stored redundantly on the block row itself.

None of the above exists in the code yet. Do not depend on this crate's behavior in anything else until it
does.

## Config

Not yet defined.

## How to test

```sh
cargo test -p rome-zk-indexer
```

There is nothing to test yet beyond the crate compiling.

## Security notes

Not applicable yet — there is no behavior to secure. This crate is a read path only; nothing about it
should ever be able to influence sequencing, settlement or finality.

## Design references

The lane design covers the explorer and indexer in full (the two-ingest-path design and the settlement data model).

## Depends on

Expected to depend on [`rome-zk-executor-reth`](../rome-zk-executor-reth) for the notification stream it
reads from, and to share its Postgres schema with
[`rome-zk-settlement-watcher`](../rome-zk-settlement-watcher) and
[`rome-zk-explorer-api`](../rome-zk-explorer-api) — none of this is wired up yet. See
[`migrations/README.md`](../../migrations/README.md) for where this crate's schema will land.
