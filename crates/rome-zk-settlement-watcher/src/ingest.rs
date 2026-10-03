//! The ingest loop: per-program cursor, paged `getSignaturesForAddress`, `getTransaction`
//! per new signature, UNNEST-batched writes into `settlement_tx`/`settlement_tx_program` only —
//! `inbox_chunk`/`batch` lifecycle mutations are a *separate* pass (`crate::lifecycle::derive_once`)
//! over already-ingested rows in true `(slot, id)` order, so the order this loop
//! happens to observe signatures in (newest-first across pages during a cold backfill) never has to match
//! the order their on-chain effects actually happened in. This loop also persists each row's already-
//! decoded lifecycle events (`settlement_tx_program.events`) so the derive pass never has to
//! re-fetch or re-decode the transaction body from Solana a second time.
//!
//! [`run_once`] walks backward (`before`) from wherever the durable cursor left off, down to
//! `cursor.last_sig` (or to genesis, on a brand-new cursor), committing rows in `commit_batch_size`-sized
//! chunks as soon as they are safe to commit (a cold walk against Tiber's
//! ≈1–3.6M-signature inbox history used to accumulate the *entire* backlog before committing anything at
//! all). A crash mid-walk resumes from the durable `backfill_before` marker (at most one committed batch's
//! worth of redundant, idempotent re-work), never from the very tip again; `last_sig`/`last_slot` only
//! advance once the whole walk reaches its end (an empty or short page).
//!
//! **A page boundary never splits a Solana slot.** `getSignaturesForAddress` pages are
//! newest-first with no guarantee that one slot's signatures all land in one page; naively committing each
//! fetched page as its own unit lets a slot's later signature land in an *earlier*-committed page (since a
//! backward walk commits newer pages first), giving it the *lower* `settlement_tx.id` despite being
//! chronologically later -- inverting `(slot, id)` order for exactly that pair (a same-slot `Open`/`Seal`
//! split across a `rpc_page_size = 1` boundary derived unsealed). This
//! loop instead buffers a trailing run of signatures sharing the *oldest slot seen so far* across as many
//! page fetches as it takes to observe a strictly older signature (confirming the run is complete) -- only
//! then does that whole run commit, together, reversed to on-chain order. The buffer is in-memory only
//! (never durable): a crash while a run is buffered simply re-fetches it from `backfill_before` next time,
//! same as any other uncommitted work.

use crate::cursor::{
    advance_backfill_progress, finish_walk, read_cursor, write_backfill_head, ProgramKind,
};
use crate::decode::{decode_inbox_tx, decode_settlement_tx, DecodedTx, DerivedEvents};
use crate::rpc::{SignatureInfo, Source, SourceError};
use solana_program::pubkey::Pubkey;
use solana_transaction_status_client_types::TransactionConfirmationStatus;
use sqlx::PgPool;
use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(
        "getTransaction returned no record for {0} after retrying -- treating the page as failed \
         rather than silently skipping the row (Agave returns null on a transient Bigtable read error)"
    )]
    MissingTransactionBody(String),
    #[error(
        "settlement_tx_program row for settlement_tx_id={settlement_tx_id} ({kind}) could not be \
         decoded as DerivedEvents -- skipping this row's lifecycle events for this derive pass rather \
         than aborting (the events payload is versioned and additive-only; a genuine format break means \
         this row predates a decoder change). Repair: re-ingest the row \
         (`ON CONFLICT (settlement_tx_id, kind) DO UPDATE SET events, events_version`) then reset \
         `derive_cursor` to re-derive it"
    )]
    UndecodableEvents { settlement_tx_id: i64, kind: String },
}

/// The `settlement_tx_program.events_version` this crate writes at ingest time -- the compatibility rule is
/// additive-only: a new field on an existing `ChunkEvent`/`BatchEvent` variant is `#[serde(default)]` and does not
/// require bumping this constant; a renamed/removed variant or a changed field type does.
pub const DERIVED_EVENTS_VERSION: i16 = 1;

/// How many times a `None` `getTransaction` body for a signature `getSignaturesForAddress` already listed
/// is retried before this pass gives up and fails the whole page (Agave's own
/// `bigtable_ledger_storage` read maps a transient storage error to `null`, indistinguishable on the wire
/// from "never existed" -- a signature already listed can never be the latter).
pub const NULL_BODY_MAX_RETRIES: u32 = 3;
pub const NULL_BODY_RETRY_DELAY: Duration = Duration::from_millis(50);

/// Tuning knobs independent of the on-chain wire format: how many signatures one
/// `getSignaturesForAddress` call requests (bounded by Solana's own 1,000/call ceiling), and how many
/// signatures' worth of writes land in one DB transaction before the cursor advances.
#[derive(Debug, Clone, Copy)]
pub struct WatcherConfig {
    pub rpc_page_size: usize,
    pub commit_batch_size: usize,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            rpc_page_size: 1_000,
            commit_batch_size: 500,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageOutcome {
    NoNewSignatures,
    Processed {
        signatures: usize,
        commits: usize,
        last_sig: String,
        last_slot: i64,
    },
}

/// Confirmation-status string for `settlement_tx.status` -- `None` (a source that does not report it)
/// is treated as `confirmed`, the least commitment `getSignaturesForAddress` can ever return (see
/// `rpc::SignatureInfo::confirmation_status`'s own doc).
fn status_str(s: &Option<TransactionConfirmationStatus>) -> &'static str {
    match s {
        Some(TransactionConfirmationStatus::Finalized) => "finalized",
        Some(TransactionConfirmationStatus::Processed)
        | Some(TransactionConfirmationStatus::Confirmed)
        | None => "confirmed",
    }
}

/// Decodes one signature's transaction body via `source.get_transaction`, retrying a `None` body up to
/// [`NULL_BODY_MAX_RETRIES`] times before giving up with a named, terminal [`IngestError`]
/// -- a signature `getSignaturesForAddress` itself just listed must never be silently dropped from
/// the feed. Returns the decoded instruction data alongside whether the transaction itself failed
/// on-chain (`RawTx::err`): a failed transaction changed nothing, so `ingest::write_page` persists no
/// lifecycle events for it (mirroring what the old re-fetching `derive_once` used to check for itself).
async fn fetch_and_decode<S: Source>(
    source: &mut S,
    program_id: &Pubkey,
    settlement_program: &Pubkey,
    kind: ProgramKind,
    sig_info: &SignatureInfo,
) -> Result<(DecodedTx, bool), IngestError> {
    let program_id_str = program_id.to_string();
    let mut attempt = 0u32;
    loop {
        match source.get_transaction(&sig_info.signature).await? {
            Some(tx) => {
                let decoded = match kind {
                    ProgramKind::Inbox => {
                        decode_inbox_tx(&tx.message, &program_id_str, settlement_program)
                    }
                    ProgramKind::Root => decode_settlement_tx(&tx.message, &program_id_str),
                };
                return Ok((decoded, tx.err));
            }
            None if attempt < NULL_BODY_MAX_RETRIES => {
                attempt += 1;
                tracing::warn!(
                    signature = %sig_info.signature,
                    attempt,
                    "getTransaction returned no record for a signature getSignaturesForAddress just \
                     listed; retrying (transient Bigtable-null, not a terminal skip)"
                );
                tokio::time::sleep(NULL_BODY_RETRY_DELAY).await;
            }
            None => {
                return Err(IngestError::MissingTransactionBody(
                    sig_info.signature.clone(),
                ));
            }
        }
    }
}

/// Runs one walk step for `kind` (against `program_id`): resumes an in-progress walk from the durable
/// cursor (whether that is a cold backfill or a bounded steady-state poll since the last checkpoint),
/// fetching pages until the walk reaches its end (an empty or short page), at which point `last_sig`/
/// `last_slot` advance to the walk's own head. Call it again on a timer for the steady-state "keep up with
/// the tip" loop; a very large backlog may take many calls' worth of RPC round trips to finish its first
/// walk, but every batch committed along the way is durable (never re-walked from scratch after a crash).
///
/// `settlement_program` is the settlement program this watcher follows. For `ProgramKind::Inbox` it decides which
/// inbox instructions belong to the chain (see `decode::decode_inbox_tx`); for `ProgramKind::Root` it is the same
/// program as `program_id`.
pub async fn run_once<S: Source>(
    pool: &PgPool,
    source: &mut S,
    program_id: &Pubkey,
    settlement_program: &Pubkey,
    kind: ProgramKind,
    cfg: WatcherConfig,
) -> Result<PageOutcome, IngestError> {
    let cursor = read_cursor(pool, kind).await?;
    let until = cursor.last_sig.clone();
    let mut before = cursor.backfill_before.clone();
    let mut head: Option<(String, i64)> =
        match (&cursor.backfill_head_sig, cursor.backfill_head_slot) {
            (Some(sig), Some(slot)) => Some((sig.clone(), slot)),
            _ => None,
        };
    // Whether the walk-in-progress marker is already durable -- either this is a fresh walk (written the
    // first time a batch commits below) or we are resuming one that already wrote it.
    let mut head_marker_persisted = cursor.backfill_head_sig.is_some();
    let mut source_max_slot: Option<i64> = None;

    let mut total_signatures = 0usize;
    let mut commits = 0usize;
    let mut last_committed: Option<(String, i64)> = None;

    // Entries fetched but not yet safe to commit -- newest-first, possibly spanning more than one RPC
    // page when a slot's own signature count exceeds `rpc_page_size` (see the module doc: a page boundary
    // must never split a slot).
    let mut pending: Vec<SignatureInfo> = Vec::new();

    loop {
        let page = source
            .get_signatures_for_address(
                program_id,
                before.clone(),
                until.clone(),
                cfg.rpc_page_size,
            )
            .await?;
        let reached_end = page.len() < cfg.rpc_page_size;

        if head.is_none() {
            if let Some(first) = page.first() {
                head = Some((first.signature.clone(), first.slot as i64));
            }
        }
        if source_max_slot.is_none() {
            source_max_slot = page.first().map(|s| s.slot as i64);
        }
        if let Some(last) = page.last() {
            before = Some(last.signature.clone());
        }
        pending.extend(page);

        // Cut the safe-to-commit prefix off the front of `pending`: everything except a trailing run that
        // still shares the oldest signature's slot (that run might grow if more of the same slot lands in
        // the next fetch). `pending` is kept newest-first throughout, so entries strictly newer than the
        // trailing run's slot are provably complete already (Solana reports signatures in non-increasing
        // slot order).
        let to_commit: Vec<SignatureInfo> = if reached_end {
            // No more history (or `until`) beyond this fetch -- the whole buffer is now definitely
            // complete, trailing same-slot run included.
            std::mem::take(&mut pending)
        } else if pending.is_empty() {
            Vec::new()
        } else {
            let oldest_slot = pending.last().expect("checked non-empty above").slot;
            match pending.iter().rposition(|s| s.slot != oldest_slot) {
                Some(cut) => pending.drain(..=cut).collect(),
                // The whole buffer shares one slot -- cannot confirm the run is complete yet; keep
                // accumulating (the "fall back to the whole page" case, generalized across
                // fetches rather than bounded to one page).
                None => Vec::new(),
            }
        };

        if !to_commit.is_empty() {
            // The oldest entry actually committed this round -- the new `backfill_before` bound. Never
            // the raw page's own oldest entry, which may still be sitting uncommitted in `pending`.
            let commit_marker = to_commit.last().map(|s| s.signature.clone());

            let mut oldest_first = to_commit;
            oldest_first.reverse();

            for chunk in oldest_first.chunks(cfg.commit_batch_size.max(1)) {
                let mut rows: Vec<(SignatureInfo, DecodedTx, bool)> =
                    Vec::with_capacity(chunk.len());
                for sig_info in chunk {
                    let (decoded, tx_err) =
                        fetch_and_decode(source, program_id, settlement_program, kind, sig_info)
                            .await?;
                    rows.push((sig_info.clone(), decoded, tx_err));
                }
                let last = chunk.last().expect("chunk is non-empty");

                let mut db_tx = pool.begin().await?;
                write_page(&mut db_tx, kind, &rows).await?;
                if !head_marker_persisted {
                    let (h, hs) = head
                        .clone()
                        .expect("head is set once the first page is fetched");
                    write_backfill_head(&mut db_tx, kind, &h, hs).await?;
                    head_marker_persisted = true;
                }
                db_tx.commit().await?;

                total_signatures += rows.len();
                commits += 1;
                last_committed = Some((last.signature.clone(), last.slot as i64));
            }

            if let (Some((h, hs)), Some(marker)) = (&head, &commit_marker) {
                let mut db_tx = pool.begin().await?;
                advance_backfill_progress(&mut db_tx, kind, marker, h, *hs, source_max_slot)
                    .await?;
                db_tx.commit().await?;
            }
        }

        if reached_end {
            break;
        }
    }

    if let Some((h, hs)) = &head {
        let mut db_tx = pool.begin().await?;
        finish_walk(&mut db_tx, kind, h, *hs).await?;
        db_tx.commit().await?;
    }

    match last_committed {
        Some((last_sig, last_slot)) => Ok(PageOutcome::Processed {
            signatures: total_signatures,
            commits,
            last_sig,
            last_slot,
        }),
        None => Ok(PageOutcome::NoNewSignatures),
    }
}

/// One DB transaction's worth of raw ingest: a bulk UNNEST insert into `settlement_tx`, then one row per
/// `(tx, kind)` into `settlement_tx_program` carrying that row's already-decoded
/// lifecycle events as JSON so `crate::lifecycle::derive_once` never re-fetches or
/// re-decodes the transaction body. Writes no derived state (`inbox_chunk`/`batch`) itself -- that is
/// `derive_once`'s job, run as its own pass so this function's commit order (arrival order, which during a
/// backward walk is newest-first) never has to match on-chain chronological order. Never opens or commits
/// `tx` itself -- the caller (`run_once`) owns the transaction boundary.
pub(crate) async fn write_page(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    kind: ProgramKind,
    rows: &[(SignatureInfo, DecodedTx, bool)],
) -> Result<(), sqlx::Error> {
    let sigs: Vec<String> = rows.iter().map(|(s, _, _)| s.signature.clone()).collect();
    let slots: Vec<i64> = rows.iter().map(|(s, _, _)| s.slot as i64).collect();
    let block_times: Vec<Option<i64>> = rows.iter().map(|(s, _, _)| s.block_time).collect();
    let signers: Vec<String> = rows
        .iter()
        .map(|(_, d, _)| d.signer.clone().unwrap_or_default())
        .collect();
    let statuses: Vec<&str> = rows
        .iter()
        .map(|(s, _, _)| status_str(&s.confirmation_status))
        .collect();

    // UNNEST-batched insert: one round trip
    // for the whole chunk instead of one INSERT per signature. `ON CONFLICT (sig) DO NOTHING` is the
    // idempotency guard a re-processed page (a retried chunk, or this same page replayed by the replay
    // test) relies on -- flip it to a plain INSERT and a duplicate-signature re-run fails on the UNIQUE
    // constraint instead of silently doing nothing (mutation-tested, `tests/replay.rs`).
    sqlx::query(
        "INSERT INTO settlement_tx (sig, slot, block_time, signer, status)
         SELECT * FROM UNNEST($1::text[], $2::bigint[], $3::bigint[], $4::text[], $5::text[])
         ON CONFLICT (sig) DO NOTHING",
    )
    .bind(&sigs)
    .bind(&slots)
    .bind(&block_times)
    .bind(&signers)
    .bind(&statuses)
    .execute(&mut **tx)
    .await?;

    let id_rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, sig FROM settlement_tx WHERE sig = ANY($1)")
            .bind(&sigs)
            .fetch_all(&mut **tx)
            .await?;
    let id_by_sig: HashMap<String, i64> = id_rows.into_iter().map(|(id, sig)| (sig, id)).collect();

    let mut tx_ids: Vec<i64> = Vec::with_capacity(rows.len());
    let mut kinds: Vec<&str> = Vec::with_capacity(rows.len());
    let mut ix_kinds: Vec<String> = Vec::with_capacity(rows.len());
    let mut chain_ids: Vec<Option<i64>> = Vec::with_capacity(rows.len());
    let mut batch_ids: Vec<Option<i64>> = Vec::with_capacity(rows.len());
    let mut events_json: Vec<String> = Vec::with_capacity(rows.len());
    let mut events_versions: Vec<i16> = Vec::with_capacity(rows.len());

    for (sig_info, decoded, tx_err) in rows {
        // A sig this call did not insert lost the `ON CONFLICT` race to a PRIOR commit of the same
        // signature -- never expected in normal operation (a signature is only ever offered to one
        // chunk within one walk), but a re-run of this exact chunk (the replay/idempotency test does
        // this deliberately) hits this path: there is nothing new to attribute for a row this call did
        // not (re-)insert, since a `settlement_tx_program` row already exists for it from the first run.
        let Some(&tx_id) = id_by_sig.get(&sig_info.signature) else {
            continue;
        };
        tx_ids.push(tx_id);
        kinds.push(kind.as_str());
        ix_kinds.push(if decoded.ix_names.is_empty() {
            "Unknown".to_string()
        } else {
            decoded.ix_names.join("+")
        });
        chain_ids.push(decoded.chain_id.map(|c| c as i64));
        batch_ids.push(decoded.batch_id.map(|b| b as i64));

        // A failed transaction changed nothing on-chain -- persist no lifecycle events for it (the
        // settlement_tx/settlement_tx_program row above still records that it happened, so the explorer
        // can show it, but `lifecycle::derive_once` must derive nothing from it -- mirrors what the old
        // re-fetching derive pass used to check via `RawTx::err` itself).
        let derived = if *tx_err {
            DerivedEvents::default()
        } else {
            DerivedEvents {
                chunk_events: decoded.chunk_events.clone(),
                batch_events: decoded.batch_events.clone(),
                exit_events: decoded.exit_events.clone(),
            }
        };
        events_json.push(serde_json::to_string(&derived).expect("DerivedEvents always serializes"));
        events_versions.push(DERIVED_EVENTS_VERSION);
    }

    if !tx_ids.is_empty() {
        sqlx::query(
            "INSERT INTO settlement_tx_program
                 (settlement_tx_id, kind, ix_kind, chain_id, batch_id, events, events_version)
             SELECT tx_id, k, ixk, cid, bid, ev::jsonb, ver
             FROM UNNEST($1::bigint[], $2::text[], $3::text[], $4::bigint[], $5::bigint[], $6::text[], $7::smallint[])
                 AS u(tx_id, k, ixk, cid, bid, ev, ver)
             ON CONFLICT (settlement_tx_id, kind) DO NOTHING",
        )
        .bind(&tx_ids)
        .bind(&kinds)
        .bind(&ix_kinds)
        .bind(&chain_ids)
        .bind(&batch_ids)
        .bind(&events_json)
        .bind(&events_versions)
        .execute(&mut **tx)
        .await?;
    }

    Ok(())
}
