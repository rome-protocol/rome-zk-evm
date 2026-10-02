//! Read-time status (per-block status is derived at read time and never stored). There
//! is no per-L2-block table yet (a `block_batch`/`l2_block_settlement` mapping is the natural
//! follow-on once one lands), so `block_status` is queried at the grain this schema actually has --
//! `(chain_id, batch_id)` -- via the `block_status` SQL view (`migrations/explorer/0001_settlement.sql`)
//! rather than re-deriving the same CASE logic in Rust: the view is the one place this status is
//! computed, whether a caller reads it with `psql` or through this function.

use sqlx::PgPool;

/// One `(chain_id, batch_id)`'s derived status: `sequenced | data posted | root posted | final |
/// abandoned`. `None` if this crate has never observed that batch at all.
pub async fn block_status(
    pool: &PgPool,
    chain_id: i64,
    batch_id: i64,
) -> Result<Option<String>, sqlx::Error> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT status FROM block_status WHERE chain_id = $1 AND batch_id = $2")
            .bind(chain_id)
            .bind(batch_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(s,)| s))
}

/// Every batch this crate has observed for `chain_id`, oldest first, with its derived status --
/// the shape an explorer's "batches" list page reads.
pub async fn list_block_status(
    pool: &PgPool,
    chain_id: i64,
) -> Result<Vec<(i64, String)>, sqlx::Error> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT batch_id, status FROM block_status WHERE chain_id = $1 ORDER BY batch_id ASC",
    )
    .bind(chain_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
