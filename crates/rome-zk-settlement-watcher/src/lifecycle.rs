//! The derive pass: applies inbox lifecycle mutations
//! (`inbox_chunk`/`batch`) from already-ingested `settlement_tx`/`settlement_tx_program` rows, in true
//! `(slot, id)` order, tracked by its own `derive_cursor` -- entirely independent of `ingest::run_once`'s
//! own arrival order (which, during a cold backward walk, is newest-first across pages: applying
//! lifecycle events in *that* order would run a `Seal` before its `Open`, or a later partial
//! `FinalizeBatch` before an earlier one).
//!
//! **Reads only the database, never Solana.** `ingest::write_page` already decoded
//! and persisted each row's `ChunkEvent`/`BatchEvent` list as JSON on `settlement_tx_program.events`; this
//! pass deserializes that column instead of re-fetching the transaction body a second time. The old
//! "feed, not shadow table" design re-derived from a fresh `get_transaction` per row -- doubling RPC load
//! and, worse, exposed to Agave mapping a transient Bigtable read error to a bare `null`: a row that
//! genuinely exists in `settlement_tx` (ingest saw it) could still silently lose its lifecycle event
//! forever on a second, unlucky fetch. Persisting the decode once, at
//! ingest time, removes that exposure entirely rather than adding a retry rule to a second fetch.
//!
//! **Gated on feed completeness.** The row-fetch query below joins
//! `settlement_cursor` and only returns rows while `backfill_head_sig IS NULL` for the ingest cursor of
//! the same kind -- i.e. no ingest walk is currently in progress. An ingest walk that fails partway
//! through (an RPC timeout on an older page, the ordinary case on a multi-million-signature cold start)
//! must never let this pass advance past rows a not-yet-ingested older page will eventually supply: the
//! walk-in-progress marker (`ingest::run_once` / `cursor::write_backfill_head`) stays durable exactly
//! until the walk finishes, so this gate and that marker share one truth (before this fix, an ingest error
//! mid-walk let the derive cursor jump straight to the newest ingested
//! slot, and every older, not-yet-ingested page was excluded forever once it did land).
//!
//! Every write here mirrors `programs/zk-inbox/src/batch.rs` exactly:
//! - `Open` inserts an `inbox_chunk` row keyed by `chunk_pda`, never re-inserted or overwritten by a later
//!   `Seal` (which carries no `(chain_id, batch, idx)` of its own -- `decode::decode_inbox_tx` already
//!   refuses to invent one).
//! - `Seal` only ever `UPDATE`s an existing `inbox_chunk` row by `chunk_pda`; a `Seal`-only transaction
//!   with no matching row is a genuine no-op (0 rows affected), not a fabricated one.
//! - `Close` (rent reclaimed) sets `inbox_chunk.closed_tx`, never deletes the row.
//! - `FinalizeBatch { step }` mirrors `finalize_batch_inner`'s own `cursor`/`cap`/`end` arithmetic
//!   (`batch.rs`): `step == 0` means "the rest, in this call"; `status` only becomes `'finalized'` once
//!   the cursor reaches `expected_count`. Applying the same
//!   `(batch_pda, settlement_tx_id)` step twice (a `derive_cursor` reset for repair) must not double-count
//!   it -- `batch_finalize_step` is the idempotency ledger for that.
//! - `AbandonBatch`/`CloseBatch` write the orthogonal `abandoned_tx`/`closed_tx` columns, never
//!   `finalized_tx` and never a fourth `status` value (`status = 'closed'`
//!   had no arm in the `block_status` view, regressing every recycled batch to `'sequenced'`).

use crate::cursor::{advance_derive_cursor, read_cursor, read_derive_cursor, ProgramKind};
use crate::decode::{BatchEvent, ChunkEvent, ChunkEventKind, DerivedEvents, ExitEvent};
use crate::ingest::IngestError;
use sqlx::PgPool;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeriveOutcome {
    NoNewRows,
    /// An ingest walk is currently in progress for this kind (`settlement_cursor.backfill_head_sig` is
    /// set) -- the feed below the derive cursor may still be missing an older, not-yet-ingested page, so
    /// this pass refuses to advance at all until the walk finishes.
    IngestWalkInProgress,
    Processed {
        rows: usize,
    },
}

/// Applies lifecycle events for up to `limit` `settlement_tx` rows past the derive cursor, in `(slot, id)` order, in
/// one DB transaction alongside the cursor advance (so a crash mid-pass never leaves the cursor ahead of state it did
/// not actually apply). Call it again on a timer, same as `ingest::run_once` -- but never in an iteration whose
/// `ingest::run_once` call itself returned `Err` (the gate below also catches this case, since a failed ingest call
/// never clears `backfill_head_sig`, but the binary skips the call entirely as a second, independent guard).
pub async fn derive_once(pool: &PgPool, limit: i64) -> Result<DeriveOutcome, IngestError> {
    let cursor = read_derive_cursor(pool, ProgramKind::Inbox).await?;
    // The gate lives in this one statement: joining `settlement_cursor` with
    // `backfill_head_sig IS NULL` means a row is only ever returned once the ingest walk that produced it
    // (and every walk before it) has fully finished -- there is no separate check-then-fetch race for a
    // concurrent ingest call to land inside.
    let rows: Vec<(i64, i64, String)> = sqlx::query_as(
        "SELECT st.id, st.slot, stp.events::text
         FROM settlement_tx st
         JOIN settlement_tx_program stp ON stp.settlement_tx_id = st.id
         JOIN settlement_cursor sc ON sc.kind = 'inbox' AND sc.backfill_head_sig IS NULL
         WHERE stp.kind = 'inbox' AND (st.slot, st.id) > ($1, $2)
         ORDER BY st.slot ASC, st.id ASC
         LIMIT $3",
    )
    .bind(cursor.last_slot)
    .bind(cursor.last_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        // Distinguish "genuinely caught up" from "gated by an in-progress walk" -- a plain read, not
        // load-bearing for correctness (the query above already enforced the gate atomically); it only
        // decides which empty-result variant to report.
        let ingest_cursor = read_cursor(pool, ProgramKind::Inbox).await?;
        return Ok(if ingest_cursor.backfill_head_sig.is_some() {
            DeriveOutcome::IngestWalkInProgress
        } else {
            DeriveOutcome::NoNewRows
        });
    }

    let mut db_tx = pool.begin().await?;
    for (tx_id, _slot, events_json) in &rows {
        // A row this crate itself wrote can still become undecodable by a LATER binary -- the repair path resets
        // `derive_cursor` and re-derives the OLDEST rows with the NEWEST decoder, which is exactly where a
        // renamed/removed `ChunkEventKind`/`BatchEvent` variant (a real break, not covered by the additive-only
        // `#[serde(default)]` rule) would bite. This must never abort the whole pass -- one bad row is logged by name
        // and skipped (its own lifecycle events are not applied), and the pass keeps making progress on every row it
        // does understand.
        match decode_events(*tx_id, events_json) {
            Ok(derived) => {
                apply_chunk_events(&mut db_tx, *tx_id, &derived.chunk_events).await?;
                apply_batch_events(&mut db_tx, *tx_id, &derived.batch_events).await?;
            }
            Err(err) => {
                tracing::error!(%err, "skipping this row's lifecycle events for this derive pass");
            }
        }
    }

    let last = rows.last().expect("checked non-empty above");
    let (last_id, last_slot) = (last.0, last.1);
    advance_derive_cursor(&mut db_tx, ProgramKind::Inbox, last_slot, last_id).await?;
    db_tx.commit().await?;
    Ok(DeriveOutcome::Processed { rows: rows.len() })
}

/// The exit-lifecycle derive pass: applies `exit` row mutations
/// (`ProveExit`/`ConsumeExit` events) from already-ingested `settlement_tx_program` rows of the
/// SETTLEMENT program's own ingest kind (`ProgramKind::Root` -- `ProveExit`/`ConsumeExit` are
/// instructions of the same on-chain program `PostRoot`/`FinalizeBatch`/etc. already ingest under, never
/// a separate ingest source), in true `(slot, id)` order, tracked by its own `derive_cursor` row (`kind =
/// 'root'`) -- entirely independent of [`derive_once`]'s own `'inbox'`-kind cursor. Same shape as
/// `derive_once`: gated on `settlement_cursor`'s own `backfill_head_sig IS NULL` for the SAME kind,
/// one bad row's events skipped and logged by name rather than aborting the whole pass.
pub async fn derive_exit_once(pool: &PgPool, limit: i64) -> Result<DeriveOutcome, IngestError> {
    let cursor = read_derive_cursor(pool, ProgramKind::Root).await?;
    let rows: Vec<(i64, i64, String)> = sqlx::query_as(
        "SELECT st.id, st.slot, stp.events::text
         FROM settlement_tx st
         JOIN settlement_tx_program stp ON stp.settlement_tx_id = st.id
         JOIN settlement_cursor sc ON sc.kind = 'root' AND sc.backfill_head_sig IS NULL
         WHERE stp.kind = 'root' AND (st.slot, st.id) > ($1, $2)
         ORDER BY st.slot ASC, st.id ASC
         LIMIT $3",
    )
    .bind(cursor.last_slot)
    .bind(cursor.last_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        let ingest_cursor = read_cursor(pool, ProgramKind::Root).await?;
        return Ok(if ingest_cursor.backfill_head_sig.is_some() {
            DeriveOutcome::IngestWalkInProgress
        } else {
            DeriveOutcome::NoNewRows
        });
    }

    let mut db_tx = pool.begin().await?;
    for (tx_id, _slot, events_json) in &rows {
        match decode_events(*tx_id, events_json) {
            Ok(derived) => {
                apply_exit_events(&mut db_tx, *tx_id, &derived.exit_events).await?;
            }
            Err(err) => {
                tracing::error!(%err, "skipping this row's exit lifecycle events for this derive pass");
            }
        }
    }

    let last = rows.last().expect("checked non-empty above");
    let (last_id, last_slot) = (last.0, last.1);
    advance_derive_cursor(&mut db_tx, ProgramKind::Root, last_slot, last_id).await?;
    db_tx.commit().await?;
    Ok(DeriveOutcome::Processed { rows: rows.len() })
}

/// `ProveExit` inserts a fresh `proved` row keyed by `(chain_id, message_hash)` -- never re-inserted or
/// overwritten by a later duplicate (`ON CONFLICT DO NOTHING`, matching `inbox_chunk`'s own `Open`
/// pattern); `ConsumeExit` only ever `UPDATE`s an existing row to `released`, resolving purely by that same
/// key (a `ConsumeExit` with no matching row -- should never happen against a real chain, since
/// `ConsumeExit` itself refuses `ExitNotProved` for anything that was never proved -- is a genuine no-op,
/// 0 rows affected, exactly the same shape `Seal`-before-`Open` gets in `apply_chunk_events`).
async fn apply_exit_events(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tx_id: i64,
    events: &[ExitEvent],
) -> Result<(), sqlx::Error> {
    for event in events {
        match event {
            ExitEvent::Proved {
                chain_id,
                batch,
                message_hash,
                sol_recipient,
                amount,
            } => {
                sqlx::query(
                    "INSERT INTO exit
                         (chain_id, message_hash, batch, sol_recipient, amount, proved_sig, status)
                     VALUES ($1, $2, $3, $4, $5, (SELECT sig FROM settlement_tx WHERE id = $6), 'proved')
                     ON CONFLICT (message_hash) DO NOTHING",
                )
                .bind(*chain_id as i64)
                .bind(message_hash.to_vec())
                .bind(*batch as i64)
                .bind(sol_recipient.to_vec())
                .bind(amount.to_string())
                .bind(tx_id)
                .execute(&mut **tx)
                .await?;
            }
            ExitEvent::Released {
                message_hash,
                chain_id,
            } => {
                sqlx::query(
                    "UPDATE exit SET status = 'released',
                         released_sig = (SELECT sig FROM settlement_tx WHERE id = $1)
                     WHERE message_hash = $2 AND chain_id = $3 AND status = 'proved'",
                )
                .bind(tx_id)
                .bind(message_hash.to_vec())
                .bind(*chain_id as i64)
                .execute(&mut **tx)
                .await?;
            }
        }
    }
    Ok(())
}

/// Decodes one row's `settlement_tx_program.events` JSON, naming the row (`settlement_tx_id`, the ingest
/// `kind` this pass always reads -- `'inbox'` today) in a structured [`IngestError::UndecodableEvents`]
/// on failure rather than a bare `expect` panic.
fn decode_events(tx_id: i64, events_json: &str) -> Result<DerivedEvents, IngestError> {
    serde_json::from_str(events_json).map_err(|e| IngestError::UndecodableEvents {
        settlement_tx_id: tx_id,
        kind: e.to_string(),
    })
}

async fn apply_chunk_events(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tx_id: i64,
    events: &[ChunkEvent],
) -> Result<(), sqlx::Error> {
    for event in events {
        match &event.kind {
            ChunkEventKind::Opened {
                chain_id,
                batch,
                idx,
                byte_len,
            } => {
                let chunk_id = format!("{chain_id}-{batch}-{idx}");
                sqlx::query(
                    "INSERT INTO inbox_chunk
                         (chunk_id, chunk_pda, settlement_tx_id, chain_id, batch_id, idx, byte_len, sealed)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, FALSE)
                     ON CONFLICT (chunk_pda) DO NOTHING",
                )
                .bind(&chunk_id)
                .bind(&event.chunk_pda)
                .bind(tx_id)
                .bind(*chain_id as i64)
                .bind(*batch as i64)
                .bind(*idx as i64)
                .bind(*byte_len as i64)
                .execute(&mut **tx)
                .await?;
            }
            ChunkEventKind::Sealed {
                byte_len,
                body_hash,
            } => {
                // Attributed purely by the chunk PDA account -- never
                // inserts. A Seal-only transaction with no prior Open for this chunk is a genuine no-op:
                // 0 rows affected, not a fabricated (chain_id, batch, idx) row.
                sqlx::query(
                    "UPDATE inbox_chunk SET byte_len = $1, sealed = TRUE, body_hash = $2
                     WHERE chunk_pda = $3",
                )
                .bind(*byte_len as i64)
                .bind(body_hash.to_vec())
                .bind(&event.chunk_pda)
                .execute(&mut **tx)
                .await?;
            }
            ChunkEventKind::Closed => {
                // Rent reclaimed: the row stays, `closed_tx` records that the DA it
                // names is no longer live on Solana's own state. Idempotent (`closed_tx IS NULL` guard) --
                // a re-derive after a cursor reset must not error re-setting the same value.
                sqlx::query(
                    "UPDATE inbox_chunk SET closed_tx = $1 WHERE chunk_pda = $2 AND closed_tx IS NULL",
                )
                .bind(tx_id)
                .bind(&event.chunk_pda)
                .execute(&mut **tx)
                .await?;
            }
        }
    }
    Ok(())
}

async fn apply_batch_events(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tx_id: i64,
    events: &[BatchEvent],
) -> Result<(), sqlx::Error> {
    for event in events {
        match event {
            BatchEvent::Opened {
                chain_id,
                batch,
                expected_count,
                batch_pda,
            } => {
                sqlx::query(
                    "INSERT INTO batch (chain_id, batch_id, opened_tx, expected_count, batch_pda, status)
                     VALUES ($1, $2, $3, $4, $5, 'open')
                     ON CONFLICT (chain_id, batch_id) DO NOTHING",
                )
                .bind(*chain_id as i64)
                .bind(*batch as i64)
                .bind(tx_id)
                .bind(*expected_count as i64)
                .bind(batch_pda)
                .execute(&mut **tx)
                .await?;
            }
            // GrowBatch changes no column this schema tracks (expected_count was already declared
            // at OpenBatch; the account growing toward it is an on-chain-only detail) -- recorded
            // only via the settlement_tx_program row's own ix_kind.
            BatchEvent::Grown { .. } => {}
            BatchEvent::Finalized { batch_pda, step } => {
                // Idempotency: apply this
                // (batch_pda, settlement_tx_id) step's arithmetic at most once, ever -- a re-derive after
                // a `derive_cursor` reset for repair must not double-count a step it already applied. The
                // ledger insert is the single source of truth for "already applied"; the cursor is allowed
                // to move backward in a repair, this ledger is not.
                let inserted = sqlx::query(
                    "INSERT INTO batch_finalize_step (batch_pda, settlement_tx_id, step)
                     VALUES ($1, $2, $3)
                     ON CONFLICT (batch_pda, settlement_tx_id) DO NOTHING",
                )
                .bind(batch_pda)
                .bind(tx_id)
                .bind(*step as i64)
                .execute(&mut **tx)
                .await?;
                if inserted.rows_affected() == 0 {
                    continue;
                }
                // Mirrors `programs/zk-inbox/src/batch.rs::finalize_batch_inner` exactly: `step == 0`
                // means "the rest, in this call" (cursor jumps to expected_count); a nonzero step advances
                // by at most that many leaves. `status` becomes 'finalized' only once the cursor reaches
                // expected_count -- the WHERE guard keeps applying across any number of partial calls
                // (status stays 'open' the whole time), so a later genuine AbandonBatch is still honored.
                sqlx::query(
                    "UPDATE batch SET
                         finalize_cursor = LEAST(expected_count,
                             CASE WHEN $1 = 0 THEN expected_count ELSE finalize_cursor + $1 END),
                         status = CASE WHEN LEAST(expected_count,
                             CASE WHEN $1 = 0 THEN expected_count ELSE finalize_cursor + $1 END) >= expected_count
                             THEN 'finalized' ELSE status END,
                         finalized_tx = CASE WHEN LEAST(expected_count,
                             CASE WHEN $1 = 0 THEN expected_count ELSE finalize_cursor + $1 END) >= expected_count
                             THEN $2 ELSE finalized_tx END
                     WHERE batch_pda = $3 AND status = 'open'",
                )
                .bind(*step as i64)
                .bind(tx_id)
                .bind(batch_pda)
                .execute(&mut **tx)
                .await?;
            }
            BatchEvent::Abandoned { batch_pda } => {
                // `abandoned_tx`, never `finalized_tx` (the two were once conflated,
                // so an abandon could overwrite the column a real finalize sets).
                sqlx::query(
                    "UPDATE batch SET status = 'abandoned', abandoned_tx = $1
                     WHERE batch_pda = $2 AND status = 'open'",
                )
                .bind(tx_id)
                .bind(batch_pda)
                .execute(&mut **tx)
                .await?;
            }
            BatchEvent::Closed { batch_pda } => {
                // Rent recycling never touches `status` -- a finalized or
                // abandoned batch reads exactly the same after `CloseBatch` as before it. Idempotent
                // (`closed_tx IS NULL` guard).
                sqlx::query(
                    "UPDATE batch SET closed_tx = $1
                     WHERE batch_pda = $2 AND status IN ('finalized', 'abandoned') AND closed_tx IS NULL",
                )
                .bind(tx_id)
                .bind(batch_pda)
                .execute(&mut **tx)
                .await?;
            }
        }
    }
    Ok(())
}
