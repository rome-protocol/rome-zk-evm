-- rome_zk_explorer schema: the settlement watcher's own tables. One row per Solana signature the watcher
-- cares about (`settlement_tx`), one row per program that signature touched (`settlement_tx_program`),
-- derived per-chunk and per-batch rows below, and a durable cursor per ingest source. A single
-- transaction may carry instructions for several programs ("root + action"), and each ingester that
-- observes it records its own attribution. Forward-only; applied by the service that owns it (this crate,
-- via `rome_zk_settlement_watcher::migrate`), never by hand.
--
-- Status vocabularies are CHECK constraints, not Postgres ENUM types: adding a value later is an
-- `ALTER TABLE ... DROP CONSTRAINT / ADD CONSTRAINT`, not the migration-locking `ALTER TYPE ... ADD VALUE`
-- Postgres enums require.

-- One row per Solana transaction signature this system's services care about, tx-level facts only.
-- They are split from the per-program facts below so a transaction carrying instructions for two
-- different programs ("root + action") is not squeezed into one program's row.
-- `status` starts at 'confirmed' -- `getSignaturesForAddress` never returns a signature below that
-- commitment. Everything downstream of Solana treats 'confirmed' as provisional and 'finalized' as the
-- only safe read, and the finality pass (`finality::track_finality`) advances the status to
-- 'finalized' by re-checking each row's own status against what Solana now reports, or to the terminal
-- 'dropped' once `getSignatureStatuses` reports no status at all (pruned/never landed) for 150 slots
-- past the node's own finalized slot. 'processed' is not a value this watcher's own RPC method ever
-- observes (`getSignaturesForAddress` never returns it), so it is not in the vocabulary.
CREATE TABLE settlement_tx (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    sig         TEXT NOT NULL UNIQUE,
    slot        BIGINT NOT NULL,
    block_time  BIGINT,
    signer      TEXT NOT NULL,
    status      TEXT NOT NULL CHECK (status IN ('confirmed', 'finalized', 'dropped'))
);
CREATE INDEX settlement_tx_slot_idx ON settlement_tx (slot);
-- `lifecycle::derive_once`'s row-fetch scans `(slot, id) > (cursor.last_slot, cursor.last_id) ORDER BY
-- slot ASC, id ASC` -- a composite index in that same order, not the plain `slot` index above, is what
-- keeps that scan an index range scan instead of an index-then-filter as Tiber's real inbox history
-- (1-3.6M rows) grows.
CREATE INDEX settlement_tx_slot_id_idx ON settlement_tx (slot, id);
-- The finality pass (`finality::track_finality`) only ever reads not-yet-finalized rows; a plain index on
-- `slot` would make it scan every already-finalized row in the table as history grows. A partial index on
-- the still-open vocabulary keeps that scan bounded by the number of rows actually being tracked, not the
-- whole table.
CREATE INDEX settlement_tx_unfinalized_idx ON settlement_tx (slot) WHERE status NOT IN ('finalized', 'dropped');

-- One row per (transaction, program) this watcher's ingesters observed -- the per-program facts a single
-- signature can carry more than one of (a transaction may carry root instructions for several chains plus
-- arbitrary instructions of other programs). Exactly one ingester (`kind = 'inbox'` or `'root'` today)
-- ever writes a given `(settlement_tx_id, kind)` row, so two ingesters observing the same signature both
-- get to record their own program's `ix_kind`/`chain_id`/`batch_id` rather than losing to whichever
-- commits first (a single-valued `kind` column on `settlement_tx` would lose one of them).
-- `chain_id`/`batch_id` are nullable: a signature whose instruction data this crate cannot decode (a
-- future variant, a pre-current-shape `InitChain`, a program deploy/upgrade tx) is still recorded -- with
-- no chain attributed, rather than the misleading `chain_id = 0` a real chain could someday hold.
CREATE TABLE settlement_tx_program (
    settlement_tx_id  BIGINT NOT NULL REFERENCES settlement_tx (id),
    -- 'exit' is reserved the same way 'proof'/'challenge' already were: `ProveExit`/`ConsumeExit` are
    -- instructions of the SAME on-chain settlement program `PostRoot`/`FinalizeBatch`/etc. already ingest
    -- under `kind = 'root'`, so this crate's ingester never writes an 'exit'-kind row itself -- adding the
    -- value here only keeps the vocabulary open for a future dedicated ingest source, without a
    -- migration-locking `ALTER TYPE`-style change later.
    kind              TEXT NOT NULL CHECK (kind IN ('inbox', 'root', 'proof', 'challenge', 'exit')),
    -- Human-readable instruction summary, e.g. "Open+Write+Seal+SealLeaf" for the one-V1-tx-per-frame
    -- bundle, or a single name ("OpenBatch", "PostRoot", ...) for a lifecycle instruction --
    -- never re-derived from `kind` alone, since one signature can bundle several instructions.
    ix_kind           TEXT NOT NULL,
    chain_id          BIGINT,
    batch_id          BIGINT,
    -- Every `ChunkEvent`/`BatchEvent` this ingester already decoded from the transaction body, as JSON:
    -- `lifecycle::derive_once` reads this column instead of re-fetching and re-decoding the transaction
    -- from Solana a second time ("derive reads the feed, never Solana"), halving RPC load and removing the
    -- Bigtable-null-on-retry loss a re-fetch was exposed to. Empty
    -- (`{"chunk_events":[],"batch_events":[],"exit_events":[]}`) for a failed on-chain transaction (ingest
    -- already keeps the row so the explorer can still show it happened; nothing derives from a revert). A
    -- `root`-kind row never has chunk or batch events (the settlement-program ingester has none of its
    -- own); it carries `exit_events` for `ProveExit`/`ConsumeExit`.
    events            JSONB NOT NULL DEFAULT '{"chunk_events":[],"batch_events":[],"exit_events":[]}'::jsonb,
    -- Schema version of the `events` payload, for the repair path that re-derives the OLDEST rows with the
    -- NEWEST decoder. The compatibility rule is additive-only: a new field on an existing variant is
    -- `#[serde(default)]` and does not bump this; anything else (a renamed/removed variant, a changed
    -- field type) bumps it. `lifecycle::derive_once` never branches on this column today (only version 1
    -- has ever existed) -- it exists so an operator repairing an old row after a real format bump
    -- (re-ingest with `ON CONFLICT (settlement_tx_id, kind) DO UPDATE SET events, events_version`, then
    -- reset `derive_cursor` to re-derive it) can tell which rows still need it.
    events_version    SMALLINT NOT NULL DEFAULT 1,
    PRIMARY KEY (settlement_tx_id, kind)
);
CREATE INDEX settlement_tx_program_chain_batch_idx ON settlement_tx_program (chain_id, batch_id);
-- `lifecycle::derive_once`'s own row-fetch query filters `stp.kind = 'inbox'` then joins back to
-- `settlement_tx_id` -- the table's own PRIMARY KEY is `(settlement_tx_id, kind)`, the wrong leading
-- column for that access pattern once this table holds millions of rows (Tiber's real inbox history).
CREATE INDEX settlement_tx_program_kind_tx_idx ON settlement_tx_program (kind, settlement_tx_id);

-- One row per sealed inbox chunk (one PDA, one V1 tx per frame). `byte_len` and
-- `body_hash` come from that frame's `Seal { len, body_hash }` instruction -- the authoritative sealed
-- length -- never from `Open`'s `size` (an upper allocation bound) or `Write`'s payload length (this
-- watcher never reads chunk body bytes at all; that is rome-zk-derive's DA path, not this one's).
-- `chunk_id` is `<chain_id>-<batch_id>-<idx>`, a human-readable key computed from `Open`'s own decoded
-- data. `chunk_pda` -- the chunk PDA account's own base58 address, exactly as `zk_inbox_client::chunk_pda`
-- would derive it -- is the *lifecycle* key: `Open` inserts a row keyed by it; `Seal`/`SealLeaf`, which
-- carry no `(chain_id, batch, idx)` in their own instruction data, resolve back to a chunk purely by this
-- account address and only ever UPDATE an existing row, never invent one (a Seal-only tx must not
-- fabricate a `(0, 0, 0)` row).
-- `closed_tx`: `Close` reclaims the chunk PDA account's rent -- the account is gone from Solana's own
-- state, but its DA history must still read as posted, not vanish. Set once, from
-- `ChunkEventKind::Closed`; the row itself is never deleted.
CREATE TABLE inbox_chunk (
    chunk_id            TEXT PRIMARY KEY,
    chunk_pda           TEXT NOT NULL UNIQUE,
    settlement_tx_id    BIGINT NOT NULL REFERENCES settlement_tx (id),
    chain_id            BIGINT NOT NULL,
    batch_id            BIGINT NOT NULL,
    idx                 BIGINT NOT NULL,
    byte_len            BIGINT,
    sealed              BOOLEAN NOT NULL DEFAULT FALSE,
    body_hash           BYTEA,
    closed_tx           BIGINT REFERENCES settlement_tx (id)
);
CREATE INDEX inbox_chunk_chain_batch_idx ON inbox_chunk (chain_id, batch_id);

-- One row per `(chain_id, batch_id)` the watcher has ever seen `OpenBatch`'d (the accumulator).
-- `status` is the inbox-side TERMINAL lifecycle -- `open` (chunks arriving, or `FinalizeBatch` partially
-- stepped through -- see `finalize_cursor` below) -> `finalized` (`FinalizeBatch` reached
-- `expected_count`) or `abandoned` (`AbandonBatch`) -- and stays there forever. A finalized or abandoned
-- batch's `status` never changes again, independent of whether a root has ever been posted for it (that
-- is `root_post`, below); a chain may run for a long time with its inbox-side batching live and no root
-- poster running.
--
-- `CloseBatch` (rent reclaimed) is recorded on the orthogonal `closed_tx` column, never as a `status`
-- value: a fourth 'closed' status value would need its own arm in `block_status`'s CASE, and without one
-- every finalized batch would regress to reading as 'sequenced' the moment its rent was reclaimed -- the
-- ordinary steady-state outcome for every batch. Keeping recycling orthogonal means the view needs no
-- 'closed' arm at all: a closed batch still reads exactly like the finalized/abandoned batch it was.
--
-- `finalize_cursor` mirrors `programs/zk-inbox/src/batch.rs::finalize_batch_inner` exactly:
-- `FinalizeBatch { step }` is permissionless and may be called any number of times; `step == 0` means
-- "transform every remaining leaf in this call" (cursor jumps straight to `expected_count`), a nonzero
-- `step` advances the cursor by at most that many leaves and may leave the batch still open. `status`
-- only ever becomes `'finalized'` once this cursor reaches `expected_count`, so a partial call is not
-- recorded as `finalized` -- until then the batch stays `'open'`, so a later genuine `AbandonBatch` on it
-- is still recognized (the `WHERE status = 'open'` guard both updates share never stops applying
-- mid-finalize). Applying the same `(batch_pda, settlement_tx_id)` step twice (a `derive_cursor` reset for
-- repair is a real operational path) must not double-count it -- see `batch_finalize_step`, below.
--
-- `finalized_tx`/`abandoned_tx` are separate columns, so `AbandonBatch` never overwrites the
-- `finalized_tx` a real finalize sets -- neither is ever overwritten by the other event.
--
-- `batch_pda` is the inbox program's own `["batch", chain_id, batch]` PDA address (base58, computed by
-- `zk_inbox_client::batch_pda` -- never re-derived here), recorded the moment `OpenBatch` is first seen.
-- `FinalizeBatch`/`AbandonBatch`/`CloseBatch`/`GrowBatch` carry no `chain_id`/`batch` in their own
-- instruction data (design: the target account is named in the accounts list, not the data) -- this
-- column is what lets the ingest code resolve that account address back to `(chain_id, batch_id)`
-- without decoding an account's bytes (this watcher never reads account state, only tx history).
--
-- `root`/`acc` are reserved, NULL: they are computed on-chain and stored in the batch ACCOUNT, never
-- passed as a `FinalizeBatch` instruction argument, so a pure transaction-history watcher cannot
-- populate them without an added `getAccountInfo` read, which it does not perform.
CREATE TABLE batch (
    chain_id        BIGINT NOT NULL,
    batch_id        BIGINT NOT NULL,
    opened_tx       BIGINT NOT NULL REFERENCES settlement_tx (id),
    finalized_tx    BIGINT REFERENCES settlement_tx (id),
    abandoned_tx    BIGINT REFERENCES settlement_tx (id),
    closed_tx       BIGINT REFERENCES settlement_tx (id),
    expected_count  BIGINT NOT NULL,
    finalize_cursor BIGINT NOT NULL DEFAULT 0,
    batch_pda       TEXT NOT NULL,
    root            BYTEA,
    acc             BYTEA,
    status          TEXT NOT NULL CHECK (status IN ('open', 'finalized', 'abandoned')),
    PRIMARY KEY (chain_id, batch_id)
);
CREATE UNIQUE INDEX batch_pda_idx ON batch (batch_pda);

-- Idempotency record for `FinalizeBatch { step }` application: keyed by `(batch_pda, settlement_tx_id)`
-- rather than relying on `derive_cursor` never being reset -- the repair path (re-deriving after a cursor
-- reset, e.g. to pick up a row `lifecycle.rs` failed to apply the first time) must not re-add the same
-- step's leaves to `finalize_cursor` twice. `lifecycle.rs` only performs the `finalize_cursor`
-- arithmetic when its `INSERT ... ON CONFLICT DO NOTHING` here actually inserts a new row.
CREATE TABLE batch_finalize_step (
    batch_pda           TEXT NOT NULL,
    settlement_tx_id    BIGINT NOT NULL REFERENCES settlement_tx (id),
    step                BIGINT NOT NULL,
    PRIMARY KEY (batch_pda, settlement_tx_id)
);

-- One row per posted root (`PostRoot`/`PostRootProved`). Batch ids are sequential and never reused, so
-- one row per `(chain_id, batch_id)` is exact, not a simplification. Reserved: no ingester writes this
-- table, so it is exercised by unit tests on hand-built rows, not the replay fixture.
CREATE TABLE root_post (
    chain_id            BIGINT NOT NULL,
    batch_id            BIGINT NOT NULL,
    settlement_tx_id    BIGINT NOT NULL REFERENCES settlement_tx (id),
    first_block         BIGINT NOT NULL,
    last_block          BIGINT NOT NULL,
    state_root          BYTEA NOT NULL,
    block_hash          BYTEA NOT NULL,
    proved              BOOLEAN NOT NULL DEFAULT FALSE,
    status              TEXT NOT NULL CHECK (status IN ('pending', 'final')),
    PRIMARY KEY (chain_id, batch_id)
);

-- Reserved, empty (the challenge state machine is not implemented; the settlement program's own
-- `RejectBatch` discriminant is likewise reserved). Kept here rather than added later so the explorer
-- schema, which this crate owns, does not need a second migration file the moment the challenger crate
-- needs a home for its rows.
CREATE TABLE proof (
    id                  BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    chain_id            BIGINT NOT NULL,
    batch_id            BIGINT NOT NULL,
    settlement_tx_id    BIGINT NOT NULL REFERENCES settlement_tx (id),
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE challenge (
    id                  BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    chain_id            BIGINT NOT NULL,
    batch_id            BIGINT NOT NULL,
    settlement_tx_id    BIGINT NOT NULL REFERENCES settlement_tx (id),
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- One row per exit message, keyed by its own globally-unique `message_hash`. `ProveExit` (28) inserts the
-- row (`status = 'proved'`); `ConsumeExit` (29) is the only thing that ever moves it to `'released'` --
-- resolved purely by `message_hash`, mirroring how `inbox_chunk`'s `Seal`/`Close` resolve back to an
-- existing row by `chunk_pda` rather than re-deriving one. `window_index` is deliberately absent here (see
-- `decode::ExitEvent`'s own doc): it is a runtime `Clock::slot`-derived on-chain value, not an instruction
-- argument, and this watcher never reads Solana account state -- the same documented bound
-- `batch.root`/`batch.acc` already state for the inbox-side accumulator. `amount` is stored as decimal
-- TEXT (Postgres has no native 128-bit integer type, and this explorer-only column does not need
-- `NUMERIC`'s arbitrary-precision arithmetic -- a plain string is exact and sufficient for display).
CREATE TABLE exit (
    id              BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    chain_id        BIGINT NOT NULL,
    message_hash    BYTEA NOT NULL UNIQUE,
    batch           BIGINT,
    sol_recipient   BYTEA,
    amount          TEXT,
    window_index    BIGINT,
    proved_sig      TEXT,
    released_sig    TEXT,
    status          TEXT NOT NULL CHECK (status IN ('proved', 'released'))
);
CREATE INDEX exit_chain_id_idx ON exit (chain_id);

-- One row per ingest source (`kind` = 'inbox' | 'root', one per program). `last_sig`/`last_slot` are the
-- newest signature this source has fully walked past -- set only once a whole backward walk (down to the
-- previous `last_sig`, or to genesis on the very first walk) has completed; `getSignaturesForAddress`
-- pages `until: last_sig` forward from here on every later call (MAX(slot) alone wedges on a run of
-- skipped slots -- pinning to the newest fully-committed *signature*, not a slot number, has no such gap).
--
-- `backfill_before`/`backfill_head_sig`/`backfill_head_slot` are the two-phase cold-start cursor: a walk
-- with a multi-million-signature backlog (Tiber's inbox program today) would, if it buffered every page in
-- memory and committed nothing until the *entire* walk finished, restart from zero after a crash or RPC
-- failure anywhere in the walk. Instead each committed batch of rows advances `backfill_before` to the
-- oldest signature it just committed, so a resumed walk starts from there (at most one batch of
-- possibly-redundant, idempotent re-fetch) rather than from the very tip again -- the real atomicity
-- contract is **the marker never names a signature that is not actually committed in `settlement_tx`**,
-- not that rows and the marker share one transaction (they do not: each commit batch is its own
-- transaction, and the marker update is a separate one immediately after). A crash between the two costs
-- at most one batch of idempotent re-fetch on resume, never a gap or a duplicate.
-- `backfill_head_sig`/`backfill_head_slot` are the newest signature this walk saw at its very start (the
-- first page's first entry) -- written in the SAME transaction as the first row this walk ever commits
-- (writing it any later would leave a window where committed rows exist with no walk-in-progress marker
-- to gate `derive_once` on) -- and carried across restarts so that whenever the walk *does* complete,
-- `last_sig`/`last_slot` become exactly that value, not whatever the last-processed page happened to end
-- on.
--
-- Row derivation (inbox_chunk/batch lifecycle mutations) is not tied to this ingest cursor's own ordering
-- at all: a backward walk visits signatures newest-first across pages, and applying lifecycle events in
-- that order would run `Seal` before `Open`, a partial `FinalizeBatch` after a later one, and so on.
-- `derive_cursor` (below) tracks a *separate* pass over already-ingested `settlement_tx` rows in true
-- `(slot, id)` order, so arrival order during ingest never matters (the feed, not a shadow table) --
-- **provided the feed below the derive cursor is complete**, which is why `derive_once`'s own query joins
-- this table and refuses to run at all while `backfill_head_sig IS NOT NULL` (an ingest walk still in
-- progress: some older page may not have landed yet, which an unguarded `(slot, id) > cursor` scan cannot
-- tell apart from "nothing older exists"). `ingest::run_once` also never splits one Solana slot across
-- two pages (a trailing run of signatures sharing the page's oldest slot is held back and merged into the
-- next fetch instead) -- otherwise the newer half of a same-slot pair would land in an earlier-committed
-- page and get the lower `settlement_tx.id`, inverting `(slot, id)` order for exactly that pair.
--
-- `source_max_slot` is the newest slot the RPC reported as existing for this program the last time this
-- source fetched its very first (newest) page -- for lag reporting (`source_max_slot - last_slot`).
CREATE TABLE settlement_cursor (
    kind                TEXT PRIMARY KEY,
    last_sig            TEXT,
    last_slot           BIGINT,
    backfill_before     TEXT,
    backfill_head_sig   TEXT,
    backfill_head_slot  BIGINT,
    source_max_slot     BIGINT,
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The derive pass's own durable position: the last `settlement_tx` row, in `(slot, id)` order, whose
-- inbox lifecycle events (`inbox_chunk`/`batch` mutations) have been applied. `lifecycle::derive_once`
-- reads this cursor and `settlement_cursor.backfill_head_sig` in the SAME statement (a `JOIN` on
-- `sc.kind = 'inbox' AND sc.backfill_head_sig IS NULL`) so it never advances past rows an unfinished
-- ingest walk might still be missing older pages for. `last_slot`/`last_id` default to a value below any
-- real row (`slot`/`id` are never negative) so a fresh derive pass starts from the very first row. One
-- row per ingester kind that has lifecycle state to derive: `'inbox'` (`inbox_chunk`/`batch` mutations,
-- `lifecycle::derive_once`) and `'root'` (`exit` row mutations from `ProveExit`/`ConsumeExit`,
-- `lifecycle::derive_exit_once` -- an entirely separate pass and cursor from the inbox one, sharing only
-- this table's shape). Safe to reset for repair (deleting a row re-derives that kind from the beginning)
-- -- `batch_finalize_step` (above) makes a re-applied `FinalizeBatch { step }` idempotent, so a reset
-- never double-counts one; `exit` rows are similarly idempotent by construction (`ON CONFLICT
-- (message_hash) DO NOTHING` / a `status = 'proved'`-guarded `UPDATE`), so a reset of the `'root'` row
-- cannot double-apply either.
CREATE TABLE derive_cursor (
    kind        TEXT PRIMARY KEY,
    last_slot   BIGINT NOT NULL DEFAULT -1,
    last_id     BIGINT NOT NULL DEFAULT 0,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Read-time status: per-block status is derived at read time, never stored. There is no per-L2-block
-- table (that needs a block<->chunk mapping), so the view is keyed at the grain this schema actually
-- has -- (chain_id, batch_id) -- and is the batch-level proxy for the block-level
-- `sequenced | data posted | root posted | final` states:
--   sequenced          - batch opened, not yet finalized on the inbox side (chunks still arriving)
--   data posted (confirmed) - inbox-side FinalizeBatch succeeded, but its own transaction is not yet
--                              `finalized` on Solana (`confirmed` is provisional);
--                              no root posted yet
--   data posted (dropped)   - the finalizing transaction itself was dropped (`finality::track_finality`'s
--                              terminal state) -- the batch's on-chain finalization can no
--                              longer be confirmed from this signature; a downstream reader must treat
--                              this the same as "not actually finalized"
--   data posted        - the same, once the finalizing transaction itself reports `finalized`
--   root posted        - a root_post row exists and is still 'pending' (challenge window / prove_window open)
--   final              - root_post.status = 'final'
--   abandoned          - AbandonBatch (no root will ever cover this batch id)
-- `status` never leaves `open`/`finalized`/`abandoned` -- a `CloseBatch` (rent reclaimed) only ever sets
-- the orthogonal `batch.closed_tx`, so this view needs no arm for it at all: a closed batch reads exactly
-- as it did before its rent was reclaimed (a 'closed' status value would regress every recycled batch to
-- 'sequenced').
-- The `(confirmed)`/`(dropped)` provisional suffix applies to the `abandoned` arm the same way it applies
-- to the finalized/data-posted arm: `abandoned (confirmed)` -> `abandoned` once the abandoning transaction
-- itself reaches Solana `finalized`, or `abandoned (dropped)` if it instead reaches the terminal
-- `dropped`. Without the suffix, a `dropped` `AbandonBatch` (or one still only `confirmed`) would read
-- identically to a `finalized` one. The same suffix is planned for `root_post` once an ingester writes it;
-- the view does not apply it yet.
-- This is a documented BOUND, not a gap this view can close on its own: once a row is derived from a
-- `confirmed` transaction, it is never un-applied later if that transaction turns out `dropped`
-- (`batch.status`/`abandoned_tx`/`finalized_tx` keep pointing at it) -- an optimistic-confirmation failure
-- this narrow is accepted; the repair is an operator re-deriving from a reset `derive_cursor` once the
-- real on-chain outcome is known.
-- A per-L2-block `block_status` (joining a future `block_batch`/`l2_block_settlement` table down to this
-- same `batch`/`root_post` join) is the natural follow-on once that table lands; nothing here blocks it.
-- `root_post`'s own confirmed/finalized split is not made here: no ingester writes `root_post` rows, so
-- there is nothing to exercise it against.
CREATE VIEW block_status AS
SELECT
    b.chain_id,
    b.batch_id,
    CASE
        WHEN b.status = 'abandoned' AND abt.status = 'finalized' THEN 'abandoned'
        WHEN b.status = 'abandoned' AND abt.status = 'dropped' THEN 'abandoned (dropped)'
        WHEN b.status = 'abandoned' THEN 'abandoned (confirmed)'
        WHEN rp.status = 'final' THEN 'final'
        WHEN rp.chain_id IS NOT NULL THEN 'root posted'
        WHEN b.status = 'finalized' AND ft.status = 'finalized' THEN 'data posted'
        WHEN b.status = 'finalized' AND ft.status = 'dropped' THEN 'data posted (dropped)'
        WHEN b.status = 'finalized' THEN 'data posted (confirmed)'
        ELSE 'sequenced'
    END AS status
FROM batch b
LEFT JOIN settlement_tx ft ON ft.id = b.finalized_tx
LEFT JOIN settlement_tx abt ON abt.id = b.abandoned_tx
LEFT JOIN root_post rp ON rp.chain_id = b.chain_id AND rp.batch_id = b.batch_id;
