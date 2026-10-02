# rome-zk-explorer-api

**Skeleton only — no behavior yet.** This crate will become the read API a block-explorer UI talks to: a
service that answers block, transaction, address and settlement-status queries over the schema
[`rome-zk-indexer`](../rome-zk-indexer) and [`rome-zk-settlement-watcher`](../rome-zk-settlement-watcher)
populate.

## What it will guarantee

Once built, this crate is expected to implement:

- Point lookups and paginated listing over Postgres (block, transaction, address history) — not
  aggregate analytics, which is why this data model is Postgres rather than a column store.
- The "sequencer view": a per-transaction path from its sub-block, through its block, through the inbox
  chunk it was posted in, to the Solana signature that posted it, to the batch and root it belongs to —
  the same data a chain's own monitoring dashboards read, surfaced for an end user asking "where is my
  transaction."
- Node-style reads (block and transaction detail, tracing) proxied to the derivation node rather than the
  sequencer, since the derivation node is the one party guaranteed to hold exactly what Solana finalized.

None of the above exists in the code yet. Do not depend on this crate's behavior in anything else until it
does.

## Config

Not yet defined.

## How to test

```sh
cargo test -p rome-zk-explorer-api
```

There is nothing to test yet beyond the crate compiling.

## Security notes

Not applicable yet — there is no behavior to secure. This crate is a read path only; nothing about it
should ever be able to influence sequencing, settlement or finality.

## Design references

The lane design covers the explorer and indexer in full (the UI shell reuse and the sequencer-view data model)
and the operations and topology.

## Depends on

Expected to read the schema [`rome-zk-indexer`](../rome-zk-indexer) and
[`rome-zk-settlement-watcher`](../rome-zk-settlement-watcher) populate, and to be served behind a
TypeScript/React UI shell reused from an existing Rome block-explorer front end — none of this is wired up
yet.
