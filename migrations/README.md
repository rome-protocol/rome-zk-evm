# migrations

Postgres schema for the off-chain services (prover jobs, indexer, settlement watcher). One Postgres
instance per environment, **two databases**: `rome_zk_prover` (the prover orchestrator's job queue) and
`rome_zk_explorer` (the indexer and settlement watcher's read-path schema) are the design's own generic
names — a deployment may name its databases differently, and the `database_url` it configures is the
truth. Each database gets its own subdirectory here, so `sqlx::migrate!` for one service's migrator never
sees the other database's files:

```
migrations/
  explorer/   0001_settlement.sql, ...   -- rome_zk_explorer (rome-zk-settlement-watcher, rome-zk-indexer)
  prover/     0001_proof_jobs.sql        -- rome_zk_prover (rome-zk-prover)
```

Convention within a subdirectory: `sqlx` migrations, `NNNN_<name>.sql`, forward-only, applied by the
service that owns that database (never by hand). `explorer/0001_settlement.sql` is the first migration to
land (`rome-zk-settlement-watcher`'s own tables: `settlement_tx`, `inbox_chunk`, `batch`, `root_post`,
`proof`, `challenge`, `settlement_cursor`, and the `block_status` read-time view) — `rome-zk-indexer`
extends the same `explorer/` directory with its own numbered migrations when it lands. `prover/0001_proof_jobs.sql`
is `rome-zk-prover`'s own history schema (`proof_jobs`, `proofs`) — applied by `rome_zk_prover::store::PgStore::connect`
at process start, the chain stays the only cursor the follower loop itself ever reads.
