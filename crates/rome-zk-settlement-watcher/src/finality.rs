//! Upgrades `settlement_tx.status` from `confirmed` toward its two terminal states by re-checking each
//! not-yet-terminal row's own signature against what Solana's `getSignatureStatuses` reports right now
//! (finality). `ingest.rs` writes every row at whatever `getSignaturesForAddress` reported at
//! insert time (never below `confirmed`); this pass is the one writer allowed to move a row's status
//! afterward: forward to `finalized` once Solana reports that confirmation status, or forward to
//! `dropped` once a signature Solana reports **no status at all** for has been behind the node's own
//! finalized slot for [`DROPPED_HORIZON_SLOTS`] slots — a status never moves
//! backward, and once a row reaches either terminal state this pass stops re-checking it, so a re-run
//! after a transient RPC hiccup is always safe to repeat.

use crate::ingest::IngestError;
use crate::rpc::Source;
use solana_transaction_status_client_types::TransactionConfirmationStatus;
use sqlx::PgPool;

/// Solana's own `getSignatureStatuses` cap per call.
pub const MAX_SIGNATURES_PER_CALL: usize = 256;

/// How far behind the node's current finalized slot a signature with **no** reported status (pruned,
/// never landed, or the node simply never saw it) must be before this pass calls it `dropped` — a
/// terminal state, so a signature is never re-checked forever.
pub const DROPPED_HORIZON_SLOTS: u64 = 150;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FinalityOutcome {
    pub checked: usize,
    pub upgraded: usize,
    pub dropped: usize,
}

/// Re-checks up to `limit` not-yet-terminal rows (oldest slot first -- the ones most likely to have
/// crossed into finality, or into the dropped horizon, since they were last checked), upgrading any Solana
/// now reports `finalized` and terminating any whose status has come back `None` for
/// [`DROPPED_HORIZON_SLOTS`] slots past the node's current finalized slot. Never touches
/// `settlement_tx_program` or any derived table -- this pass owns exactly one column.
pub async fn track_finality<S: Source>(
    pool: &PgPool,
    source: &mut S,
    limit: i64,
) -> Result<FinalityOutcome, IngestError> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT sig, slot FROM settlement_tx WHERE status NOT IN ('finalized', 'dropped') ORDER BY slot ASC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        return Ok(FinalityOutcome::default());
    }

    let mut outcome = FinalityOutcome::default();
    for batch in rows.chunks(MAX_SIGNATURES_PER_CALL) {
        let sigs: Vec<String> = batch.iter().map(|(sig, _)| sig.clone()).collect();
        let statuses = source.get_signature_statuses(&sigs).await?;
        outcome.checked += batch.len();

        let mut finalized_sigs: Vec<String> = Vec::new();
        let mut none_status: Vec<(String, i64)> = Vec::new();
        for ((sig, slot), status) in batch.iter().zip(statuses.iter()) {
            match status {
                Some(TransactionConfirmationStatus::Finalized) => finalized_sigs.push(sig.clone()),
                None => none_status.push((sig.clone(), *slot)),
                _ => {}
            }
        }

        if !finalized_sigs.is_empty() {
            let result = sqlx::query(
                "UPDATE settlement_tx SET status = 'finalized' WHERE sig = ANY($1) AND status <> 'finalized'",
            )
            .bind(&finalized_sigs)
            .execute(pool)
            .await?;
            outcome.upgraded += result.rows_affected() as usize;
        }

        if !none_status.is_empty() {
            let current_slot = source.current_slot().await?;
            let dropped_sigs: Vec<String> = none_status
                .iter()
                .filter(|(_, slot)| {
                    current_slot.saturating_sub(*slot as u64) > DROPPED_HORIZON_SLOTS
                })
                .map(|(sig, _)| sig.clone())
                .collect();
            if !dropped_sigs.is_empty() {
                let result = sqlx::query(
                    "UPDATE settlement_tx SET status = 'dropped' WHERE sig = ANY($1) AND status = 'confirmed'",
                )
                .bind(&dropped_sigs)
                .execute(pool)
                .await?;
                outcome.dropped += result.rows_affected() as usize;
            }
        }
    }
    Ok(outcome)
}
