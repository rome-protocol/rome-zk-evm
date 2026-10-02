-- rome_zk_prover schema ("the chain is the cursor; Postgres is history"): one row per (chain, batch,
-- prove attempt) the follower loop has ever worked on, and the proof bytes for whichever attempt reached
-- Verified. This schema is history, never an input to the follower's own next-candidate decision --
-- `rome-zk-prover::store::Store` is write-only from the loop's point of view (no `get`/`query` method
-- exists on that trait at all); a chain reset that restarts batch ids from 1 is expected to eventually
-- collide with an old row's `(chain_id, batch)` under a NEW `attempt` sequence -- that is a distinct row,
-- by design, never a decision input either way.
--
-- Status is a CHECK constraint, not a Postgres ENUM type, matching migrations/explorer/0001_settlement.sql
-- (adding a value later is an ALTER TABLE, not the migration-locking ALTER TYPE ... ADD VALUE Postgres
-- enums require).

-- One row per prove attempt of one batch. The follower's own state machine (`rome-zk-prover::follower`)
-- writes one `JobEvent` per transition (`Queued -> InputBuilt -> Proving -> Proved -> Verified -> Posted
-- -> Finalized`, or `AlreadyPosted`/`Superseded`/`Failed`); each event is an idempotent UPSERT keyed by
-- `(chain_id, batch, attempt)`, so replaying the exact same sequence of events (a resumed/retried run)
-- leaves exactly one row, and a genuinely new prove attempt for the SAME batch (a resumed job whose cached
-- artefact failed a fresh check is re-proved rather than halted) gets its own row rather than overwriting
-- the failed attempt's own history.
--
-- `first_block`/`last_block`/`program_vk`/`backend`/`input_bytes`/`gas_used`/every `wall_*_ms`/`cost_usd`/
-- `sig`/`finalize_sig` are filled progressively, one transition at a time -- NULL until the transition
-- that knows that fact has run. `status` always reflects the MOST RECENT transition recorded for this
-- `(chain_id, batch, attempt)`, never a fact this table infers on its own.
CREATE TABLE proof_jobs (
    id             BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    chain_id       BIGINT NOT NULL,
    batch          BIGINT NOT NULL,
    attempt        INT NOT NULL,
    first_block    BIGINT,
    last_block     BIGINT,
    program_vk     TEXT,
    status         TEXT NOT NULL CHECK (status IN (
                       'queued', 'input_built', 'proving', 'proved', 'verified',
                       'posted', 'already_posted', 'superseded', 'failed', 'finalized'
                   )),
    backend        TEXT,
    input_bytes    BIGINT,
    gas_used       BIGINT,
    wall_input_ms  DOUBLE PRECISION,
    wall_stark_ms  DOUBLE PRECISION,
    wall_plonk_ms  DOUBLE PRECISION,
    wall_verify_ms DOUBLE PRECISION,
    wall_post_ms   DOUBLE PRECISION,
    cost_usd       DOUBLE PRECISION,
    sig            TEXT,
    finalize_sig   TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- The idempotency key `store::PgStore::record`'s own `ON CONFLICT` targets: a batch is proved at
    -- most once per attempt, and the SAME attempt replayed (a restart resuming or re-recording an
    -- already-seen transition) updates this ONE row rather than duplicating it.
    UNIQUE (chain_id, batch, attempt)
);
CREATE INDEX proof_jobs_chain_batch_idx ON proof_jobs (chain_id, batch);

-- The proof bytes for whichever attempt reached Verified: the 768-byte ZisK PLONK proof and the 512-byte
-- packaged public values `rome-zk-prover::calldata::Calldata` itself carries
-- (`proof_bytes_768`/`publics_512`) -- never the larger 1,344-byte on-chain ABI, which is reconstructible
-- from these two fields plus the vkey of record at read time.
CREATE TABLE proofs (
    job_id    BIGINT PRIMARY KEY REFERENCES proof_jobs (id),
    proof_abi BYTEA NOT NULL,
    publics   BYTEA NOT NULL
);
