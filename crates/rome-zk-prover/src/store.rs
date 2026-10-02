//! Postgres history for the follower loop: the chain is the cursor; Postgres is history. `Store` is a
//! fifth [`crate::follower::Deps`] field, a plain trait — the whole follower loop still runs, unchanged,
//! against a fake in tests. The candidate batch is ALWAYS derived from the finalized chain anchor, never
//! from this store — `Store` has no `get`/`query` method at all, on purpose, so the loop cannot read it
//! back even by accident. A store failure never stops proving: every `record` call from the follower loop
//! is logged and counted on error, never propagated with `?`.
//!
//! Two implementations ship: [`NoopStore`] (the default — `database_url` unset means history is simply
//! not kept) and [`PgStore`] (`sqlx` against `migrations/prover/`, the same pattern
//! `rome-zk-settlement-watcher` already uses for `migrations/explorer`). `PgStore::connect` failing at
//! START is fail-closed by name ([`HistoryUnreachable`]) — the operator configured a `database_url` and
//! asked for history; a box that cannot reach it should not start silently believing it has none. Once
//! running, every `record` failure is fail-OPEN — the follower's whole reason to exist is proving, not
//! bookkeeping.

use std::fmt;

/// One job transition, carrying only what that transition itself knows. `(chain_id, batch, attempt)` is
/// the idempotency key `PgStore` upserts on — `attempt` is the follower's own prove-attempt counter
/// (`RunConfig::max_prove_attempts`'s loop variable): a batch id is never reused by a live chain, but a
/// RESTART legitimately re-attempts the same candidate, and a resumed cached proof counts as attempt 1 of
/// this run — see `follower`'s own module docs for exactly which events fire on
/// which path.
#[derive(Debug, Clone)]
pub struct JobEvent {
    pub chain_id: u64,
    pub batch: u64,
    pub attempt: u32,
    pub kind: JobEventKind,
}

/// One variant per follower transition, each carrying only the fields THAT transition
/// knows. `Idle`/`Retry` (see [`crate::follower::Outcome`]) are not job transitions — they never
/// construct a `JobEvent` at all.
#[derive(Debug, Clone)]
pub enum JobEventKind {
    Queued {
        first_block: u64,
        last_block: u64,
    },
    InputBuilt {
        input_bytes: u64,
        wall_ms: f64,
    },
    Proving {
        backend: String,
    },
    Proved {
        program_vk: String,
        wall_stark_ms: f64,
        wall_plonk_ms: f64,
    },
    Verified {
        wall_ms: f64,
        /// The 768-byte ZisK PLONK proof (`Calldata::proof_bytes_768`) — never the larger 1,344-byte
        /// on-chain ABI, which is reconstructible from this plus the vkey of record at read time.
        proof_abi: Vec<u8>,
        /// The 512-byte packaged public values (`Calldata::publics_512`).
        publics: Vec<u8>,
    },
    Posted {
        sig: String,
        wall_ms: f64,
        gas_used: u64,
        cost_usd: f64,
    },
    AlreadyPosted,
    Superseded {
        head_pending_batch: u64,
    },
    Failed {
        reason: String,
    },
    Finalized {
        /// `None` when the chain itself is the only source of this fact: a
        /// restart between a post and its own finalize sweep discovers `candidate_batch <=
        /// head_final_batch` on the very next anchor, with no local record of which signature (this
        /// job's own post, a `FinalizeBatch`, or a completely different run's) actually made it final.
        /// The chain says final; the signature is unknown; recording `None` is honest, `finalize_sig`
        /// itself must be nullable to carry it.
        finalize_sig: Option<String>,
    },
}

impl JobEventKind {
    /// The `proof_jobs.status` value this transition writes — the one mapping both the writer and the
    /// migration's own CHECK constraint (`migrations/prover/0001_proof_jobs.sql`) agree on.
    pub fn status(&self) -> &'static str {
        match self {
            JobEventKind::Queued { .. } => "queued",
            JobEventKind::InputBuilt { .. } => "input_built",
            JobEventKind::Proving { .. } => "proving",
            JobEventKind::Proved { .. } => "proved",
            JobEventKind::Verified { .. } => "verified",
            JobEventKind::Posted { .. } => "posted",
            JobEventKind::AlreadyPosted => "already_posted",
            JobEventKind::Superseded { .. } => "superseded",
            JobEventKind::Failed { .. } => "failed",
            JobEventKind::Finalized { .. } => "finalized",
        }
    }
}

/// A `record` failure — logged and counted by the follower loop, never propagated (history never stops
/// proving).
#[derive(Debug, Clone)]
pub struct StoreError(pub String);

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for StoreError {}

/// The store seam. Write-only, deliberately: no `get`/`query` method exists on this
/// trait, so nothing in the follower loop can read history back even by accident. Matches
/// [`rome_zk_solana_sender::Sender`]'s own `impl Future` shape rather than `#[async_trait]`, so `Deps`
/// stays generic (never `dyn Store`) and a test double needs no extra macro dependency.
pub trait Store: Send + Sync {
    fn record(
        &self,
        event: &JobEvent,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;
}

/// `database_url` unset (the default) — every event is dropped. One log line at
/// construction, never repeated per event.
pub struct NoopStore;

impl NoopStore {
    pub fn new() -> Self {
        tracing::info!("prover history disabled (no database_url configured)");
        Self
    }
}

impl Default for NoopStore {
    fn default() -> Self {
        Self::new()
    }
}

impl Store for NoopStore {
    async fn record(&self, _event: &JobEvent) -> Result<(), StoreError> {
        Ok(())
    }
}

/// `PgStore::connect` failed: the operator configured a `database_url` and cannot have
/// history — a refusal by name, never a silent `NoopStore` fallback. `url_redacted` never carries the
/// password (`redact_url`) — this type's own `Display`, and every caller that logs it, are safe to print.
#[derive(Debug, thiserror::Error)]
#[error("prover history unreachable at {url_redacted}: {source}")]
pub struct HistoryUnreachable {
    pub url_redacted: String,
    #[source]
    pub source: sqlx::Error,
}

/// Redacts a `postgres://user:password@host/db` URL to `postgres://user:***@host/db` — this module's own
/// rule (and the deployment's render script's): the store must never log or print
/// `database_url` unredacted. A URL with no `@` carries no `user:password` segment at all — there is
/// nothing to redact, and it is returned unchanged past the scheme. A string with no `://` at all is not
/// a URL this function can parse; it returns `***` rather than echo unparsed input back into a log line.
pub fn redact_url(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return "***".to_string();
    };
    let (scheme, rest) = url.split_at(scheme_end + 3);
    let Some(at) = rest.find('@') else {
        return format!("{scheme}{rest}");
    };
    let userinfo = &rest[..at];
    let after_at = &rest[at..]; // includes the leading '@'
    match userinfo.find(':') {
        Some(colon) => format!("{scheme}{}:***{after_at}", &userinfo[..colon]),
        None => format!("{scheme}{userinfo}{after_at}"),
    }
}

/// `sqlx`-backed [`Store`]: one connection pool, migrated once at [`PgStore::connect`]
/// against `migrations/prover/` — the same `sqlx::migrate!` call
/// `rome_zk_settlement_watcher::migrate::migrate` makes against `migrations/explorer/`, copied here
/// verbatim.
pub struct PgStore {
    pool: sqlx::PgPool,
}

impl PgStore {
    /// Connects and applies every pending migration. A failure at EITHER step is [`HistoryUnreachable`]
    /// — fail-closed at start: the operator asked for history and cannot have it,
    /// rather than the process silently running with none.
    pub async fn connect(database_url: &str) -> Result<Self, HistoryUnreachable> {
        let unreachable = |source: sqlx::Error| HistoryUnreachable {
            url_redacted: redact_url(database_url),
            source,
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(database_url)
            .await
            .map_err(unreachable)?;
        sqlx::migrate!("../../migrations/prover")
            .run(&pool)
            .await
            .map_err(|e| unreachable(sqlx::Error::Migrate(Box::new(e))))?;
        tracing::info!("prover history enabled at {}", redact_url(database_url));
        Ok(Self { pool })
    }
}

/// The CLI's own runtime choice between the two implementations, resolved once at start from
/// `Config::database_url` — `run`/`run_one` are generic over `Store`, but a single
/// binary needs ONE concrete type decided at runtime, not a `dyn Store` (this trait's RPITIT shape is not
/// object-safe).
pub enum AnyStore {
    Noop(NoopStore),
    Pg(PgStore),
}

impl Store for AnyStore {
    async fn record(&self, event: &JobEvent) -> Result<(), StoreError> {
        match self {
            AnyStore::Noop(s) => s.record(event).await,
            AnyStore::Pg(s) => s.record(event).await,
        }
    }
}

impl Store for PgStore {
    async fn record(&self, event: &JobEvent) -> Result<(), StoreError> {
        let map_err = |e: sqlx::Error| StoreError(e.to_string());

        // Every column this event does NOT know is bound NULL; `ON CONFLICT ... DO UPDATE` uses
        // `COALESCE(EXCLUDED.col, proof_jobs.col)` so a later transition's NULLs never clobber an
        // earlier transition's already-recorded facts — each transition fills only its own columns,
        // exactly the row contract.
        let mut first_block: Option<i64> = None;
        let mut last_block: Option<i64> = None;
        let mut program_vk: Option<&str> = None;
        let mut backend: Option<&str> = None;
        let mut input_bytes: Option<i64> = None;
        let mut gas_used: Option<i64> = None;
        let mut wall_input_ms: Option<f64> = None;
        let mut wall_stark_ms: Option<f64> = None;
        let mut wall_plonk_ms: Option<f64> = None;
        let mut wall_verify_ms: Option<f64> = None;
        let mut wall_post_ms: Option<f64> = None;
        let mut cost_usd: Option<f64> = None;
        let mut sig: Option<&str> = None;
        let mut finalize_sig: Option<&str> = None;

        match &event.kind {
            JobEventKind::Queued {
                first_block: f,
                last_block: l,
            } => {
                first_block = Some(*f as i64);
                last_block = Some(*l as i64);
            }
            JobEventKind::InputBuilt {
                input_bytes: b,
                wall_ms,
            } => {
                input_bytes = Some(*b as i64);
                wall_input_ms = Some(*wall_ms);
            }
            JobEventKind::Proving { backend: b } => {
                backend = Some(b.as_str());
            }
            JobEventKind::Proved {
                program_vk: vk,
                wall_stark_ms: stark,
                wall_plonk_ms: plonk,
            } => {
                program_vk = Some(vk.as_str());
                wall_stark_ms = Some(*stark);
                wall_plonk_ms = Some(*plonk);
            }
            JobEventKind::Verified { wall_ms, .. } => {
                wall_verify_ms = Some(*wall_ms);
            }
            JobEventKind::Posted {
                sig: s,
                wall_ms,
                gas_used: g,
                cost_usd: c,
            } => {
                sig = Some(s.as_str());
                wall_post_ms = Some(*wall_ms);
                gas_used = Some(*g as i64);
                cost_usd = Some(*c);
            }
            JobEventKind::AlreadyPosted => {}
            JobEventKind::Superseded { .. } => {}
            JobEventKind::Failed { .. } => {}
            JobEventKind::Finalized { finalize_sig: fs } => {
                finalize_sig = fs.as_deref();
            }
        }

        let row = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO proof_jobs (
                chain_id, batch, attempt, status,
                first_block, last_block, program_vk, backend, input_bytes, gas_used,
                wall_input_ms, wall_stark_ms, wall_plonk_ms, wall_verify_ms, wall_post_ms,
                cost_usd, sig, finalize_sig, updated_at
            ) VALUES (
                $1, $2, $3, $4,
                $5, $6, $7, $8, $9, $10,
                $11, $12, $13, $14, $15,
                $16, $17, $18, now()
            )
            ON CONFLICT (chain_id, batch, attempt) DO UPDATE SET
                status         = EXCLUDED.status,
                first_block    = COALESCE(EXCLUDED.first_block, proof_jobs.first_block),
                last_block     = COALESCE(EXCLUDED.last_block, proof_jobs.last_block),
                program_vk     = COALESCE(EXCLUDED.program_vk, proof_jobs.program_vk),
                backend        = COALESCE(EXCLUDED.backend, proof_jobs.backend),
                input_bytes    = COALESCE(EXCLUDED.input_bytes, proof_jobs.input_bytes),
                gas_used       = COALESCE(EXCLUDED.gas_used, proof_jobs.gas_used),
                wall_input_ms  = COALESCE(EXCLUDED.wall_input_ms, proof_jobs.wall_input_ms),
                wall_stark_ms  = COALESCE(EXCLUDED.wall_stark_ms, proof_jobs.wall_stark_ms),
                wall_plonk_ms  = COALESCE(EXCLUDED.wall_plonk_ms, proof_jobs.wall_plonk_ms),
                wall_verify_ms = COALESCE(EXCLUDED.wall_verify_ms, proof_jobs.wall_verify_ms),
                wall_post_ms   = COALESCE(EXCLUDED.wall_post_ms, proof_jobs.wall_post_ms),
                cost_usd       = COALESCE(EXCLUDED.cost_usd, proof_jobs.cost_usd),
                sig            = COALESCE(EXCLUDED.sig, proof_jobs.sig),
                finalize_sig   = COALESCE(EXCLUDED.finalize_sig, proof_jobs.finalize_sig),
                updated_at     = now()
            RETURNING id
            "#,
        )
        .bind(event.chain_id as i64)
        .bind(event.batch as i64)
        .bind(event.attempt as i32)
        .bind(event.kind.status())
        .bind(first_block)
        .bind(last_block)
        .bind(program_vk)
        .bind(backend)
        .bind(input_bytes)
        .bind(gas_used)
        .bind(wall_input_ms)
        .bind(wall_stark_ms)
        .bind(wall_plonk_ms)
        .bind(wall_verify_ms)
        .bind(wall_post_ms)
        .bind(cost_usd)
        .bind(sig)
        .bind(finalize_sig)
        .fetch_one(&self.pool)
        .await
        .map_err(map_err)?;

        if let JobEventKind::Verified {
            proof_abi, publics, ..
        } = &event.kind
        {
            sqlx::query(
                r#"
                INSERT INTO proofs (job_id, proof_abi, publics)
                VALUES ($1, $2, $3)
                ON CONFLICT (job_id) DO UPDATE SET
                    proof_abi = EXCLUDED.proof_abi,
                    publics   = EXCLUDED.publics
                "#,
            )
            .bind(row)
            .bind(proof_abi.as_slice())
            .bind(publics.as_slice())
            .execute(&self.pool)
            .await
            .map_err(map_err)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_url_hides_the_password_but_keeps_everything_else() {
        assert_eq!(
            redact_url("postgres://prover:s3cr3t@db-host:5432/prover_db"),
            "postgres://prover:***@db-host:5432/prover_db"
        );
    }

    #[test]
    fn redact_url_never_falls_back_to_the_raw_input() {
        assert_eq!(redact_url("not-a-url-at-all"), "***");
        assert_eq!(redact_url("postgres://host/db"), "postgres://host/db");
    }

    #[test]
    fn job_event_status_matches_every_migration_check_constraint_value() {
        // The one mapping the writer and `migrations/prover/0001_proof_jobs.sql`'s CHECK constraint
        // must agree on — spelled out here so a renamed variant or a typo'd status string is caught by
        // `cargo test`, not by a live INSERT failing the CHECK in CI.
        let expected = [
            (
                JobEventKind::Queued {
                    first_block: 1,
                    last_block: 2,
                },
                "queued",
            ),
            (
                JobEventKind::InputBuilt {
                    input_bytes: 1,
                    wall_ms: 1.0,
                },
                "input_built",
            ),
            (
                JobEventKind::Proving {
                    backend: "x".to_string(),
                },
                "proving",
            ),
            (
                JobEventKind::Proved {
                    program_vk: "x".to_string(),
                    wall_stark_ms: 1.0,
                    wall_plonk_ms: 1.0,
                },
                "proved",
            ),
            (
                JobEventKind::Verified {
                    wall_ms: 1.0,
                    proof_abi: vec![],
                    publics: vec![],
                },
                "verified",
            ),
            (
                JobEventKind::Posted {
                    sig: "x".to_string(),
                    wall_ms: 1.0,
                    gas_used: 1,
                    cost_usd: 1.0,
                },
                "posted",
            ),
            (JobEventKind::AlreadyPosted, "already_posted"),
            (
                JobEventKind::Superseded {
                    head_pending_batch: 1,
                },
                "superseded",
            ),
            (
                JobEventKind::Failed {
                    reason: "x".to_string(),
                },
                "failed",
            ),
            (
                JobEventKind::Finalized {
                    finalize_sig: Some("x".to_string()),
                },
                "finalized",
            ),
        ];
        for (kind, want) in expected {
            assert_eq!(kind.status(), want);
        }
    }
}
