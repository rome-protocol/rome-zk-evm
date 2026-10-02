//! Postgres integration tests for `rome-zk-prover::store`: a throwaway `postgres:16`
//! container per test (`tests/support::TestPg`), against the REAL `PgStore` — never a fake — since these
//! tests exist to prove the schema + the idempotency key, not the follower's own call sites (those are
//! covered by `follower`'s own fake-driven `RecordingStore`/`FailingStore` tests).
//!
//! Heavy (`docker run`): needs Docker, as CI has. Where plain `docker` as the operator's own user is permission-denied,
//! run this file with `sudo -E env PATH="$PATH" cargo test --locked -- store` from `crates/rome-zk-prover`, or let the
//! CI `test` job run it (it already has Docker as `runner`, `.github/workflows/ci.yml:170`, and already sweeps
//! `rome-zk-test`-labelled containers).

mod support;

use rome_zk_prover::store::{JobEvent, JobEventKind, PgStore, Store};
use support::TestPg;

const CHAIN_ID: u64 = 200_101;
const BATCH: u64 = 42;

fn proof_abi_bytes() -> Vec<u8> {
    vec![0xAB; 768]
}

fn publics_bytes() -> Vec<u8> {
    vec![0xCD; 512]
}

/// The real follower sequence for one successful job: every event a fresh
/// prove-and-post produces, in order, for one `attempt`.
fn full_sequence(attempt: u32) -> Vec<JobEvent> {
    let ev = |kind: JobEventKind| JobEvent {
        chain_id: CHAIN_ID,
        batch: BATCH,
        attempt,
        kind,
    };
    vec![
        ev(JobEventKind::Queued {
            first_block: 2461,
            last_block: 2520,
        }),
        ev(JobEventKind::InputBuilt {
            input_bytes: 4_096,
            wall_ms: 12.5,
        }),
        ev(JobEventKind::Proving {
            backend: "cargo-zisk".to_string(),
        }),
        ev(JobEventKind::Proved {
            program_vk: "e5ea5c14".to_string(),
            wall_stark_ms: 60_000.0,
            wall_plonk_ms: 15_000.0,
        }),
        ev(JobEventKind::Verified {
            wall_ms: 5.0,
            proof_abi: proof_abi_bytes(),
            publics: publics_bytes(),
        }),
        ev(JobEventKind::Posted {
            sig: "5t1sig".to_string(),
            wall_ms: 800.0,
            gas_used: 1_234_567,
            cost_usd: 0.42,
        }),
        ev(JobEventKind::Finalized {
            finalize_sig: Some("5t1finalize".to_string()),
        }),
    ]
}

async fn record_sequence(store: &PgStore, attempt: u32) {
    for ev in full_sequence(attempt) {
        store.record(&ev).await.expect("record must succeed");
    }
}

#[derive(sqlx::FromRow, Debug, PartialEq)]
struct ProofJobRow {
    id: i64,
    status: String,
    first_block: Option<i64>,
    last_block: Option<i64>,
    program_vk: Option<String>,
    backend: Option<String>,
    input_bytes: Option<i64>,
    gas_used: Option<i64>,
    wall_input_ms: Option<f64>,
    wall_stark_ms: Option<f64>,
    wall_plonk_ms: Option<f64>,
    wall_verify_ms: Option<f64>,
    wall_post_ms: Option<f64>,
    cost_usd: Option<f64>,
    sig: Option<String>,
    finalize_sig: Option<String>,
}

async fn fetch_rows(pool: &sqlx::PgPool) -> Vec<ProofJobRow> {
    sqlx::query_as::<_, ProofJobRow>(
        "SELECT id, status, first_block, last_block, program_vk, backend, input_bytes, gas_used, \
         wall_input_ms, wall_stark_ms, wall_plonk_ms, wall_verify_ms, wall_post_ms, cost_usd, sig, \
         finalize_sig \
         FROM proof_jobs WHERE chain_id = $1 AND batch = $2 ORDER BY attempt",
    )
    .bind(CHAIN_ID as i64)
    .bind(BATCH as i64)
    .fetch_all(pool)
    .await
    .expect("select proof_jobs")
}

fn assert_row_fully_populated(row: &ProofJobRow) {
    assert_eq!(row.status, "finalized");
    assert_eq!(row.first_block, Some(2461));
    assert_eq!(row.last_block, Some(2520));
    assert_eq!(row.program_vk.as_deref(), Some("e5ea5c14"));
    assert_eq!(row.backend.as_deref(), Some("cargo-zisk"));
    assert_eq!(row.input_bytes, Some(4_096));
    assert_eq!(row.gas_used, Some(1_234_567));
    assert_eq!(row.wall_input_ms, Some(12.5));
    assert_eq!(row.wall_stark_ms, Some(60_000.0));
    assert_eq!(row.wall_plonk_ms, Some(15_000.0));
    assert_eq!(row.wall_verify_ms, Some(5.0));
    assert_eq!(row.wall_post_ms, Some(800.0));
    assert_eq!(row.cost_usd, Some(0.42));
    assert_eq!(row.sig.as_deref(), Some("5t1sig"));
    assert_eq!(row.finalize_sig.as_deref(), Some("5t1finalize"));
}

/// After the ordered events for one job, `proof_jobs` has ONE row with
/// `status='finalized'` and every wall/sig column filled, and `proofs` has the ABI (768 B) + publics
/// (512 B). Mutation target: drop `ON CONFLICT` from `PgStore::record`'s INSERT (the SAME sequence
/// replayed below would then hit the UNIQUE constraint as a duplicate-key error instead of upserting).
#[tokio::test]
async fn one_job_produces_one_fully_populated_row_and_its_proof_bytes() {
    let pg = TestPg::start().await;
    let store = PgStore::connect(&pg.url).await.expect("connect + migrate");

    record_sequence(&store, 1).await;

    let rows = fetch_rows(&pg.pool).await;
    assert_eq!(rows.len(), 1, "exactly one row: {rows:?}");
    assert_row_fully_populated(&rows[0]);

    let (proof_abi, publics): (Vec<u8>, Vec<u8>) =
        sqlx::query_as("SELECT proof_abi, publics FROM proofs WHERE job_id = $1")
            .bind(rows[0].id)
            .fetch_one(&pg.pool)
            .await
            .expect("select proofs");
    assert_eq!(
        proof_abi.len(),
        768,
        "proof_abi must be the 768-byte ZisK proof"
    );
    assert_eq!(
        publics.len(),
        512,
        "publics must be the 512-byte packaged public values"
    );
    assert_eq!(proof_abi, proof_abi_bytes());
    assert_eq!(publics, publics_bytes());
}

/// Replaying the WHOLE sequence again leaves the same one row, byte-identical —
/// idempotent replay (a resumed/restarted follower re-recording events it already recorded once).
/// Mutation target: drop `ON CONFLICT` — this exact replay then errors on the UNIQUE constraint instead
/// of upserting, going red.
#[tokio::test]
async fn replaying_the_whole_sequence_again_leaves_one_byte_identical_row() {
    let pg = TestPg::start().await;
    let store = PgStore::connect(&pg.url).await.expect("connect + migrate");

    record_sequence(&store, 1).await;
    let first_pass = fetch_rows(&pg.pool).await;
    assert_eq!(first_pass.len(), 1);

    record_sequence(&store, 1).await;
    let second_pass = fetch_rows(&pg.pool).await;
    assert_eq!(
        second_pass.len(),
        1,
        "still one row after a full replay: {second_pass:?}"
    );
    assert_eq!(
        first_pass, second_pass,
        "the replayed row must be byte-identical to the first pass"
    );
}

/// Two attempts of the SAME batch produce two distinct rows — the idempotency key
/// is `(chain_id, batch, attempt)`, never `(chain_id, batch)` alone (a resumed job whose cached artefact
/// failed a fresh check re-proves under a NEW attempt, and its own history must not overwrite
/// the failed attempt's). Mutation target: narrow the migration's own UNIQUE constraint to
/// `(chain_id, batch)` — this test then finds one row, not two, going red.
#[tokio::test]
async fn two_attempts_of_one_batch_produce_two_rows() {
    let pg = TestPg::start().await;
    let store = PgStore::connect(&pg.url).await.expect("connect + migrate");

    record_sequence(&store, 1).await;
    record_sequence(&store, 2).await;

    let rows = fetch_rows(&pg.pool).await;
    assert_eq!(rows.len(), 2, "one row per attempt: {rows:?}");
    assert_row_fully_populated(&rows[0]);
    assert_row_fully_populated(&rows[1]);
}

/// A batch's real prove attempts restart from 1 on every process restart (a resumed job is the
/// one exception, continuing its own artefact's real attempt — never relevant here, this batch is
/// never resumed). Life 1 exhausts 3 attempts and halts, leaving attempt 3's own row at `failed`. Life 2 (a
/// restart) succeeds on its very first attempt, `UPSERT`ing a `finalized` row at attempt 1 — a LOWER
/// attempt number than life 1's stale `failed` row, because the counter restarted. The README's own
/// documented query, `ORDER BY attempt DESC`, surfaces life 1's stale `failed` row instead of the real,
/// later `finalized` one; `ORDER BY updated_at DESC` (the fix) surfaces the correct row regardless.
/// Mutation: swap the fixed query back to `attempt DESC` — the "correct" assertion below goes red.
#[tokio::test]
async fn latest_job_query_must_order_by_updated_at_not_attempt_across_a_restart() {
    let pg = TestPg::start().await;
    let store = PgStore::connect(&pg.url).await.expect("connect + migrate");
    let batch = 99u64;
    let ev = |attempt: u32, kind: JobEventKind| JobEvent {
        chain_id: CHAIN_ID,
        batch,
        attempt,
        kind,
    };

    // Life 1: three attempts, all failing (a real halt after `max_prove_attempts` exhausted) — the last
    // one, attempt 3, is the highest attempt number this batch will EVER see.
    for attempt in 1..=3u32 {
        store
            .record(&ev(
                attempt,
                JobEventKind::Proved {
                    program_vk: "e5ea5c14".to_string(),
                    wall_stark_ms: 1.0,
                    wall_plonk_ms: 1.0,
                },
            ))
            .await
            .expect("record must succeed");
        store
            .record(&ev(
                attempt,
                JobEventKind::Failed {
                    reason: "pairing failed".to_string(),
                },
            ))
            .await
            .expect("record must succeed");
    }

    // Life 2 (a restart): the attempt counter restarts from 1, and this time it succeeds all the way to
    // `finalized` on the very first attempt.
    for kind in [
        JobEventKind::Queued {
            first_block: 1,
            last_block: 2,
        },
        JobEventKind::Verified {
            wall_ms: 5.0,
            proof_abi: proof_abi_bytes(),
            publics: publics_bytes(),
        },
        JobEventKind::Posted {
            sig: "life2sig".to_string(),
            wall_ms: 1.0,
            gas_used: 1,
            cost_usd: 0.1,
        },
        JobEventKind::Finalized {
            finalize_sig: Some("life2final".to_string()),
        },
    ] {
        store
            .record(&ev(1, kind))
            .await
            .expect("record must succeed");
    }

    #[derive(sqlx::FromRow, Debug)]
    struct Latest {
        attempt: i32,
        status: String,
    }

    let by_attempt: Latest = sqlx::query_as(
        "SELECT attempt, status FROM proof_jobs WHERE chain_id = $1 AND batch = $2 \
         ORDER BY attempt DESC LIMIT 1",
    )
    .bind(CHAIN_ID as i64)
    .bind(batch as i64)
    .fetch_one(&pg.pool)
    .await
    .expect("select by attempt DESC");
    assert_eq!(
        (by_attempt.attempt, by_attempt.status.as_str()),
        (3, "failed"),
        "documents the bug: `attempt DESC` surfaces life 1's stale, higher-numbered failed row"
    );

    let by_updated_at: Latest = sqlx::query_as(
        "SELECT attempt, status FROM proof_jobs WHERE chain_id = $1 AND batch = $2 \
         ORDER BY updated_at DESC LIMIT 1",
    )
    .bind(CHAIN_ID as i64)
    .bind(batch as i64)
    .fetch_one(&pg.pool)
    .await
    .expect("select by updated_at DESC");
    assert_eq!(
        (by_updated_at.attempt, by_updated_at.status.as_str()),
        (1, "finalized"),
        "the fixed query must surface life 2's real, later, finalized row"
    );
}

/// `PgStore::connect` against an unreachable database is a named, fail-closed refusal
/// — never a silent `NoopStore` fallback — and the error never carries the password in clear.
#[tokio::test]
async fn connect_to_an_unreachable_database_is_a_named_refusal_with_the_password_redacted() {
    let result = PgStore::connect("postgres://tiber:s3cr3t-password@127.0.0.1:1/nope").await;
    let err = match result {
        Ok(_) => panic!("an unreachable database must refuse to connect"),
        Err(e) => e,
    };
    let msg = err.to_string();
    assert!(
        !msg.contains("s3cr3t-password"),
        "the password must never appear in the error: {msg}"
    );
    assert!(
        msg.contains("tiber:***@"),
        "the redacted URL must still name the user/host: {msg}"
    );
}
