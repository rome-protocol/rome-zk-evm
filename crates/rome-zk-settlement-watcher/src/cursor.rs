//! `settlement_cursor` (migrations/explorer/0001_settlement.sql): one durable row per ingest source, so
//! a restart resumes exactly where it left off (one cursor per program). Only ever advanced by
//! [`crate::ingest::run_once`], and only after the DB transaction covering the page it names has already
//! committed.
//!
//! Two independent cursors live here: the *ingest* cursor (`last_sig`/`last_slot`,
//! plus the `backfill_*` two-phase cold-start marker) tracks how far `run_once` has walked
//! `getSignaturesForAddress`; the *derive* cursor (`derive_cursor` table, [`DeriveCursorState`]) tracks a
//! separate pass applying lifecycle events (`inbox_chunk`/`batch` mutations) over already-ingested rows in
//! true `(slot, id)` order, independent of the order ingest happened to observe them in.

use sqlx::PgPool;

/// The two Solana signature sources this watcher tracks. `kind()` is the `settlement_cursor.kind` /
/// `settlement_tx_program.kind` value on the wire — also what the
/// `proof`/`challenge` reserved tables call the other two, unused, values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramKind {
    Inbox,
    Root,
}

impl ProgramKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProgramKind::Inbox => "inbox",
            ProgramKind::Root => "root",
        }
    }
}

/// A cursor's durable state. `last_sig`/`last_slot` are `None` before this source has ever completed a
/// full walk (a brand-new watcher against a chain with history: the very first walk has no `until` bound
/// and walks all the way back to genesis). `backfill_before`/`backfill_head_sig`/`backfill_head_slot` are
/// `Some` exactly while a walk is in progress and has not yet reached its end
/// (`migrations/explorer/0001_settlement.sql`'s own doc comment on this table has the full contract).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CursorState {
    pub last_sig: Option<String>,
    pub last_slot: Option<i64>,
    pub backfill_before: Option<String>,
    pub backfill_head_sig: Option<String>,
    pub backfill_head_slot: Option<i64>,
}

type CursorRow = (
    Option<String>,
    Option<i64>,
    Option<String>,
    Option<String>,
    Option<i64>,
);

pub async fn read_cursor(pool: &PgPool, kind: ProgramKind) -> Result<CursorState, sqlx::Error> {
    let row: Option<CursorRow> = sqlx::query_as(
        "SELECT last_sig, last_slot, backfill_before, backfill_head_sig, backfill_head_slot
         FROM settlement_cursor WHERE kind = $1",
    )
    .bind(kind.as_str())
    .fetch_optional(pool)
    .await?;
    Ok(match row {
        Some((last_sig, last_slot, backfill_before, backfill_head_sig, backfill_head_slot)) => {
            CursorState {
                last_sig,
                last_slot,
                backfill_before,
                backfill_head_sig,
                backfill_head_slot,
            }
        }
        None => CursorState::default(),
    })
}

/// Records progress mid-walk: `backfill_before` (the oldest signature of the most recently fully-
/// committed page -- the next call's `before` bound), the walk's own `head_sig`/`head_slot` (unchanged
/// once first set), and the newest slot this source's RPC reported at the walk's very first page
/// (`source_max_slot`, monotonic — `COALESCE` never regresses it). Never touches `last_sig`/`last_slot`:
/// those only move once [`finish_walk`] runs. Runs in the caller's own transaction, same commit as the
/// page of rows it follows (`ingest::run_once`).
pub async fn advance_backfill_progress(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    kind: ProgramKind,
    backfill_before: &str,
    head_sig: &str,
    head_slot: i64,
    source_max_slot: Option<i64>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO settlement_cursor
             (kind, backfill_before, backfill_head_sig, backfill_head_slot, source_max_slot, updated_at)
         VALUES ($1, $2, $3, $4, $5, now())
         ON CONFLICT (kind) DO UPDATE SET
             backfill_before = EXCLUDED.backfill_before,
             backfill_head_sig = EXCLUDED.backfill_head_sig,
             backfill_head_slot = EXCLUDED.backfill_head_slot,
             source_max_slot = GREATEST(
                 COALESCE(EXCLUDED.source_max_slot, settlement_cursor.source_max_slot),
                 COALESCE(settlement_cursor.source_max_slot, EXCLUDED.source_max_slot)
             ),
             updated_at = now()",
    )
    .bind(kind.as_str())
    .bind(backfill_before)
    .bind(head_sig)
    .bind(head_slot)
    .bind(source_max_slot)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Establishes the walk-in-progress marker: `backfill_head_sig`/`backfill_head_slot`
/// only, leaving `backfill_before` untouched (it stays `NULL` -- nothing has committed yet). Called in the
/// SAME transaction as the first row this walk ever commits (`ingest::run_once`) -- writing it any later
/// left a window where already-committed rows existed with no walk-in-progress marker to gate
/// `lifecycle::derive_once` on (the derive query's `backfill_head_sig IS
/// NULL` gate cannot tell "nothing has been ingested yet" apart from "a walk crashed after committing rows
/// but before this marker existed" unless the two are atomic). A no-op if a walk is already marked in
/// progress (the caller only calls this once per fresh walk, guarded by `cursor.backfill_head_sig` having
/// been `None` at `run_once`'s own start).
pub async fn write_backfill_head(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    kind: ProgramKind,
    head_sig: &str,
    head_slot: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO settlement_cursor (kind, backfill_head_sig, backfill_head_slot, updated_at)
         VALUES ($1, $2, $3, now())
         ON CONFLICT (kind) DO UPDATE SET
             backfill_head_sig = EXCLUDED.backfill_head_sig,
             backfill_head_slot = EXCLUDED.backfill_head_slot,
             updated_at = now()",
    )
    .bind(kind.as_str())
    .bind(head_sig)
    .bind(head_slot)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// A walk has reached its end (an empty or short page, `getSignaturesForAddress`'s own "reached `until`
/// or genesis" signal): promote `head_sig`/`head_slot` to the durable `last_sig`/`last_slot` checkpoint
/// and clear the in-progress markers. Runs in the caller's own transaction, same commit as the final page
/// of rows (or alone, if the walk found nothing new at all).
pub async fn finish_walk(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    kind: ProgramKind,
    head_sig: &str,
    head_slot: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO settlement_cursor (kind, last_sig, last_slot, updated_at)
         VALUES ($1, $2, $3, now())
         ON CONFLICT (kind) DO UPDATE SET
             last_sig = EXCLUDED.last_sig,
             last_slot = EXCLUDED.last_slot,
             backfill_before = NULL,
             backfill_head_sig = NULL,
             backfill_head_slot = NULL,
             updated_at = now()",
    )
    .bind(kind.as_str())
    .bind(head_sig)
    .bind(head_slot)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The derive pass's own durable position — the last `settlement_tx` row (in `(slot, id)` order) whose
/// lifecycle events have been applied. Below-any-real-row defaults (`slot = -1, id = 0`) mean "nothing
/// derived yet" without needing an `Option`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeriveCursorState {
    pub last_slot: i64,
    pub last_id: i64,
}

impl Default for DeriveCursorState {
    fn default() -> Self {
        Self {
            last_slot: -1,
            last_id: 0,
        }
    }
}

pub async fn read_derive_cursor(
    pool: &PgPool,
    kind: ProgramKind,
) -> Result<DeriveCursorState, sqlx::Error> {
    let row: Option<(i64, i64)> =
        sqlx::query_as("SELECT last_slot, last_id FROM derive_cursor WHERE kind = $1")
            .bind(kind.as_str())
            .fetch_optional(pool)
            .await?;
    Ok(match row {
        Some((last_slot, last_id)) => DeriveCursorState { last_slot, last_id },
        None => DeriveCursorState::default(),
    })
}

/// Advances the derive cursor in the caller's own transaction — the same commit as the lifecycle
/// mutations (`inbox_chunk`/`batch` upserts) it follows, so a crash mid-derive-pass never leaves the
/// cursor ahead of state it did not actually apply.
pub async fn advance_derive_cursor(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    kind: ProgramKind,
    last_slot: i64,
    last_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO derive_cursor (kind, last_slot, last_id, updated_at)
         VALUES ($1, $2, $3, now())
         ON CONFLICT (kind) DO UPDATE SET
             last_slot = EXCLUDED.last_slot,
             last_id = EXCLUDED.last_id,
             updated_at = now()",
    )
    .bind(kind.as_str())
    .bind(last_slot)
    .bind(last_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
