//! Replay test: recorded Tiber devnet signatures for the zk-inbox program's real batches
//! 4003 (finalized, 290 chunks), 4004 (abandoned, 800 Open instructions submitted, 799 that actually
//! landed -- see below) and 4005
//! (finalized, 899 chunks) -- `fixtures/settlement-watcher/tiber-devnet-batches-4003-4005.json`, 2,794
//! inbox signatures + 11 settlement-program signatures, captured read-only from the real chain
//! (the exact per-batch instruction counts here were verified against the fixture's own
//! account-derived PDA matching, not assumed).
//!
//! Every test in this file needs Docker (`TestPg::start`); run it with
//! `cargo test -p rome-zk-settlement-watcher --test replay` on a machine that has Docker.

mod support;

use rome_zk_settlement_watcher::cursor::{read_cursor, read_derive_cursor, ProgramKind};
use rome_zk_settlement_watcher::ingest::{run_once, PageOutcome, WatcherConfig};
use rome_zk_settlement_watcher::lifecycle::{derive_once, DeriveOutcome};
use rome_zk_settlement_watcher::status::block_status;
use solana_program::pubkey::Pubkey;
use std::time::Instant;
use support::{
    raw_message_from_ixs, FailAfter, FixtureSource, NullBodyOnce, ScriptedSource, TestPg,
};
use zk_inbox_client::{
    abandon_batch_ix, close_batch_ix, finalize_batch_ix, open_batch_ix, open_chunk_ix,
    seal_chunk_ix,
};

const CHAIN_ID: i64 = 200_101;

/// Stand-in settlement program the scripted chain is registered under (inbox accounts are keyed by it).
const SETTLEMENT_PROGRAM: Pubkey = Pubkey::new_from_array([7u8; 32]);

async fn run_inbox_to_completion(
    pool: &sqlx::PgPool,
    source: &mut FixtureSource,
    cfg: WatcherConfig,
) {
    let program_id = source.inbox_program_id();
    let settlement_program = source.settlement_program_id();
    loop {
        match run_once(
            pool,
            source,
            &program_id,
            &settlement_program,
            ProgramKind::Inbox,
            cfg,
        )
        .await
        .expect("run_once")
        {
            PageOutcome::NoNewSignatures => break,
            PageOutcome::Processed { .. } => continue,
        }
    }
}

async fn run_root_to_completion(
    pool: &sqlx::PgPool,
    source: &mut FixtureSource,
    cfg: WatcherConfig,
) {
    let program_id = source.settlement_program_id();
    loop {
        match run_once(
            pool,
            source,
            &program_id,
            &program_id,
            ProgramKind::Root,
            cfg,
        )
        .await
        .expect("run_once")
        {
            PageOutcome::NoNewSignatures => break,
            PageOutcome::Processed { .. } => continue,
        }
    }
}

/// Drains the derive pass (a separate pass, reading only the database, over
/// already-ingested `settlement_tx` rows) until nothing is left to apply. Every caller of this helper has
/// already run its ingest walk(s) to completion, so the gate (`IngestWalkInProgress`) is never expected
/// here -- a test that means to exercise the gate itself calls `derive_once` directly instead.
async fn derive_to_completion(pool: &sqlx::PgPool) {
    loop {
        match derive_once(pool, 1_000).await.expect("derive_once") {
            DeriveOutcome::NoNewRows => break,
            DeriveOutcome::Processed { .. } => continue,
            DeriveOutcome::IngestWalkInProgress => {
                panic!("derive_to_completion called while an ingest walk was still in progress")
            }
        }
    }
}

/// The core replay assertion: after ingesting every recorded inbox signature and deriving lifecycle
/// state, the `batch` table shows exactly what the real chain recorded -- 4003 and 4005 finalized at
/// their real `expected_count` (290, 899), 4004 abandoned. These numbers come from the fixture's own
/// `OpenBatch` instruction data (verified independently against the chain before this fixture was
/// recorded), not assumed.
#[tokio::test]
async fn replay_reproduces_batch_rows_for_4003_4004_and_4005() {
    let pg = TestPg::start().await;
    let mut source = FixtureSource::load();
    run_inbox_to_completion(&pg.pool, &mut source, WatcherConfig::default()).await;
    derive_to_completion(&pg.pool).await;

    let rows: Vec<(i64, i64, String)> = sqlx::query_as(
        "SELECT batch_id, expected_count, status FROM batch WHERE chain_id = $1 ORDER BY batch_id",
    )
    .bind(CHAIN_ID)
    .fetch_all(&pg.pool)
    .await
    .unwrap();

    assert_eq!(
        rows,
        vec![
            (4003, 290, "finalized".to_string()),
            (4004, 899, "abandoned".to_string()),
            (4005, 899, "finalized".to_string()),
        ]
    );
}

/// `inbox_chunk` row counts per batch match the real number of chunks the fixture actually carries --
/// not just the `expected_count` a batch declared at `OpenBatch` time (4004's is 899 but only 800 chunk
/// bundles were ever submitted before the batch was abandoned) -- and not even 800: one of those 800
/// signatures (`Rvzc9XWD...`, slot 496424507) failed on-chain (`ComputationalBudgetExceeded` on its
/// `SealLeaf` instruction, which reverts the whole atomic transaction, `Open` included), so only 799
/// chunk PDAs actually exist on-chain for batch 4004 -- the derive pass correctly derives no chunk row
/// from a failed transaction (`lifecycle::derive_once`: `if raw.err { continue }`), matching real chain
/// state rather than the raw instruction count. Every row comes from `Open` (which always decodes: its
/// shape has not changed).
///
/// `sealed` is asserted at 0 for all three batches -- a real, verified result, not a gap in this test:
/// Tiber's *live* inbox program predates this repo's `Seal { len, body_hash }` shape.
/// Every real `Seal` instruction in the fixture, in every batch including the newest (4005),
/// is the older 5-byte `{ len: u32 }` payload -- confirmed byte-for-byte against the raw fixture
/// data (`disc=2, len=5` on every chunk-bundle transaction sampled). `zk_inbox_client::decode_instruction`
/// (this repo's current `InboxIx`) cannot borsh-decode a 5-byte buffer into the 37-byte `Seal` variant --
/// correctly (the old 4-byte payload is rejected as InvalidInstructionData,
/// never silently accepted) -- and this crate's decode-resilience design (skip an instruction it cannot
/// decode, keep going) means the chunk row still exists (from `Open`) with `sealed = false` rather than
/// the whole page wedging. Tiber needs a `zk-inbox` redeploy
/// bringing it forward to this shape (the settlement program's own history shows exactly this kind of
/// bring-forward already happened once, via `MigrateChain`) before `sealed`/`body_hash` can ever be
/// populated from this chain's real history.
#[tokio::test]
async fn replay_reproduces_chunk_counts_per_batch() {
    let pg = TestPg::start().await;
    let mut source = FixtureSource::load();
    run_inbox_to_completion(&pg.pool, &mut source, WatcherConfig::default()).await;
    derive_to_completion(&pg.pool).await;

    for (batch_id, expected_chunks) in [(4003i64, 290i64), (4004, 799), (4005, 899)] {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM inbox_chunk WHERE chain_id = $1 AND batch_id = $2",
        )
        .bind(CHAIN_ID)
        .bind(batch_id)
        .fetch_one(&pg.pool)
        .await
        .unwrap();
        assert_eq!(count, expected_chunks, "batch {batch_id} chunk count");

        let (sealed_count,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM inbox_chunk WHERE chain_id = $1 AND batch_id = $2 AND sealed",
        )
        .bind(CHAIN_ID)
        .bind(batch_id)
        .fetch_one(&pg.pool)
        .await
        .unwrap();
        assert_eq!(sealed_count, 0, "batch {batch_id}: Tiber predates the body_hash Seal shape (see doc comment above) -- 0 is the real, verified count");
    }

    // Batch 4004's 799 chunks were `Close`d after the
    // abandon (the fixture's own instruction counts: 799 real `Close`s) -- `closed_tx` must be set for
    // every one of them, never a deleted or forever-live-looking row.
    let (closed_4004,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM inbox_chunk WHERE chain_id = $1 AND batch_id = 4004 AND closed_tx IS NOT NULL",
    )
    .bind(CHAIN_ID)
    .fetch_one(&pg.pool)
    .await
    .unwrap();
    assert_eq!(
        closed_4004, 799,
        "batch 4004: every real Close must be recorded"
    );
}

/// `block_status` (derived at read time): 4003/4005 are inbox-finalized with no root
/// posted yet (Tiber has not posted a root -- real chain state, not a fixture gap) -> "data posted"
/// (the fixture's signatures all report `Finalized` confirmation, so the finalizing tx is itself
/// finalized -- never the "(confirmed)" provisional suffix); 4004 -> "abandoned".
#[tokio::test]
async fn block_status_view_reflects_inbox_lifecycle_with_no_root_posted_yet() {
    let pg = TestPg::start().await;
    let mut source = FixtureSource::load();
    run_inbox_to_completion(&pg.pool, &mut source, WatcherConfig::default()).await;
    derive_to_completion(&pg.pool).await;

    assert_eq!(
        block_status(&pg.pool, CHAIN_ID, 4003).await.unwrap(),
        Some("data posted".to_string())
    );
    assert_eq!(
        block_status(&pg.pool, CHAIN_ID, 4005).await.unwrap(),
        Some("data posted".to_string())
    );
    assert_eq!(
        block_status(&pg.pool, CHAIN_ID, 4004).await.unwrap(),
        Some("abandoned".to_string())
    );
    // A batch id this crate has never seen has no row at all -- read-time, not a stored default.
    assert_eq!(
        block_status(&pg.pool, CHAIN_ID, 999_999).await.unwrap(),
        None
    );
}

/// Re-running ingest+derive after everything has already committed must not error and must not change
/// row counts -- the `ON CONFLICT (sig) DO NOTHING` / upsert guards this file's mutation testing exercises
/// (with the dedupe on sig removed, this test goes red). `until = cursor.last_sig` means a
/// second full ingest naturally finds nothing new; this test additionally rewinds both cursors by hand to
/// force the exact same rows to be reprocessed, proving idempotency rather than merely "there was
/// nothing left to do".
#[tokio::test]
async fn rerunning_an_already_processed_page_is_idempotent() {
    let pg = TestPg::start().await;
    let mut source = FixtureSource::load();
    run_inbox_to_completion(&pg.pool, &mut source, WatcherConfig::default()).await;
    derive_to_completion(&pg.pool).await;

    let (tx_count_before,): (i64,) = sqlx::query_as("SELECT count(*) FROM settlement_tx")
        .fetch_one(&pg.pool)
        .await
        .unwrap();
    let (chunk_count_before,): (i64,) = sqlx::query_as("SELECT count(*) FROM inbox_chunk")
        .fetch_one(&pg.pool)
        .await
        .unwrap();

    // Force a full reprocess: rewind both cursors to "nothing processed yet".
    sqlx::query("DELETE FROM settlement_cursor WHERE kind = 'inbox'")
        .execute(&pg.pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM derive_cursor WHERE kind = 'inbox'")
        .execute(&pg.pool)
        .await
        .unwrap();
    run_inbox_to_completion(&pg.pool, &mut source, WatcherConfig::default()).await;
    derive_to_completion(&pg.pool).await;

    let (tx_count_after,): (i64,) = sqlx::query_as("SELECT count(*) FROM settlement_tx")
        .fetch_one(&pg.pool)
        .await
        .unwrap();
    let (chunk_count_after,): (i64,) = sqlx::query_as("SELECT count(*) FROM inbox_chunk")
        .fetch_one(&pg.pool)
        .await
        .unwrap();

    assert_eq!(
        tx_count_before, tx_count_after,
        "settlement_tx row count must not grow on replay"
    );
    assert_eq!(
        chunk_count_before, chunk_count_after,
        "inbox_chunk row count must not grow on replay"
    );
}

/// A watcher killed mid-backlog resumes from its durable cursor with no gap and no duplicate. `FailAfter`
/// injects a failure partway through fetching one *page's* transactions (a page's chunks are separate DB
/// transactions purely to bound transaction size, but the durable `backfill_before` marker only advances
/// once the *whole page* has committed -- ingest.rs's own doc comment explains why a marker set mid-page
/// would make the next fetch skip the newer, still-unprocessed remainder of that page permanently). So a
/// kill partway through page 1 leaves the rows that *did* commit durably in place (idempotent, `ON
/// CONFLICT DO NOTHING`), but the cursor unmoved (`backfill_before` still `None`, `last_sig` still `None`
/// -- exactly as if nothing had been walked yet). A second, working run re-walks page 1 from scratch
/// (the first two chunks are harmless no-ops; the rest are new) and finishes the backlog; the final state
/// exactly matches an uninterrupted single run (this file's first two tests already establish that exact
/// expected state).
#[tokio::test]
async fn watcher_killed_mid_page_resumes_with_no_gap_or_duplicate() {
    let pg = TestPg::start().await;
    let cfg = WatcherConfig {
        rpc_page_size: 1_000,
        commit_batch_size: 100,
    };

    // Fail on the 250th get_transaction call -- partway through the 3rd 100-row commit chunk, so the
    // first two chunks (200 rows) must have committed, but page 1 as a whole has not, so the cursor must
    // not have moved at all yet.
    let program_id = FixtureSource::load().inbox_program_id();
    let mut failing = FailAfter::new(FixtureSource::load(), 250);
    let err = run_once(
        &pg.pool,
        &mut failing,
        &program_id,
        &FixtureSource::load().settlement_program_id(),
        ProgramKind::Inbox,
        cfg,
    )
    .await
    .expect_err("the injected failure must propagate");
    let _ = err; // just needs to be an error; message not asserted (source-specific)

    let cursor_after_kill = read_cursor(&pg.pool, ProgramKind::Inbox).await.unwrap();
    let (tx_count_after_kill,): (i64,) = sqlx::query_as("SELECT count(*) FROM settlement_tx")
        .fetch_one(&pg.pool)
        .await
        .unwrap();
    assert_eq!(
        tx_count_after_kill, 200,
        "exactly the two fully-committed 100-row chunks must be visible after the kill"
    );
    assert!(
        cursor_after_kill.backfill_before.is_none(),
        "page 1 never fully committed -- the marker must not have moved (else the newer, \
         still-unprocessed remainder of page 1 would be skipped on resume, never fetched again)"
    );
    assert!(
        cursor_after_kill.last_sig.is_none(),
        "the walk has not reached its end yet -- last_sig must not have moved"
    );

    // Resume with a working source and finish the backlog.
    let mut source = FixtureSource::load();
    run_inbox_to_completion(&pg.pool, &mut source, cfg).await;
    derive_to_completion(&pg.pool).await;

    let rows: Vec<(i64, i64, String)> = sqlx::query_as(
        "SELECT batch_id, expected_count, status FROM batch WHERE chain_id = $1 ORDER BY batch_id",
    )
    .bind(CHAIN_ID)
    .fetch_all(&pg.pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![
            (4003, 290, "finalized".to_string()),
            (4004, 899, "abandoned".to_string()),
            (4005, 899, "finalized".to_string()),
        ],
        "resumed run must reach the exact same final state as an uninterrupted run"
    );

    let cursor_after_resume = read_cursor(&pg.pool, ProgramKind::Inbox).await.unwrap();
    assert!(
        cursor_after_resume.last_sig.is_some(),
        "the walk must have reached its end after the resume completes"
    );

    // No duplicates: settlement_tx.sig is UNIQUE, but also assert row count == distinct signature count
    // in the fixture that were actually processed (every inbox record has a decodable message here).
    let (total_tx,): (i64,) = sqlx::query_as("SELECT count(*) FROM settlement_tx")
        .fetch_one(&pg.pool)
        .await
        .unwrap();
    let (distinct_sig,): (i64,) = sqlx::query_as("SELECT count(DISTINCT sig) FROM settlement_tx")
        .fetch_one(&pg.pool)
        .await
        .unwrap();
    assert_eq!(
        total_tx, distinct_sig,
        "no duplicate signatures after resume"
    );
    assert_eq!(
        total_tx, 2_794,
        "every fixture signature landed exactly once"
    );
}

/// The settlement-program half of the fixture: 11 real signatures, none of which are `PostRoot` (Tiber
/// has not posted a root yet). Every one still lands as a `settlement_tx` row, one
/// `settlement_tx_program` row with `kind = 'root'` carrying the real instruction name(s) it carried.
#[tokio::test]
async fn replay_ingests_the_settlement_program_history() {
    let pg = TestPg::start().await;
    let mut source = FixtureSource::load();
    run_root_to_completion(&pg.pool, &mut source, WatcherConfig::default()).await;

    let (count,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM settlement_tx_program WHERE kind = 'root'")
            .fetch_one(&pg.pool)
            .await
            .unwrap();
    assert_eq!(count, 11);

    let mut ix_kinds: Vec<String> = sqlx::query_scalar(
        "SELECT stp.ix_kind FROM settlement_tx_program stp
         JOIN settlement_tx st ON st.id = stp.settlement_tx_id
         WHERE stp.kind = 'root' ORDER BY st.slot",
    )
    .fetch_all(&pg.pool)
    .await
    .unwrap();
    ix_kinds.sort();
    ix_kinds.dedup();
    // Real Tiber history (verified against the fixture, not assumed): InitGlobalConfig + AllowReservedId
    // registered the chain -- no PostRoot/PostRootProved anywhere yet. The genesis `InitChain` call (the
    // very first settlement-program signature, `4iPFNq3cyoDr...`, slot 496028512) is real on-chain
    // history too, but its 281-byte payload predates this repo's current `InitChainArgs`
    // (`registry_entries: Vec<RegistryEntryArg>` and related fields were added later) --
    // `zk_settlement_client::decode_instruction` correctly fails closed on it rather than misreading
    // bytes, so it joins the `"Unknown"` bucket.
    //
    // The real, already-recorded `MigrateChain` call (chain_id only, 8-byte body, discriminant 15) still
    // decodes by name: that body was retired in place instead of grown — a shipped
    // instruction's bytes never change at its discriminant, so recorded history stays readable; the v2
    // bring-forward is `MigrateChainV2` (23).
    // The other 7 `"Unknown"` signatures are the program's own deploy/upgrade transactions
    // (`BPFLoaderUpgradeable`/System Program as the executing program, the settlement program only as the
    // account being deployed to) -- real history `getSignaturesForAddress` legitimately returns for this
    // address. Every signature still gets a row (never dropped) even when nothing in it decodes.
    assert_eq!(
        ix_kinds,
        vec![
            "AllowReservedId".to_string(),
            "InitGlobalConfig".to_string(),
            "MigrateChain".to_string(),
            "Unknown".to_string(),
        ]
    );
}

/// Measured throughput (pages/s and rows/s against the local Postgres) ingesting all
/// 2,794 real inbox signatures from a cold cursor, against the ephemeral test Postgres this file's other
/// tests already use -- printed with `--nocapture`, not asserted against a fixed threshold (a CI machine's
/// own disk/CPU varies run to run; this is a measurement, not a regression gate).
#[tokio::test]
async fn measures_ingest_throughput_against_local_postgres() {
    let pg = TestPg::start().await;
    let mut source = FixtureSource::load();
    let program_id = source.inbox_program_id();
    let settlement_program = source.settlement_program_id();
    let cfg = WatcherConfig {
        rpc_page_size: 1_000,
        commit_batch_size: 500,
    };

    let started = Instant::now();
    let mut pages = 0usize;
    let mut rows = 0usize;
    loop {
        match run_once(
            &pg.pool,
            &mut source,
            &program_id,
            &settlement_program,
            ProgramKind::Inbox,
            cfg,
        )
        .await
        .unwrap()
        {
            PageOutcome::NoNewSignatures => break,
            PageOutcome::Processed {
                signatures,
                commits,
                ..
            } => {
                pages += commits;
                rows += signatures;
            }
        }
    }
    let elapsed = started.elapsed();
    println!(
        "ingest measurement: {rows} rows across {pages} commit(s) in {elapsed:?} -- {:.1} rows/s, {:.2} commits/s",
        rows as f64 / elapsed.as_secs_f64(),
        pages as f64 / elapsed.as_secs_f64(),
    );
    assert_eq!(rows, 2_794);
}

// ---------------------------------------------------------------------------------------------
// Lifecycle correctness: scripted, hand-built transactions via the real
// zk-inbox-client builders -- the fixture's own real history never carries a partial FinalizeBatch or a
// two-Open transaction, so these use `ScriptedSource` instead (same `Source` contract).
// ---------------------------------------------------------------------------------------------

fn payer() -> Pubkey {
    Pubkey::new_unique()
}

/// The inbox program is shared by every settlement program, and `batch` is keyed on `(chain_id, batch_id)`. An
/// `OpenBatch` for the same `(chain_id, batch)` under another settlement program must neither create the row
/// nor take it from the real one, whichever of the two lands first.
#[tokio::test]
async fn a_foreign_open_batch_for_the_same_chain_and_batch_neither_creates_nor_overrides_the_row() {
    let pg = TestPg::start().await;
    let program_id = Pubkey::new_unique();
    let foreign_settlement = Pubkey::new_unique();
    let payer = payer();

    // Only the foreign open: no row.
    let mut source = ScriptedSource::new(program_id);
    let foreign_ix = open_batch_ix(&program_id, &payer, 200_101, 1, 99, &foreign_settlement);
    source.push_newest(
        "1foreign0000000000000000000000000000000000000000000000000",
        100,
        raw_message_from_ixs(&payer, &[foreign_ix]),
        false,
    );
    run_inbox_to_completion_scripted(&pg.pool, &mut source, &program_id).await;
    derive_to_completion(&pg.pool).await;
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT expected_count, batch_pda FROM batch WHERE chain_id = 200101 AND batch_id = 1",
    )
    .fetch_all(&pg.pool)
    .await
    .unwrap();
    assert!(
        rows.is_empty(),
        "a foreign OpenBatch created a row: {rows:?}"
    );

    // The real open lands after the foreign one, then a second foreign open lands after it: the row is the real one.
    let own_ix = open_batch_ix(&program_id, &payer, 200_101, 1, 10, &SETTLEMENT_PROGRAM);
    source.push_newest(
        "2own000000000000000000000000000000000000000000000000000000",
        101,
        raw_message_from_ixs(&payer, &[own_ix]),
        false,
    );
    let later_foreign_ix = open_batch_ix(&program_id, &payer, 200_101, 1, 77, &foreign_settlement);
    source.push_newest(
        "3foreign0000000000000000000000000000000000000000000000000",
        102,
        raw_message_from_ixs(&payer, &[later_foreign_ix]),
        false,
    );
    run_inbox_to_completion_scripted(&pg.pool, &mut source, &program_id).await;
    derive_to_completion(&pg.pool).await;
    let own_pda = zk_inbox_client::batch_pda(&program_id, &SETTLEMENT_PROGRAM, 200_101, 1)
        .0
        .to_string();
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT expected_count, batch_pda FROM batch WHERE chain_id = 200101 AND batch_id = 1",
    )
    .fetch_all(&pg.pool)
    .await
    .unwrap();
    assert_eq!(rows, vec![(10, own_pda)]);
}

/// RED before the fix: a partial `FinalizeBatch { step: 1 }` (fewer leaves than `expected_count`) must
/// leave the batch `'open'`, mirroring `programs/zk-inbox/src/batch.rs::finalize_batch_inner` exactly --
/// only a call whose cursor reaches `expected_count` may set `status = 'finalized'`.
#[tokio::test]
async fn partial_finalize_batch_leaves_the_batch_open_until_the_cursor_completes() {
    let pg = TestPg::start().await;
    let program_id = Pubkey::new_unique();
    let payer = payer();
    let mut source = ScriptedSource::new(program_id);

    let open_ix = open_batch_ix(&program_id, &payer, 200_101, 1, 10, &SETTLEMENT_PROGRAM);
    source.push_newest(
        "1open00000000000000000000000000000000000000000000000000000",
        100,
        raw_message_from_ixs(&payer, &[open_ix]),
        false,
    );
    let step1_ix = finalize_batch_ix(&program_id, &payer, &SETTLEMENT_PROGRAM, 200_101, 1, 1);
    source.push_newest(
        "2step1000000000000000000000000000000000000000000000000000",
        101,
        raw_message_from_ixs(&payer, &[step1_ix]),
        false,
    );

    run_inbox_to_completion_scripted(&pg.pool, &mut source, &program_id).await;
    derive_to_completion(&pg.pool).await;

    let (finalize_cursor, status): (i64, String) = sqlx::query_as(
        "SELECT finalize_cursor, status FROM batch WHERE chain_id = 200101 AND batch_id = 1",
    )
    .fetch_one(&pg.pool)
    .await
    .unwrap();
    assert_eq!(finalize_cursor, 1);
    assert_eq!(status, "open", "step=1 of 10 must not finalize the batch");

    // The completing call: step=0 means "the rest, in this call".
    let step0_ix = finalize_batch_ix(&program_id, &payer, &SETTLEMENT_PROGRAM, 200_101, 1, 0);
    source.push_newest(
        "3step0000000000000000000000000000000000000000000000000000",
        102,
        raw_message_from_ixs(&payer, &[step0_ix]),
        false,
    );
    run_inbox_to_completion_scripted(&pg.pool, &mut source, &program_id).await;
    derive_to_completion(&pg.pool).await;

    let (finalize_cursor2, status2): (i64, String) = sqlx::query_as(
        "SELECT finalize_cursor, status FROM batch WHERE chain_id = 200101 AND batch_id = 1",
    )
    .fetch_one(&pg.pool)
    .await
    .unwrap();
    assert_eq!(finalize_cursor2, 10);
    assert_eq!(status2, "finalized", "step=0 must complete the batch");
}

/// RED before the fix: a `Seal`-only transaction (no `Open` for that chunk anywhere) must not fabricate an
/// `inbox_chunk` row -- the chunk PDA it names has never been `Open`ed, so the `UPDATE ... WHERE
/// chunk_pda = $1` is a genuine no-op.
#[tokio::test]
async fn seal_only_transaction_creates_no_inbox_chunk_row() {
    let pg = TestPg::start().await;
    let program_id = Pubkey::new_unique();
    let payer = payer();
    let mut source = ScriptedSource::new(program_id);

    let seal_ix = seal_chunk_ix(
        &program_id,
        &payer,
        &SETTLEMENT_PROGRAM,
        200_101,
        1,
        17,
        3_681,
        [7u8; 32],
    );
    source.push_newest(
        "1seal00000000000000000000000000000000000000000000000000000",
        100,
        raw_message_from_ixs(&payer, &[seal_ix]),
        false,
    );

    run_inbox_to_completion_scripted(&pg.pool, &mut source, &program_id).await;
    derive_to_completion(&pg.pool).await;

    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM inbox_chunk")
        .fetch_one(&pg.pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "a Seal with no prior Open must not create a row");
}

/// `Open` in one transaction, `Seal` in a *later* one (the ordinary two-tx case, not the one-frame-one-tx
/// bundle): one row, ending sealed. Proves attribution is keyed by the chunk PDA across transactions, not
/// merged only within a single decoded message.
#[tokio::test]
async fn open_then_seal_in_separate_transactions_produce_one_sealed_row() {
    let pg = TestPg::start().await;
    let program_id = Pubkey::new_unique();
    let payer = payer();
    let mut source = ScriptedSource::new(program_id);

    let open_ix = open_chunk_ix(
        &program_id,
        &payer,
        &SETTLEMENT_PROGRAM,
        200_101,
        1,
        17,
        3_681,
    );
    source.push_newest(
        "1openA000000000000000000000000000000000000000000000000000",
        100,
        raw_message_from_ixs(&payer, &[open_ix]),
        false,
    );
    let seal_ix = seal_chunk_ix(
        &program_id,
        &payer,
        &SETTLEMENT_PROGRAM,
        200_101,
        1,
        17,
        3_681,
        [7u8; 32],
    );
    source.push_newest(
        "2sealB000000000000000000000000000000000000000000000000000",
        200,
        raw_message_from_ixs(&payer, &[seal_ix]),
        false,
    );

    run_inbox_to_completion_scripted(&pg.pool, &mut source, &program_id).await;
    derive_to_completion(&pg.pool).await;

    let rows: Vec<(bool, i64)> = sqlx::query_as(
        "SELECT sealed, byte_len FROM inbox_chunk WHERE chain_id = 200101 AND batch_id = 1",
    )
    .fetch_all(&pg.pool)
    .await
    .unwrap();
    assert_eq!(rows, vec![(true, 3_681)]);
}

/// The derive pass reads `settlement_tx` in `(slot, id)` order, never `settlement_tx.id` (arrival) order:
/// with `rpc_page_size = 1`, the backward walk ingests `Seal` (the newer signature, slot 200) *before*
/// `Open` (the older one, slot 100) -- `Open` gets the higher `settlement_tx.id` despite the lower slot.
/// A derive pass that sorted by `id` instead of `slot` would apply `Seal` first (no row to
/// update, a no-op) and `Open` second (inserting an unsealed row) -- ending unsealed. The real,
/// slot-ordered pass must still end up sealed regardless of this arrival order (the feed, not a shadow table).
#[tokio::test]
async fn derive_applies_lifecycle_events_in_slot_order_not_ingest_arrival_order() {
    let pg = TestPg::start().await;
    let program_id = Pubkey::new_unique();
    let payer = payer();
    let mut source = ScriptedSource::new(program_id);

    let open_ix = open_chunk_ix(
        &program_id,
        &payer,
        &SETTLEMENT_PROGRAM,
        200_101,
        1,
        17,
        3_681,
    );
    source.push_newest(
        "1openOld0000000000000000000000000000000000000000000000000",
        100, // older slot
        raw_message_from_ixs(&payer, &[open_ix]),
        false,
    );
    let seal_ix = seal_chunk_ix(
        &program_id,
        &payer,
        &SETTLEMENT_PROGRAM,
        200_101,
        1,
        17,
        3_681,
        [7u8; 32],
    );
    source.push_newest(
        "2sealNew0000000000000000000000000000000000000000000000000",
        200, // newer slot -- ingested FIRST because the walk goes backward from the tip
        raw_message_from_ixs(&payer, &[seal_ix]),
        false,
    );

    // rpc_page_size = 1 forces Seal (page 1, the newest) to commit (and get the lower settlement_tx.id)
    // strictly before Open (page 2, older) does.
    let cfg = WatcherConfig {
        rpc_page_size: 1,
        commit_batch_size: 1,
    };
    loop {
        match run_once(
            &pg.pool,
            &mut source,
            &program_id,
            &SETTLEMENT_PROGRAM,
            ProgramKind::Inbox,
            cfg,
        )
        .await
        .unwrap()
        {
            PageOutcome::NoNewSignatures => break,
            PageOutcome::Processed { .. } => continue,
        }
    }

    let ids: Vec<(i64, i64)> = sqlx::query_as("SELECT id, slot FROM settlement_tx ORDER BY id")
        .fetch_all(&pg.pool)
        .await
        .unwrap();
    assert_eq!(ids.len(), 2);
    assert!(
        ids[0].1 > ids[1].1,
        "Seal (slot 200) must have the LOWER id than Open (slot 100) -- arrival order is newest-first: {ids:?}"
    );

    derive_to_completion(&pg.pool).await;

    let (sealed,): (bool,) =
        sqlx::query_as("SELECT sealed FROM inbox_chunk WHERE chain_id = 200101 AND batch_id = 1")
            .fetch_one(&pg.pool)
            .await
            .unwrap();
    assert!(
        sealed,
        "derive must apply Open before Seal by slot order, regardless of ingest arrival order"
    );
}

/// RED before the fix: `Open` and `Seal`
/// for the SAME chunk, sharing the SAME Solana slot, must still end up sealed regardless of
/// `rpc_page_size` -- a page boundary must never split a slot. With `rpc_page_size = 1`, the unfixed code
/// puts Seal (the later, so newer, signature) in its own page and commits it (getting the LOWER
/// `settlement_tx.id`) strictly before Open's page commits -- inverting `(slot, id)` order for this pair
/// and making `derive_once` apply Seal (a no-op, no row yet) before Open (inserts unsealed).
#[tokio::test]
async fn same_slot_open_then_seal_ends_sealed_regardless_of_rpc_page_size() {
    let pg = TestPg::start().await;
    let program_id = Pubkey::new_unique();
    let payer = payer();
    let mut source = ScriptedSource::new(program_id);

    let open_ix = open_chunk_ix(
        &program_id,
        &payer,
        &SETTLEMENT_PROGRAM,
        200_101,
        1,
        17,
        3_681,
    );
    source.push_newest(
        "1openSameSlot0000000000000000000000000000000000000000000000",
        100,
        raw_message_from_ixs(&payer, &[open_ix]),
        false,
    );
    let seal_ix = seal_chunk_ix(
        &program_id,
        &payer,
        &SETTLEMENT_PROGRAM,
        200_101,
        1,
        17,
        3_681,
        [7u8; 32],
    );
    source.push_newest(
        "2sealSameSlot0000000000000000000000000000000000000000000000",
        100, // SAME slot as Open -- Seal is the later on-chain instruction, so the newer signature.
        raw_message_from_ixs(&payer, &[seal_ix]),
        false,
    );

    // rpc_page_size = 1 forces Open and Seal into two separate RPC pages despite sharing a slot -- the
    // exact scenario the fix must handle.
    let cfg = WatcherConfig {
        rpc_page_size: 1,
        commit_batch_size: 1,
    };
    loop {
        match run_once(
            &pg.pool,
            &mut source,
            &program_id,
            &SETTLEMENT_PROGRAM,
            ProgramKind::Inbox,
            cfg,
        )
        .await
        .unwrap()
        {
            PageOutcome::NoNewSignatures => break,
            PageOutcome::Processed { .. } => continue,
        }
    }

    derive_to_completion(&pg.pool).await;

    let (sealed,): (bool,) =
        sqlx::query_as("SELECT sealed FROM inbox_chunk WHERE chain_id = 200101 AND batch_id = 1")
            .fetch_one(&pg.pool)
            .await
            .unwrap();
    assert!(
        sealed,
        "Open and Seal sharing one slot must land in the same committed unit and derive in on-chain \
         order, regardless of rpc_page_size"
    );
}

/// RED before the fix: a transient Bigtable-null
/// `getTransaction` body for a signature `getSignaturesForAddress` already listed must be retried, not
/// silently dropped from the feed -- the unfixed code loses the row's lifecycle event forever the first
/// time this happens while still advancing past it.
#[tokio::test]
async fn a_transient_null_body_is_retried_not_silently_skipped() {
    let pg = TestPg::start().await;
    let program_id = Pubkey::new_unique();
    let payer = payer();
    let mut inner = ScriptedSource::new(program_id);
    let open_ix = open_chunk_ix(
        &program_id,
        &payer,
        &SETTLEMENT_PROGRAM,
        200_101,
        1,
        17,
        3_681,
    );
    let sig = "1nullOnceThenReal00000000000000000000000000000000000000000".to_string();
    inner.push_newest(
        sig.clone(),
        100,
        raw_message_from_ixs(&payer, &[open_ix]),
        false,
    );

    // Two transient nulls, well inside the retry budget (NULL_BODY_MAX_RETRIES = 3) -- must succeed once
    // retried, not be skipped.
    let mut source = NullBodyOnce::new(inner, sig, 2);
    loop {
        match run_once(
            &pg.pool,
            &mut source,
            &program_id,
            &SETTLEMENT_PROGRAM,
            ProgramKind::Inbox,
            WatcherConfig::default(),
        )
        .await
        .unwrap()
        {
            PageOutcome::NoNewSignatures => break,
            PageOutcome::Processed { .. } => continue,
        }
    }
    derive_to_completion(&pg.pool).await;

    let (count,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM inbox_chunk WHERE chain_id = 200101 AND batch_id = 1")
            .fetch_one(&pg.pool)
            .await
            .unwrap();
    assert_eq!(
        count, 1,
        "the Open's chunk event must still land once the transient null resolves"
    );
}

/// RED before the fix: `derive_once` must
/// refuse to run at all while an ingest walk is unfinished -- an ingest error on an older page (an RPC
/// timeout mid-backfill is the ordinary case) must never let the derive cursor jump past rows a
/// not-yet-ingested page will still supply. Here Seal (the newer signature) commits, then Open's (older)
/// fetch fails; `derive_once` must report `IngestWalkInProgress` and derive nothing until the walk
/// actually completes.
#[tokio::test]
async fn derive_is_gated_while_an_ingest_walk_is_unfinished() {
    let pg = TestPg::start().await;
    let program_id = Pubkey::new_unique();
    let payer = payer();

    fn scripted(program_id: Pubkey, payer: &Pubkey) -> ScriptedSource {
        let mut source = ScriptedSource::new(program_id);
        let open_ix = open_chunk_ix(
            &program_id,
            payer,
            &SETTLEMENT_PROGRAM,
            200_101,
            1,
            17,
            3_681,
        );
        source.push_newest(
            "1openOlder000000000000000000000000000000000000000000000000",
            100,
            raw_message_from_ixs(payer, &[open_ix]),
            false,
        );
        let seal_ix = seal_chunk_ix(
            &program_id,
            payer,
            &SETTLEMENT_PROGRAM,
            200_101,
            1,
            17,
            3_681,
            [7u8; 32],
        );
        source.push_newest(
            "2sealNewer000000000000000000000000000000000000000000000000",
            200,
            raw_message_from_ixs(payer, &[seal_ix]),
            false,
        );
        source
    }

    let cfg = WatcherConfig {
        rpc_page_size: 1,
        commit_batch_size: 1,
    };
    // Seal's own fetch (call 1) succeeds; Open's (call 2) fails -- the walk aborts with the newer
    // signature already durably committed but the older one not ingested at all.
    let mut failing = FailAfter::new(scripted(program_id, &payer), 2);
    let err = run_once(
        &pg.pool,
        &mut failing,
        &program_id,
        &SETTLEMENT_PROGRAM,
        ProgramKind::Inbox,
        cfg,
    )
    .await
    .expect_err("the injected failure must propagate");
    let _ = err;

    let cursor = read_cursor(&pg.pool, ProgramKind::Inbox).await.unwrap();
    assert!(
        cursor.backfill_head_sig.is_some(),
        "the walk must still be marked in progress after the failure"
    );

    let outcome = derive_once(&pg.pool, 1_000).await.unwrap();
    assert_eq!(
        outcome,
        DeriveOutcome::IngestWalkInProgress,
        "derive must refuse to run while an ingest walk is unfinished"
    );
    let (chunk_count_gated,): (i64,) = sqlx::query_as("SELECT count(*) FROM inbox_chunk")
        .fetch_one(&pg.pool)
        .await
        .unwrap();
    assert_eq!(
        chunk_count_gated, 0,
        "nothing may be derived while the walk is gated"
    );

    // Resume with a working source and let the walk finish.
    let mut source = scripted(program_id, &payer);
    loop {
        match run_once(
            &pg.pool,
            &mut source,
            &program_id,
            &SETTLEMENT_PROGRAM,
            ProgramKind::Inbox,
            cfg,
        )
        .await
        .unwrap()
        {
            PageOutcome::NoNewSignatures => break,
            PageOutcome::Processed { .. } => continue,
        }
    }
    derive_to_completion(&pg.pool).await;

    let (sealed,): (bool,) =
        sqlx::query_as("SELECT sealed FROM inbox_chunk WHERE chain_id = 200101 AND batch_id = 1")
            .fetch_one(&pg.pool)
            .await
            .unwrap();
    assert!(
        sealed,
        "the older Open must still be derived once the walk completes"
    );
}

/// DB-fault atomicity: a CHECK constraint that fires partway through one
/// commit chunk's `write_page` call must roll back that whole chunk's transaction -- the committed row
/// count stays a multiple of `commit_batch_size`. A prior page's own `backfill_before` marker (set only
/// once that whole page committed) must still name a signature that really is in `settlement_tx`; it is
/// never advanced into the page the fault occurred in.
#[tokio::test]
async fn a_db_fault_mid_chunk_never_advances_the_cursor_past_uncommitted_rows() {
    let pg = TestPg::start().await;
    let program_id = Pubkey::new_unique();
    let payer = payer();
    let mut source = ScriptedSource::new(program_id);

    // 6 plain no-op transactions (empty messages decode to nothing -- fine, this test only exercises the
    // raw settlement_tx write path, not lifecycle derivation). Paired by slot (`100 + i / 2`), two
    // signatures per slot: with `rpc_page_size = 2`, a page boundary never splits a slot,
    // so each conceptual "page" here still lands and commits as one whole 2-row unit -- exactly
    // what this test's commit-count assertions below assume.
    let poison_sig = "3poison00000000000000000000000000000000000000000000000000";
    for i in 0..6u64 {
        let sig = if i == 2 {
            poison_sig.to_string()
        } else {
            format!("sig{i}00000000000000000000000000000000000000000000000000000")
        };
        let ix = open_chunk_ix(&program_id, &payer, &SETTLEMENT_PROGRAM, 1, i, 0, 1);
        source.push_newest(sig, 100 + i / 2, raw_message_from_ixs(&payer, &[ix]), false);
    }

    sqlx::query(&format!(
        "ALTER TABLE settlement_tx ADD CONSTRAINT review_fail CHECK (sig <> '{poison_sig}')"
    ))
    .execute(&pg.pool)
    .await
    .unwrap();

    // Two rows per page: page 1 (the newest two, sig5/sig4) has no poison and fully commits -- its
    // backfill marker advances -- before page 2 (sig3/poison) hits the constraint and fails whole.
    let cfg = WatcherConfig {
        rpc_page_size: 2,
        commit_batch_size: 2,
    };
    let err = run_once(
        &pg.pool,
        &mut source,
        &program_id,
        &SETTLEMENT_PROGRAM,
        ProgramKind::Inbox,
        cfg,
    )
    .await
    .expect_err("the poisoned row must make its whole page's transaction fail");
    let _ = err;

    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM settlement_tx")
        .fetch_one(&pg.pool)
        .await
        .unwrap();
    assert_eq!(
        count % cfg.commit_batch_size as i64,
        0,
        "committed rows must be a whole number of commit chunks: got {count}"
    );
    assert!(
        count > 0,
        "the chunk(s) before the poisoned one must have committed"
    );

    let cursor = read_cursor(&pg.pool, ProgramKind::Inbox).await.unwrap();
    let named_sig = cursor
        .backfill_before
        .expect("a mid-walk marker must exist -- at least one chunk committed");
    let (exists,): (bool,) =
        sqlx::query_as("SELECT EXISTS(SELECT 1 FROM settlement_tx WHERE sig = $1)")
            .bind(&named_sig)
            .fetch_one(&pg.pool)
            .await
            .unwrap();
    assert!(
        exists,
        "the cursor must never name an uncommitted signature"
    );
}

/// `block_status`'s `(confirmed)` suffix: a finalized batch
/// whose *finalizing transaction* has not itself reached `finalized` status must read `data posted
/// (confirmed)`, not `data posted` -- `getSignaturesForAddress`/`ingest::run_once` write every row at
/// `confirmed` (`ScriptedSource` mirrors this: every ingested row starts `confirmed`), so this is the
/// state immediately after ingest+derive, before `finality::track_finality` ever runs. Once the
/// finalizing tx's own row is flipped to `finalized`, the suffix must disappear.
#[tokio::test]
async fn block_status_reports_the_confirmed_suffix_until_the_finalizing_tx_itself_finalizes() {
    let pg = TestPg::start().await;
    let program_id = Pubkey::new_unique();
    let payer = payer();
    let mut source = ScriptedSource::new(program_id);

    let open_ix = open_batch_ix(&program_id, &payer, 200_101, 1, 1, &SETTLEMENT_PROGRAM);
    source.push_newest(
        "1open00000000000000000000000000000000000000000000000000000",
        100,
        raw_message_from_ixs(&payer, &[open_ix]),
        false,
    );
    let finalize_ix = finalize_batch_ix(&program_id, &payer, &SETTLEMENT_PROGRAM, 200_101, 1, 0);
    source.push_newest(
        "2finalize0000000000000000000000000000000000000000000000000",
        101,
        raw_message_from_ixs(&payer, &[finalize_ix]),
        false,
    );

    run_inbox_to_completion_scripted(&pg.pool, &mut source, &program_id).await;
    derive_to_completion(&pg.pool).await;

    assert_eq!(
        block_status(&pg.pool, 200_101, 1).await.unwrap(),
        Some("data posted (confirmed)".to_string()),
        "the finalizing tx is itself only 'confirmed' so far -- the view must say so"
    );

    sqlx::query(
        "UPDATE settlement_tx SET status = 'finalized' WHERE sig = '2finalize0000000000000000000000000000000000000000000000000'",
    )
    .execute(&pg.pool)
    .await
    .unwrap();

    assert_eq!(
        block_status(&pg.pool, 200_101, 1).await.unwrap(),
        Some("data posted".to_string()),
        "once the finalizing tx itself finalizes, the suffix must drop"
    );
}

/// Helper for the scripted tests above: drains `run_once` against a [`ScriptedSource`] the same way
/// `run_inbox_to_completion` does for the real fixture.
async fn run_inbox_to_completion_scripted(
    pool: &sqlx::PgPool,
    source: &mut ScriptedSource,
    program_id: &Pubkey,
) {
    loop {
        match run_once(
            pool,
            source,
            program_id,
            &SETTLEMENT_PROGRAM,
            ProgramKind::Inbox,
            WatcherConfig::default(),
        )
        .await
        .expect("run_once")
        {
            PageOutcome::NoNewSignatures => break,
            PageOutcome::Processed { .. } => continue,
        }
    }
}

/// Sanity check that the derive cursor is actually persisted and monotonic (not re-checked in this file's
/// other tests, which only check end state) -- after a full derive-to-completion pass, `derive_cursor`
/// names the newest row derived.
#[tokio::test]
async fn derive_cursor_advances_to_the_newest_derived_row() {
    let pg = TestPg::start().await;
    let mut source = FixtureSource::load();
    run_inbox_to_completion(&pg.pool, &mut source, WatcherConfig::default()).await;
    derive_to_completion(&pg.pool).await;

    let cursor = read_derive_cursor(&pg.pool, ProgramKind::Inbox)
        .await
        .unwrap();
    let (max_slot,): (i64,) = sqlx::query_as(
        "SELECT max(st.slot) FROM settlement_tx st JOIN settlement_tx_program stp ON stp.settlement_tx_id = st.id WHERE stp.kind = 'inbox'",
    )
    .fetch_one(&pg.pool)
    .await
    .unwrap();
    assert_eq!(cursor.last_slot, max_slot);
}

/// `source_max_slot` (for lag reporting) is populated from the walk's very first (newest)
/// page and never regresses -- previously verified only by reading `ingest.rs`/`cursor.rs`, not exercised
/// by a test.
#[tokio::test]
async fn source_max_slot_is_populated_from_the_first_page_and_never_regresses() {
    let pg = TestPg::start().await;
    let mut source = FixtureSource::load();
    run_inbox_to_completion(&pg.pool, &mut source, WatcherConfig::default()).await;

    let cursor = read_cursor(&pg.pool, ProgramKind::Inbox).await.unwrap();
    let source_max_slot = cursor
        .last_slot
        .expect("a completed walk always sets last_slot");
    let (source_max_slot_row,): (Option<i64>,) =
        sqlx::query_as("SELECT source_max_slot FROM settlement_cursor WHERE kind = 'inbox'")
            .fetch_one(&pg.pool)
            .await
            .unwrap();
    assert_eq!(
        source_max_slot_row,
        Some(source_max_slot),
        "source_max_slot must equal the newest slot this walk ever saw"
    );

    // A later poll (nothing new -- the fixture's whole history is already ingested) must not regress it.
    run_inbox_to_completion(&pg.pool, &mut source, WatcherConfig::default()).await;
    let (source_max_slot_after,): (Option<i64>,) =
        sqlx::query_as("SELECT source_max_slot FROM settlement_cursor WHERE kind = 'inbox'")
            .fetch_one(&pg.pool)
            .await
            .unwrap();
    assert_eq!(source_max_slot_after, Some(source_max_slot));
}

// ---------------------------------------------------------------------------------------------
// Batch terminal state: status stays in {open, finalized,
// abandoned} forever; recycling/abandonment are orthogonal columns, never a fourth status value and
// never sharing `finalized_tx`.
// ---------------------------------------------------------------------------------------------

/// RED before the fix: a finalized batch that is then `CloseBatch`'d (rent
/// reclaimed -- the ordinary steady-state outcome) must keep reading `data posted (confirmed)`
/// forever, never regress to `sequenced`. The unfixed code gave `status` a fourth `'closed'` value the
/// `block_status` view had no arm for.
#[tokio::test]
async fn a_closed_finalized_batch_still_reads_data_posted() {
    let pg = TestPg::start().await;
    let program_id = Pubkey::new_unique();
    let payer = payer();
    let mut source = ScriptedSource::new(program_id);

    let open_ix = open_batch_ix(&program_id, &payer, 200_101, 1, 1, &SETTLEMENT_PROGRAM);
    source.push_newest(
        "1open00000000000000000000000000000000000000000000000000000",
        100,
        raw_message_from_ixs(&payer, &[open_ix]),
        false,
    );
    let finalize_ix = finalize_batch_ix(&program_id, &payer, &SETTLEMENT_PROGRAM, 200_101, 1, 0);
    source.push_newest(
        "2finalize0000000000000000000000000000000000000000000000000",
        101,
        raw_message_from_ixs(&payer, &[finalize_ix]),
        false,
    );
    let settlement_program = SETTLEMENT_PROGRAM;
    let close_ix = close_batch_ix(&program_id, &payer, &settlement_program, 200_101, 1);
    source.push_newest(
        "3close000000000000000000000000000000000000000000000000000",
        102,
        raw_message_from_ixs(&payer, &[close_ix]),
        false,
    );

    run_inbox_to_completion_scripted(&pg.pool, &mut source, &program_id).await;
    derive_to_completion(&pg.pool).await;

    let (status, closed_tx): (String, Option<i64>) = sqlx::query_as(
        "SELECT status, closed_tx FROM batch WHERE chain_id = 200101 AND batch_id = 1",
    )
    .fetch_one(&pg.pool)
    .await
    .unwrap();
    assert_eq!(status, "finalized", "status must never become 'closed'");
    assert!(closed_tx.is_some(), "CloseBatch must record closed_tx");
    assert_eq!(
        block_status(&pg.pool, 200_101, 1).await.unwrap(),
        Some("data posted (confirmed)".to_string()),
        "a closed, finalized batch must not read as 'sequenced'"
    );
}

/// RED before the fix: `AbandonBatch` must write `abandoned_tx`, never `finalized_tx`
/// -- the unfixed code let an abandon overwrite the same column a real finalize sets. A subsequent
/// `CloseBatch` on the abandoned batch must set `closed_tx` without disturbing `status`.
#[tokio::test]
async fn abandon_then_close_never_touches_finalized_tx() {
    let pg = TestPg::start().await;
    let program_id = Pubkey::new_unique();
    let payer = payer();
    let mut source = ScriptedSource::new(program_id);

    let open_ix = open_batch_ix(&program_id, &payer, 200_101, 1, 10, &SETTLEMENT_PROGRAM);
    source.push_newest(
        "1open00000000000000000000000000000000000000000000000000000",
        100,
        raw_message_from_ixs(&payer, &[open_ix]),
        false,
    );
    let abandon_ix = abandon_batch_ix(&program_id, &payer, &SETTLEMENT_PROGRAM, 200_101, 1);
    source.push_newest(
        "2abandon00000000000000000000000000000000000000000000000000",
        101,
        raw_message_from_ixs(&payer, &[abandon_ix]),
        false,
    );
    let settlement_program = SETTLEMENT_PROGRAM;
    let close_ix = close_batch_ix(&program_id, &payer, &settlement_program, 200_101, 1);
    source.push_newest(
        "3close000000000000000000000000000000000000000000000000000",
        102,
        raw_message_from_ixs(&payer, &[close_ix]),
        false,
    );

    run_inbox_to_completion_scripted(&pg.pool, &mut source, &program_id).await;
    derive_to_completion(&pg.pool).await;

    let (status, finalized_tx, abandoned_tx, closed_tx): (
        String,
        Option<i64>,
        Option<i64>,
        Option<i64>,
    ) = sqlx::query_as(
        "SELECT status, finalized_tx, abandoned_tx, closed_tx FROM batch \
         WHERE chain_id = 200101 AND batch_id = 1",
    )
    .fetch_one(&pg.pool)
    .await
    .unwrap();
    assert_eq!(status, "abandoned");
    assert!(
        finalized_tx.is_none(),
        "abandon must never write finalized_tx"
    );
    assert!(abandoned_tx.is_some(), "abandon must write abandoned_tx");
    assert!(
        closed_tx.is_some(),
        "the later CloseBatch must record closed_tx"
    );
}

/// A finalizing transaction that itself becomes `dropped` (`finality::track_finality`'s terminal state)
/// must read `data posted (dropped)`, not linger at `data posted (confirmed)` forever
/// or silently read as fully `data posted`.
#[tokio::test]
async fn a_dropped_finalizing_tx_shows_data_posted_dropped() {
    let pg = TestPg::start().await;
    let program_id = Pubkey::new_unique();
    let payer = payer();
    let mut source = ScriptedSource::new(program_id);

    let open_ix = open_batch_ix(&program_id, &payer, 200_101, 1, 1, &SETTLEMENT_PROGRAM);
    source.push_newest(
        "1open00000000000000000000000000000000000000000000000000000",
        100,
        raw_message_from_ixs(&payer, &[open_ix]),
        false,
    );
    let finalize_ix = finalize_batch_ix(&program_id, &payer, &SETTLEMENT_PROGRAM, 200_101, 1, 0);
    source.push_newest(
        "2finalize0000000000000000000000000000000000000000000000000",
        101,
        raw_message_from_ixs(&payer, &[finalize_ix]),
        false,
    );

    run_inbox_to_completion_scripted(&pg.pool, &mut source, &program_id).await;
    derive_to_completion(&pg.pool).await;

    sqlx::query(
        "UPDATE settlement_tx SET status = 'dropped' WHERE sig = '2finalize0000000000000000000000000000000000000000000000000'",
    )
    .execute(&pg.pool)
    .await
    .unwrap();

    assert_eq!(
        block_status(&pg.pool, 200_101, 1).await.unwrap(),
        Some("data posted (dropped)".to_string())
    );
}

/// RED before the fix: re-deriving the same
/// partial `FinalizeBatch { step }` after a `derive_cursor` reset (the repair path, a real operational tool)
/// must not double-count it -- idempotency is keyed to
/// `(batch_pda, settlement_tx_id)` via `batch_finalize_step`, not to `derive_cursor` never moving
/// backward.
#[tokio::test]
async fn re_deriving_after_a_cursor_reset_does_not_double_count_a_finalize_step() {
    let pg = TestPg::start().await;
    let program_id = Pubkey::new_unique();
    let payer = payer();
    let mut source = ScriptedSource::new(program_id);

    let open_ix = open_batch_ix(&program_id, &payer, 200_101, 1, 10, &SETTLEMENT_PROGRAM);
    source.push_newest(
        "1open00000000000000000000000000000000000000000000000000000",
        100,
        raw_message_from_ixs(&payer, &[open_ix]),
        false,
    );
    let step_ix = finalize_batch_ix(&program_id, &payer, &SETTLEMENT_PROGRAM, 200_101, 1, 1);
    source.push_newest(
        "2step100000000000000000000000000000000000000000000000000",
        101,
        raw_message_from_ixs(&payer, &[step_ix]),
        false,
    );

    run_inbox_to_completion_scripted(&pg.pool, &mut source, &program_id).await;
    derive_to_completion(&pg.pool).await;

    let (cursor_before,): (i64,) = sqlx::query_as(
        "SELECT finalize_cursor FROM batch WHERE chain_id = 200101 AND batch_id = 1",
    )
    .fetch_one(&pg.pool)
    .await
    .unwrap();
    assert_eq!(cursor_before, 1);

    sqlx::query("DELETE FROM derive_cursor WHERE kind = 'inbox'")
        .execute(&pg.pool)
        .await
        .unwrap();
    derive_to_completion(&pg.pool).await;

    let (cursor_after,): (i64,) = sqlx::query_as(
        "SELECT finalize_cursor FROM batch WHERE chain_id = 200101 AND batch_id = 1",
    )
    .fetch_one(&pg.pool)
    .await
    .unwrap();
    assert_eq!(
        cursor_after, 1,
        "re-deriving the same FinalizeBatch step after a cursor reset must not double-count it"
    );
}

// ---------------------------------------------------------------------------------------------
// Regression tests for later fixes.
// ---------------------------------------------------------------------------------------------

/// RED before the fix: a `settlement_tx_program` row
/// whose `events` JSON cannot decode as `DerivedEvents` (an unknown/renamed variant -- exactly the
/// repair path's own failure mode, re-deriving the OLDEST rows with the NEWEST decoder) must
/// not panic the whole derive pass. It logs a named error and continues: a later, genuinely decodable row
/// still derives.
#[tokio::test]
async fn derive_skips_an_undecodable_events_row_without_panicking() {
    let pg = TestPg::start().await;
    let program_id = Pubkey::new_unique();
    let payer = payer();
    let mut source = ScriptedSource::new(program_id);

    let open1 = open_chunk_ix(&program_id, &payer, &SETTLEMENT_PROGRAM, 200_101, 1, 1, 100);
    source.push_newest(
        "1garbage000000000000000000000000000000000000000000000000000",
        100,
        raw_message_from_ixs(&payer, &[open1]),
        false,
    );
    let open2 = open_chunk_ix(&program_id, &payer, &SETTLEMENT_PROGRAM, 200_101, 1, 2, 100);
    source.push_newest(
        "2good00000000000000000000000000000000000000000000000000000",
        101,
        raw_message_from_ixs(&payer, &[open2]),
        false,
    );

    run_inbox_to_completion_scripted(&pg.pool, &mut source, &program_id).await;

    // Corrupt the first row's already-ingested `events` to a shape this binary's decoder cannot parse --
    // simulates a future/renamed `ChunkEventKind` variant a decoder change would leave behind on old rows.
    sqlx::query(
        "UPDATE settlement_tx_program SET events = \
         '{\"chunk_events\":[{\"chunk_pda\":\"x\",\"kind\":{\"Renamed\":{}}}],\"batch_events\":[]}'::jsonb \
         WHERE settlement_tx_id = (SELECT id FROM settlement_tx WHERE sig = \
         '1garbage000000000000000000000000000000000000000000000000000')",
    )
    .execute(&pg.pool)
    .await
    .unwrap();

    // Must not panic -- and row 2's Open must still derive despite row 1 being undecodable.
    derive_to_completion(&pg.pool).await;

    let (count,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM inbox_chunk WHERE chain_id = 200101 AND batch_id = 1")
            .fetch_one(&pg.pool)
            .await
            .unwrap();
    assert_eq!(
        count, 1,
        "row 2's Open must still be derived despite row 1's events being undecodable"
    );
}

/// RED before the fix: `block_status`'s provisional
/// `(confirmed)`/`(dropped)` suffix must apply to the `abandoned` arm the same way it
/// already applies to the finalized/data-posted arm -- the unfixed view read a `dropped` (or still only
/// `confirmed`) `AbandonBatch` identically to a `finalized` one.
#[tokio::test]
async fn block_status_abandoned_suffix_reflects_the_abandoning_transactions_own_status() {
    let pg = TestPg::start().await;
    let program_id = Pubkey::new_unique();
    let payer = payer();
    let mut source = ScriptedSource::new(program_id);

    let open_ix = open_batch_ix(&program_id, &payer, 200_101, 1, 10, &SETTLEMENT_PROGRAM);
    source.push_newest(
        "1open00000000000000000000000000000000000000000000000000000",
        100,
        raw_message_from_ixs(&payer, &[open_ix]),
        false,
    );
    let abandon_ix = abandon_batch_ix(&program_id, &payer, &SETTLEMENT_PROGRAM, 200_101, 1);
    source.push_newest(
        "2abandon00000000000000000000000000000000000000000000000000",
        101,
        raw_message_from_ixs(&payer, &[abandon_ix]),
        false,
    );

    run_inbox_to_completion_scripted(&pg.pool, &mut source, &program_id).await;
    derive_to_completion(&pg.pool).await;

    assert_eq!(
        block_status(&pg.pool, 200_101, 1).await.unwrap(),
        Some("abandoned (confirmed)".to_string()),
        "the abandoning tx is itself only 'confirmed' so far -- the view must say so, mirroring the \
         finalized arm"
    );

    sqlx::query(
        "UPDATE settlement_tx SET status = 'dropped' \
         WHERE sig = '2abandon00000000000000000000000000000000000000000000000000'",
    )
    .execute(&pg.pool)
    .await
    .unwrap();
    assert_eq!(
        block_status(&pg.pool, 200_101, 1).await.unwrap(),
        Some("abandoned (dropped)".to_string()),
        "an abandoning tx that itself becomes dropped must not read identically to a finalized abandon"
    );

    sqlx::query(
        "UPDATE settlement_tx SET status = 'finalized' \
         WHERE sig = '2abandon00000000000000000000000000000000000000000000000000'",
    )
    .execute(&pg.pool)
    .await
    .unwrap();
    assert_eq!(
        block_status(&pg.pool, 200_101, 1).await.unwrap(),
        Some("abandoned".to_string()),
        "once the abandoning tx itself finalizes, the suffix must drop"
    );
}

/// The earlier `source_max_slot_is_populated_from_the_first_page_and_never_regresses` test only re-runs
/// an already-completed walk (a no-op second call): `advance_backfill_progress` -- the only writer of
/// `source_max_slot` -- is never invoked a second time there, so the GREATEST/COALESCE guard at
/// `cursor.rs` is never actually exercised. This test
/// forces a genuine RESUME: three signatures at descending slots (300, 200, 100), a one-signature-per-page
/// config so each is its own internal page fetch, and an injected failure right after the walk's first
/// page (slot 300) has committed. The interrupted call's own `source_max_slot` is 300 (from that first
/// page); the RESUMED call is a fresh `run_once` invocation whose own first internal page fetch is the
/// SECOND signature (slot 200, since `backfill_before` now resumes past the first) -- without the
/// GREATEST guard, that resumed call's smaller value would silently overwrite the walk's true head.
#[tokio::test]
async fn source_max_slot_never_regresses_across_a_resumed_walk() {
    let pg = TestPg::start().await;
    let program_id = Pubkey::new_unique();
    let payer = payer();

    fn build_source(program_id: Pubkey, payer: Pubkey) -> ScriptedSource {
        let mut source = ScriptedSource::new(program_id);
        for (sig, slot, batch) in [
            (
                "3oldest00000000000000000000000000000000000000000000000000",
                100u64,
                100u64,
            ),
            (
                "2middle00000000000000000000000000000000000000000000000000",
                200,
                200,
            ),
            (
                "1newest00000000000000000000000000000000000000000000000000",
                300,
                300,
            ),
        ] {
            let ix = open_batch_ix(
                &program_id,
                &payer,
                200_101,
                batch,
                1,
                &Pubkey::new_unique(),
            );
            source.push_newest(sig, slot, raw_message_from_ixs(&payer, &[ix]), false);
        }
        source
    }

    let cfg = WatcherConfig {
        rpc_page_size: 1,
        commit_batch_size: 1,
    };

    // The walk's own first (newest) page commits (slot 300), then the very next get_transaction call
    // (the second signature, slot 200) is killed -- `backfill_before` has moved past the first signature,
    // but the walk is far from finished.
    let mut failing = FailAfter::new(build_source(program_id, payer), 2);
    let _ = run_once(
        &pg.pool,
        &mut failing,
        &program_id,
        &SETTLEMENT_PROGRAM,
        ProgramKind::Inbox,
        cfg,
    )
    .await;

    // Resume with a fresh, working source until the walk finishes.
    let mut resumed = build_source(program_id, payer);
    loop {
        match run_once(
            &pg.pool,
            &mut resumed,
            &program_id,
            &SETTLEMENT_PROGRAM,
            ProgramKind::Inbox,
            cfg,
        )
        .await
        .expect("resumed run_once")
        {
            PageOutcome::NoNewSignatures => break,
            PageOutcome::Processed { .. } => continue,
        }
    }

    let (source_max_slot,): (Option<i64>,) =
        sqlx::query_as("SELECT source_max_slot FROM settlement_cursor WHERE kind = 'inbox'")
            .fetch_one(&pg.pool)
            .await
            .unwrap();
    assert_eq!(
        source_max_slot,
        Some(300),
        "source_max_slot must stay at the walk's true head slot (300) even though the resumed call's \
         own first fetched page has a lower slot (200)"
    );
}
