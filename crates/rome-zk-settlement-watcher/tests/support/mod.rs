//! Shared test support: a throwaway `postgres:16` Docker container per test (in the test harness, never a
//! managed database), and [`FixtureSource`],
//! a [`Source`] that replays the recorded Tiber devnet fixture
//! (`fixtures/settlement-watcher/tiber-devnet-batches-4003-4005.json`) instead of a live RPC endpoint --
//! same `before`/`until`/`limit` paging contract a real node has, so `ingest::run_once` cannot tell the
//! difference.
//!
//! `mod support;` is included by several separate integration-test binaries (`replay.rs`, `finality.rs`),
//! each of which uses only part of this file -- cargo compiles each test binary on its own, so the parts
//! a given binary does not call are, correctly, dead code from that binary's point of view.
#![allow(dead_code)]

use rome_zk_settlement_watcher::{RawTx, SignatureInfo, Source, SourceError};
use serde::Deserialize;
use solana_program::pubkey::Pubkey;
use solana_transaction_status_client_types::{TransactionConfirmationStatus, UiRawMessage};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::process::Command;
use std::time::Duration;

/// Spins up `postgres:16` in Docker on a random host port, waits for it to accept connections, runs
/// this crate's migrations, and force-removes the container on drop -- a lighter
/// alternative to a `testcontainers` dependency, without the extra compile-time
/// cost of that crate for a workspace this large.
pub struct TestPg {
    container: String,
    pub pool: PgPool,
}

impl TestPg {
    pub async fn start() -> Self {
        // Deliberately no `--name`: cargo's test harness runs every `#[tokio::test]` in this binary as
        // a thread within one process, so a name derived from `std::process::id()` collides across
        // concurrently-running tests (found running this suite un-pinned to one thread: "Conflict...
        // container name... already in use"). Docker assigns each `run` its own unique container ID
        // regardless of concurrency; capturing that ID from stdout is what makes concurrent test
        // threads (the cargo test harness's default) safe without serializing them.
        //
        // `--label rome-zk-test=1` + an entrypoint override that `timeout`s the real postgres process:
        // `--rm` only reaps a container once its process
        // *exits* -- a `SIGKILL` of this test binary itself (a superseded CI push, `cancel-in-progress:
        // true`) leaves the container running with nothing left to stop it. The label lets CI's own
        // cleanup step find and remove every one of this crate's leftover containers regardless of why
        // the test process died; the `timeout 600` (10 min) `exec` is the container's own last-resort
        // self-destruct if even that cleanup step never runs -- measured in CI: the whole suite runs
        // in well under 10 s, so 600 s is ample headroom while still much shorter than the earlier 1200 s.
        // `exec` (rather than a plain `sh -c` subshell) makes
        // `timeout` itself PID 1, so a SIGTERM from `docker stop` reaches it directly instead of waiting
        // out the subshell's own 10 s grace period first.
        let out = Command::new("docker")
            .args([
                "run",
                "-d",
                "--rm",
                "--label",
                "rome-zk-test=1",
                "-e",
                "POSTGRES_PASSWORD=postgres",
                "-e",
                "POSTGRES_DB=rome_zk_explorer_test",
                "-P", // publish every EXPOSE'd port to a random host port
                "--entrypoint",
                "sh",
                "postgres:16",
                "-c",
                "exec timeout 600 docker-entrypoint.sh postgres",
            ])
            .output()
            .expect("docker run -- is Docker installed and running?");
        assert!(
            out.status.success(),
            "docker run postgres:16 failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let container = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert!(
            !container.is_empty(),
            "docker run printed no container ID on stdout"
        );

        let port = Self::discover_port(&container);
        let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/rome_zk_explorer_test");

        let pool = Self::wait_for_ready(&url).await;
        sqlx::migrate!("../../migrations/explorer")
            .run(&pool)
            .await
            .expect("apply migrations/explorer/*.sql");

        Self { container, pool }
    }

    fn discover_port(container: &str) -> u16 {
        for _ in 0..20 {
            if let Ok(out) = Command::new("docker")
                .args(["port", container, "5432/tcp"])
                .output()
            {
                let text = String::from_utf8_lossy(&out.stdout);
                if let Some(port_str) = text.trim().rsplit(':').next() {
                    if let Ok(port) = port_str.trim().parse::<u16>() {
                        return port;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!("could not discover the published port for container {container}");
    }

    async fn wait_for_ready(url: &str) -> PgPool {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            match PgPoolOptions::new()
                .max_connections(5)
                .acquire_timeout(Duration::from_secs(2))
                .connect(url)
                .await
            {
                Ok(pool) => return pool,
                Err(e) if std::time::Instant::now() < deadline => {
                    tracing::debug!("waiting for test postgres: {e}");
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                Err(e) => panic!("postgres never became ready: {e}"),
            }
        }
    }
}

impl Drop for TestPg {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", "-v", &self.container]) // -v: postgres:16 declares a data volume; without it every test leaks one
            .output();
    }
}

#[derive(Debug, Clone, Deserialize)]
struct FixtureRecord {
    signature: String,
    slot: u64,
    #[serde(rename = "blockTime")]
    block_time: Option<i64>,
    err: Option<serde_json::Value>,
    message: Option<UiRawMessage>,
}

#[derive(Debug, Deserialize)]
struct FixtureFile {
    #[allow(dead_code)]
    chain_id: u64,
    inbox_program: String,
    settlement_program: String,
    inbox_records: Vec<FixtureRecord>,
    settlement_records: Vec<FixtureRecord>,
}

impl FixtureFile {
    /// The fixture is a real capture from before inbox accounts were keyed by the settlement program, so its chunk
    /// accounts sit at the old chain-id-only addresses. The watcher accepts a chunk `Open` only at the
    /// settlement-keyed address, so every old chunk address that an `Open` created is rewritten, in every
    /// transaction that mentions it, to the address the same `(chain, batch, idx)` has under the fixture's
    /// settlement program. Nothing else about the capture changes.
    fn rekey_chunk_accounts(&mut self) {
        use std::collections::HashMap;
        let inbox: Pubkey = self.inbox_program.parse().expect("valid pubkey");
        let settlement: Pubkey = self.settlement_program.parse().expect("valid pubkey");
        let mut rekey: HashMap<String, String> = HashMap::new();
        for record in &self.inbox_records {
            let Some(message) = &record.message else {
                continue;
            };
            for ins in &message.instructions {
                if message.account_keys.get(ins.program_id_index as usize)
                    != Some(&inbox.to_string())
                {
                    continue;
                }
                let Ok(raw) = bs58::decode(&ins.data).into_vec() else {
                    continue;
                };
                if let Ok(zk_inbox_client::InboxIx::Open {
                    chain_id,
                    batch,
                    idx,
                    ..
                }) = zk_inbox_client::decode_instruction(&raw)
                {
                    let old = ins
                        .accounts
                        .get(1)
                        .and_then(|&i| message.account_keys.get(i as usize));
                    if let Some(old) = old {
                        let new =
                            zk_inbox_client::chunk_pda(&inbox, &settlement, chain_id, batch, idx).0;
                        rekey.insert(old.clone(), new.to_string());
                    }
                }
            }
        }
        for record in &mut self.inbox_records {
            if let Some(message) = &mut record.message {
                for key in &mut message.account_keys {
                    if let Some(new) = rekey.get(key) {
                        *key = new.clone();
                    }
                }
            }
        }
    }
}

/// Replays `fixtures/settlement-watcher/tiber-devnet-batches-4003-4005.json` behind the exact same
/// [`Source`] contract `RpcSource` implements. Records are stored oldest-first (ascending slot, matching
/// how they were captured); `get_signatures_for_address` reverses that to the real RPC's newest-first
/// `before`/`until` semantics.
pub struct FixtureSource {
    inbox_program: String,
    settlement_program: String,
    inbox_records: Vec<FixtureRecord>,
    settlement_records: Vec<FixtureRecord>,
}

pub const FIXTURE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/settlement-watcher/tiber-devnet-batches-4003-4005.json"
);

impl FixtureSource {
    pub fn load() -> Self {
        let raw = std::fs::read_to_string(FIXTURE_PATH)
            .unwrap_or_else(|e| panic!("read {FIXTURE_PATH}: {e}"));
        let mut file: FixtureFile =
            serde_json::from_str(&raw).expect("fixture JSON must parse into FixtureFile");
        file.rekey_chunk_accounts();
        Self {
            inbox_program: file.inbox_program,
            settlement_program: file.settlement_program,
            inbox_records: file.inbox_records,
            settlement_records: file.settlement_records,
        }
    }

    pub fn inbox_program_id(&self) -> Pubkey {
        self.inbox_program.parse().expect("valid pubkey")
    }

    pub fn settlement_program_id(&self) -> Pubkey {
        self.settlement_program.parse().expect("valid pubkey")
    }

    fn records_for(&self, program_id: &Pubkey) -> &[FixtureRecord] {
        let s = program_id.to_string();
        if s == self.inbox_program {
            &self.inbox_records
        } else if s == self.settlement_program {
            &self.settlement_records
        } else {
            &[]
        }
    }

    fn find(&self, signature: &str) -> Option<&FixtureRecord> {
        self.inbox_records
            .iter()
            .chain(self.settlement_records.iter())
            .find(|r| r.signature == signature)
    }
}

impl Source for FixtureSource {
    async fn get_signatures_for_address(
        &mut self,
        program_id: &Pubkey,
        before: Option<String>,
        until: Option<String>,
        limit: usize,
    ) -> Result<Vec<SignatureInfo>, SourceError> {
        let records = self.records_for(program_id);
        // Stored oldest-first; a real node's getSignaturesForAddress is newest-first.
        let newest_first: Vec<&FixtureRecord> = records.iter().rev().collect();
        let start = match &before {
            Some(b) => newest_first
                .iter()
                .position(|r| &r.signature == b)
                .map(|i| i + 1)
                .unwrap_or(newest_first.len()),
            None => 0,
        };
        let mut out = Vec::new();
        for r in newest_first.iter().skip(start) {
            if let Some(u) = &until {
                if &r.signature == u {
                    break;
                }
            }
            out.push(SignatureInfo {
                signature: r.signature.clone(),
                slot: r.slot,
                block_time: r.block_time,
                err: r.err.is_some(),
                // These signatures are months-old real Tiber devnet history by the time this fixture is
                // replayed -- every one of them is finalized on the real chain; the fixture does not
                // pretend otherwise.
                confirmation_status: Some(TransactionConfirmationStatus::Finalized),
            });
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    async fn get_transaction(&mut self, signature: &str) -> Result<Option<RawTx>, SourceError> {
        Ok(self.find(signature).and_then(|r| {
            r.message.clone().map(|message| RawTx {
                message,
                err: r.err.is_some(),
            })
        }))
    }

    async fn get_signature_statuses(
        &mut self,
        signatures: &[String],
    ) -> Result<Vec<Option<TransactionConfirmationStatus>>, SourceError> {
        Ok(signatures
            .iter()
            .map(|_| Some(TransactionConfirmationStatus::Finalized))
            .collect())
    }

    async fn current_slot(&mut self) -> Result<u64, SourceError> {
        // Every record in this fixture is already real, months-old finalized history -- an arbitrarily
        // high "now" is as truthful as this replay-only source can be.
        Ok(u64::MAX)
    }
}

/// Wraps any [`Source`] and fails `get_transaction` from the `fail_at`-th call onward (1-indexed) --
/// simulates a crash partway through fetching a page's transactions, so the test can assert the DB
/// state after a "kill" is exactly what committed chunks left behind, then resume with a working source.
pub struct FailAfter<S> {
    inner: S,
    calls: usize,
    fail_at: usize,
}

impl<S> FailAfter<S> {
    pub fn new(inner: S, fail_at: usize) -> Self {
        Self {
            inner,
            calls: 0,
            fail_at,
        }
    }
}

impl<S: Source> Source for FailAfter<S> {
    async fn get_signatures_for_address(
        &mut self,
        program_id: &Pubkey,
        before: Option<String>,
        until: Option<String>,
        limit: usize,
    ) -> Result<Vec<SignatureInfo>, SourceError> {
        self.inner
            .get_signatures_for_address(program_id, before, until, limit)
            .await
    }

    async fn get_transaction(&mut self, signature: &str) -> Result<Option<RawTx>, SourceError> {
        self.calls += 1;
        if self.calls >= self.fail_at {
            return Err(SourceError::Rpc("injected failure (FailAfter)".to_string()));
        }
        self.inner.get_transaction(signature).await
    }

    async fn get_signature_statuses(
        &mut self,
        signatures: &[String],
    ) -> Result<Vec<Option<TransactionConfirmationStatus>>, SourceError> {
        self.inner.get_signature_statuses(signatures).await
    }

    async fn current_slot(&mut self) -> Result<u64, SourceError> {
        self.inner.current_slot().await
    }
}

/// Wraps a [`Source`] and returns `Ok(None)` from `get_transaction` for one specific signature the first
/// `remaining` times it is asked, then delegates normally -- simulates Agave mapping a transient Bigtable
/// read error to a bare `null` for a signature `getSignaturesForAddress` already listed,
/// so a test can prove the caller retries rather than silently skipping the row.
pub struct NullBodyOnce<S> {
    inner: S,
    target: String,
    remaining: usize,
}

impl<S> NullBodyOnce<S> {
    pub fn new(inner: S, target: impl Into<String>, remaining: usize) -> Self {
        Self {
            inner,
            target: target.into(),
            remaining,
        }
    }
}

impl<S: Source> Source for NullBodyOnce<S> {
    async fn get_signatures_for_address(
        &mut self,
        program_id: &Pubkey,
        before: Option<String>,
        until: Option<String>,
        limit: usize,
    ) -> Result<Vec<SignatureInfo>, SourceError> {
        self.inner
            .get_signatures_for_address(program_id, before, until, limit)
            .await
    }

    async fn get_transaction(&mut self, signature: &str) -> Result<Option<RawTx>, SourceError> {
        if signature == self.target && self.remaining > 0 {
            self.remaining -= 1;
            return Ok(None);
        }
        self.inner.get_transaction(signature).await
    }

    async fn get_signature_statuses(
        &mut self,
        signatures: &[String],
    ) -> Result<Vec<Option<TransactionConfirmationStatus>>, SourceError> {
        self.inner.get_signature_statuses(signatures).await
    }

    async fn current_slot(&mut self) -> Result<u64, SourceError> {
        self.inner.current_slot().await
    }
}

/// A small, fully hand-built [`Source`] for tests that need exact control over signature order and
/// transaction bodies (the fixture is real history and cannot be made to contain, say, a partial
/// `FinalizeBatch` or a poison signature at a chosen position) -- one program only, records given
/// **newest-first** (a real node's own order), the same `before`/`until`/`limit` contract `RpcSource` and
/// `FixtureSource` both implement.
pub struct ScriptedSource {
    pub program_id: Pubkey,
    /// Newest-first, mirroring a real node's `getSignaturesForAddress`.
    pub records: Vec<(SignatureInfo, RawTx)>,
    pub current_slot: u64,
}

impl ScriptedSource {
    pub fn new(program_id: Pubkey) -> Self {
        Self {
            program_id,
            records: Vec::new(),
            current_slot: u64::MAX,
        }
    }

    /// Appends one record at the **newest** position (index 0) -- callers build a script oldest-called-
    /// first, matching the order transactions would actually have landed on-chain.
    pub fn push_newest(
        &mut self,
        sig: impl Into<String>,
        slot: u64,
        message: UiRawMessage,
        err: bool,
    ) {
        self.records.insert(
            0,
            (
                SignatureInfo {
                    signature: sig.into(),
                    slot,
                    block_time: Some(1_700_000_000 + slot as i64),
                    err,
                    confirmation_status: Some(TransactionConfirmationStatus::Confirmed),
                },
                RawTx { message, err },
            ),
        );
    }
}

impl Source for ScriptedSource {
    async fn get_signatures_for_address(
        &mut self,
        program_id: &Pubkey,
        before: Option<String>,
        until: Option<String>,
        limit: usize,
    ) -> Result<Vec<SignatureInfo>, SourceError> {
        if *program_id != self.program_id {
            return Ok(vec![]);
        }
        let start = match &before {
            Some(b) => self
                .records
                .iter()
                .position(|(s, _)| &s.signature == b)
                .map(|i| i + 1)
                .unwrap_or(self.records.len()),
            None => 0,
        };
        let mut out = Vec::new();
        for (s, _) in self.records.iter().skip(start) {
            if let Some(u) = &until {
                if &s.signature == u {
                    break;
                }
            }
            out.push(s.clone());
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    async fn get_transaction(&mut self, signature: &str) -> Result<Option<RawTx>, SourceError> {
        Ok(self
            .records
            .iter()
            .find(|(s, _)| s.signature == signature)
            .map(|(_, tx)| tx.clone()))
    }

    async fn get_signature_statuses(
        &mut self,
        signatures: &[String],
    ) -> Result<Vec<Option<TransactionConfirmationStatus>>, SourceError> {
        Ok(signatures
            .iter()
            .map(|sig| {
                self.records
                    .iter()
                    .find(|(s, _)| &s.signature == sig)
                    .map(|_| TransactionConfirmationStatus::Confirmed)
            })
            .collect())
    }

    async fn current_slot(&mut self) -> Result<u64, SourceError> {
        Ok(self.current_slot)
    }
}

/// Compiles real `zk_inbox_client`/`zk_settlement_client` builder `Instruction`s into a `UiRawMessage` --
/// the exact shape a real `getTransaction` call returns -- for a [`ScriptedSource`] record. Shared by
/// every test file that needs to script a transaction against real builders rather than hand-rolling
/// instruction bytes.
pub fn raw_message_from_ixs(
    payer: &Pubkey,
    ixs: &[solana_program::instruction::Instruction],
) -> UiRawMessage {
    let message = solana_sdk::message::Message::new(ixs, Some(payer));
    UiRawMessage {
        header: solana_sdk::message::MessageHeader {
            num_required_signatures: message.header.num_required_signatures,
            num_readonly_signed_accounts: message.header.num_readonly_signed_accounts,
            num_readonly_unsigned_accounts: message.header.num_readonly_unsigned_accounts,
        },
        account_keys: message.account_keys.iter().map(|k| k.to_string()).collect(),
        recent_blockhash: message.recent_blockhash.to_string(),
        instructions: message
            .instructions
            .iter()
            .map(
                |ci| solana_transaction_status_client_types::UiCompiledInstruction {
                    program_id_index: ci.program_id_index,
                    accounts: ci.accounts.clone(),
                    data: bs58::encode(&ci.data).into_string(),
                    stack_height: None,
                },
            )
            .collect(),
        address_table_lookups: None,
        transaction_config: None,
    }
}
