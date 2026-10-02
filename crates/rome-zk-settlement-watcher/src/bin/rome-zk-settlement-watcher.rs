//! The settlement watcher's own runnable service: applies `migrations/explorer/*.sql`
//! (`rome_zk_settlement_watcher::migrate`), then loops ingest (both programs) + the derive pass + the
//! finality pass on a fixed interval, forever. Config comes entirely from environment variables —
//! named, fail-closed: an absent required variable is a startup error naming exactly which one, never a
//! silent default standing in for a real value.
//!
//! Required:
//! - `ROME_ZK_WATCHER_DATABASE_URL` — Postgres connection string for `rome_zk_explorer`.
//! - `ROME_ZK_WATCHER_RPC_PRIMARY` — primary Solana RPC endpoint URL.
//! - `ROME_ZK_WATCHER_INBOX_PROGRAM` — the zk-inbox program id (base58).
//! - `ROME_ZK_WATCHER_SETTLEMENT_PROGRAM` — the zk-settlement program id (base58).
//!
//! Optional:
//! - `ROME_ZK_WATCHER_RPC_SECONDARY` — failover Solana RPC endpoint URL (`rpc::RpcSource`'s own
//!   two-endpoint failover).
//! - `ROME_ZK_WATCHER_POLL_INTERVAL_SECS` — seconds between loop iterations (default 5).

use rome_zk_settlement_watcher::cursor::ProgramKind;
use rome_zk_settlement_watcher::finality::track_finality;
use rome_zk_settlement_watcher::{
    derive_exit_once, derive_once, migrate, run_once, DeriveOutcome, PageOutcome, RpcSource,
    WatcherConfig,
};
use solana_program::pubkey::Pubkey;
use sqlx::postgres::PgPoolOptions;
use std::str::FromStr;
use std::time::Duration;

struct Config {
    database_url: String,
    rpc_endpoints: Vec<String>,
    inbox_program: Pubkey,
    settlement_program: Pubkey,
    poll_interval: Duration,
}

fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        eprintln!("fatal: {name} is not set — refusing to start with a guessed value");
        std::process::exit(1);
    })
}

fn required_pubkey_env(name: &str) -> Pubkey {
    let raw = required_env(name);
    Pubkey::from_str(&raw).unwrap_or_else(|e| {
        eprintln!("fatal: {name}={raw:?} is not a valid pubkey: {e}");
        std::process::exit(1);
    })
}

impl Config {
    fn from_env() -> Self {
        let mut rpc_endpoints = vec![required_env("ROME_ZK_WATCHER_RPC_PRIMARY")];
        if let Ok(secondary) = std::env::var("ROME_ZK_WATCHER_RPC_SECONDARY") {
            rpc_endpoints.push(secondary);
        }
        let poll_interval = std::env::var("ROME_ZK_WATCHER_POLL_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(5);
        Self {
            database_url: required_env("ROME_ZK_WATCHER_DATABASE_URL"),
            rpc_endpoints,
            inbox_program: required_pubkey_env("ROME_ZK_WATCHER_INBOX_PROGRAM"),
            settlement_program: required_pubkey_env("ROME_ZK_WATCHER_SETTLEMENT_PROGRAM"),
            poll_interval: Duration::from_secs(poll_interval),
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let cfg = Config::from_env();

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&cfg.database_url)
        .await?;
    migrate(&pool).await?;
    tracing::info!("rome_zk_explorer migrations applied");

    let mut source = RpcSource::new(cfg.rpc_endpoints.clone());
    let watcher_cfg = WatcherConfig::default();

    loop {
        let inbox_ingest_ok = match run_once(
            &pool,
            &mut source,
            &cfg.inbox_program,
            ProgramKind::Inbox,
            watcher_cfg,
        )
        .await
        {
            Ok(PageOutcome::Processed { signatures, .. }) => {
                tracing::info!(signatures, "ingested inbox signatures");
                true
            }
            Ok(PageOutcome::NoNewSignatures) => true,
            Err(e) => {
                tracing::warn!(error = %e, "inbox ingest step failed");
                false
            }
        };

        let settlement_ingest_ok = match run_once(
            &pool,
            &mut source,
            &cfg.settlement_program,
            ProgramKind::Root,
            watcher_cfg,
        )
        .await
        {
            Ok(PageOutcome::Processed { signatures, .. }) => {
                tracing::info!(signatures, "ingested settlement signatures");
                true
            }
            Ok(PageOutcome::NoNewSignatures) => true,
            Err(e) => {
                tracing::warn!(error = %e, "settlement ingest step failed");
                false
            }
        };

        // Never derive in an iteration whose own inbox ingest call returned Err -- an
        // ingest walk that failed partway through may still be missing an older page, and `derive_once`'s
        // own `backfill_head_sig IS NULL` gate already refuses in that case too (this is a second,
        // independent guard at the call site, not a substitute for it).
        if inbox_ingest_ok {
            loop {
                match derive_once(&pool, 1_000).await {
                    Ok(DeriveOutcome::Processed { rows }) => {
                        tracing::info!(rows, "derived inbox lifecycle state");
                        continue;
                    }
                    Ok(DeriveOutcome::NoNewRows) => break,
                    Ok(DeriveOutcome::IngestWalkInProgress) => break,
                    Err(e) => {
                        tracing::warn!(error = %e, "derive step failed");
                        break;
                    }
                }
            }
        }

        // Same gate, for the settlement/exit ingester's own derive pass (never derive
        // past rows a not-yet-finished settlement ingest walk might still be missing an older page for).
        if settlement_ingest_ok {
            loop {
                match derive_exit_once(&pool, 1_000).await {
                    Ok(DeriveOutcome::Processed { rows }) => {
                        tracing::info!(rows, "derived exit lifecycle state");
                        continue;
                    }
                    Ok(DeriveOutcome::NoNewRows) => break,
                    Ok(DeriveOutcome::IngestWalkInProgress) => break,
                    Err(e) => {
                        tracing::warn!(error = %e, "exit derive step failed");
                        break;
                    }
                }
            }
        }

        match track_finality(&pool, &mut source, 1_000).await {
            Ok(outcome) => {
                if outcome.upgraded > 0 || outcome.dropped > 0 {
                    tracing::info!(
                        checked = outcome.checked,
                        upgraded = outcome.upgraded,
                        dropped = outcome.dropped,
                        "finality pass"
                    );
                }
            }
            Err(e) => tracing::warn!(error = %e, "finality step failed"),
        }

        tokio::time::sleep(cfg.poll_interval).await;
    }
}
