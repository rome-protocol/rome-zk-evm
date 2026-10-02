//! Applies this crate's own migrations (`migrations/explorer/*.sql`, `rome_zk_explorer`, shared with the indexer) — the
//! watcher binary calls this at startup, so the service itself applies them, not only the test harness.

use sqlx::PgPool;

/// Applies every pending migration under `migrations/explorer/`, in order. Idempotent — safe to call on
/// every service start (`sqlx::migrate!` tracks what has already run in its own `_sqlx_migrations`
/// table).
pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("../../migrations/explorer").run(pool).await
}
