//! The always-on follower loop: reuses `anchor`/`poster`/
//! `finality`'s own steps (never copies them) to prove every finalized inbox batch, in order, with zero
//! `Prover::prove` calls while idle, a resume rule that re-proves rather than halts, an
//! inbox-binding check on every send, and Prometheus metrics. `--once` (the CLI) is the degenerate
//! single-iteration case of this same state machine — both entry points call [`run_one`].
//!
//! ## State machine
//! `Queued → InputBuilt → Proving → Decoded → LocallyVerified → Posting → Relayed → Finalized`;
//! terminal `Idle` (nothing to prove) and `Superseded` (the chain moved on while we proved); halting
//! `AbandonedInboxBatch` (no skip instruction exists, so the follower stops and alarms),
//! `VerifierBehindAlarm` (the reth-verifier never caught up after `verifier_behind_alarm_polls`
//! consecutive retries), `StaleAnchorAlarm` (a `StaleAnchor` retry streak that never clears after
//! `stale_anchor_alarm_polls` consecutive retries), and `ProveAttemptsExhausted` (every
//! prove attempt for a batch failed `max_prove_attempts` times in a row).
//!
//! ## Artefact binding + resume that re-proves, never halts
//! Every job's artefacts live under `work_dir/<batch>/{input.bin, sidecar.json, proof}`. Entering a job,
//! [`try_resume`] resumes at `Decoded` (skipping `fetch_and_verify_batch`/`build_batch_input`/`prove`
//! entirely) only when the on-disk `proof` is bound to BOTH the ELF of record AND the fresh anchor's own
//! inbox batch (`chain_id`, `inbox_commitment`, `open_unix_ts` — never the ELF sha alone, which a chain
//! reset or a hand `--once` against another chain would satisfy while still being a stale artefact) and
//! the resumed proof decodes, checks against the record, and reproduces its own sidecar's recorded
//! publics; ANY failure wipes the whole directory and proves again (bounded by `max_prove_attempts`),
//! never halts on a merely-corrupt cache — only a run that exhausts every attempt halts, by name. A
//! restart after `Relayed` needs no special case at all: the very next `anchor()` call for that same
//! batch id returns `HeadAhead` (the chain head already advanced), which [`prepare_checked_proof`] turns
//! into `Outcome::AlreadyPosted` before touching the work directory. [`poster::build_post_ix`] mirrors
//! this same binding one layer down, locally, before every send: a proof whose own packaged
//! `inbox_commitment`/`open_unix_ts` disagree with the candidate batch's real inbox account is refused
//! before any fee is spent, the same two facts `settle.rs` itself refuses on chain
//! (`AccMismatch`/`OpenTsMismatch`).
//!
//! ## History under a restart
//! A resumed job's every event (`Queued`/`Verified`/`Posted`/`Finalized`) carries the sidecar's own
//! recorded `provenance.attempt` — the real winning attempt of whichever run produced the artefact, never
//! a placeholder — so a restart's history UPSERTs onto the SAME `(chain_id, batch, attempt)` row the
//! original run already used, rather than stranding it under a different one. A freshly produced attempt
//! that fails (the subprocess itself, or the decode/record-check/pairing closure) records `Failed` at
//! that SAME attempt before retrying, so its own row shows it actually failed rather than freezing at
//! whichever transition last succeeded. A restart onto a candidate that is already posted AND already
//! final (`candidate_batch <= head_final_batch` on the very first, fresh anchor) records `Finalized` with
//! no signature — the chain says final, this run has no local record of which signature made it so.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use solana_program::pubkey::Pubkey;

use crate::anchor::{self, AnchorError, SnapshotFetch};
use crate::calldata::RecordChecked;
use crate::config::VkeyOfRecord;
use crate::metrics::{self, Metrics};
use crate::poster::{self, PostParams, PostRefusal, PreSendRefusal};
use crate::prover::{ProveError, Prover};
use crate::store::{JobEvent, JobEventKind, Store};
use rome_zk_prover_input::verifier::VerifierFetch;
use rome_zk_solana_sender::{SendTuning, Sender, SenderError};

/// Records one job transition and never lets a store failure interrupt the caller ("history never stops
/// proving"). Logged + counted on error; the loop's own outcome never depends on this call's own result.
async fn record(
    store: &impl Store,
    metrics: &Metrics,
    chain_id: u64,
    batch: u64,
    attempt: u32,
    kind: JobEventKind,
) {
    let event = JobEvent {
        chain_id,
        batch,
        attempt,
        kind,
    };
    if let Err(e) = store.record(&event).await {
        tracing::warn!(
            batch,
            attempt,
            kind = event.kind.status(),
            "prover history: record failed (proving continues): {e}"
        );
        metrics.store_errors_total.inc();
    }
}

/// Static identity for a follower run — everything that does not change batch to batch. Built from
/// [`crate::config::Config`] plus the resolved program ids/vkey/authority the CLI already loads.
pub struct RunConfig {
    pub settlement_program: Pubkey,
    pub inbox_program: Pubkey,
    pub chain_id: u64,
    pub vkey: VkeyOfRecord,
    pub elf_path: PathBuf,
    pub work_dir: PathBuf,
    /// How many of the MOST RECENT candidates' `work_dir/<batch>` directories to
    /// keep on disk once they reach a terminal outcome (`Posted`/`AlreadyPosted`) — default 0 (remove
    /// immediately; a follower proving one batch/minute otherwise accumulates ~120 MB/day of dead
    /// artefacts no resume ever reads again).
    pub keep_work_dirs: u32,
    pub authority: Pubkey,
    /// Every [`SendTuning`] field EXCEPT `loaded_accounts_data_size_limit`, which is re-derived from
    /// each fresh anchor's own live account lengths — never carried statically here.
    pub tuning: SendTuning,
    pub loaded_accounts_data_size_limit: Option<u32>,
    pub finalize_walk: u64,
    pub close_pending_after_batches: Option<u64>,
    pub poll_interval: Duration,
    pub verifier_behind_alarm_polls: u32,
    /// Consecutive `StaleAnchor` retries for the same posted candidate before halting
    /// (`FollowerError::StaleAnchorAlarm`).
    pub stale_anchor_alarm_polls: u32,
    /// Consecutive prove attempts for the same batch (each with a freshly wiped work
    /// directory) before halting (`FollowerError::ProveAttemptsExhausted`).
    pub max_prove_attempts: u32,
    /// Consecutive `TransientFetchError` retries before halting
    /// (`FollowerError::FetchAlarm`) — a streak reset by any other outcome, same shape as
    /// `verifier_behind_alarm_polls`/`stale_anchor_alarm_polls`.
    pub fetch_alarm_polls: u32,
    /// Provenance-only (the sidecar's own `Provenance` block) — never read back by the
    /// resume check itself, which compares only `elf_sha256`/`chain_id`/`inbox_commitment`/
    /// `open_unix_ts`/the proof's own publics.
    pub solana_rpc_url: String,
    /// Provenance-only, see `solana_rpc_url` above.
    pub verifier_rpc_url: String,
    /// Reporting knob only — the cost gauge, never the bill.
    pub gpu_hourly_usd: f64,
    /// The candidate batch id whose most recent send was classified `StaleAnchor`
    /// (landed for real, per a later FINALIZED read, but reported as a refusal by a lagging one) — set
    /// by [`PostFuture::run`], cleared the moment a fresh re-anchor resolves it either way (`HeadAhead`
    /// or a genuinely new send outcome). While set for the SAME candidate, `post_checked` refuses to
    /// resend and reports another `StaleAnchor` retry instead — a FINALIZED read can lag a send that
    /// already confirmed by several seconds, and blindly resending into that lag pays a
    /// real fee for a transaction that already landed.
    pub stale_send_pending: std::sync::Mutex<Option<u64>>,
}

/// This job's outcome, named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The proof landed under this run's own send.
    Posted { batch: u64, sig: String },
    /// A re-anchor (before or after proving) showed this exact batch already posted — success.
    AlreadyPosted { batch: u64 },
    /// A re-anchor showed the chain head has moved PAST our candidate — this job is stale; the caller
    /// re-derives the next candidate from `head_pending_batch + 1`.
    Superseded { batch: u64, head_pending_batch: u64 },
    /// `batches_behind == 0` (`cursor_next_batch - 1 - head_pending_batch`) — nothing to prove.
    /// `Prover::prove` is never called on this path.
    Idle { batches_behind: u64 },
    /// A retryable, non-halting condition — sleep and try the same candidate again.
    Retry {
        batches_behind: u64,
        reason: RetryReason,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryReason {
    /// The inbox batch account exists but is not yet finalized (a batch is open; `batches_behind > 0`
    /// but not idle).
    InboxNotFinalizedYet,
    /// The reth-verifier's own head has not yet reached this batch's last block.
    VerifierBehind { need: u64, have: u64 },
    /// A re-anchor at post time classified the failed send `StaleAnchor` — the
    /// classifier's own contract (`poster::PostRefusal::StaleAnchor` doc) is "retry the re-anchor",
    /// since a FINALIZED read can genuinely lag a send that already landed. The SAME
    /// candidate is retried; the next iteration's fresh anchor classifies `AlreadyPosted`/`Superseded`
    /// once the read catches up.
    StaleAnchor,
    /// A [`crate::anchor::AnchorError::Fetch`] — a transient snapshot
    /// read failure, never a decoded on-chain fact. The same candidate is retried.
    TransientFetchError(String),
}

/// Halting errors — the follower loop stops.
#[derive(Debug, thiserror::Error)]
pub enum FollowerError {
    /// No skip instruction exists. Alarm and stop; the operator's own repair resumes it.
    #[error("abandoned inbox batch {batch}: halting (no skip instruction)")]
    AbandonedInboxBatch { batch: u64 },
    /// The reth-verifier never caught up after `polls` consecutive retries.
    #[error("verifier behind alarm: need block {need}, have {have}, after {polls} polls")]
    VerifierBehindAlarm { need: u64, have: u64, polls: u32 },
    /// A `StaleAnchor` streak for the same posted candidate never cleared after
    /// `polls` consecutive retries — a FINALIZED read that stays behind a send this long is no longer
    /// "about to catch up", it is stuck.
    #[error("stale anchor alarm: {polls} consecutive retries never cleared")]
    StaleAnchorAlarm { polls: u32 },
    /// Every prove attempt for this batch — each with a freshly wiped work directory —
    /// failed (either the subprocess itself, or the freshly-produced proof's own decode/record/publics
    /// check) `attempts` times in a row, `Config::max_prove_attempts`'s own bound.
    #[error("prove attempts exhausted: {attempts} consecutive attempts all failed")]
    ProveAttemptsExhausted { attempts: u32 },
    /// A `TransientFetchError` streak for the same job never cleared after `polls`
    /// consecutive retries — a read failure this persistent is no longer "about to recover," same shape
    /// as `VerifierBehindAlarm`/`StaleAnchorAlarm`.
    #[error("fetch alarm: {polls} consecutive transient fetch errors never cleared")]
    FetchAlarm { polls: u32 },
    #[error("anchor: {0}")]
    Anchor(#[from] AnchorError),
    #[error("input: {0}")]
    Input(String),
    #[error("io: {0}")]
    Io(String),
    /// A [`crate::anchor::FetchError`] surfaced where the loop cannot retry in place. Every read that
    /// CAN retry now does: the top-of-job anchor and the post-time re-anchor become
    /// `Retry { TransientFetchError }` (bounded by `fetch_alarm_polls`), the finalize sweep and the
    /// retention-gated close defer to the next iteration with a warning. This variant is what remains
    /// for a read failure with no such path.
    #[error("fetch: {0}")]
    Fetch(String),
    #[error(transparent)]
    Prove(#[from] ProveError),
    #[error("calldata: {0}")]
    Calldata(String),
    #[error(transparent)]
    PreSend(#[from] PreSendRefusal),
    #[error("post refused: {0:?}")]
    PostRefused(PostRefusal),
    #[error("send: {0}")]
    Send(String),
}

/// Whichever refusal fires first when `candidate_batch != head_pending_batch + 1`
/// (`AnchorError::HeadAhead`): the exact same batch already posted (`AlreadyPosted`), or the chain has
/// moved strictly past it (`Superseded`) — used identically at [`prepare_checked_proof`]'s FIRST anchor
/// (a restart after `Relayed`) and [`post_checked`]'s SECOND, re-anchor (the chain moved on while we
/// proved).
/// `cursor_next_batch - 1 - head_pending_batch`: finalized batches not yet posted —
/// the ONE formula every site that reports/decides on this quantity shares (`prepare_checked_proof`'s two
/// call sites here, and the CLI's `--follow --dry-run` live gate) rather than each re-deriving it.
pub fn batches_behind(cursor_next_batch: u64, head_pending_batch: u64) -> u64 {
    cursor_next_batch
        .saturating_sub(1)
        .saturating_sub(head_pending_batch)
}

fn outcome_for_head_ahead(candidate_batch: u64, head_pending_batch: u64) -> Outcome {
    if head_pending_batch == candidate_batch {
        Outcome::AlreadyPosted {
            batch: candidate_batch,
        }
    } else {
        Outcome::Superseded {
            batch: candidate_batch,
            head_pending_batch,
        }
    }
}

fn batch_paths(work_dir: &Path, batch: u64) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let dir = work_dir.join(batch.to_string());
    (
        dir.clone(),
        dir.join("input.bin"),
        dir.join("sidecar.json"),
        dir.join("proof"),
    )
}

fn wipe_dir(dir: &Path) -> Result<(), FollowerError> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(FollowerError::Io(format!("wipe {}: {e}", dir.display()))),
    }
}

/// Attempts to resume at `Decoded` using the on-disk artefacts for this batch (this replaces the old
/// ELF-sha-only `ResumeSidecar`). Returns `Some(checked)` only when EVERY one of
/// these holds, in order:
/// - `input.bin` and `proof` both exist as real files;
/// - `sidecar.json` decodes as the full [`rome_zk_prover_input::build::Sidecar`] the input-build
///   step itself writes (expected publics + provenance) — never a crate-private, stripped-down shape;
/// - its own `provenance.elf_sha256` equals the vkey of record's (never the config's `elf_path`
///   re-hashed — the sidecar is the record of what ELF actually produced this artefact);
/// - its own `expected.chain_id`/`expected.inbox_commitment`/`expected.open_unix_ts` equal the FRESH
///   anchor's own inbox batch for this candidate — a cache keyed only on the ELF sha would happily
///   resume an artefact built for a different chain reset or a different chain entirely
///   (a cache is keyed by the identity of the TARGET, never only by the producer);
/// - the resumed `proof` file itself decodes, `check_against_record` accepts it, and its own packaged
///   public values reproduce the sidecar's own recorded expectation, field for field.
///
/// ANY failure returns `None` — a resume attempt that does not pan out is not itself a fault; the
/// caller wipes the directory and proves again. This function never returns an `Err` of its own.
///
/// Returns `(checked, attempt, verify_wall_ms)` on success: `attempt` is the
/// sidecar's OWN recorded `provenance.attempt` — the real winning attempt of whichever run produced this
/// artefact, never a literal `1` (a resumed job has no attempt counter of its own to invent, and
/// mislabeling it collides history under the wrong `(chain_id, batch, attempt)` row).
/// `verify_wall_ms` is the real wall time of the pairing check this function itself just ran — the
/// resumed job's own re-verify, never a fabricated `0.0` (a resumed job did not skip verifying, it
/// skipped only `fetch_and_verify_batch`/`build_batch_input`/`prove`).
fn try_resume(
    input_path: &Path,
    sidecar_path: &Path,
    proof_path: &Path,
    vkey: &VkeyOfRecord,
    chain_id: u64,
    inbox_batch: &zk_inbox_client::BatchAccount,
) -> Option<(RecordChecked, u32, f64)> {
    if !input_path.is_file() || !proof_path.is_file() {
        return None;
    }
    let s = std::fs::read_to_string(sidecar_path).ok()?;
    let sidecar: rome_zk_prover_input::build::Sidecar = serde_json::from_str(&s).ok()?;

    let elf_sha_hex = sidecar.provenance.elf_sha256.as_deref()?;
    let elf_sha_bytes = hex::decode(elf_sha_hex.trim_start_matches("0x")).ok()?;
    if elf_sha_bytes != vkey.elf_sha256.to_vec() {
        return None;
    }
    if sidecar.expected.chain_id != chain_id {
        return None;
    }
    if sidecar.expected.inbox_commitment != hex::encode(inbox_batch.acc) {
        return None;
    }
    if sidecar.expected.open_unix_ts != inbox_batch.open_unix_ts as u64 {
        return None;
    }

    let proof_bytes = std::fs::read(proof_path).ok()?;
    let cd = crate::calldata::from_zisk_proof_file(&proof_bytes).ok()?;
    let checked = crate::calldata::check_against_record(&cd, vkey).ok()?;
    let pv = crate::poster::derive_public_values(&checked).ok()?;
    if hex::encode(pv.inbox_commitment) != sidecar.expected.inbox_commitment
        || pv.chain_id != sidecar.expected.chain_id
        || pv.first_number != sidecar.expected.first_number
        || pv.last_number != sidecar.expected.last_number
        || pv.open_unix_ts != sidecar.expected.open_unix_ts
        || pv.gas_used != sidecar.expected.gas_used
    {
        return None;
    }
    // A decode + record-check + publics match is not enough — a proof whose
    // `proof_bytes` were corrupted after the fact (a torn write, a bit flip) still decodes clean and
    // still reports the same packaged publics, but no longer satisfies the real BN254 pairing. The
    // SAME check `build_post_ix` runs before every send, run here too before trusting a cached
    // artefact — any failure falls through to `None` (wipe + rebuild), never a halt.
    let verify_started = std::time::Instant::now();
    poster::verify_checked(&checked, vkey).ok()?;
    let verify_wall_ms = verify_started.elapsed().as_secs_f64() * 1000.0;
    Some((checked, sidecar.provenance.attempt, verify_wall_ms))
}

/// Writes the full [`rome_zk_prover_input::build::Sidecar`] a fresh build produces — the shape
/// [`try_resume`] reads back.
fn write_full_sidecar(
    path: &Path,
    expected: rome_zk_prover_input::build::ExpectedPublicValues,
    elf_sha256: &[u8; 32],
    input_bytes: u64,
    solana_rpc_url: &str,
    verifier_rpc_url: &str,
    attempt: u32,
) -> Result<(), FollowerError> {
    let fetched_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let sidecar = rome_zk_prover_input::build::Sidecar {
        expected,
        last_block_hash: None,
        state_root: None,
        provenance: rome_zk_prover_input::build::Provenance {
            solana_rpc: solana_rpc_url.to_string(),
            verifier_rpc: verifier_rpc_url.to_string(),
            fetched_at,
            // Not queried by the follower loop (an extra RPC call per batch for a purely informational
            // field is not worth it — `VerifierFetch` has no `client_version` method); the fields the
            // resume check actually compares (elf_sha256, chain_id, inbox_commitment, open_unix_ts, the
            // proof's own publics) are all populated for real, never placeholders.
            verifier_version: "not queried (follower loop)".to_string(),
            input_bytes,
            elf_sha256: Some(hex::encode(elf_sha256)),
            genesis_sha256: "n/a (wire v2 — chain rules are baked into the guest ELF)".to_string(),
            attempt,
        },
    };
    let s = serde_json::to_string(&sidecar)
        .map_err(|e| FollowerError::Io(format!("encode sidecar: {e}")))?;
    std::fs::write(path, s).map_err(|e| FollowerError::Io(format!("write {}: {e}", path.display())))
}

/// Steps 1–5: anchor, resolve resume vs. rebuild, (unless resuming) fetch the finalized
/// inbox batch + chunks, wait for the verifier's head, build the guest input, and (unless resuming)
/// prove — then decode + record-check the proof file either way. Returns early (no error) for every
/// non-halting condition the loop must react to (idle, retry, already-posted, superseded).
#[allow(clippy::too_many_arguments)]
async fn prepare_checked_proof<F, P, V, St>(
    fetch: &mut F,
    prover: &P,
    verifier: &mut V,
    store: &St,
    cfg: &RunConfig,
    candidate_batch: u64,
    metrics: &Metrics,
) -> Result<Result<(anchor::Anchor, RecordChecked, PathBuf, u32, f64), Outcome>, FollowerError>
where
    F: SnapshotFetch + rome_zk_prover_input::inbox::AccountFetch,
    P: Prover,
    V: VerifierFetch,
    St: Store,
{
    let anchor1 = match anchor::anchor(
        fetch,
        &cfg.settlement_program,
        &cfg.inbox_program,
        cfg.chain_id,
        candidate_batch,
        &cfg.vkey,
    ) {
        Ok(a) => a,
        Err(AnchorError::HeadAhead {
            head_pending_batch,
            head_final_batch,
            ..
        }) => {
            let outcome = outcome_for_head_ahead(candidate_batch, head_pending_batch);
            // A restart between a post and its own finalize sweep
            // (never entered this run's own resume-or-rebuild pipeline at all, this is the FIRST anchor)
            // leaves the row stuck at `posted` forever — the chain itself already says `candidate_batch`
            // is final. This job has no local knowledge of which attempt actually posted it (a fresh box,
            // or simply no on-disk artefact for this candidate any more), so it is recorded under the
            // same "not one specific attempt" sentinel a loop-level halt uses; the README's own
            // latest-job query orders by `updated_at DESC`, so this fresh write correctly outranks the
            // stale `posted` row regardless of which attempt number that one carries.
            if matches!(outcome, Outcome::AlreadyPosted { .. })
                && candidate_batch <= head_final_batch
            {
                record(
                    store,
                    metrics,
                    cfg.chain_id,
                    candidate_batch,
                    0,
                    JobEventKind::Finalized { finalize_sig: None },
                )
                .await;
            }
            return Ok(Err(outcome));
        }
        Err(AnchorError::InboxNotFinalizedYet {
            head_pending_batch,
            cursor_next_batch,
            ..
        }) => {
            let batches_behind_now = batches_behind(cursor_next_batch, head_pending_batch);
            metrics.batches_behind.set(batches_behind_now as i64);
            metrics.set_head("pending", head_pending_batch as i64);
            metrics.set_head("cursor", cursor_next_batch as i64);
            return Ok(Err(if batches_behind_now == 0 {
                metrics.set_state("idle");
                Outcome::Idle {
                    batches_behind: batches_behind_now,
                }
            } else {
                Outcome::Retry {
                    batches_behind: batches_behind_now,
                    reason: RetryReason::InboxNotFinalizedYet,
                }
            }));
        }
        Err(AnchorError::AbandonedInboxBatch { batch }) => {
            metrics.abandoned_batch_alarm.inc();
            return Err(FollowerError::AbandonedInboxBatch { batch });
        }
        Err(AnchorError::Fetch(e)) => {
            // A transient snapshot-read failure, never a decoded on-chain fact — retry the same
            // candidate rather than halting or panicking.
            return Ok(Err(Outcome::Retry {
                batches_behind: 0,
                reason: RetryReason::TransientFetchError(e.0),
            }));
        }
        Err(other) => return Err(FollowerError::Anchor(other)),
    };

    let batches_behind_now =
        batches_behind(anchor1.cursor_next_batch, anchor1.root.head_pending_batch);
    metrics.batches_behind.set(batches_behind_now as i64);
    metrics.set_head("pending", anchor1.root.head_pending_batch as i64);
    metrics.set_head("final", anchor1.root.head_final_batch as i64);
    metrics.set_head("cursor", anchor1.cursor_next_batch as i64);
    metrics.set_state("queued");

    let (batch_dir, input_path, sidecar_path, proof_path) =
        batch_paths(&cfg.work_dir, candidate_batch);

    let (checked, attempt, cost_usd) = if let Some((checked, resumed_attempt, verify_wall_ms)) =
        try_resume(
            &input_path,
            &sidecar_path,
            &proof_path,
            &cfg.vkey,
            cfg.chain_id,
            &anchor1.inbox_batch_head_plus_1,
        ) {
        metrics.set_state("decoded");
        // A resumed job skipped `fetch_and_verify_batch`/`build_batch_input`/
        // `prove` entirely this run — there is no fresh Queued/InputBuilt/Proving/Proved to report. What
        // IS free to report, from the resumed proof's own already-decoded public values (no extra
        // fetch): the block range, and the fact it re-verified clean, with the wall time that re-verify
        // actually took. Every resumed event is recorded under `resumed_attempt` — the sidecar's own
        // recorded `provenance.attempt`, the REAL winning attempt of whichever run produced this
        // artefact, never a literal `1` (that would UPSERT onto the wrong `(chain_id, batch, attempt)`
        // row, stranding the real winning attempt's own history at `verified` forever).
        if let Ok(pv) = poster::derive_public_values(&checked) {
            record(
                store,
                metrics,
                cfg.chain_id,
                candidate_batch,
                resumed_attempt,
                JobEventKind::Queued {
                    first_block: pv.first_number,
                    last_block: pv.last_number,
                },
            )
            .await;
        }
        let cd = checked.calldata();
        record(
            store,
            metrics,
            cfg.chain_id,
            candidate_batch,
            resumed_attempt,
            JobEventKind::Verified {
                wall_ms: verify_wall_ms,
                proof_abi: cd.proof_bytes_768.to_vec(),
                publics: cd.publics_512.to_vec(),
            },
        )
        .await;
        (checked, resumed_attempt, 0.0f64)
    } else {
        // The resume gate re-proves, it never halts. Every failure short of the
        // subprocess/fixture layer itself being broken — a corrupt cached proof, a sidecar bound to
        // another chain reset, a resumed proof whose publics disagree with its own sidecar — retries
        // with a freshly wiped directory, up to `Config::max_prove_attempts`. Only when every attempt
        // in that bounded run fails does this halt, by name.
        let max_attempts = cfg.max_prove_attempts.max(1);
        let mut checked = None;
        let mut winning_attempt: u32 = 0;
        let mut winning_cost_usd: f64 = 0.0;
        for attempt in 1..=max_attempts {
            wipe_dir(&batch_dir)?;
            std::fs::create_dir_all(&batch_dir)
                .map_err(|e| FollowerError::Io(format!("create {}: {e}", batch_dir.display())))?;

            metrics.set_state("input_built");
            let (batch_account, chunk_bodies) =
                match rome_zk_prover_input::inbox::fetch_and_verify_batch(
                    fetch,
                    &cfg.inbox_program,
                    cfg.chain_id,
                    candidate_batch,
                ) {
                    Ok(v) => v,
                    // A transient failure of the chunk/batch-account read itself — never
                    // a decoded on-chain fact — retries the same candidate rather than halting; the work
                    // directory this attempt just wiped/recreated is harmless to retry into (nothing was
                    // written to it yet).
                    Err(rome_zk_prover_input::inbox::InboxError::Fetch(e)) => {
                        return Ok(Err(Outcome::Retry {
                            batches_behind: batches_behind_now,
                            reason: RetryReason::TransientFetchError(e.0),
                        }));
                    }
                    Err(e) => return Err(FollowerError::Input(e.to_string())),
                };
            let (first, last) = rome_zk_prover_input::inbox::decode_block_range(&chunk_bodies)
                .map_err(|e| FollowerError::Input(e.to_string()))?;
            record(
                store,
                metrics,
                cfg.chain_id,
                candidate_batch,
                attempt,
                JobEventKind::Queued {
                    first_block: first,
                    last_block: last,
                },
            )
            .await;

            // A transport-level failure on the reth-verifier read (connection, timeout, torn body) is
            // retry material under the same fetch alarm bound as a Solana-side read; a JSON-RPC error,
            // a decode failure or a missing block is a fact about the data and still halts by name.
            let have = match verifier.head() {
                Ok(h) => h,
                Err(e) if e.is_transient() => {
                    return Ok(Err(Outcome::Retry {
                        batches_behind: batches_behind_now,
                        reason: RetryReason::TransientFetchError(e.to_string()),
                    }));
                }
                Err(e) => return Err(FollowerError::Input(e.to_string())),
            };
            if have < last {
                return Ok(Err(Outcome::Retry {
                    batches_behind: batches_behind_now,
                    reason: RetryReason::VerifierBehind { need: last, have },
                }));
            }

            let max_drift_secs = anchor1.chain_config.max_drift_secs.ok_or_else(|| {
                FollowerError::Input("chain_config has no v2 max_drift_secs".to_string())
            })?;

            let started = Instant::now();
            let (public, witness, expected) = match rome_zk_prover_input::build::build_batch_input(
                batch_account,
                chunk_bodies,
                max_drift_secs,
                verifier,
            ) {
                Ok(v) => v,
                Err(rome_zk_prover_input::build::BuildError::Verifier(e)) if e.is_transient() => {
                    return Ok(Err(Outcome::Retry {
                        batches_behind: batches_behind_now,
                        reason: RetryReason::TransientFetchError(e.to_string()),
                    }));
                }
                Err(e) => return Err(FollowerError::Input(e.to_string())),
            };
            rome_zk_prover_input::build::write_stdin_file(&input_path, &public, &witness)
                .map_err(|e| FollowerError::Input(e.to_string()))?;
            let input_wall_secs = started.elapsed().as_secs_f64();
            metrics.observe_stage_seconds(metrics::STAGE_INPUT, input_wall_secs);
            let input_bytes = std::fs::metadata(&input_path).map(|m| m.len()).unwrap_or(0);
            record(
                store,
                metrics,
                cfg.chain_id,
                candidate_batch,
                attempt,
                JobEventKind::InputBuilt {
                    input_bytes,
                    wall_ms: input_wall_secs * 1000.0,
                },
            )
            .await;

            // The sidecar is written from the DA-derived expectation `fetch_and_verify_batch`/
            // `build_batch_input` just computed — BEFORE proving runs, and never re-derived from a
            // second, independent read of the inbox batch ("a cache is keyed by the
            // identity of the TARGET"; `fetch_and_verify_batch`'s own read of the inbox batch and this
            // anchor's are the same one real account in production, so they already agree byte for
            // byte — no override needed). Writing it here, before `prove` runs, means a kill mid-prove
            // leaves a sidecar plus a partial-or-absent proof file on disk: `try_resume`'s own
            // `is_file()`/decode checks return `None` for that shape (never a stale trust), and the
            // retry loop wipes the whole directory and rebuilds from scratch.
            write_full_sidecar(
                &sidecar_path,
                expected,
                &cfg.vkey.elf_sha256,
                input_bytes,
                &cfg.solana_rpc_url,
                &cfg.verifier_rpc_url,
                attempt,
            )?;

            metrics.set_state("proving");
            metrics.prove_attempts_total.inc();
            // The only backend there is — reporting only, `Prover` itself has no name of its own to
            // ask for.
            record(
                store,
                metrics,
                cfg.chain_id,
                candidate_batch,
                attempt,
                JobEventKind::Proving {
                    backend: "cargo-zisk".to_string(),
                },
            )
            .await;
            let proof = match prover.prove(&cfg.elf_path, &input_path, &proof_path) {
                Ok(p) => p,
                // A freshly produced proof that the subprocess itself refused (timeout, non-zero exit,
                // never verified) is retry material, same as a decode/record/publics failure below —
                // `LocalCargoZisk::prove` has already deleted any partial `-o` file on this path.
                // Recorded `Failed` at THIS attempt, never silently —
                // without it, this attempt's row is frozen at whatever the LAST successful transition
                // was (here, `Proving`), never reflecting that it actually failed.
                Err(e) => {
                    record(
                        store,
                        metrics,
                        cfg.chain_id,
                        candidate_batch,
                        attempt,
                        JobEventKind::Failed {
                            reason: e.to_string(),
                        },
                    )
                    .await;
                    continue;
                }
            };
            if let Some(secs) = proof.walls.proof_generated_secs {
                metrics.observe_stage_seconds(metrics::STAGE_STARK, secs);
            }
            let plonk_secs = proof.walls.wrapper_snark_ms.map(|ms| ms as f64 / 1000.0);
            if let Some(secs) = plonk_secs {
                metrics.observe_stage_seconds(metrics::STAGE_PLONK, secs);
            }
            let stark_secs = proof.walls.proof_generated_secs.unwrap_or(0.0);
            let plonk_secs_val = plonk_secs.unwrap_or(0.0);
            let total = stark_secs + plonk_secs_val;
            let cost_usd_this_attempt = cfg.gpu_hourly_usd * total / 3600.0;
            metrics.batch_cost_usd.set(cost_usd_this_attempt);
            record(
                store,
                metrics,
                cfg.chain_id,
                candidate_batch,
                attempt,
                JobEventKind::Proved {
                    program_vk: hex::encode(cfg.vkey.program_vk),
                    wall_stark_ms: stark_secs * 1000.0,
                    wall_plonk_ms: plonk_secs_val * 1000.0,
                },
            )
            .await;

            let verify_started = Instant::now();
            let verified = (|| -> Result<RecordChecked, String> {
                let proof_bytes = std::fs::read(&proof_path).map_err(|e| e.to_string())?;
                let cd = crate::calldata::from_zisk_proof_file(&proof_bytes)
                    .map_err(|e| e.to_string())?;
                let checked = crate::calldata::check_against_record(&cd, &cfg.vkey)
                    .map_err(|e| e.to_string())?;
                // The SAME pairing check `build_post_ix` runs before every send, run
                // here too — a freshly produced proof that decodes and checks clean against the record
                // but fails the real BN254 pairing (a subprocess writing a structurally-valid but
                // cryptographically broken file) is retry material, counted against
                // `max_prove_attempts`, never an unbounded halt.
                poster::verify_checked(&checked, &cfg.vkey).map_err(|e| e.to_string())?;
                Ok(checked)
            })();
            match verified {
                Ok(c) => {
                    let verify_wall_secs = verify_started.elapsed().as_secs_f64();
                    metrics.observe_stage_seconds(metrics::STAGE_VERIFY, verify_wall_secs);
                    let cd = c.calldata();
                    record(
                        store,
                        metrics,
                        cfg.chain_id,
                        candidate_batch,
                        attempt,
                        JobEventKind::Verified {
                            wall_ms: verify_wall_secs * 1000.0,
                            proof_abi: cd.proof_bytes_768.to_vec(),
                            publics: cd.publics_512.to_vec(),
                        },
                    )
                    .await;
                    winning_attempt = attempt;
                    winning_cost_usd = cost_usd_this_attempt;
                    checked = Some(c);
                    break;
                }
                // A FRESHLY produced proof that fails decode/record-check/pairing is retry material too
                // — this used to halt: before this fix, a decode/record failure propagated as
                // `FollowerError::Calldata` and the loop exited, so a preemption right after the
                // sidecar was written (before `prove` finished) repeated on every restart forever.
                // Recorded `Failed` at THIS attempt — without it, this attempt's row stays frozen at
                // `proved`, never showing that it actually failed.
                Err(reason) => {
                    record(
                        store,
                        metrics,
                        cfg.chain_id,
                        candidate_batch,
                        attempt,
                        JobEventKind::Failed { reason },
                    )
                    .await;
                    continue;
                }
            }
        }
        match checked {
            Some(c) => (c, winning_attempt, winning_cost_usd),
            None => {
                return Err(FollowerError::ProveAttemptsExhausted {
                    attempts: max_attempts,
                })
            }
        }
    };

    metrics.set_state("locally_verified");
    Ok(Ok((anchor1, checked, batch_dir, attempt, cost_usd)))
}

/// Extracts `(custom_code, insufficient_funds)` from a failed send (mirrors the CLI's own former
/// `classify_sender_error`, moved here so the loop and `--once` share one definition).
fn classify_sender_error(err: &SenderError) -> (Option<u32>, bool) {
    use rome_zk_solana_sender::{InstructionError, TransactionError};
    match rome_zk_solana_sender::sender_error_transaction_error(err) {
        Some(TransactionError::InstructionError(_, InstructionError::Custom(code))) => {
            (Some(code), false)
        }
        Some(TransactionError::InsufficientFundsForFee)
        | Some(TransactionError::InsufficientFundsForRent { .. }) => (None, true),
        _ => (None, false),
    }
}

/// Steps 6–8: re-anchor (a stale anchor here means the chain moved on while we proved —
/// `Superseded`, zero sends), build + send `PostRootProved`, classify a failed send, then the finalize
/// sweep and retention-gated close.
#[allow(clippy::too_many_arguments)]
fn post_checked<'a, F, S, St>(
    fetch: &'a mut F,
    sender: &'a S,
    store: &'a St,
    cfg: &'a RunConfig,
    candidate_batch: u64,
    checked: &RecordChecked,
    attempt: u32,
    cost_usd: f64,
    metrics: &'a Metrics,
) -> PostFuture<'a, F, S, St>
where
    F: SnapshotFetch,
    S: Sender,
    St: Store,
{
    PostFuture {
        fetch,
        sender,
        store,
        cfg,
        candidate_batch,
        checked: checked.clone(),
        attempt,
        cost_usd,
        metrics,
    }
}

// `post_checked` is written as a named future (rather than an `async fn`) only so `RecordChecked` can
// be passed by value without an extra lifetime parameter fighting the borrow checker across the
// `.await` in `send_and_confirm` — functionally identical to an `async fn` with the same body.
struct PostFuture<'a, F, S, St> {
    fetch: &'a mut F,
    sender: &'a S,
    store: &'a St,
    cfg: &'a RunConfig,
    candidate_batch: u64,
    checked: RecordChecked,
    attempt: u32,
    cost_usd: f64,
    metrics: &'a Metrics,
}

impl<F, S, St> PostFuture<'_, F, S, St>
where
    F: SnapshotFetch,
    S: Sender,
    St: Store,
{
    async fn run(self) -> Result<Outcome, FollowerError> {
        let Self {
            fetch,
            sender,
            store,
            cfg,
            candidate_batch,
            checked,
            attempt,
            cost_usd,
            metrics,
        } = self;

        let anchor2 = match anchor::anchor(
            fetch,
            &cfg.settlement_program,
            &cfg.inbox_program,
            cfg.chain_id,
            candidate_batch,
            &cfg.vkey,
        ) {
            Ok(a) => a,
            Err(AnchorError::HeadAhead {
                head_pending_batch, ..
            }) => {
                // Resolved one way or the other — clear any remembered stale-send state for this
                // candidate.
                *cfg.stale_send_pending.lock().unwrap() = None;
                let outcome = outcome_for_head_ahead(candidate_batch, head_pending_batch);
                match &outcome {
                    Outcome::Superseded { .. } => metrics.set_state("superseded"),
                    Outcome::AlreadyPosted { .. } => metrics.inc_posts("already_posted"),
                    _ => {}
                }
                let kind = match &outcome {
                    Outcome::Superseded { .. } => JobEventKind::Superseded { head_pending_batch },
                    _ => JobEventKind::AlreadyPosted,
                };
                record(store, metrics, cfg.chain_id, candidate_batch, attempt, kind).await;
                return Ok(outcome);
            }
            // A transient re-anchor read failure — the checked proof stays on disk
            // and this same candidate is retried; `try_resume` finds it and skips straight back to
            // `post_checked` next time, never re-proving. Any remembered stale-send state is left
            // untouched (we still don't know whether the head has moved).
            Err(AnchorError::Fetch(e)) => {
                return Ok(Outcome::Retry {
                    batches_behind: 0,
                    reason: RetryReason::TransientFetchError(e.0),
                });
            }
            Err(other) => return Err(FollowerError::Anchor(other)),
        };

        // The FINALIZED read just above still agrees with what it showed when a
        // PREVIOUS send for this SAME candidate was classified `StaleAnchor` (a genuinely landed send
        // reported as a refusal by a lagging read) — do not resend and pay a second fee for a
        // transaction that may already have confirmed; report the same retry again and wait for the
        // read to catch up. Once it does, the `HeadAhead` arm above clears this and classifies
        // `AlreadyPosted`/`Superseded` on its own.
        if *cfg.stale_send_pending.lock().unwrap() == Some(candidate_batch) {
            metrics.inc_posts("stale_anchor_retry");
            return Ok(Outcome::Retry {
                batches_behind: 0,
                reason: RetryReason::StaleAnchor,
            });
        }

        let authority = cfg.authority;
        let treasury = anchor2.global_config.treasury;
        let params = PostParams {
            anchor: &anchor2,
            vkey: &cfg.vkey,
            checked: &checked,
            settlement_program: &cfg.settlement_program,
            inbox_program: &cfg.inbox_program,
            authority: &authority,
            treasury: &treasury,
        };
        let ix = poster::build_post_ix(&params)?;

        let loaded_accounts_data_size_limit = poster::resolve_loaded_accounts_data_size_limit(
            cfg.loaded_accounts_data_size_limit,
            &anchor2.account_data_lens,
        )
        .map_err(|e| FollowerError::Input(e.to_string()))?;
        let tuning = SendTuning {
            loaded_accounts_data_size_limit,
            ..cfg.tuning
        };

        metrics.set_state("posting");
        let our_batch = anchor2.root.head_pending_batch + 1;
        let (root_pda, _) = zk_settlement_client::root_pda(&cfg.settlement_program, cfg.chain_id);
        let post_started = Instant::now();
        let outcome = match sender
            .send_and_confirm(std::slice::from_ref(&ix), tuning)
            .await
        {
            Ok(sig) => {
                *cfg.stale_send_pending.lock().unwrap() = None;
                let post_wall_secs = post_started.elapsed().as_secs_f64();
                metrics.observe_stage_seconds(metrics::STAGE_POST, post_wall_secs);
                metrics.set_state("relayed");
                metrics.inc_posts("relayed");
                // `batch_gas_used`/`lag_seconds` from the proof's own packaged public
                // values — the same decode `build_post_ix` already ran, no extra fetch of any kind.
                let gas_used = poster::derive_public_values(&checked)
                    .map(|pv| {
                        metrics.batch_gas_used.set(pv.gas_used as f64);
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        metrics
                            .lag_seconds
                            .set(now.saturating_sub(pv.open_unix_ts) as f64);
                        pv.gas_used
                    })
                    .unwrap_or(0);
                record(
                    store,
                    metrics,
                    cfg.chain_id,
                    candidate_batch,
                    attempt,
                    JobEventKind::Posted {
                        sig: sig.to_string(),
                        wall_ms: post_wall_secs * 1000.0,
                        gas_used,
                        cost_usd,
                    },
                )
                .await;
                Outcome::Posted {
                    batch: our_batch,
                    sig: sig.to_string(),
                }
            }
            Err(e) => {
                let (custom_code, insufficient_funds) = classify_sender_error(&e);
                let head = match fetch.get_multiple_accounts(&[root_pda]) {
                    Ok((_slot, mut accs)) => accs
                        .remove(0)
                        .and_then(|d| zk_settlement_client::decode_root_account(&d).ok())
                        .map(|r| r.head_pending_batch)
                        .unwrap_or(our_batch),
                    // A transient re-read failure here is not itself informative about whether the
                    // send landed — fall back to `our_batch`, the same "no better information"
                    // baseline `classify_send_failure` already uses for an undecodable root.
                    Err(_) => our_batch,
                };
                match poster::classify_send_failure(
                    custom_code,
                    insufficient_funds,
                    head,
                    our_batch,
                ) {
                    PostRefusal::AlreadyPosted => {
                        *cfg.stale_send_pending.lock().unwrap() = None;
                        metrics.inc_posts("already_posted");
                        record(
                            store,
                            metrics,
                            cfg.chain_id,
                            candidate_batch,
                            attempt,
                            JobEventKind::AlreadyPosted,
                        )
                        .await;
                        Outcome::AlreadyPosted { batch: our_batch }
                    }
                    PostRefusal::HeadAhead => {
                        *cfg.stale_send_pending.lock().unwrap() = None;
                        metrics.inc_posts("superseded");
                        record(
                            store,
                            metrics,
                            cfg.chain_id,
                            candidate_batch,
                            attempt,
                            JobEventKind::Superseded {
                                head_pending_batch: head,
                            },
                        )
                        .await;
                        Outcome::Superseded {
                            batch: our_batch,
                            head_pending_batch: head,
                        }
                    }
                    PostRefusal::StaleAnchor => {
                        // The classifier's own contract is "retry the re-anchor" — a
                        // FINALIZED read can genuinely lag a send that already landed. Remember this
                        // candidate as pending so no retry resends while the read still lags (see the
                        // check right after the re-anchor above): the fee was spent once, here, and is
                        // never spent again for this candidate — the next re-anchor either classifies
                        // it `AlreadyPosted`/`Superseded` (memory cleared) or the streak reaches
                        // `stale_anchor_alarm_polls` and halts by name.
                        *cfg.stale_send_pending.lock().unwrap() = Some(candidate_batch);
                        metrics.inc_posts("stale_anchor_retry");
                        Outcome::Retry {
                            batches_behind: 0,
                            reason: RetryReason::StaleAnchor,
                        }
                    }
                    other => {
                        *cfg.stale_send_pending.lock().unwrap() = None;
                        metrics.inc_posts("failed");
                        return Err(FollowerError::PostRefused(other));
                    }
                }
            }
        };

        if matches!(
            outcome,
            Outcome::Posted { .. } | Outcome::AlreadyPosted { .. }
        ) {
            match finalize_and_close(fetch, sender, cfg, candidate_batch, &tuning, metrics).await {
                // `AlreadyFinal` is the common case for a proved, in-order post —
                // `PostRootProved` itself advances `head_final_batch` in the same instruction
                // (settle.rs "posted+proved (final immediately)"), so THIS job's own post signature
                // (never fabricated for a batch a DIFFERENT run posted — that's `AlreadyPosted`, not
                // ours to attribute) is the truthful `finalize_sig`.
                Ok(FinalizeSweepOutcome::AlreadyFinal) => {
                    if let Outcome::Posted { sig, .. } = &outcome {
                        record(
                            store,
                            metrics,
                            cfg.chain_id,
                            candidate_batch,
                            attempt,
                            JobEventKind::Finalized {
                                finalize_sig: Some(sig.clone()),
                            },
                        )
                        .await;
                    }
                }
                Ok(FinalizeSweepOutcome::FinalizedByThisCall(finalize_sig)) => {
                    record(
                        store,
                        metrics,
                        cfg.chain_id,
                        candidate_batch,
                        attempt,
                        JobEventKind::Finalized {
                            finalize_sig: Some(finalize_sig),
                        },
                    )
                    .await;
                }
                Ok(FinalizeSweepOutcome::NotYetFinal) => {}
                // A transient read failure in the finalize/close sweep never
                // undoes THIS job's own already-successful post — the batch is genuinely Relayed
                // regardless. Every later successful post re-runs this exact same sweep over a fresh
                // root read, so a missed finalize/close here is picked up on the next one, not lost.
                Err(FollowerError::Fetch(msg)) => {
                    tracing::warn!(
                        "finalize/close sweep hit a transient fetch error, deferred to the next \
                         successful post's own sweep: {msg}"
                    );
                }
                Err(e) => return Err(e),
            }
        }

        Ok(outcome)
    }
}

/// Step 7–8: the finalize sweep (needed whenever a post landed with
/// `advances_head == false`) and the retention-gated close, over a fresh root read. `candidate_batch`
/// identifies the job THIS caller is tracking; the returned [`FinalizeSweepOutcome`] tells the caller
/// whether THAT job is now final and, if so, which signature to attribute it to — the sweep's own plan
/// can walk past several already-Final batches in one call (`FinalizePlan::walk`), but this function
/// itself never writes history (the caller decides, since only it knows the job's own
/// attempt number and its own post signature for the common "final immediately" case).
async fn finalize_and_close<F, S>(
    fetch: &mut F,
    sender: &S,
    cfg: &RunConfig,
    candidate_batch: u64,
    tuning: &SendTuning,
    metrics: &Metrics,
) -> Result<FinalizeSweepOutcome, FollowerError>
where
    F: SnapshotFetch,
    S: Sender,
{
    let (root_pda, _) = zk_settlement_client::root_pda(&cfg.settlement_program, cfg.chain_id);
    let (_slot, mut accs) = fetch
        .get_multiple_accounts(&[root_pda])
        .map_err(|e| FollowerError::Fetch(e.0))?;
    let root_data = accs
        .remove(0)
        .ok_or_else(|| FollowerError::Io("root account missing after post".to_string()))?;
    let root = zk_settlement_client::decode_root_account(&root_data)
        .map_err(|e| FollowerError::Io(e.to_string()))?;

    // `PostRootProved` writes its own pending PDA `Final` immediately and advances `head_final_batch`
    // in the SAME instruction whenever the batch posts in strict order (`settle.rs::advances_head`,
    // "posted+proved (final immediately)") — the common, only path this crate's product shape ever
    // takes (one proof per batch, no challenge window). So THIS read, taken right after our
    // own post, already shows our own job final more often than not, with no separate `FinalizeBatch`
    // send in this call at all.
    let already_final = candidate_batch <= root.head_final_batch;

    let mut finalized_by_this_call = None;
    if let Some(plan) = crate::finality::plan_finalize(
        root.head_pending_batch,
        root.head_final_batch,
        cfg.finalize_walk,
    ) {
        let finalize_ix = zk_settlement_client::finalize_batch_ix(
            &cfg.settlement_program,
            cfg.chain_id,
            plan.batch,
            &plan.walk,
        );
        let started = Instant::now();
        let finalize_sig = sender
            .send_and_confirm(&[finalize_ix], *tuning)
            .await
            .map_err(|e| FollowerError::Send(e.to_string()))?;
        metrics.observe_stage_seconds(metrics::STAGE_FINALIZE, started.elapsed().as_secs_f64());
        metrics.set_state("finalized");
        // This sweep can finalize several already-Final batches in one call (`plan.walk`) — only the
        // caller's own job (`candidate_batch`) gets a history row from THIS call; a sibling batch swept
        // past here has no attempt number this call site knows (see the function doc above).
        if plan.batch == candidate_batch || plan.walk.contains(&candidate_batch) {
            finalized_by_this_call = Some(finalize_sig.to_string());
        }
    }

    if let Some(k) = cfg.close_pending_after_batches {
        if let Some(candidate) = root.head_pending_batch.checked_sub(k + 1) {
            if candidate >= 1 {
                let (pending_pda, _) = zk_settlement_client::pending_pda(
                    &cfg.settlement_program,
                    cfg.chain_id,
                    candidate,
                );
                let (_slot, mut accs2) = fetch
                    .get_multiple_accounts(&[root_pda, pending_pda])
                    .map_err(|e| FollowerError::Fetch(e.0))?;
                let pending_d = accs2.pop().flatten();
                let root_d = accs2.pop().flatten();
                let root_now = root_d
                    .map(|d| zk_settlement_client::decode_root_account(&d))
                    .transpose()
                    .map_err(|e| FollowerError::Io(e.to_string()))?;
                if let (Some(root_now), Some(pending_data)) = (root_now, pending_d) {
                    if root_now.head_pending_batch == root.head_pending_batch {
                        let pending = zk_settlement_client::decode_pending_account(&pending_data)
                            .map_err(|e| FollowerError::Io(e.to_string()))?;
                        if crate::finality::should_close(
                            root_now.head_pending_batch,
                            candidate,
                            pending.status,
                            Some(k),
                        ) {
                            let close_ix = zk_settlement_client::close_pending_ix(
                                &cfg.settlement_program,
                                &cfg.authority,
                                cfg.chain_id,
                                candidate,
                            );
                            sender
                                .send_and_confirm(&[close_ix], *tuning)
                                .await
                                .map_err(|e| FollowerError::Send(e.to_string()))?;
                        }
                    }
                }
            }
        }
    }

    Ok(match finalized_by_this_call {
        Some(finalize_sig) => FinalizeSweepOutcome::FinalizedByThisCall(finalize_sig),
        None if already_final => FinalizeSweepOutcome::AlreadyFinal,
        None => FinalizeSweepOutcome::NotYetFinal,
    })
}

/// What [`finalize_and_close`] learned about the CALLER's own `candidate_batch` (never about any sibling
/// batch its sweep happens to walk past) — a job is `Finalized` in history the moment
/// it is actually final on chain, whichever instruction made it so.
enum FinalizeSweepOutcome {
    /// `head_final_batch` already covered `candidate_batch` on this call's own fresh read — most often
    /// because `PostRootProved` itself advanced it in the same instruction as the post (settle.rs
    /// "posted+proved (final immediately)"), sometimes because an earlier iteration's sweep already
    /// covered it. No `FinalizeBatch` was sent this call.
    AlreadyFinal,
    /// This call sent `FinalizeBatch` and its own walk covered `candidate_batch`, with this signature.
    FinalizedByThisCall(String),
    /// `candidate_batch` is not yet final — no history to record.
    NotYetFinal,
}

/// Runs the whole state machine once for `candidate_batch` — the follower loop's own per-iteration
/// call, and `--once`'s entire job (`--once` is the degenerate single-iteration case).
#[allow(clippy::too_many_arguments)]
pub async fn run_one<F, P, S, V, St>(
    fetch: &mut F,
    prover: &P,
    sender: &S,
    verifier: &mut V,
    store: &St,
    cfg: &RunConfig,
    candidate_batch: u64,
    metrics: &Metrics,
) -> Result<Outcome, FollowerError>
where
    F: SnapshotFetch + rome_zk_prover_input::inbox::AccountFetch,
    P: Prover,
    S: Sender,
    V: VerifierFetch,
    St: Store,
{
    match run_one_inner(
        fetch,
        prover,
        sender,
        verifier,
        store,
        cfg,
        candidate_batch,
        metrics,
    )
    .await
    {
        Ok(outcome) => Ok(outcome),
        // A halting error is a job transition too, and every
        // caller of `run_one` must see it recorded, not only `run`'s own loop — `--once` (the CLI's
        // direct call, `bin/rome-zk-prover.rs`) halts here exactly the same way. `attempt` 0 is the same
        // "not one specific prove attempt" sentinel `run`'s own halting path always used (this error can
        // surface before or after the attempt loop, with no attempt number of its own to report). A store
        // failure recording IT never masks the real halting error below.
        Err(e) => {
            metrics.set_state("failed");
            record(
                store,
                metrics,
                cfg.chain_id,
                candidate_batch,
                0,
                JobEventKind::Failed {
                    reason: e.to_string(),
                },
            )
            .await;
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_one_inner<F, P, S, V, St>(
    fetch: &mut F,
    prover: &P,
    sender: &S,
    verifier: &mut V,
    store: &St,
    cfg: &RunConfig,
    candidate_batch: u64,
    metrics: &Metrics,
) -> Result<Outcome, FollowerError>
where
    F: SnapshotFetch + rome_zk_prover_input::inbox::AccountFetch,
    P: Prover,
    S: Sender,
    V: VerifierFetch,
    St: Store,
{
    match prepare_checked_proof(
        fetch,
        prover,
        verifier,
        store,
        cfg,
        candidate_batch,
        metrics,
    )
    .await?
    {
        Err(outcome) => Ok(outcome),
        Ok((_anchor1, checked, _batch_dir, attempt, cost_usd)) => {
            let outcome = post_checked(
                fetch,
                sender,
                store,
                cfg,
                candidate_batch,
                &checked,
                attempt,
                cost_usd,
                metrics,
            )
            .run()
            .await?;
            // Work-dir retention: a terminal outcome for this candidate — its own
            // artefacts are never needed again (a resume only ever targets the CURRENT, not-yet-final
            // candidate). `keep_work_dirs` (default 0) keeps the most recent N directories around
            // regardless, for a human inspecting a recent failure.
            if let Outcome::Posted { batch, .. } | Outcome::AlreadyPosted { batch } = outcome {
                if let Some(to_remove) = batch.checked_sub(cfg.keep_work_dirs as u64) {
                    if to_remove >= 1 {
                        let _ = wipe_dir(&cfg.work_dir.join(to_remove.to_string()));
                    }
                }
            }
            Ok(outcome)
        }
    }
}

/// The `--once --dry-run` / `--follow --dry-run` live gate's own entry point (never sends): runs steps
/// 1–5 (anchor, resume-or-rebuild, prove, decode, record-check) exactly like [`run_one`], then hands
/// the caller the fresh anchor and checked proof to print/simulate instead of posting. A non-`Ready`
/// early outcome (idle/retry/already-posted/superseded) is returned as-is — the CLI prints it and stops.
pub async fn prepare_for_dry_run<F, P, V>(
    fetch: &mut F,
    prover: &P,
    verifier: &mut V,
    cfg: &RunConfig,
    candidate_batch: u64,
    metrics: &Metrics,
) -> Result<Result<(anchor::Anchor, RecordChecked, PathBuf, u32, f64), Outcome>, FollowerError>
where
    F: SnapshotFetch + rome_zk_prover_input::inbox::AccountFetch,
    P: Prover,
    V: VerifierFetch,
{
    // The live gate never sends and never keeps history of its own — a dry run is not a job the
    // operator asked the store to remember (the history rows are for real jobs); it shares
    // `prepare_checked_proof`'s pipeline with a `NoopStore` rather than duplicating that whole function.
    prepare_checked_proof(
        fetch,
        prover,
        verifier,
        &crate::store::NoopStore,
        cfg,
        candidate_batch,
        metrics,
    )
    .await
}

/// A graceful-stop signal, checked once at the top of every loop iteration (never mid-post — the
/// current stage's send always finishes first).
pub trait StopSignal {
    fn should_stop(&self) -> bool;
}

/// Never stops — the default for `--once` (a single iteration) and any test that runs a bounded
/// `RunUntil::Iterations`.
pub struct NeverStop;
impl StopSignal for NeverStop {
    fn should_stop(&self) -> bool {
        false
    }
}

impl StopSignal for std::sync::atomic::AtomicBool {
    fn should_stop(&self) -> bool {
        self.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// How long [`run`] keeps going: forever (real `--follow`) or a fixed iteration count (the
/// `--follow --dry-run --iterations N` live gate, and every test).
#[derive(Debug, Clone, Copy)]
pub enum RunUntil {
    Forever,
    Iterations(u64),
}

/// Every dependency the follower loop needs, bundled — real implementations wrap the live RPC/subprocess
/// paths; a fake-driven test constructs one type implementing every trait, over an in-memory chain.
/// `store` is a fifth dependency, a plain trait — the loop runs unchanged against a
/// `NoopStore`/fake in every test that does not care about history, and never reads it back.
pub struct Deps<F, P, S, V, St> {
    pub fetch: F,
    pub prover: P,
    pub sender: S,
    pub verifier: V,
    pub store: St,
}

/// The follower loop: at the top of every iteration, tries the next candidate batch
/// (`last known head_pending_batch + 1`, self-correcting via `HeadAhead`/`Superseded` whenever the
/// guess is stale). Sleeps between iterations on `Idle` and every `Retry`; halts on
/// `AbandonedInboxBatch` or a `VerifierBehindAlarm` (after `verifier_behind_alarm_polls` CONSECUTIVE
/// `VerifierBehind` retries for the same job — a streak reset by any other outcome).
pub async fn run<F, P, S, V, St>(
    deps: &mut Deps<F, P, S, V, St>,
    cfg: &RunConfig,
    until: RunUntil,
    stop: &impl StopSignal,
    metrics: &Metrics,
    sleep: impl Fn(Duration) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
) -> Result<(), FollowerError>
where
    F: SnapshotFetch + rome_zk_prover_input::inbox::AccountFetch,
    P: Prover,
    S: Sender,
    V: VerifierFetch,
    St: Store,
{
    let mut next_candidate: u64 = 1;
    let mut verifier_behind_streak: u32 = 0;
    let mut stale_anchor_streak: u32 = 0;
    let mut fetch_error_streak: u32 = 0;
    let mut iterations: u64 = 0;

    loop {
        if let RunUntil::Iterations(n) = until {
            if iterations >= n {
                return Ok(());
            }
        }
        if stop.should_stop() {
            return Ok(());
        }

        // Reporting only, sampled once per iteration regardless of outcome — never
        // a decision input (see `SnapshotFetch::payer_lamports`'s own doc). A read failure here is not
        // worth interrupting the loop over; the gauge simply keeps its last known value.
        if let Ok(lamports) = deps.fetch.payer_lamports(&cfg.authority) {
            metrics.payer_lamports.set(lamports as i64);
        }

        let outcome = match run_one(
            &mut deps.fetch,
            &deps.prover,
            &deps.sender,
            &mut deps.verifier,
            &deps.store,
            cfg,
            next_candidate,
            metrics,
        )
        .await
        {
            Ok(o) => o,
            // `run_one` itself already recorded `Failed` at the attempt-0 sentinel before
            // returning this error (so `--once`'s direct call to `run_one` gets the same guarantee) —
            // recording it a second time here would double the row's `updated_at` bump for nothing.
            Err(e) => return Err(e),
        };
        iterations += 1;

        match &outcome {
            Outcome::Posted { batch, .. } | Outcome::AlreadyPosted { batch } => {
                next_candidate = batch + 1;
                verifier_behind_streak = 0;
                stale_anchor_streak = 0;
                fetch_error_streak = 0;
            }
            Outcome::Superseded {
                head_pending_batch, ..
            } => {
                next_candidate = head_pending_batch + 1;
                verifier_behind_streak = 0;
                stale_anchor_streak = 0;
                fetch_error_streak = 0;
            }
            Outcome::Idle { .. } => {
                verifier_behind_streak = 0;
                stale_anchor_streak = 0;
                fetch_error_streak = 0;
                sleep(cfg.poll_interval).await;
            }
            Outcome::Retry { reason, .. } => {
                match reason {
                    RetryReason::InboxNotFinalizedYet => {
                        verifier_behind_streak = 0;
                        stale_anchor_streak = 0;
                        fetch_error_streak = 0;
                    }
                    RetryReason::TransientFetchError(_) => {
                        verifier_behind_streak = 0;
                        stale_anchor_streak = 0;
                        fetch_error_streak += 1;
                        if fetch_error_streak >= cfg.fetch_alarm_polls {
                            metrics.fetch_alarm.inc();
                            metrics.set_state("failed");
                            return Err(FollowerError::FetchAlarm {
                                polls: fetch_error_streak,
                            });
                        }
                    }
                    RetryReason::VerifierBehind { need, have } => {
                        verifier_behind_streak += 1;
                        stale_anchor_streak = 0;
                        fetch_error_streak = 0;
                        if verifier_behind_streak >= cfg.verifier_behind_alarm_polls {
                            metrics.set_state("failed");
                            return Err(FollowerError::VerifierBehindAlarm {
                                need: *need,
                                have: *have,
                                polls: verifier_behind_streak,
                            });
                        }
                    }
                    RetryReason::StaleAnchor => {
                        verifier_behind_streak = 0;
                        stale_anchor_streak += 1;
                        fetch_error_streak = 0;
                        if stale_anchor_streak >= cfg.stale_anchor_alarm_polls {
                            metrics.stale_anchor_alarm.inc();
                            metrics.set_state("failed");
                            return Err(FollowerError::StaleAnchorAlarm {
                                polls: stale_anchor_streak,
                            });
                        }
                    }
                }
                sleep(cfg.poll_interval).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{NoopStore, StoreError};
    use rome_zk_solana_sender::SendTuning;
    use solana_program::instruction::Instruction;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    // ===================== the gate fixture (real proof, real BN254 pairing) =====================

    const GATE_CHAIN_ID: u64 = 200_101;
    const GATE_OPEN_UNIX_TS: i64 = 1_789_413_402;
    /// The gate fixture's own sidecar `inbox_commitment`
    /// (`fixtures/prover-input/txv1-dev-reset6-batch-1.json`) — every batch the fake chain presents
    /// must carry this SAME `acc`, since every test posts the one real, reused gate
    /// proof against whichever candidate batch id is under test. Deriving `acc` fresh per batch from
    /// synthetic block content (as this fixture used to) would make the new local inbox-binding check
    /// refuse batches 2..5 in the in-order scenario — the fakes must be honest about the identity the
    /// proof is actually bound to, not merely shape-compatible.
    const GATE_ACC_HEX: &str = "0774682b67c04292e4614fb5bf8540c3044208cacb2469c73f0613e515859b07";

    fn gate_acc() -> [u8; 32] {
        hex::decode(GATE_ACC_HEX).unwrap().try_into().unwrap()
    }

    fn gate_proof_bytes() -> Vec<u8> {
        std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/prover-input/txv1-dev-reset6-batch-1.plonk.bin"
        ))
        .expect("fixtures/prover-input/txv1-dev-reset6-batch-1.plonk.bin")
    }

    /// The real reset-6 batch-1 chunk bodies + `open_slot`, decoded straight out of the committed
    /// `.bin` fixture's own `RomePublicInput` frame ("honest fakes" — a fake answers one
    /// inbox batch PDA identically to `AccountFetch` and `SnapshotFetch`). This is
    /// the SAME real bytes `build_batch_input` fed the guest that produced the one real gate proof, so
    /// serving them for every synthetic batch makes `decode_block_range`/`build_batch_input`'s own
    /// DA-derived `first_number`/`last_number`/`gas_used` agree with the reused proof's own packaged
    /// publics by construction — no per-test overwrite needed. Decoded once and cached (a `OnceLock`,
    /// not a `lazy_static` — this crate's only such cache): the `.bin` fixture is ~80 KB, re-reading and
    /// re-decoding it from every one of the many calls per test would be needless I/O.
    struct GateFixture {
        open_slot: u64,
        chunk_bodies: Vec<Vec<u8>>,
    }
    fn gate_fixture() -> &'static GateFixture {
        static F: std::sync::OnceLock<GateFixture> = std::sync::OnceLock::new();
        F.get_or_init(|| {
            let bytes = std::fs::read(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/prover-input/txv1-dev-reset6-batch-1.bin"
            ))
            .expect("fixtures/prover-input/txv1-dev-reset6-batch-1.bin");
            // `write_stdin_file`'s own framing: an 8-byte LE length prefix, the payload (the
            // bincode-encoded `RomePublicInput`), then zero-padding to an 8-byte boundary — the public
            // frame is written first (`rome_zk_prover_input::build::write_stdin_file`).
            let len = u64::from_le_bytes(bytes[0..8].try_into().unwrap()) as usize;
            let payload = &bytes[8..8 + len];
            let (public, _): (rome_zk_prover_input::RomePublicInput, usize) =
                bincode::serde::decode_from_slice(payload, bincode::config::standard())
                    .expect("decode the real fixture's own RomePublicInput frame");
            assert_eq!(public.chain_id, GATE_CHAIN_ID);
            assert_eq!(public.batch, 1, "the fixture is reset-6's own batch 1");
            GateFixture {
                open_slot: public.open_slot,
                chunk_bodies: public.chunk_bodies,
            }
        })
    }

    /// Sanity check for the honest-fake claim `encode_inbox_batch`/`encode_inbox_batch_da_view`'s own
    /// docs make: at batch id 1 specifically, honestly recomputing `acc` from the real fixture's own
    /// chunk bodies (the SAME formula `fetch_and_verify_batch`'s self-check runs) reproduces
    /// `GATE_ACC_HEX` exactly — the one batch id where the "hardcode" and the honest computation
    /// coincide, which is what lets batch 1's fake identity be genuinely unified.
    #[test]
    fn the_default_batch_one_identity_is_honestly_gate_acc_hex() {
        let fx = gate_fixture();
        let chunk_hashes: Vec<[u8; 32]> = fx
            .chunk_bodies
            .iter()
            .map(|b| rome_zk_merkle::keccak256(&[b]))
            .collect();
        let (_root, _forced_root, acc) =
            zk_inbox_client::reference_commitment(GATE_CHAIN_ID, 1, fx.open_slot, &chunk_hashes);
        assert_eq!(
            acc,
            gate_acc(),
            "batch 1's honestly-recomputed acc must equal the real proof's own packaged inbox_commitment"
        );
    }

    fn tiber_vkey() -> VkeyOfRecord {
        VkeyOfRecord::load(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/vkeys/tiber-200101-layout1.json"
        )))
        .expect("load vkey of record")
    }

    // ===================== fake pieces: Prover, Verifier, Clock =====================

    /// Always writes the SAME real gate proof to `out_file`, regardless of `elf`/`input_bin` — the
    /// follower's own logic (resume, prove-call counting, the post pipeline) is what these tests
    /// exercise, not ZisK itself. Counts every real invocation (never incremented on a resumed job).
    struct FakeProver {
        calls: Arc<AtomicU32>,
    }
    impl Prover for FakeProver {
        fn prove(
            &self,
            _elf: &Path,
            _input_bin: &Path,
            out_file: &Path,
        ) -> Result<crate::prover::ProofFile, ProveError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::fs::write(out_file, gate_proof_bytes())
                .unwrap_or_else(|e| panic!("write fake proof: {e}"));
            Ok(crate::prover::ProofFile {
                path: out_file.to_path_buf(),
                walls: crate::prover::StageWalls {
                    verified: true,
                    proof_generated_secs: Some(1.0),
                    wrapper_snark_ms: Some(1),
                    steps: Some(1),
                },
            })
        }
    }

    /// One block per batch (block number == batch id, arbitrary) — enough for
    /// `fetch_and_verify_batch`/`decode_block_range`/`build_batch_input` to run for real against no
    /// network at all. The verifier's own `head()` always reports far ahead, so `VerifierBehind` never
    /// fires unless a test explicitly asks for it via [`FakeChain::set_verifier_head`].
    #[derive(Clone)]
    struct FakeVerifier(Arc<ChainState>);
    impl VerifierFetch for FakeVerifier {
        fn head(&mut self) -> Result<u64, rome_zk_prover_input::verifier::VerifierError> {
            let mut st = self.0.state.lock().unwrap();
            if st.verifier_head_fail_once {
                st.verifier_head_fail_once = false;
                return Err(
                    rome_zk_prover_input::verifier::VerifierError::BadResponseJson {
                        method: "eth_blockNumber",
                        source: std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "torn response body",
                        ),
                    },
                );
            }
            Ok(st.verifier_head)
        }
        fn header(
            &mut self,
            number: u64,
        ) -> Result<alloy_consensus::Header, rome_zk_prover_input::verifier::VerifierError>
        {
            Ok(alloy_consensus::Header {
                number,
                ..Default::default()
            })
        }
        fn block(
            &mut self,
            number: u64,
        ) -> Result<reth_ethereum_primitives::Block, rome_zk_prover_input::verifier::VerifierError>
        {
            Ok(reth_ethereum_primitives::Block {
                header: alloy_consensus::Header {
                    number,
                    ..Default::default()
                },
                body: Default::default(),
            })
        }
        fn witness(
            &mut self,
            _number: u64,
        ) -> Result<
            alloy_rpc_types_debug::ExecutionWitness,
            rome_zk_prover_input::verifier::VerifierError,
        > {
            Ok(alloy_rpc_types_debug::ExecutionWitness::default())
        }
    }

    // ===================== fake pieces: Store =====================

    /// Records every `JobEvent` it is given, in call order — the test double for `Store`. Also
    /// carries a `seeded_max_batch` this concrete type exposes on a method the `Store` TRAIT itself does
    /// not have (the trait is write-only, no `get`/`query` at all) — `reads()` proves nothing
    /// in the follower loop ever calls it.
    #[derive(Clone)]
    struct RecordingStore {
        events: Arc<Mutex<Vec<JobEvent>>>,
        seeded_max_batch: u64,
        reads: Arc<AtomicU32>,
    }
    impl RecordingStore {
        fn new() -> Self {
            Self {
                events: Arc::new(Mutex::new(Vec::new())),
                seeded_max_batch: 0,
                reads: Arc::new(AtomicU32::new(0)),
            }
        }
        /// Pre-loads a fact ONLY this test double can be asked for — production code has no way to ask
        /// for it at all (not on the `Store` trait). Used by the "stale DB row ignored" RED to prove the
        /// follower's own candidate derivation never reaches for it.
        fn preloaded(seeded_max_batch: u64) -> Self {
            Self {
                seeded_max_batch,
                ..Self::new()
            }
        }
        /// Test-only accessor, never called by `crate::follower` — proving that is the point of the
        /// "never reads the store" check (asserted via `reads()` staying 0).
        #[allow(dead_code)]
        fn seeded_max_batch(&self) -> u64 {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.seeded_max_batch
        }
        fn reads(&self) -> u32 {
            self.reads.load(Ordering::SeqCst)
        }
        fn events(&self) -> Vec<JobEvent> {
            self.events.lock().unwrap().clone()
        }
        fn statuses(&self) -> Vec<&'static str> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .map(|e| e.kind.status())
                .collect()
        }
    }
    impl Store for RecordingStore {
        async fn record(&self, event: &JobEvent) -> Result<(), StoreError> {
            self.events.lock().unwrap().push(event.clone());
            Ok(())
        }
    }

    /// Every `record` call fails — the follower's outcome must never depend
    /// on this.
    #[derive(Clone)]
    struct FailingStore {
        calls: Arc<AtomicU32>,
    }
    impl FailingStore {
        fn new() -> Self {
            Self {
                calls: Arc::new(AtomicU32::new(0)),
            }
        }
        fn calls(&self) -> u32 {
            self.calls.load(Ordering::SeqCst)
        }
    }
    impl Store for FailingStore {
        async fn record(&self, _event: &JobEvent) -> Result<(), StoreError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(StoreError("injected store failure".to_string()))
        }
    }

    // ===================== the fake chain: SnapshotFetch + AccountFetch + Sender =====================

    struct ChainState {
        settlement_program: Pubkey,
        inbox_program: Pubkey,
        authority: Pubkey,
        treasury: Pubkey,
        state: Mutex<Inner>,
    }

    /// `(last_block, state_root, status, parent_hash, last_block_hash)`.
    type PendingEntry = (u64, [u8; 32], u8, [u8; 32], [u8; 32]);

    struct Inner {
        head_pending_batch: u64,
        head_final_batch: u64,
        number: u64,
        state_root: [u8; 32],
        block_hash: [u8; 32],
        cursor_next_batch: u64,
        pending: HashMap<u64, PendingEntry>,
        inbox_batches: std::collections::HashSet<u64>, // finalized inbox batch ids present
        abandoned: std::collections::HashSet<u64>,
        registry_entry_activation_slot: u64,
        registry_retired: bool,
        verifier_head: u64,
        sent_posts: Vec<u64>, // PostRootFields.batch, in send order
        send_calls: u32,
        snapshot_calls: u32,
        /// When `> 0`, `get_multiple_accounts` reports `head_pending_batch` one behind its real value
        /// and decrements this — models a FINALIZED read genuinely lagging a send that already landed.
        /// Consumed by whichever call happens first, same as a real, unordered lag.
        stale_reads_left: u32,
        /// The root view (number, state_root, block_hash, head_final) from BEFORE the send whose
        /// landing the stale reads have not yet observed — a lagging FINALIZED read is ONE consistent
        /// older slot, never a fresh root under a stale head.
        stale_root: Option<(u64, [u8; 32], [u8; 32], u64)>,
        /// Armed by a test, consumed once: the NEXT `VerifierFetch::head` fails with a transport-level
        /// error (a torn response body), as a dropped connection to the reth-verifier would.
        verifier_head_fail_once: bool,
        /// Armed by a test, consumed once: the NEXT `PostRootProved` still commits normally (head
        /// advances for real) but returns `Custom(1)` (`BadBatchSequence`) instead of `Ok`, and arms
        /// `stale_reads_left`.
        simulate_stale_send_once: bool,
        /// How many `stale_reads_left` [`Self::simulate_stale_send_once`]'s own refusal arms — default
        /// 1 via `arm_stale_send_once`, configurable via `arm_stale_send_for` (a streak of
        /// several consecutive stale reads, not just one).
        stale_reads_after_send: u32,
        /// Armed by a test, consumed once: the NEXT `PostRootProved` is refused outright (no state
        /// change at all) with this custom error code — a genuine on-chain refusal, distinct from the
        /// stale-anchor race above.
        fail_next_send_with_custom_code: Option<u32>,
        /// A worst-case, never-converging `StaleAnchor` streak: every `PostRootProved` is refused
        /// `BadBatchSequence` with NO state change at all (so `head_pending_batch` never advances and
        /// every subsequent read keeps agreeing it is still behind `our_batch`) — the alarm's own bound
        /// test does not need the one-shot `stale_reads_left` mechanism, which converges after one retry
        /// by design.
        jam_stale_forever: bool,
        /// A per-batch `open_unix_ts` override, applied IDENTICALLY to both
        /// `SnapshotFetch`'s and `AccountFetch`'s views of the same batch id — "a chain reset in
        /// miniature." Absent (the default) means the batch's own inbox account carries the real
        /// fixture's `GATE_OPEN_UNIX_TS`, matching the one real reused proof.
        inbox_open_unix_ts_override: HashMap<u64, i64>,
        /// The fee payer's own fake lamport balance — [`SnapshotFetch::
        /// payer_lamports`]'s answer, distinct from every account read `get_multiple_accounts` serves.
        payer_lamports: u64,
    }

    #[derive(Clone)]
    struct FakeChain(Arc<ChainState>);

    impl FakeChain {
        fn new() -> Self {
            let settlement_program = Pubkey::new_unique();
            let inbox_program = Pubkey::new_unique();
            let authority = Pubkey::new_unique();
            let treasury = Pubkey::new_unique();
            FakeChain(Arc::new(ChainState {
                settlement_program,
                inbox_program,
                authority,
                treasury,
                state: Mutex::new(Inner {
                    head_pending_batch: 0,
                    head_final_batch: 0,
                    number: 0,
                    state_root: [0u8; 32],
                    block_hash: [0u8; 32],
                    cursor_next_batch: 1,
                    pending: HashMap::new(),
                    inbox_batches: std::collections::HashSet::new(),
                    abandoned: std::collections::HashSet::new(),
                    registry_entry_activation_slot: 0,
                    registry_retired: false,
                    verifier_head: u64::MAX / 2,
                    sent_posts: Vec::new(),
                    send_calls: 0,
                    snapshot_calls: 0,
                    stale_reads_left: 0,
                    stale_root: None,
                    verifier_head_fail_once: false,
                    simulate_stale_send_once: false,
                    stale_reads_after_send: 1,
                    fail_next_send_with_custom_code: None,
                    jam_stale_forever: false,
                    inbox_open_unix_ts_override: HashMap::new(),
                    payer_lamports: 4_200_000_000,
                }),
            }))
        }

        fn verifier(&self) -> FakeVerifier {
            FakeVerifier(self.0.clone())
        }

        /// The NEXT `VerifierFetch::head` fails with a transport-level error, once.
        fn arm_verifier_head_failure_once(&self) {
            self.0.state.lock().unwrap().verifier_head_fail_once = true;
        }

        /// Opens the inbox batch `batch` — from now on `fetch_and_verify_batch` sees it finalized, with
        /// one real chunk carrying one empty block numbered `batch`.
        fn open_and_finalize_inbox_batch(&self, batch: u64) {
            let mut st = self.0.state.lock().unwrap();
            st.inbox_batches.insert(batch);
            st.cursor_next_batch = st.cursor_next_batch.max(batch + 1);
        }

        /// Simulates the batcher's own `AbandonBatch` sweep: the id is neither present nor ever will
        /// be, and the cursor has already moved past it.
        fn abandon_inbox_batch(&self, batch: u64) {
            let mut st = self.0.state.lock().unwrap();
            st.inbox_batches.remove(&batch);
            st.abandoned.insert(batch);
            st.cursor_next_batch = st.cursor_next_batch.max(batch + 1);
        }

        fn set_verifier_head(&self, head: u64) {
            self.0.state.lock().unwrap().verifier_head = head;
        }

        /// Overrides `batch`'s own `open_unix_ts` — applied identically to BOTH
        /// `SnapshotFetch`'s and `AccountFetch`'s views (see `encode_inbox_batch`/
        /// `encode_inbox_batch_da_view`, which both read this SAME override) — "a chain reset in
        /// miniature": the batch id is unchanged, but its real on-chain identity is not.
        fn set_inbox_identity(&self, batch: u64, open_unix_ts: i64) {
            self.0
                .state
                .lock()
                .unwrap()
                .inbox_open_unix_ts_override
                .insert(batch, open_unix_ts);
        }

        fn sent_posts(&self) -> Vec<u64> {
            self.0.state.lock().unwrap().sent_posts.clone()
        }

        fn snapshot_calls(&self) -> u32 {
            self.0.state.lock().unwrap().snapshot_calls
        }

        /// Arms the next `PostRootProved` to land for real (the chain's own head advances) but return a
        /// `BadBatchSequence` error anyway, with the following `get_multiple_accounts` read reporting a
        /// stale (one-behind) `head_pending_batch` — the late-landing-first-submission-plus-refused-
        /// resubmit shape.
        fn arm_stale_send_once(&self) {
            self.arm_stale_send_for(1);
        }

        /// Same as [`Self::arm_stale_send_once`], but the FINALIZED read stays stale for
        /// `extra_stale_reads` MORE `get_multiple_accounts` calls after the send itself — models a
        /// lagging read that takes several poll intervals to catch up, rather than
        /// converging on the very next read.
        fn arm_stale_send_for(&self, extra_stale_reads: u32) {
            let mut st = self.0.state.lock().unwrap();
            st.simulate_stale_send_once = true;
            st.stale_reads_after_send = extra_stale_reads;
        }

        /// Arms the next `PostRootProved` to be refused outright (no state change) with `code` — a
        /// genuine on-chain refusal distinct from the stale-anchor race above.
        fn arm_failed_send_once(&self, code: u32) {
            self.0.state.lock().unwrap().fail_next_send_with_custom_code = Some(code);
        }

        /// Arms every future `PostRootProved` to be refused `BadBatchSequence` with no state change at
        /// all — see `jam_stale_forever`'s own doc.
        fn arm_jam_stale_forever(&self) {
            self.0.state.lock().unwrap().jam_stale_forever = true;
        }

        fn send_calls(&self) -> u32 {
            self.0.state.lock().unwrap().send_calls
        }

        fn head_pending_batch(&self) -> u64 {
            self.0.state.lock().unwrap().head_pending_batch
        }

        fn pending_contains(&self, batch: u64) -> bool {
            self.0.state.lock().unwrap().pending.contains_key(&batch)
        }

        // ---- account encoders ----

        /// `reported_head_pending_batch` lets a caller present a STALE `head_pending_batch` (see
        /// `stale_reads_left`) without disturbing `st`'s own, real, already-committed value.
        fn encode_root(
            &self,
            reported_head_pending_batch: u64,
            (number, state_root, block_hash, head_final_batch): (u64, [u8; 32], [u8; 32], u64),
        ) -> Vec<u8> {
            rome_zk_layouts::root::write(&rome_zk_layouts::root::RootFields {
                chain_id: GATE_CHAIN_ID,
                number,
                parent_hash: [0u8; 32],
                state_root,
                block_hash,
                updates: 0,
                profile: 0,
                challenge_window_slots: 0,
                prove_window_slots: 0,
                proving_policy: 0,
                poster_bond: 0,
                exit_cap_per_window: 0,
                authority: self.0.authority.to_bytes(),
                head_pending_batch: reported_head_pending_batch,
                head_final_batch,
                pending_count: 0,
                max_pending: 16,
            })
            .to_vec()
        }

        fn encode_registry(&self, st: &Inner) -> Vec<u8> {
            use rome_zk_layouts::registry::*;
            let vkey = tiber_vkey();
            let mut d = vec![0u8; REGISTRY_LEN_V2];
            d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
            d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].copy_from_slice(&GATE_CHAIN_ID.to_le_bytes());
            d[OFF_INBOX_PROGRAM..OFF_INBOX_PROGRAM + 32]
                .copy_from_slice(self.0.inbox_program.as_ref());
            d[OFF_COUNT] = 1;
            let e = OFF_ENTRIES;
            d[e] = CURVE_BN254;
            d[e + 1] = SCHEME_PLONK;
            d[e + 2..e + 34].copy_from_slice(&vkey.program_vk);
            d[e + 34] = LAYOUT_ZISK_V1;
            let a = OFF_ACTIVATION;
            let activation = if st.registry_retired {
                RETIRED_SLOT
            } else {
                st.registry_entry_activation_slot
            };
            d[a..a + 8].copy_from_slice(&activation.to_le_bytes());
            d
        }

        fn encode_chain_config(&self) -> Vec<u8> {
            let mut d = vec![0u8; rome_zk_layouts::chain_config::LEN_V2];
            rome_zk_layouts::chain_config::write(
                &mut d,
                &rome_zk_layouts::chain_config::ChainConfigFields {
                    chain_id: GATE_CHAIN_ID,
                    reserved: false,
                    deposit_lamports: 0,
                    deposit_refunded: false,
                    registered_slot: 0,
                    posted_batches: 0,
                    fee_base_lamports: 0,
                    fee_bps: 0,
                    max_drift_secs: Some(60),
                },
            );
            d
        }

        fn encode_global_config(&self) -> Vec<u8> {
            let mut d = vec![0u8; rome_zk_layouts::global_config::LEN];
            rome_zk_layouts::global_config::write(
                &mut d,
                &rome_zk_layouts::global_config::GlobalConfigFields {
                    registry_authority: Pubkey::new_unique().to_bytes(),
                    treasury: self.0.treasury.to_bytes(),
                    permissionless_init_enabled: false,
                    reclaim_window_slots: 0,
                    deposit_lamports: 0,
                    default_fee_base_lamports: 0,
                    default_fee_bps: 0,
                    pending_registry_authority: [0u8; 32],
                },
            );
            d
        }

        fn encode_pending(
            batch: u64,
            last_block: u64,
            state_root: [u8; 32],
            status: u8,
        ) -> Vec<u8> {
            use rome_zk_layouts::pending::*;
            let mut d = vec![0u8; PENDING_LEN];
            d[OFF_BATCH..OFF_BATCH + 8].copy_from_slice(&batch.to_le_bytes());
            d[OFF_PREV_BATCH..OFF_PREV_BATCH + 8]
                .copy_from_slice(&batch.saturating_sub(1).to_le_bytes());
            d[OFF_LAST_BLOCK..OFF_LAST_BLOCK + 8].copy_from_slice(&last_block.to_le_bytes());
            d[OFF_STATE_ROOT..OFF_STATE_ROOT + 32].copy_from_slice(&state_root);
            d[OFF_STATUS] = status;
            d
        }

        /// The one honest per-batch identity every encoder below reads: the real reset-6 batch-1 chunk
        /// bodies (see [`gate_fixture`]'s own doc) served for EVERY synthetic batch
        /// id, with `open_unix_ts` either the default (matching the one real reused proof) or a test's
        /// own [`FakeChain::set_inbox_identity`] override.
        fn open_unix_ts_for(st: &Inner, batch: u64) -> i64 {
            st.inbox_open_unix_ts_override
                .get(&batch)
                .copied()
                .unwrap_or(GATE_OPEN_UNIX_TS)
        }

        /// The inbox batch header BOTH `anchor()` (via [`SnapshotFetch`], feeding
        /// `anchor.inbox_batch_head_plus_1`) and [`Self::encode_inbox_batch_da_view`] below build from —
        /// "a fake answers ONE inbox batch PDA identically to `AccountFetch` and
        /// `SnapshotFetch`": both functions serve the SAME real chunk bodies
        /// ([`gate_fixture`]) and the SAME `open_unix_ts` (`open_unix_ts_for`). The one place they still
        /// differ is `acc` for a batch id OTHER than 1: `acc = keccak(chain_id ‖ batch ‖ open_slot ‖
        /// count ‖ root ‖ forced_root)` (`rome-zk-layouts::acc`) is bound to the batch id itself, so the
        /// SAME real content honestly reproduces the one real proof's own packaged `inbox_commitment`
        /// (`GATE_ACC_HEX`) ONLY at batch id 1 (exactly the identity that produced it) — this function
        /// hardcodes `gate_acc()` regardless of `batch` so every synthetic candidate can still post the
        /// one real, reused proof (`GATE_ACC_HEX`'s own doc), an unavoidable consequence of reusing one
        /// captured proof across many synthetic batch ids, not a fixable dishonesty: at batch id 1 this
        /// hardcode and [`Self::encode_inbox_batch_da_view`]'s own honest recomputation are PROVABLY the
        /// same value (see `the_default_batch_one_identity_is_honestly_gate_acc_hex` below).
        fn encode_inbox_batch(&self, st: &Inner, batch: u64) -> Vec<u8> {
            let fx = gate_fixture();
            let chunk_hashes: Vec<[u8; 32]> = fx
                .chunk_bodies
                .iter()
                .map(|b| rome_zk_merkle::keccak256(&[b]))
                .collect();
            let (root, forced_root, _honest_acc) = zk_inbox_client::reference_commitment(
                GATE_CHAIN_ID,
                batch,
                fx.open_slot,
                &chunk_hashes,
            );
            rome_zk_layouts::batch::write_header(&rome_zk_layouts::batch::BatchFields {
                chain_id: GATE_CHAIN_ID,
                batch,
                open_slot: fx.open_slot,
                expected_count: fx.chunk_bodies.len() as u32,
                leaves_present: fx.chunk_bodies.len() as u32,
                finalized: true,
                settlement_program: [0u8; 32],
                authority: [0u8; 32],
                root,
                forced_root,
                acc: gate_acc(),
                finalize_cursor: fx.chunk_bodies.len() as u32,
                open_unix_ts: Self::open_unix_ts_for(st, batch),
            })
            .to_vec()
        }

        /// The inbox batch header [`rome_zk_prover_input::inbox::AccountFetch::get_account`] reads,
        /// feeding `fetch_and_verify_batch`'s own independent DA-content-integrity check (it recomputes
        /// `acc`/`root`/`forced_root` from the fetched chunk bodies and compares against this header) —
        /// HONESTLY computed (never hardcoded) from the SAME real chunk bodies [`Self::encode_chunk`]
        /// serves, self-consistent by construction. See [`Self::encode_inbox_batch`]'s own doc for the
        /// one place this still differs from it (batch ids other than 1).
        fn encode_inbox_batch_da_view(&self, st: &Inner, batch: u64) -> Vec<u8> {
            let fx = gate_fixture();
            let chunk_hashes: Vec<[u8; 32]> = fx
                .chunk_bodies
                .iter()
                .map(|b| rome_zk_merkle::keccak256(&[b]))
                .collect();
            let (root, forced_root, acc) = zk_inbox_client::reference_commitment(
                GATE_CHAIN_ID,
                batch,
                fx.open_slot,
                &chunk_hashes,
            );
            rome_zk_layouts::batch::write_header(&rome_zk_layouts::batch::BatchFields {
                chain_id: GATE_CHAIN_ID,
                batch,
                open_slot: fx.open_slot,
                expected_count: fx.chunk_bodies.len() as u32,
                leaves_present: fx.chunk_bodies.len() as u32,
                finalized: true,
                settlement_program: [0u8; 32],
                authority: [0u8; 32],
                root,
                forced_root,
                acc,
                finalize_cursor: fx.chunk_bodies.len() as u32,
                open_unix_ts: Self::open_unix_ts_for(st, batch),
            })
            .to_vec()
        }

        /// The real reset-6 batch-1 chunk body at `idx`, wrapped in a real chunk header — identical for
        /// every synthetic batch id.
        fn encode_chunk(&self, batch: u64, idx: u32) -> Vec<u8> {
            let body = &gate_fixture().chunk_bodies[idx as usize];
            let header =
                rome_zk_layouts::chunk::write_header(&rome_zk_layouts::chunk::ChunkHeaderFields {
                    authority: [0u8; 32],
                    chain_id: GATE_CHAIN_ID,
                    batch,
                    idx,
                    len: body.len() as u32,
                    sealed: true,
                });
            let mut d = header.to_vec();
            d.extend_from_slice(body);
            d
        }

        fn encode_program_account(&self) -> Vec<u8> {
            let programdata = zk_settlement_client::program_data_pda(&self.0.settlement_program);
            let mut d = vec![0u8; 36];
            d[0..4].copy_from_slice(&2u32.to_le_bytes());
            d[4..36].copy_from_slice(programdata.as_ref());
            d
        }
    }

    impl SnapshotFetch for FakeChain {
        fn get_multiple_accounts(
            &mut self,
            keys: &[Pubkey],
        ) -> Result<(u64, Vec<Option<Vec<u8>>>), anchor::FetchError> {
            let mut st = self.0.state.lock().unwrap();
            st.snapshot_calls += 1;
            // A lagging FINALIZED read is ONE consistent older slot: the head, the root's own
            // number/state_root/block_hash and the pending table all come from before the send that
            // landed — the producer never answers with a fresh root under a stale head.
            let stale = st.stale_reads_left > 0;
            let reported_head_pending_batch = if stale {
                st.stale_reads_left -= 1;
                st.head_pending_batch.saturating_sub(1)
            } else {
                st.head_pending_batch
            };
            let root_view = match (stale, st.stale_root) {
                (true, Some(snap)) => snap,
                _ => (st.number, st.state_root, st.block_hash, st.head_final_batch),
            };
            if !stale {
                st.stale_root = None;
            }
            let (root_pda, _) =
                zk_settlement_client::root_pda(&self.0.settlement_program, GATE_CHAIN_ID);
            let (registry_pda, _) =
                zk_settlement_client::registry_pda(&self.0.settlement_program, GATE_CHAIN_ID);
            let (cc_pda, _) =
                zk_settlement_client::chain_config_pda(&self.0.settlement_program, GATE_CHAIN_ID);
            let (cursor_pda, _) = zk_inbox_client::cursor_pda(&self.0.inbox_program, GATE_CHAIN_ID);
            let (global_pda, _) =
                zk_settlement_client::global_config_pda(&self.0.settlement_program);
            let programdata_pda =
                zk_settlement_client::program_data_pda(&self.0.settlement_program);

            let values: Vec<Option<Vec<u8>>> = keys
                .iter()
                .map(|k| {
                    if *k == root_pda {
                        Some(self.encode_root(reported_head_pending_batch, root_view))
                    } else if *k == registry_pda {
                        Some(self.encode_registry(&st))
                    } else if *k == cc_pda {
                        Some(self.encode_chain_config())
                    } else if *k == cursor_pda {
                        Some(
                            rome_zk_testkit::cursor_account(
                                self.0.inbox_program,
                                GATE_CHAIN_ID,
                                st.cursor_next_batch,
                            )
                            .data,
                        )
                    } else if *k == global_pda {
                        Some(self.encode_global_config())
                    } else if *k == self.0.settlement_program {
                        Some(self.encode_program_account())
                    } else if *k == programdata_pda {
                        Some(vec![0u8; 1_000])
                    } else if *k == solana_system_interface::program::id() {
                        None
                    } else {
                        // Either the predecessor's or the candidate's pending PDA.
                        st.pending
                            .iter()
                            // Under a stale read, a pending PDA created by the not-yet-observed send
                            // is not visible either — same older slot.
                            .filter(|(batch, _)| !stale || **batch <= reported_head_pending_batch)
                            .find_map(
                                |(
                                    batch,
                                    (last_block, state_root, status, parent_hash, last_block_hash),
                                )| {
                                    let (pda, _) = zk_settlement_client::pending_pda(
                                        &self.0.settlement_program,
                                        GATE_CHAIN_ID,
                                        *batch,
                                    );
                                    (pda == *k).then(|| {
                                        let mut d = FakeChain::encode_pending(
                                            *batch,
                                            *last_block,
                                            *state_root,
                                            *status,
                                        );
                                        use rome_zk_layouts::pending::{
                                            OFF_LAST_BLOCK_HASH, OFF_PARENT_HASH,
                                        };
                                        d[OFF_PARENT_HASH..OFF_PARENT_HASH + 32]
                                            .copy_from_slice(parent_hash);
                                        d[OFF_LAST_BLOCK_HASH..OFF_LAST_BLOCK_HASH + 32]
                                            .copy_from_slice(last_block_hash);
                                        d
                                    })
                                },
                            )
                            .or_else(|| {
                                // The candidate's own inbox batch account.
                                (1..=200u64).find_map(|batch| {
                                    let pda = zk_settlement_client::inbox_batch_pda(
                                        &self.0.inbox_program,
                                        GATE_CHAIN_ID,
                                        batch,
                                    );
                                    (pda == *k && st.inbox_batches.contains(&batch))
                                        .then(|| self.encode_inbox_batch(&st, batch))
                                })
                            })
                    }
                })
                .collect();
            Ok((100, values))
        }

        fn payer_lamports(&mut self, _payer: &Pubkey) -> Result<u64, anchor::FetchError> {
            Ok(self.0.state.lock().unwrap().payer_lamports)
        }
    }

    impl rome_zk_prover_input::inbox::AccountFetch for FakeChain {
        fn get_account(
            &mut self,
            pubkey: &Pubkey,
        ) -> Result<Option<Vec<u8>>, rome_zk_prover_input::inbox::FetchError> {
            let st = self.0.state.lock().unwrap();
            let chunk_count = gate_fixture().chunk_bodies.len() as u32;
            Ok((1..=200u64).find_map(|batch| {
                if !st.inbox_batches.contains(&batch) {
                    return None;
                }
                let batch_pda =
                    zk_inbox_client::batch_pda(&self.0.inbox_program, GATE_CHAIN_ID, batch).0;
                if batch_pda == *pubkey {
                    return Some(self.encode_inbox_batch_da_view(&st, batch));
                }
                for idx in 0..chunk_count {
                    let chunk_pda = zk_inbox_client::chunk_pda(
                        &self.0.inbox_program,
                        GATE_CHAIN_ID,
                        batch,
                        idx,
                    )
                    .0;
                    if chunk_pda == *pubkey {
                        return Some(self.encode_chunk(batch, idx));
                    }
                }
                None
            }))
        }
    }

    /// Decodes a layout-1 proof ABI's own packaged public values — used ONLY by the fake sender below to
    /// model the program's own `AccMismatch`/`OpenTsMismatch` refusals; production code
    /// never needs this (the real program does the equivalent on chain, in Rust it doesn't share).
    fn decode_pv_from_abi(abi: &[u8]) -> rome_zk_layouts::public_values::PublicValues {
        let publics_512 = &abi[832..832 + 512];
        let bytes = rome_zk_layouts::public_values::unpack_zisk_outputs(publics_512)
            .expect("fake sender: unpack public values");
        rome_zk_layouts::public_values::read(&bytes).expect("fake sender: read public values")
    }

    fn custom_error(code: u32) -> SenderError {
        SenderError::StepFailed {
            frame_index: 0,
            stage_index: 0,
            tx_index: 0,
            err: rome_zk_solana_sender::TransactionError::InstructionError(
                0,
                rome_zk_solana_sender::InstructionError::Custom(code),
            ),
        }
    }

    impl Sender for FakeChain {
        async fn send_and_confirm(
            &self,
            instructions: &[Instruction],
            _tuning: SendTuning,
        ) -> Result<solana_signature::Signature, SenderError> {
            let ix = &instructions[0];
            let decoded = zk_settlement_client::decode_instruction(&ix.data)
                .unwrap_or_else(|e| panic!("fake sender: decode instruction: {e}"));
            let mut st = self.0.state.lock().unwrap();
            st.send_calls += 1;
            match decoded {
                zk_settlement_client::SettleIx::PostRootProved {
                    args, proof_abi, ..
                } => {
                    // Mirror `settle.rs::validate_post_root`'s own refusals: a test
                    // double that stands in for the program must refuse what the program refuses, or
                    // its own tests are decorative — a mismatched commitment is refused here, before
                    // any state change, exactly as `AccMismatch`/`OpenTsMismatch` would refuse it on
                    // chain.
                    if args.inbox_commitment != gate_acc() {
                        return Err(custom_error(6)); // AccMismatch
                    }
                    let pv = decode_pv_from_abi(&proof_abi);
                    if pv.open_unix_ts != GATE_OPEN_UNIX_TS as u64 {
                        return Err(custom_error(50)); // OpenTsMismatch
                    }
                    if let Some(code) = st.fail_next_send_with_custom_code.take() {
                        return Err(custom_error(code));
                    }
                    if st.jam_stale_forever {
                        return Err(custom_error(1)); // BadBatchSequence, no state change, forever
                    }
                    if st.simulate_stale_send_once {
                        // The stale reads that follow report the slot from BEFORE this send landed.
                        st.stale_root =
                            Some((st.number, st.state_root, st.block_hash, st.head_final_batch));
                    }
                    st.sent_posts.push(args.batch);
                    st.head_pending_batch = args.batch;
                    st.head_final_batch = args.batch; // born Final, matching the real proved path
                    st.number = args.last_block;
                    st.state_root = args.state_root;
                    st.block_hash = args.last_block_hash;
                    st.pending.insert(
                        args.batch,
                        (
                            // Every batch in these tests reuses the SAME real gate proof (its own
                            // packaged public values always commit `first_number == 1`), so the
                            // predecessor this fake presents for the NEXT candidate must always
                            // read back `last_block == 0` for continuity to hold — never the
                            // proof's own real `args.last_block` (60), which would only be right
                            // for a genuinely different proof per batch.
                            0,
                            args.state_root,
                            rome_zk_layouts::pending::STATUS_FINAL,
                            args.parent_hash,
                            args.last_block_hash,
                        ),
                    );
                    // The send actually landed (state committed, above), but this test
                    // reports it as a `BadBatchSequence` refusal anyway, arming the NEXT snapshot read
                    // to still show the OLD head — a FINALIZED read genuinely lagging a send that
                    // already confirmed.
                    if st.simulate_stale_send_once {
                        st.simulate_stale_send_once = false;
                        st.stale_reads_left = st.stale_reads_after_send;
                        return Err(custom_error(1)); // BadBatchSequence
                    }
                }
                zk_settlement_client::SettleIx::FinalizeBatch { batch, .. } => {
                    st.head_final_batch = st.head_final_batch.max(batch);
                }
                zk_settlement_client::SettleIx::ClosePending { batch, .. } => {
                    st.pending.remove(&batch);
                }
                other => panic!("fake sender: unexpected instruction {other:?}"),
            }
            Ok(solana_signature::Signature::default())
        }
    }

    fn run_config(chain: &FakeChain, work_dir: &Path) -> RunConfig {
        RunConfig {
            settlement_program: chain.0.settlement_program,
            inbox_program: chain.0.inbox_program,
            chain_id: GATE_CHAIN_ID,
            vkey: tiber_vkey(),
            elf_path: PathBuf::from("unused-in-fake-tests"),
            work_dir: work_dir.to_path_buf(),
            keep_work_dirs: 0,
            authority: chain.0.authority,
            tuning: SendTuning {
                compute_unit_limit: 700_000,
                loaded_accounts_data_size_limit: 256 * 1024,
                priority_fee_micro_lamports: 0,
                max_priority_fee_micro_lamports: 0,
                confirm_timeout: Duration::from_millis(1),
                ..Default::default()
            },
            loaded_accounts_data_size_limit: Some(1_000_000),
            finalize_walk: 8,
            close_pending_after_batches: None,
            poll_interval: Duration::from_millis(1),
            verifier_behind_alarm_polls: 3,
            stale_anchor_alarm_polls: 3,
            max_prove_attempts: 3,
            fetch_alarm_polls: 3,
            solana_rpc_url: "http://127.0.0.1:8899".to_string(),
            verifier_rpc_url: "http://127.0.0.1:8547".to_string(),
            gpu_hourly_usd: 0.0,
            stale_send_pending: std::sync::Mutex::new(None),
        }
    }

    fn temp_work_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rome-zk-prover-follower-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn noop_sleep(_d: Duration) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(async {})
    }

    // ===================== RED: batches 1..5 in order =====================

    /// Batches 1..5 are proved/posted/finalized in order — the fake sender's own
    /// recorded `PostRootFields.batch` sequence is exactly `[1,2,3,4,5]`, then the loop goes idle
    /// (cursor caught up) with zero further prove calls. Per batch, the `RecordingStore` sees
    /// `Queued -> InputBuilt -> Proving -> Proved -> Verified -> Posted -> Finalized`, in that exact order.
    #[tokio::test]
    async fn batches_1_through_5_are_proved_and_posted_in_order_then_idle() {
        let chain = FakeChain::new();
        for b in 1..=5u64 {
            chain.open_and_finalize_inbox_batch(b);
        }
        let work_dir = temp_work_dir("in-order");
        let cfg = run_config(&chain, &work_dir);
        let prove_calls = Arc::new(AtomicU32::new(0));
        let store = RecordingStore::new();
        let mut deps = Deps {
            fetch: chain.clone(),
            prover: FakeProver {
                calls: prove_calls.clone(),
            },
            sender: chain.clone(),
            verifier: chain.verifier(),
            store: store.clone(),
        };
        let metrics = Metrics::new();

        // 5 real jobs + 1 idle iteration.
        run(
            &mut deps,
            &cfg,
            RunUntil::Iterations(6),
            &NeverStop,
            &metrics,
            noop_sleep,
        )
        .await
        .expect("must not halt");

        assert_eq!(chain.sent_posts(), vec![1, 2, 3, 4, 5]);
        assert_eq!(
            prove_calls.load(Ordering::SeqCst),
            5,
            "one prove per batch, never more"
        );
        assert_eq!(chain.head_pending_batch(), 5);

        let rendered = metrics.render();
        assert!(
            rendered.contains("rome_zk_prover_posts_total{result=\"relayed\"} 5"),
            "{rendered}"
        );
        assert!(
            rendered.contains("rome_zk_prover_batches_behind 0"),
            "{rendered}"
        );
        // The payer-lamports gauge is written every iteration, not only on a real post —
        // this gauge was once registered but never written in production at all.
        assert!(
            rendered.contains("rome_zk_prover_payer_lamports 4200000000"),
            "{rendered}"
        );
        // `batch_gas_used` and `lag_seconds` are set independently from the proof's own
        // publics, not both left at whatever sentinel they started at.
        assert!(
            rendered.contains("rome_zk_prover_batch_gas_used 0"),
            "{rendered}"
        );
        assert!(
            rendered
                .lines()
                .find(|l| l.starts_with("rome_zk_prover_lag_seconds"))
                .is_some_and(|l| {
                    l.split_whitespace()
                        .next_back()
                        .and_then(|v| v.parse::<f64>().ok())
                        .is_some_and(|v| v > 0.0)
                }),
            "lag_seconds must be > 0 (open_unix_ts is in the past): {rendered}"
        );
        // With the default `keep_work_dirs = 0`, every one of the 5 posted batches' own
        // `work_dir/<batch>` is removed once it reaches a terminal outcome — the directory holds no
        // batch subdirectories at all once the run finishes.
        let remaining: Vec<_> = std::fs::read_dir(&work_dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            remaining.is_empty(),
            "work_dir must hold no batch dirs after every batch posted, got {remaining:?}"
        );

        // Per batch, the exact ordered transition sequence, five times over.
        let expected: Vec<&str> = (1..=5)
            .flat_map(|_| {
                [
                    "queued",
                    "input_built",
                    "proving",
                    "proved",
                    "verified",
                    "posted",
                    "finalized",
                ]
            })
            .collect();
        assert_eq!(
            store.statuses(),
            expected,
            "per batch: Queued -> InputBuilt -> Proving -> Proved -> Verified -> Posted -> Finalized"
        );
        // Every event belongs to its own batch, in order, all under attempt 1 (no retries in this
        // scenario).
        for (i, ev) in store.events().iter().enumerate() {
            let expected_batch = (i / 7) as u64 + 1;
            assert_eq!(ev.chain_id, cfg.chain_id);
            assert_eq!(ev.batch, expected_batch, "event {i}: {ev:?}");
            assert_eq!(ev.attempt, 1, "event {i}: {ev:?}");
        }

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    /// The candidate batch is ALWAYS derived from the finalized
    /// chain anchor, never from the store — proven through `run`'s OWN candidate derivation, never a
    /// candidate the test itself hands to `run_one` directly. This test's phase 2 once called
    /// `run_one(…, 4, …)` with the candidate hardcoded in the test, which made `reads() == 0` tautological
    /// (a mutation to `run`'s own self-correction, deriving the NEXT candidate from
    /// `store.seeded_max_batch() + 1` rather than `head_pending_batch + 1`, would never even run — that
    /// line lives inside `run`'s loop, never reached by a direct `run_one` call). Phase 2 here goes
    /// through `run` instead: a fresh follower run (as a genuine restart would be) always guesses
    /// candidate 1 first ("the chain is the cursor", never persisted state) — its first iteration finds
    /// the real chain already at head 3 (`Superseded`) and self-corrects to 4 via the exact line the
    /// mutation targets; its second iteration proves and posts the real next batch, 4. A `RecordingStore`
    /// PRE-LOADED with a fact only IT can be asked for ("batch 7 posted") is never asked for it — the
    /// cumulative `sent_posts` end at `[1, 2, 3, 4]`, and `store.reads()` stays 0 throughout. Mutation
    /// target: `next_candidate = store.seeded_max_batch() + 1` on the `Superseded` arm — the self-correct
    /// would then aim at candidate 8, the second iteration would ALSO see `Superseded` (8 versus the real
    /// head 3) instead of posting, and `sent_posts` would stop at `[1, 2, 3]`.
    #[tokio::test]
    async fn a_stale_db_row_ahead_of_the_chain_is_ignored() {
        let chain = FakeChain::new();
        for b in 1..=4u64 {
            chain.open_and_finalize_inbox_batch(b);
        }
        let work_dir = temp_work_dir("stale-db-row-ignored");
        let cfg = run_config(&chain, &work_dir);

        // Phase 1: get the chain to a real head of 3 (batches 1..3 genuinely posted, predecessor state
        // and all) via the real pipeline, using an ordinary NoopStore — the point of this test is what
        // happens NEXT, with a store that lies.
        {
            let mut deps = Deps {
                fetch: chain.clone(),
                prover: FakeProver {
                    calls: Arc::new(AtomicU32::new(0)),
                },
                sender: chain.clone(),
                verifier: chain.verifier(),
                store: NoopStore,
            };
            let metrics = Metrics::new();
            run(
                &mut deps,
                &cfg,
                RunUntil::Iterations(3),
                &NeverStop,
                &metrics,
                noop_sleep,
            )
            .await
            .expect("must not halt");
        }
        assert_eq!(
            chain.head_pending_batch(),
            3,
            "real head is 3 after 3 real posts"
        );

        // Phase 2: a fresh `run`, preloaded with a stale claim that batch 7 is already posted. Two
        // iterations: the first self-corrects (candidate 1 vs the real head 3, `Superseded`), the second
        // proves and posts the real next batch, 4 — both through `run`'s own loop, never a candidate this
        // test supplies.
        let store = RecordingStore::preloaded(7);
        let mut deps2 = Deps {
            fetch: chain.clone(),
            prover: FakeProver {
                calls: Arc::new(AtomicU32::new(0)),
            },
            sender: chain.clone(),
            verifier: chain.verifier(),
            store,
        };
        let metrics = Metrics::new();
        run(
            &mut deps2,
            &cfg,
            RunUntil::Iterations(2),
            &NeverStop,
            &metrics,
            noop_sleep,
        )
        .await
        .expect("must not halt");

        assert_eq!(
            chain.sent_posts(),
            vec![1, 2, 3, 4],
            "the chain's own head decides the candidate, never the store"
        );
        assert_eq!(
            deps2.store.reads(),
            0,
            "the follower must never read the store back, even when it holds a stale fact"
        );

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    /// A store whose EVERY `record` call fails never stops the follower from
    /// proving — the same 1..5 scenario still posts all five batches, `run` still returns `Ok`, and
    /// `rome_zk_prover_store_errors_total` counts exactly the number of failed `record` calls (one per
    /// transition: 7 per batch x 5 batches = 35). Mutation target: propagate the `record` error with `?`
    /// in the `record` helper instead of logging + counting it — this test's own `.expect("must not
    /// halt")` goes red (the run halts on the very first transition instead).
    #[tokio::test]
    async fn a_failing_store_never_stops_the_follower_from_proving() {
        let chain = FakeChain::new();
        for b in 1..=5u64 {
            chain.open_and_finalize_inbox_batch(b);
        }
        let work_dir = temp_work_dir("failing-store");
        let cfg = run_config(&chain, &work_dir);
        let store = FailingStore::new();
        let mut deps = Deps {
            fetch: chain.clone(),
            prover: FakeProver {
                calls: Arc::new(AtomicU32::new(0)),
            },
            sender: chain.clone(),
            verifier: chain.verifier(),
            store: store.clone(),
        };
        let metrics = Metrics::new();

        run(
            &mut deps,
            &cfg,
            RunUntil::Iterations(6),
            &NeverStop,
            &metrics,
            noop_sleep,
        )
        .await
        .expect("a failing store must never halt the follower");

        assert_eq!(
            chain.sent_posts(),
            vec![1, 2, 3, 4, 5],
            "every batch still posts"
        );
        assert!(store.calls() > 0, "the failing store was actually called");
        let rendered = metrics.render();
        assert!(
            rendered.contains(&format!(
                "rome_zk_prover_store_errors_total {}",
                store.calls()
            )),
            "every failed record call is counted: {rendered}"
        );

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    // ===================== RED: abandoned batch halts =====================

    /// Batches 1 and 2 proved/posted; batch 3 is abandoned (absent, cursor past it) —
    /// the loop halts with `AbandonedInboxBatch { batch: 3 }` and zero further sends.
    #[tokio::test]
    async fn an_abandoned_batch_halts_after_the_ones_before_it_with_zero_further_sends() {
        let chain = FakeChain::new();
        chain.open_and_finalize_inbox_batch(1);
        chain.open_and_finalize_inbox_batch(2);
        chain.abandon_inbox_batch(3);
        let work_dir = temp_work_dir("abandoned");
        let cfg = run_config(&chain, &work_dir);
        let mut deps = Deps {
            fetch: chain.clone(),
            prover: FakeProver {
                calls: Arc::new(AtomicU32::new(0)),
            },
            sender: chain.clone(),
            verifier: chain.verifier(),
            store: NoopStore,
        };
        let metrics = Metrics::new();

        let err = run(
            &mut deps,
            &cfg,
            RunUntil::Iterations(10),
            &NeverStop,
            &metrics,
            noop_sleep,
        )
        .await
        .expect_err("must halt on the abandoned batch");

        assert!(
            matches!(err, FollowerError::AbandonedInboxBatch { batch: 3 }),
            "got {err:?}"
        );
        assert_eq!(chain.sent_posts(), vec![1, 2]);
        assert_eq!(chain.send_calls(), 2, "zero further sends after the halt");
        // `run`'s own Err path sets the state gauge to "failed" before returning — a
        // dashboard reading only the state gauge must be able to see a halted follower, not whatever
        // state it happened to be in mid-job.
        assert!(
            metrics
                .render()
                .contains("rome_zk_prover_state{state=\"failed\"} 1"),
            "{}",
            metrics.render()
        );

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    // ===================== RED: idle — zero prove calls (mutation-tested) =====================

    /// `cursor_next_batch - 1 == head_pending_batch` (nothing opened
    /// yet) → `Idle` every iteration, zero `Prover::prove` calls over 10 iterations. Removing the idle
    /// check (always falling through to `InboxNotFinalizedYet`'s busy-retry path) would still avoid a
    /// prove call here too — so this test's own mutation target is `prepare_checked_proof`'s idle
    /// branch specifically: it is pinned by asserting `Outcome::Idle` is what
    /// `metrics.set_state("idle")` was actually driven by (checked via the rendered state gauge),
    /// not merely that no batch was posted.
    #[tokio::test]
    async fn idle_when_nothing_is_behind_makes_zero_prove_calls_over_ten_iterations() {
        let chain = FakeChain::new(); // no batch ever opened; cursor_next_batch stays 1
        let work_dir = temp_work_dir("idle");
        let cfg = run_config(&chain, &work_dir);
        let prove_calls = Arc::new(AtomicU32::new(0));
        let mut deps = Deps {
            fetch: chain.clone(),
            prover: FakeProver {
                calls: prove_calls.clone(),
            },
            sender: chain.clone(),
            verifier: chain.verifier(),
            store: NoopStore,
        };
        let metrics = Metrics::new();

        run(
            &mut deps,
            &cfg,
            RunUntil::Iterations(10),
            &NeverStop,
            &metrics,
            noop_sleep,
        )
        .await
        .expect("idle must never halt");

        assert_eq!(prove_calls.load(Ordering::SeqCst), 0);
        let rendered = metrics.render();
        assert!(
            rendered.contains("rome_zk_prover_state{state=\"idle\"} 1"),
            "{rendered}"
        );
        assert!(
            rendered.contains("rome_zk_prover_batches_behind 0"),
            "{rendered}"
        );

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    /// Over 10 idle iterations, the fake chain's own `get_multiple_accounts` is called
    /// exactly once per iteration — proves the idle path never makes a redundant/duplicate snapshot
    /// read on top of the one `anchor()` itself always makes.
    #[tokio::test]
    async fn idle_iterations_make_exactly_one_snapshot_read_each() {
        let chain = FakeChain::new(); // no batch ever opened; cursor_next_batch stays 1
        let work_dir = temp_work_dir("idle-snapshot-count");
        let cfg = run_config(&chain, &work_dir);
        let mut deps = Deps {
            fetch: chain.clone(),
            prover: FakeProver {
                calls: Arc::new(AtomicU32::new(0)),
            },
            sender: chain.clone(),
            verifier: chain.verifier(),
            store: NoopStore,
        };
        let metrics = Metrics::new();

        run(
            &mut deps,
            &cfg,
            RunUntil::Iterations(10),
            &NeverStop,
            &metrics,
            noop_sleep,
        )
        .await
        .expect("idle must never halt");

        assert_eq!(
            chain.snapshot_calls(),
            10,
            "exactly one get_multiple_accounts call per idle iteration"
        );

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    // ===================== RED: transient fetch error retries =====================

    /// Wraps a [`FakeChain`] and errors its very first [`SnapshotFetch::get_multiple_accounts`] call,
    /// then delegates normally forever after — models a transient RPC failure a real `RpcFetch` maps
    /// into `AnchorError::Fetch`.
    struct FlakyOnceFetch {
        inner: FakeChain,
        errored: Arc<std::sync::atomic::AtomicBool>,
    }
    impl SnapshotFetch for FlakyOnceFetch {
        fn get_multiple_accounts(
            &mut self,
            keys: &[Pubkey],
        ) -> Result<(u64, Vec<Option<Vec<u8>>>), anchor::FetchError> {
            if !self.errored.swap(true, Ordering::SeqCst) {
                return Err(anchor::FetchError(
                    "simulated transient RPC error".to_string(),
                ));
            }
            self.inner.get_multiple_accounts(keys)
        }
    }
    impl rome_zk_prover_input::inbox::AccountFetch for FlakyOnceFetch {
        fn get_account(
            &mut self,
            pubkey: &Pubkey,
        ) -> Result<Option<Vec<u8>>, rome_zk_prover_input::inbox::FetchError> {
            self.inner.get_account(pubkey)
        }
    }

    /// A [`SnapshotFetch`] wrapper that fails the NEXT `get_multiple_accounts` call exactly once
    /// whenever armed (`fail_next.store(true, ..)`), then clears itself and delegates — lets a test
    /// target a SPECIFIC re-anchor call (e.g. `post_checked`'s own `anchor2`) rather than only the
    /// first snapshot read of a run.
    #[derive(Clone)]
    struct ArmedFlakyFetch {
        inner: FakeChain,
        fail_next: Arc<std::sync::atomic::AtomicBool>,
    }
    impl SnapshotFetch for ArmedFlakyFetch {
        fn get_multiple_accounts(
            &mut self,
            keys: &[Pubkey],
        ) -> Result<(u64, Vec<Option<Vec<u8>>>), anchor::FetchError> {
            if self.fail_next.swap(false, Ordering::SeqCst) {
                return Err(anchor::FetchError(
                    "simulated transient RPC error".to_string(),
                ));
            }
            self.inner.get_multiple_accounts(keys)
        }
    }
    impl rome_zk_prover_input::inbox::AccountFetch for ArmedFlakyFetch {
        fn get_account(
            &mut self,
            pubkey: &Pubkey,
        ) -> Result<Option<Vec<u8>>, rome_zk_prover_input::inbox::FetchError> {
            self.inner.get_account(pubkey)
        }
    }

    /// An [`rome_zk_prover_input::inbox::AccountFetch`] wrapper that fails the NEXT `get_account` call
    /// exactly once whenever armed, then clears and delegates — models a transient RPC failure on the
    /// chunk/batch-account read path specifically.
    #[derive(Clone)]
    struct ArmedFlakyAccountFetch {
        inner: FakeChain,
        fail_next: Arc<std::sync::atomic::AtomicBool>,
    }
    impl SnapshotFetch for ArmedFlakyAccountFetch {
        fn get_multiple_accounts(
            &mut self,
            keys: &[Pubkey],
        ) -> Result<(u64, Vec<Option<Vec<u8>>>), anchor::FetchError> {
            self.inner.get_multiple_accounts(keys)
        }
    }
    impl rome_zk_prover_input::inbox::AccountFetch for ArmedFlakyAccountFetch {
        fn get_account(
            &mut self,
            pubkey: &Pubkey,
        ) -> Result<Option<Vec<u8>>, rome_zk_prover_input::inbox::FetchError> {
            if self.fail_next.swap(false, Ordering::SeqCst) {
                return Err(rome_zk_prover_input::inbox::FetchError(
                    "simulated transient RPC error".to_string(),
                ));
            }
            self.inner.get_account(pubkey)
        }
    }

    /// An [`rome_zk_prover_input::inbox::AccountFetch`] wrapper that ALWAYS fails — models a
    /// `TransientFetchError` streak that never recovers, for the fetch-alarm bound.
    #[derive(Clone)]
    struct AlwaysErrorsAccountFetch {
        inner: FakeChain,
    }
    impl SnapshotFetch for AlwaysErrorsAccountFetch {
        fn get_multiple_accounts(
            &mut self,
            keys: &[Pubkey],
        ) -> Result<(u64, Vec<Option<Vec<u8>>>), anchor::FetchError> {
            self.inner.get_multiple_accounts(keys)
        }
    }
    impl rome_zk_prover_input::inbox::AccountFetch for AlwaysErrorsAccountFetch {
        fn get_account(
            &mut self,
            _pubkey: &Pubkey,
        ) -> Result<Option<Vec<u8>>, rome_zk_prover_input::inbox::FetchError> {
            Err(rome_zk_prover_input::inbox::FetchError(
                "simulated permanent RPC error".to_string(),
            ))
        }
    }

    /// A transient snapshot-fetch failure retries the same candidate rather than halting
    /// or panicking — the loop simply proceeds once the next call succeeds.
    #[tokio::test]
    async fn a_transient_fetch_error_retries_rather_than_halting_or_panicking() {
        let chain = FakeChain::new();
        chain.open_and_finalize_inbox_batch(1);
        let work_dir = temp_work_dir("flaky-fetch");
        let cfg = run_config(&chain, &work_dir);
        let mut deps = Deps {
            fetch: FlakyOnceFetch {
                inner: chain.clone(),
                errored: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
            prover: FakeProver {
                calls: Arc::new(AtomicU32::new(0)),
            },
            sender: chain.clone(),
            verifier: chain.verifier(),
            store: NoopStore,
        };
        let metrics = Metrics::new();

        run(
            &mut deps,
            &cfg,
            RunUntil::Iterations(2),
            &NeverStop,
            &metrics,
            noop_sleep,
        )
        .await
        .expect("a transient fetch error must retry, never halt or panic");

        assert_eq!(chain.sent_posts(), vec![1]);

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    /// A transport-level failure on the reth-verifier read (a torn HTTP body here; a dropped connection
    /// or a timeout in production) retries under the same fetch alarm bound as a Solana-side read and
    /// never halts the loop. A JSON-RPC error or a decode failure is still a halt: that is a fact about
    /// the data, not the wire.
    #[tokio::test]
    async fn a_transient_verifier_transport_error_retries_then_posts() {
        let chain = FakeChain::new();
        chain.open_and_finalize_inbox_batch(1);
        chain.arm_verifier_head_failure_once();
        let work_dir = temp_work_dir("flaky-verifier");
        let cfg = run_config(&chain, &work_dir);
        let mut deps = Deps {
            fetch: chain.clone(),
            prover: FakeProver {
                calls: Arc::new(AtomicU32::new(0)),
            },
            sender: chain.clone(),
            verifier: chain.verifier(),
            store: NoopStore,
        };
        let metrics = Metrics::new();

        run(
            &mut deps,
            &cfg,
            RunUntil::Iterations(2),
            &NeverStop,
            &metrics,
            noop_sleep,
        )
        .await
        .expect("a transient verifier transport error must retry, never halt");

        assert_eq!(chain.sent_posts(), vec![1]);

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    // ===================== RED: a genuine failed send halts by name =====================

    /// A genuine on-chain refusal (not `AlreadyPosted`/`HeadAhead`/`StaleAnchor`/
    /// `PayerLow`) halts the loop by name, with zero further sends.
    #[tokio::test]
    async fn a_genuine_failed_send_halts_by_name_with_zero_further_sends() {
        let chain = FakeChain::new();
        chain.open_and_finalize_inbox_batch(1);
        chain.arm_failed_send_once(23); // StateRootMismatch — a real, non-racy on-chain refusal
        let work_dir = temp_work_dir("failed-send");
        let cfg = run_config(&chain, &work_dir);
        let mut deps = Deps {
            fetch: chain.clone(),
            prover: FakeProver {
                calls: Arc::new(AtomicU32::new(0)),
            },
            sender: chain.clone(),
            verifier: chain.verifier(),
            store: NoopStore,
        };
        let metrics = Metrics::new();

        let err = run(
            &mut deps,
            &cfg,
            RunUntil::Iterations(10),
            &NeverStop,
            &metrics,
            noop_sleep,
        )
        .await
        .expect_err("a genuine on-chain refusal must halt");
        assert!(
            matches!(
                err,
                FollowerError::PostRefused(PostRefusal::PostFailed { ref name }) if name == "StateRootMismatch"
            ),
            "got {err:?}"
        );
        assert!(
            chain.sent_posts().is_empty(),
            "the refused batch is never recorded as posted"
        );
        assert_eq!(chain.send_calls(), 1, "zero further sends after the halt");

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    // ===================== RED: close_pending_after_batches = Some(k) =====================

    /// With `close_pending_after_batches = Some(1)`, once batch 3 posts, batch 1's
    /// pending PDA (strictly behind `head_pending_batch - 1 = 2`, and `STATUS_FINAL`) is closed via
    /// `ClosePending` — proving the follower loop's own `finalize_and_close` actually wires the
    /// retention gate through, end to end, not merely that `finality::should_close` computes the right
    /// bool in isolation (already covered directly in `finality.rs`).
    #[tokio::test]
    async fn close_pending_after_batches_some_k_closes_the_batch_strictly_behind_it() {
        let chain = FakeChain::new();
        for b in 1..=3u64 {
            chain.open_and_finalize_inbox_batch(b);
        }
        let work_dir = temp_work_dir("close-some-k");
        let mut cfg = run_config(&chain, &work_dir);
        cfg.close_pending_after_batches = Some(1);
        let mut deps = Deps {
            fetch: chain.clone(),
            prover: FakeProver {
                calls: Arc::new(AtomicU32::new(0)),
            },
            sender: chain.clone(),
            verifier: chain.verifier(),
            store: NoopStore,
        };
        let metrics = Metrics::new();

        // 3 real jobs + 1 idle iteration.
        run(
            &mut deps,
            &cfg,
            RunUntil::Iterations(4),
            &NeverStop,
            &metrics,
            noop_sleep,
        )
        .await
        .expect("must not halt");

        assert_eq!(chain.sent_posts(), vec![1, 2, 3]);
        assert!(
            !chain.pending_contains(1),
            "batch 1 is strictly behind head_pending_batch(3) - k(1) = 2, and STATUS_FINAL — it must \
             have been closed"
        );
        assert!(
            chain.pending_contains(2),
            "batch 2 == the threshold, not strictly behind it — must stay open"
        );
        assert!(
            chain.pending_contains(3),
            "batch 3 is the head — the program refuses closing it regardless"
        );

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    // ===================== RED: HeadAhead between verify and post -> Superseded =====================

    /// The chain head moves past our candidate WHILE `prove()` is "running" (simulated
    /// by advancing the fake chain's `head_pending_batch` directly between `prepare_checked_proof` and
    /// `post_checked`) — the re-anchor before post sees `HeadAhead` and the job is `Superseded`, zero
    /// sends for it, and the loop continues at the new head.
    #[tokio::test]
    async fn head_ahead_between_verify_and_post_is_superseded_with_zero_sends_for_that_job() {
        let chain = FakeChain::new();
        chain.open_and_finalize_inbox_batch(1);
        let work_dir = temp_work_dir("supersede");
        let cfg = run_config(&chain, &work_dir);
        let prove_calls = Arc::new(AtomicU32::new(0));
        let mut fetch = chain.clone();
        let prover = FakeProver {
            calls: prove_calls.clone(),
        };
        let mut verifier = chain.verifier();
        let store = NoopStore;
        let metrics = Metrics::new();

        let (_anchor1, checked, _dir, attempt, cost) = prepare_checked_proof(
            &mut fetch,
            &prover,
            &mut verifier,
            &store,
            &cfg,
            1,
            &metrics,
        )
        .await
        .expect("must not halt")
        .expect("must reach LocallyVerified");

        // Simulate "someone else posted batches 1 AND 2 while we were proving" directly on the fake
        // chain — the head must move STRICTLY PAST our own candidate (1) for this to be `Superseded`
        // rather than `AlreadyPosted` (which is what `head_pending_batch == candidate_batch` means).
        {
            let mut st = chain.0.state.lock().unwrap();
            st.head_pending_batch = 2;
            st.head_final_batch = 2;
            st.sent_posts.push(1);
            st.sent_posts.push(2);
            st.send_calls += 2;
        }

        let outcome = post_checked(
            &mut fetch, &chain, &store, &cfg, 1, &checked, attempt, cost, &metrics,
        )
        .run()
        .await
        .expect("must not halt");
        assert_eq!(
            outcome,
            Outcome::Superseded {
                batch: 1,
                head_pending_batch: 2
            }
        );
        assert_eq!(
            chain.send_calls(),
            2,
            "only the simulated other posts, zero of our own"
        );

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    // ===================== RED: VerifierBehind retried, then proceeds =====================

    #[tokio::test]
    async fn verifier_behind_is_retried_then_proceeds_once_the_head_advances() {
        let chain = FakeChain::new();
        chain.open_and_finalize_inbox_batch(1);
        chain.set_verifier_head(0); // behind batch 1's own block
        let work_dir = temp_work_dir("verifier-behind");
        let mut cfg = run_config(&chain, &work_dir);
        cfg.verifier_behind_alarm_polls = 100; // never fire the alarm in this test
        let prove_calls = Arc::new(AtomicU32::new(0));
        let mut deps = Deps {
            fetch: chain.clone(),
            prover: FakeProver {
                calls: prove_calls.clone(),
            },
            sender: chain.clone(),
            verifier: chain.verifier(),
            store: NoopStore,
        };
        let metrics = Metrics::new();

        // First iteration: VerifierBehind retry, zero prove calls.
        run(
            &mut deps,
            &cfg,
            RunUntil::Iterations(1),
            &NeverStop,
            &metrics,
            noop_sleep,
        )
        .await
        .unwrap();
        assert_eq!(prove_calls.load(Ordering::SeqCst), 0);
        assert!(chain.sent_posts().is_empty());

        // The verifier catches up; the SAME candidate now proceeds.
        chain.set_verifier_head(u64::MAX / 2);
        run(
            &mut deps,
            &cfg,
            RunUntil::Iterations(1),
            &NeverStop,
            &metrics,
            noop_sleep,
        )
        .await
        .unwrap();
        assert_eq!(chain.sent_posts(), vec![1]);
        assert_eq!(prove_calls.load(Ordering::SeqCst), 1);

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    /// RED: a `VerifierBehind` streak that never clears fires the alarm after exactly
    /// `verifier_behind_alarm_polls` consecutive retries.
    #[tokio::test]
    async fn verifier_behind_alarms_after_the_configured_consecutive_poll_count() {
        let chain = FakeChain::new();
        chain.open_and_finalize_inbox_batch(1);
        chain.set_verifier_head(0);
        let work_dir = temp_work_dir("verifier-alarm");
        let mut cfg = run_config(&chain, &work_dir);
        cfg.verifier_behind_alarm_polls = 3;
        let mut deps = Deps {
            fetch: chain.clone(),
            prover: FakeProver {
                calls: Arc::new(AtomicU32::new(0)),
            },
            sender: chain.clone(),
            verifier: chain.verifier(),
            store: NoopStore,
        };
        let metrics = Metrics::new();

        let err = run(
            &mut deps,
            &cfg,
            RunUntil::Iterations(10),
            &NeverStop,
            &metrics,
            noop_sleep,
        )
        .await
        .expect_err("must alarm");
        assert!(
            matches!(err, FollowerError::VerifierBehindAlarm { polls: 3, .. }),
            "got {err:?}"
        );

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    // ===================== RED: batches_behind gauge in a scripted scenario =====================

    #[tokio::test]
    async fn batches_behind_gauge_equals_cursor_minus_one_minus_head() {
        let chain = FakeChain::new();
        chain.open_and_finalize_inbox_batch(1);
        chain.open_and_finalize_inbox_batch(2);
        chain.open_and_finalize_inbox_batch(3);
        let work_dir = temp_work_dir("gauge");
        let cfg = run_config(&chain, &work_dir);
        let mut deps = Deps {
            fetch: chain.clone(),
            prover: FakeProver {
                calls: Arc::new(AtomicU32::new(0)),
            },
            sender: chain.clone(),
            verifier: chain.verifier(),
            store: NoopStore,
        };
        let metrics = Metrics::new();

        // Before any work: cursor_next_batch=4, head_pending_batch=0 -> batches_behind=3.
        run(
            &mut deps,
            &cfg,
            RunUntil::Iterations(1),
            &NeverStop,
            &metrics,
            noop_sleep,
        )
        .await
        .unwrap();
        assert!(
            metrics.render().contains("rome_zk_prover_batches_behind 3"),
            "{}",
            metrics.render()
        );

        // After posting batch 1: cursor=4, head_pending=1 -> batches_behind=2, observed at the START
        // of the NEXT iteration's anchor. A fresh `run()` call always starts its own candidate guess
        // at 1 (it does not persist across separate calls, only within one) — its first iteration
        // here re-discovers batch 1 as `AlreadyPosted` (self-correcting to candidate 2, zero sends),
        // its second does batch 2's real work, so this call needs two iterations, not one.
        run(
            &mut deps,
            &cfg,
            RunUntil::Iterations(2),
            &NeverStop,
            &metrics,
            noop_sleep,
        )
        .await
        .unwrap();
        assert!(
            metrics.render().contains("rome_zk_prover_batches_behind 2"),
            "{}",
            metrics.render()
        );

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    // ===================== RED: preemption-safe resume =====================

    /// A restart between `LocallyVerified` and `Posting` — calling
    /// `prepare_checked_proof` a SECOND time (simulating the process restarting before it ever sent)
    /// must NOT call `Prover::prove` again (the on-disk artefacts are reused), and the eventual post
    /// happens exactly once.
    #[tokio::test]
    async fn restart_between_locally_verified_and_posting_proves_once_and_posts_once() {
        let chain = FakeChain::new();
        chain.open_and_finalize_inbox_batch(1);
        let work_dir = temp_work_dir("resume-before-post");
        let cfg = run_config(&chain, &work_dir);
        let prove_calls = Arc::new(AtomicU32::new(0));
        let prover = FakeProver {
            calls: prove_calls.clone(),
        };
        let metrics = Metrics::new();

        // First "life": reach LocallyVerified, then crash (never posts).
        let mut fetch1 = chain.clone();
        let mut verifier1 = chain.verifier();
        let store1 = NoopStore;
        let (_anchor1, _checked1, _dir1, _attempt1, _cost1) = prepare_checked_proof(
            &mut fetch1,
            &prover,
            &mut verifier1,
            &store1,
            &cfg,
            1,
            &metrics,
        )
        .await
        .expect("must not halt")
        .expect("must reach LocallyVerified");
        assert_eq!(prove_calls.load(Ordering::SeqCst), 1);

        // "Restart": a fresh run_one call for the SAME candidate, same work_dir, same on-chain state.
        let mut fetch2 = chain.clone();
        let sender2 = chain.clone();
        let mut verifier2 = chain.verifier();
        let store2 = NoopStore;
        let outcome = run_one(
            &mut fetch2,
            &prover,
            &sender2,
            &mut verifier2,
            &store2,
            &cfg,
            1,
            &metrics,
        )
        .await
        .expect("must not halt");
        assert_eq!(
            outcome,
            Outcome::Posted {
                batch: 1,
                sig: solana_signature::Signature::default().to_string()
            }
        );
        assert_eq!(
            prove_calls.load(Ordering::SeqCst),
            1,
            "the resumed run must NOT call Prover::prove a second time"
        );
        assert_eq!(chain.send_calls(), 1, "exactly one post total");

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    /// A restart AFTER `Relayed` (the chain head already advanced) — the next `anchor()`
    /// for the same candidate sees the job already `Final`, so `run_one` returns `AlreadyPosted` with
    /// zero further sends.
    #[tokio::test]
    async fn restart_after_relayed_makes_no_second_post() {
        let chain = FakeChain::new();
        chain.open_and_finalize_inbox_batch(1);
        let work_dir = temp_work_dir("resume-after-relayed");
        let cfg = run_config(&chain, &work_dir);
        let prover = FakeProver {
            calls: Arc::new(AtomicU32::new(0)),
        };
        let metrics = Metrics::new();

        let mut fetch = chain.clone();
        let sender = chain.clone();
        let mut verifier = chain.verifier();
        let store = NoopStore;
        let first = run_one(
            &mut fetch,
            &prover,
            &sender,
            &mut verifier,
            &store,
            &cfg,
            1,
            &metrics,
        )
        .await
        .expect("must not halt");
        assert!(matches!(first, Outcome::Posted { batch: 1, .. }));
        assert_eq!(chain.send_calls(), 1);

        // "Restart": run_one AGAIN for the same candidate (batch 1) after the chain already advanced.
        let second = run_one(
            &mut fetch,
            &prover,
            &sender,
            &mut verifier,
            &store,
            &cfg,
            1,
            &metrics,
        )
        .await
        .expect("must not halt");
        assert_eq!(second, Outcome::AlreadyPosted { batch: 1 });
        assert_eq!(chain.send_calls(), 1, "zero further sends");

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    /// The sidecar's own `elf_sha256` disagreeing with the vkey of
    /// record wipes the work directory and proves again — never resumes a mismatched-ELF artefact.
    /// **Mutation: drop the sha comparison from `resumable`** (resume whenever the files merely exist)
    /// → this test's own `prove_calls == 2` assertion goes red (it would stay 1).
    #[tokio::test]
    async fn an_elf_sha_mismatch_in_the_sidecar_wipes_the_dir_and_proves_again() {
        let chain = FakeChain::new();
        chain.open_and_finalize_inbox_batch(1);
        let work_dir = temp_work_dir("resume-sha-mismatch");
        let cfg = run_config(&chain, &work_dir);
        let prove_calls = Arc::new(AtomicU32::new(0));
        let prover = FakeProver {
            calls: prove_calls.clone(),
        };
        let metrics = Metrics::new();

        let mut fetch = chain.clone();
        let mut verifier = chain.verifier();
        let store = NoopStore;
        let (_anchor1, _checked, batch_dir, _attempt, _cost) = prepare_checked_proof(
            &mut fetch,
            &prover,
            &mut verifier,
            &store,
            &cfg,
            1,
            &metrics,
        )
        .await
        .expect("must not halt")
        .expect("must reach LocallyVerified");
        assert_eq!(prove_calls.load(Ordering::SeqCst), 1);

        // Tamper ONLY the sidecar's own recorded ELF sha — a different ELF's leftover artefact — by
        // editing the real, freshly-written full Sidecar in place, so every OTHER field (chain_id,
        // inbox_commitment, open_unix_ts, the expected publics) stays genuinely correct. This isolates
        // the sha comparison specifically: dropping it from `try_resume` would let this test wrongly
        // resume, since nothing else here is wrong.
        let sidecar_path = batch_dir.join("sidecar.json");
        let mut sidecar_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&sidecar_path).unwrap()).unwrap();
        sidecar_json["provenance"]["elf_sha256"] =
            serde_json::json!(format!("0x{}", "ff".repeat(32)));
        std::fs::write(&sidecar_path, sidecar_json.to_string()).unwrap();

        let (_anchor2, _checked2, _dir2, _attempt2, _cost2) = prepare_checked_proof(
            &mut fetch,
            &prover,
            &mut verifier,
            &store,
            &cfg,
            1,
            &metrics,
        )
        .await
        .expect("must not halt")
        .expect("must reach LocallyVerified again");
        assert_eq!(
            prove_calls.load(Ordering::SeqCst),
            2,
            "an ELF sha mismatch must wipe the dir and prove again"
        );

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    /// The resume gate's `acc` and `chain_id` comparisons, pinned on their own. The fake chain cannot
    /// present a different `acc` for a batch honestly (the on-chain accumulator binds the batch id, and
    /// every test reuses the one real proof), so the fresh anchor's batch account is hand-built here:
    /// sidecar and proof agree with each other in every field, only the anchor differs — the ONLY thing
    /// standing between a cached artefact of another chain reset and a resume is this comparison.
    #[tokio::test]
    async fn a_fresh_anchors_changed_acc_alone_refuses_the_cached_artefact() {
        let chain = FakeChain::new();
        chain.open_and_finalize_inbox_batch(1);
        let work_dir = temp_work_dir("resume-acc-only");
        let cfg = run_config(&chain, &work_dir);
        let prove_calls = Arc::new(AtomicU32::new(0));
        let prover = FakeProver {
            calls: prove_calls.clone(),
        };
        let metrics = Metrics::new();
        let mut fetch = chain.clone();
        let mut verifier = chain.verifier();
        let store = NoopStore;
        let (anchor1, _checked, _batch_dir, _attempt, _cost) = prepare_checked_proof(
            &mut fetch,
            &prover,
            &mut verifier,
            &store,
            &cfg,
            1,
            &metrics,
        )
        .await
        .expect("must not halt")
        .expect("must reach LocallyVerified");
        assert_eq!(prove_calls.load(Ordering::SeqCst), 1);
        let (_dir, input_path, sidecar_path, proof_path) = batch_paths(&cfg.work_dir, 1);

        // Control: the genuine anchor resumes the artefact it produced.
        assert!(
            try_resume(
                &input_path,
                &sidecar_path,
                &proof_path,
                &cfg.vkey,
                cfg.chain_id,
                &anchor1.inbox_batch_head_plus_1,
            )
            .is_some(),
            "the control must resume, or the refusals below prove nothing"
        );

        // Only the anchor's `acc` differs (a chain reset re-opened batch 1 with other content).
        let mut other_acc = anchor1.inbox_batch_head_plus_1.clone();
        other_acc.acc[0] ^= 0xff;
        assert!(
            try_resume(
                &input_path,
                &sidecar_path,
                &proof_path,
                &cfg.vkey,
                cfg.chain_id,
                &other_acc,
            )
            .is_none(),
            "an anchor whose acc alone differs must refuse the cached artefact"
        );

        // Only the running chain id differs (a hand `--once` against another chain's work_dir).
        assert!(
            try_resume(
                &input_path,
                &sidecar_path,
                &proof_path,
                &cfg.vkey,
                cfg.chain_id + 1,
                &anchor1.inbox_batch_head_plus_1,
            )
            .is_none(),
            "a chain id that alone differs must refuse the cached artefact"
        );

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    /// A corrupt resumed proof (a preemption mid-write, or a `-o` file
    /// truncated by any other cause) must NOT halt the loop — it must be treated exactly like "no cached
    /// artefact at all" and proved again. Before this fix: `resumable` only checked file EXISTENCE, so
    /// this exact shape halted with `FollowerError::Calldata(..)` on every restart forever — the very
    /// preemption the resume rule exists to survive.
    #[tokio::test]
    async fn a_corrupt_resumed_proof_file_is_wiped_and_reproved_then_posts_once() {
        let chain = FakeChain::new();
        chain.open_and_finalize_inbox_batch(1);
        let work_dir = temp_work_dir("resume-corrupt-proof");
        let cfg = run_config(&chain, &work_dir);
        let prove_calls = Arc::new(AtomicU32::new(0));
        let prover = FakeProver {
            calls: prove_calls.clone(),
        };
        let metrics = Metrics::new();

        let mut fetch = chain.clone();
        let mut verifier = chain.verifier();
        let store = NoopStore;
        let (_anchor1, _checked, batch_dir, _attempt, _cost) = prepare_checked_proof(
            &mut fetch,
            &prover,
            &mut verifier,
            &store,
            &cfg,
            1,
            &metrics,
        )
        .await
        .expect("must not halt")
        .expect("must reach LocallyVerified");
        assert_eq!(prove_calls.load(Ordering::SeqCst), 1);

        // Overwrite the real proof file with garbage — a preemption mid-write leaves exactly this.
        let proof_path = batch_dir.join("proof");
        std::fs::write(&proof_path, b"garbage-not-a-real-proof").unwrap();

        let sender = chain.clone();
        let outcome = run_one(
            &mut fetch,
            &prover,
            &sender,
            &mut verifier,
            &store,
            &cfg,
            1,
            &metrics,
        )
        .await
        .expect("a corrupt resumed proof must re-prove, never halt");
        assert!(
            matches!(outcome, Outcome::Posted { batch: 1, .. }),
            "got {outcome:?}"
        );
        assert_eq!(
            prove_calls.load(Ordering::SeqCst),
            2,
            "a corrupt resumed proof must be wiped and proved again"
        );
        assert_eq!(chain.send_calls(), 1, "posts exactly once");

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    /// A sidecar whose own
    /// `inbox_commitment` belongs to a DIFFERENT chain reset (a chain reset keeps the same `work_dir`
    /// but batch ids restart at 1 against a new inbox) must not be trusted merely because the ELF sha
    /// still matches — `try_resume` must wipe and prove again. **Mutation: drop the
    /// chain_id/inbox_commitment/open_unix_ts comparison from `try_resume`** → this test's own
    /// `prove_calls == 2` assertion goes red (it would stay 1, wrongly resuming a foreign artefact).
    #[tokio::test]
    async fn a_sidecar_bound_to_another_chains_inbox_commitment_is_wiped_and_reproved() {
        let chain = FakeChain::new();
        chain.open_and_finalize_inbox_batch(1);
        let work_dir = temp_work_dir("resume-foreign-commitment");
        let cfg = run_config(&chain, &work_dir);
        let prove_calls = Arc::new(AtomicU32::new(0));
        let prover = FakeProver {
            calls: prove_calls.clone(),
        };
        let metrics = Metrics::new();

        let mut fetch = chain.clone();
        let mut verifier = chain.verifier();
        let store = NoopStore;
        let (_anchor1, _checked, batch_dir, _attempt, _cost) = prepare_checked_proof(
            &mut fetch,
            &prover,
            &mut verifier,
            &store,
            &cfg,
            1,
            &metrics,
        )
        .await
        .expect("must not halt")
        .expect("must reach LocallyVerified");
        assert_eq!(prove_calls.load(Ordering::SeqCst), 1);

        // Edit ONLY the sidecar's own recorded `inbox_commitment` — as if this work_dir were retained
        // across a chain reset that opened a fresh, differently-committed batch 1.
        let sidecar_path = batch_dir.join("sidecar.json");
        let mut sidecar_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&sidecar_path).unwrap()).unwrap();
        sidecar_json["inbox_commitment"] = serde_json::json!(format!("0x{}", "ab".repeat(32)));
        std::fs::write(&sidecar_path, sidecar_json.to_string()).unwrap();

        let (_anchor2, _checked2, _dir2, _attempt2, _cost2) = prepare_checked_proof(
            &mut fetch,
            &prover,
            &mut verifier,
            &store,
            &cfg,
            1,
            &metrics,
        )
        .await
        .expect("must not halt")
        .expect("must reach LocallyVerified again");
        assert_eq!(
            prove_calls.load(Ordering::SeqCst),
            2,
            "a sidecar bound to another chain's inbox commitment must be wiped and proved again"
        );

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    // ===================== RED: StaleAnchor retries rather than halting =====================

    /// A send that actually landed (the fake commits the state change)
    /// but is reported back as `BadBatchSequence` while the very next FINALIZED read still shows the
    /// OLD head (`stale_reads_left`, modeling the FINALIZED-lags-CONFIRMED lag) must be
    /// classified `StaleAnchor` and RETRIED — never halted. The retried candidate's own next anchor
    /// then sees the head has caught up and classifies `AlreadyPosted`, with zero further sends.
    #[tokio::test]
    async fn a_stale_anchor_on_post_retries_then_classifies_already_posted_with_zero_further_posts()
    {
        let chain = FakeChain::new();
        chain.open_and_finalize_inbox_batch(1);
        chain.arm_stale_send_once();
        let work_dir = temp_work_dir("stale-anchor");
        let cfg = run_config(&chain, &work_dir);
        let mut deps = Deps {
            fetch: chain.clone(),
            prover: FakeProver {
                calls: Arc::new(AtomicU32::new(0)),
            },
            sender: chain.clone(),
            verifier: chain.verifier(),
            store: NoopStore,
        };
        let metrics = Metrics::new();

        // Iteration 1: the send "lands" (state commits) but reports StaleAnchor -> Retry, same
        // candidate. Iteration 2: the resumed candidate re-anchors, sees the head already caught up,
        // and classifies AlreadyPosted before ever building a second send.
        run(
            &mut deps,
            &cfg,
            RunUntil::Iterations(2),
            &NeverStop,
            &metrics,
            noop_sleep,
        )
        .await
        .expect("a StaleAnchor retry must never halt");

        assert_eq!(chain.sent_posts(), vec![1], "the send landed exactly once");
        assert_eq!(
            chain.send_calls(),
            1,
            "the retried iteration must post ZERO further times — its own re-anchor sees AlreadyPosted \
             before ever building a send"
        );
        assert!(
            metrics
                .render()
                .contains("rome_zk_prover_posts_total{result=\"stale_anchor_retry\"} 1"),
            "{}",
            metrics.render()
        );

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    /// A send that lands for real but is refused
    /// `StaleAnchor` — and stays refused for THREE more consecutive re-anchor reads before the
    /// FINALIZED view catches up — must send exactly ONCE, never resending into each successive stale
    /// read: `send_calls == 1` throughout, converging to `AlreadyPosted` once the read genuinely
    /// advances. Before the `stale_send_pending` memory existed, every one of these retries would rebuild
    /// `PostRootProved` and resend, `skip_preflight: true`, paying a real fee each time. **Mutation:
    /// drop the `stale_send_pending` check (always fall through to a fresh send)** → `send_calls`
    /// climbs past 1 on the second `post_checked` call, this test's own assertion goes red.
    #[tokio::test]
    async fn a_stale_anchor_streak_never_resends_converging_to_already_posted() {
        let chain = FakeChain::new();
        chain.open_and_finalize_inbox_batch(1);
        let work_dir = temp_work_dir("stale-streak-no-resend");
        let cfg = run_config(&chain, &work_dir);
        let prover = FakeProver {
            calls: Arc::new(AtomicU32::new(0)),
        };
        let metrics = Metrics::new();

        let mut fetch = chain.clone();
        let mut verifier = chain.verifier();
        let store = NoopStore;
        let (_anchor1, checked, _dir, attempt, cost) = prepare_checked_proof(
            &mut fetch,
            &prover,
            &mut verifier,
            &store,
            &cfg,
            1,
            &metrics,
        )
        .await
        .expect("must not halt")
        .expect("must reach LocallyVerified");

        // The send lands for real (state commits) but is classified StaleAnchor, and the FINALIZED
        // read stays stale for 3 MORE re-anchor reads after that — models a lagging read that takes
        // several poll intervals to catch up.
        chain.arm_stale_send_for(3);

        let o1 = post_checked(
            &mut fetch, &chain, &store, &cfg, 1, &checked, attempt, cost, &metrics,
        )
        .run()
        .await
        .expect("must not halt");
        assert!(
            matches!(
                o1,
                Outcome::Retry {
                    reason: RetryReason::StaleAnchor,
                    ..
                }
            ),
            "got {o1:?}"
        );
        assert_eq!(chain.send_calls(), 1, "the one real send attempt");

        for i in 0..3 {
            let o = post_checked(
                &mut fetch, &chain, &store, &cfg, 1, &checked, attempt, cost, &metrics,
            )
            .run()
            .await
            .expect("must not halt");
            match i {
                0 | 1 => assert!(
                    matches!(
                        o,
                        Outcome::Retry {
                            reason: RetryReason::StaleAnchor,
                            ..
                        }
                    ),
                    "retry {i}: got {o:?}"
                ),
                _ => assert!(
                    matches!(o, Outcome::AlreadyPosted { batch: 1 }),
                    "retry {i}: got {o:?}"
                ),
            }
            assert_eq!(
                chain.send_calls(),
                1,
                "must never resend while the read stays stale (retry {i})"
            );
        }

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    // ===================== RED: transient fetch errors on anchor2/finalize retry, never halt

    /// A transient fetch error on `post_checked`'s own re-anchor (`anchor2`) retries the
    /// same candidate rather than halting; the next call (once the read succeeds) posts normally.
    #[tokio::test]
    async fn a_transient_fetch_error_on_the_re_anchor_retries_then_posts_once() {
        let chain = FakeChain::new();
        chain.open_and_finalize_inbox_batch(1);
        let work_dir = temp_work_dir("anchor2-fetch-error");
        let cfg = run_config(&chain, &work_dir);
        let prover = FakeProver {
            calls: Arc::new(AtomicU32::new(0)),
        };
        let metrics = Metrics::new();

        let mut fetch = chain.clone();
        let mut verifier = chain.verifier();
        let store = NoopStore;
        let (_anchor1, checked, _dir, attempt, cost) = prepare_checked_proof(
            &mut fetch,
            &prover,
            &mut verifier,
            &store,
            &cfg,
            1,
            &metrics,
        )
        .await
        .expect("must not halt")
        .expect("must reach LocallyVerified");

        let mut flaky = ArmedFlakyFetch {
            inner: chain.clone(),
            fail_next: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let outcome1 = post_checked(
            &mut flaky, &chain, &store, &cfg, 1, &checked, attempt, cost, &metrics,
        )
        .run()
        .await
        .expect("a transient fetch error on the re-anchor must retry, never halt");
        assert!(
            matches!(
                outcome1,
                Outcome::Retry {
                    reason: RetryReason::TransientFetchError(_),
                    ..
                }
            ),
            "got {outcome1:?}"
        );
        assert_eq!(
            chain.send_calls(),
            0,
            "no send attempted on the fetch-error retry"
        );

        let outcome2 = post_checked(
            &mut flaky, &chain, &store, &cfg, 1, &checked, attempt, cost, &metrics,
        )
        .run()
        .await
        .expect("must not halt");
        assert!(
            matches!(outcome2, Outcome::Posted { batch: 1, .. }),
            "got {outcome2:?}"
        );
        assert_eq!(chain.send_calls(), 1);

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    /// A transient fetch error on `fetch_and_verify_batch`'s own chunk-page read retries
    /// the whole job rather than halting; the next iteration proves and posts normally.
    #[tokio::test]
    async fn a_transient_fetch_error_on_the_batch_account_read_retries_then_posts() {
        let chain = FakeChain::new();
        chain.open_and_finalize_inbox_batch(1);
        let work_dir = temp_work_dir("chunk-fetch-error");
        let cfg = run_config(&chain, &work_dir);
        let prove_calls = Arc::new(AtomicU32::new(0));
        let mut deps = Deps {
            fetch: ArmedFlakyAccountFetch {
                inner: chain.clone(),
                fail_next: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            },
            prover: FakeProver {
                calls: prove_calls.clone(),
            },
            sender: chain.clone(),
            verifier: chain.verifier(),
            store: NoopStore,
        };
        let metrics = Metrics::new();

        run(
            &mut deps,
            &cfg,
            RunUntil::Iterations(1),
            &NeverStop,
            &metrics,
            noop_sleep,
        )
        .await
        .expect("a transient fetch error must retry, never halt");
        assert_eq!(
            prove_calls.load(Ordering::SeqCst),
            0,
            "the errored attempt never reached prove"
        );
        assert!(chain.sent_posts().is_empty());

        run(
            &mut deps,
            &cfg,
            RunUntil::Iterations(1),
            &NeverStop,
            &metrics,
            noop_sleep,
        )
        .await
        .expect("must not halt");
        assert_eq!(chain.sent_posts(), vec![1]);

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    /// A `TransientFetchError` streak that never recovers halts by name
    /// after exactly `fetch_alarm_polls` consecutive retries. **Mutation: remove the streak/alarm** →
    /// the loop spins forever instead of halting, this test's own `expect_err` goes red.
    #[tokio::test]
    async fn a_fetch_error_streak_that_never_clears_alarms_after_the_configured_poll_count() {
        let chain = FakeChain::new();
        chain.open_and_finalize_inbox_batch(1);
        let work_dir = temp_work_dir("fetch-alarm");
        let mut cfg = run_config(&chain, &work_dir);
        cfg.fetch_alarm_polls = 3;
        let mut deps = Deps {
            fetch: AlwaysErrorsAccountFetch {
                inner: chain.clone(),
            },
            prover: FakeProver {
                calls: Arc::new(AtomicU32::new(0)),
            },
            sender: chain.clone(),
            verifier: chain.verifier(),
            store: NoopStore,
        };
        let metrics = Metrics::new();

        let err = run(
            &mut deps,
            &cfg,
            RunUntil::Iterations(20),
            &NeverStop,
            &metrics,
            noop_sleep,
        )
        .await
        .expect_err("a fetch-error streak that never clears must alarm");
        assert!(
            matches!(err, FollowerError::FetchAlarm { polls: 3 }),
            "got {err:?}"
        );
        assert!(
            metrics.render().contains("rome_zk_prover_fetch_alarm 1"),
            "{}",
            metrics.render()
        );

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    /// A `StaleAnchor` streak that never clears (the
    /// fake keeps arming it every time) halts by name after exactly `stale_anchor_alarm_polls`
    /// consecutive retries. **Mutation: classify `StaleAnchor` as a halt again (its pre-fix
    /// behaviour)** → this test's own `expect_err` still passes, but `batches_1_through_5` and the
    /// "retries then already posted" test above would instead see `Err(PostRefused(..))` where they
    /// assert success — that is this fix's own regression signal.
    #[tokio::test]
    async fn a_stale_anchor_streak_that_never_clears_alarms_after_the_configured_poll_count() {
        let chain = FakeChain::new();
        chain.open_and_finalize_inbox_batch(1);
        chain.arm_jam_stale_forever();
        let work_dir = temp_work_dir("stale-anchor-alarm");
        let mut cfg = run_config(&chain, &work_dir);
        cfg.stale_anchor_alarm_polls = 3;
        let mut deps = Deps {
            fetch: chain.clone(),
            prover: FakeProver {
                calls: Arc::new(AtomicU32::new(0)),
            },
            sender: chain.clone(),
            verifier: chain.verifier(),
            store: NoopStore,
        };
        let metrics = Metrics::new();

        let err = run(
            &mut deps,
            &cfg,
            RunUntil::Iterations(20),
            &NeverStop,
            &metrics,
            noop_sleep,
        )
        .await
        .expect_err("a StaleAnchor streak that never clears must alarm");
        assert!(
            matches!(err, FollowerError::StaleAnchorAlarm { polls: 3 }),
            "got {err:?}"
        );
        assert!(
            metrics
                .render()
                .contains("rome_zk_prover_stale_anchor_alarm 1"),
            "{}",
            metrics.render()
        );

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    /// `Config::max_prove_attempts` bounds the retry-with-wipe loop — a
    /// prover that always produces a proof failing the record check exhausts every attempt and halts
    /// by name, rather than looping forever. **This prover's own failure is a structurally-undecodable
    /// file (truncated)** — a proof that decodes clean but fails only the real pairing is a DIFFERENT
    /// failure shape, covered separately by `AlwaysFlippedProver` below.
    #[tokio::test]
    async fn prove_attempts_exhausted_halts_by_name_after_the_configured_bound() {
        /// Always "succeeds" at the subprocess layer but writes a proof file that is truncated —
        /// structurally undecodable, failing at `from_zisk_proof_file` itself, before `check_against_
        /// record` or the pairing ever run. Every attempt fails the same way, never the subprocess
        /// itself.
        struct AlwaysWrongProver {
            calls: Arc<AtomicU32>,
        }
        impl Prover for AlwaysWrongProver {
            fn prove(
                &self,
                _elf: &Path,
                _input_bin: &Path,
                out_file: &Path,
            ) -> Result<crate::prover::ProofFile, ProveError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let mut bytes = gate_proof_bytes();
                bytes.truncate(bytes.len() - 10);
                std::fs::write(out_file, bytes).unwrap();
                Ok(crate::prover::ProofFile {
                    path: out_file.to_path_buf(),
                    walls: crate::prover::StageWalls {
                        verified: true,
                        proof_generated_secs: Some(1.0),
                        wrapper_snark_ms: Some(1),
                        steps: Some(1),
                    },
                })
            }
        }

        let chain = FakeChain::new();
        chain.open_and_finalize_inbox_batch(1);
        let work_dir = temp_work_dir("prove-attempts-exhausted");
        let mut cfg = run_config(&chain, &work_dir);
        cfg.max_prove_attempts = 2;
        let prove_calls = Arc::new(AtomicU32::new(0));
        let prover = AlwaysWrongProver {
            calls: prove_calls.clone(),
        };
        let metrics = Metrics::new();

        let mut fetch = chain.clone();
        let mut verifier = chain.verifier();
        let store = NoopStore;
        let err = prepare_checked_proof(
            &mut fetch,
            &prover,
            &mut verifier,
            &store,
            &cfg,
            1,
            &metrics,
        )
        .await
        .expect_err("must halt once every attempt is exhausted");
        assert!(
            matches!(err, FollowerError::ProveAttemptsExhausted { attempts: 2 }),
            "got {err:?}"
        );
        assert_eq!(
            prove_calls.load(Ordering::SeqCst),
            2,
            "exactly max_prove_attempts calls"
        );

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    // ===================== RED: a resumed/fresh proof that decodes clean but fails the pairing =====

    /// Flips one byte INSIDE the real gate proof's own 768-byte `proof_bytes` field (never touching
    /// `program_vk`/`root_c`/the packaged public values, which live elsewhere in the file) — the shape the shared
    /// pairing check exists to catch: `from_zisk_proof_file` still decodes it, `check_against_record` still matches
    /// (the vkey words are untouched), but the real BN254 pairing (`verify_checked`) fails, since the SNARK proof no
    /// longer corresponds to those committed values. The offset is verified directly (not merely asserted) by the
    /// sanity test right below: bincode2's own varint scheme encodes the `Vec<u8>` length prefix for 768 (> 250) as
    /// `[251, lo, hi]` (a 2-byte LE tail) right after the 1-byte `Proof::Plonk` tag, so the 768 raw bytes start at
    /// offset 4.
    fn gate_proof_bytes_with_one_flipped_proof_byte() -> Vec<u8> {
        let mut bytes = gate_proof_bytes();
        let proof_bytes_start = 1 + 3; // tag (1 B) + varint-2 length prefix [251, lo, hi] (3 B)
        bytes[proof_bytes_start] ^= 0xff;
        bytes
    }

    /// Sanity check for the offset [`gate_proof_bytes_with_one_flipped_proof_byte`] flips: the flipped
    /// file still decodes and still checks clean against the record (proving the flip landed inside
    /// `proof_bytes`, not the vkey words or the packaged publics), but fails the real pairing.
    #[test]
    fn the_flipped_gate_proof_still_decodes_and_checks_clean_but_fails_only_the_pairing() {
        let flipped = gate_proof_bytes_with_one_flipped_proof_byte();
        let vkey = tiber_vkey();
        let cd = crate::calldata::from_zisk_proof_file(&flipped).expect("still decodes");
        let checked = crate::calldata::check_against_record(&cd, &vkey)
            .expect("still checks clean vs record");
        assert!(
            poster::verify_checked(&checked, &vkey).is_err(),
            "a flipped proof_bytes byte must fail the real BN254 pairing"
        );
    }

    /// A RESUMED proof file that decodes clean and checks clean against the
    /// record but fails the real pairing (a flipped `proof_bytes` byte — e.g. a torn write that landed
    /// past the length-prefixed fields `check_against_record` alone would catch) must be treated exactly
    /// like any other untrustworthy cached artefact: wiped and proved again, never halted. Before `try_resume`
    /// called `verify_zisk`, this resumed straight through to `build_post_ix`,
    /// which halted `PreSend(LocalVerifyFailed)` on every restart — the very preemption the resume rule
    /// exists to survive.
    #[tokio::test]
    async fn a_resumed_proof_that_fails_only_the_pairing_is_wiped_and_reproved_then_posts_once() {
        let chain = FakeChain::new();
        chain.open_and_finalize_inbox_batch(1);
        let work_dir = temp_work_dir("resume-flip-pairing");
        let cfg = run_config(&chain, &work_dir);
        let prove_calls = Arc::new(AtomicU32::new(0));
        let prover = FakeProver {
            calls: prove_calls.clone(),
        };
        let metrics = Metrics::new();

        let mut fetch = chain.clone();
        let mut verifier = chain.verifier();
        let store = NoopStore;
        let (_anchor1, _checked, batch_dir, _attempt, _cost) = prepare_checked_proof(
            &mut fetch,
            &prover,
            &mut verifier,
            &store,
            &cfg,
            1,
            &metrics,
        )
        .await
        .expect("must not halt")
        .expect("must reach LocallyVerified");
        assert_eq!(prove_calls.load(Ordering::SeqCst), 1);

        std::fs::write(
            batch_dir.join("proof"),
            gate_proof_bytes_with_one_flipped_proof_byte(),
        )
        .unwrap();

        let sender = chain.clone();
        let outcome = run_one(
            &mut fetch,
            &prover,
            &sender,
            &mut verifier,
            &store,
            &cfg,
            1,
            &metrics,
        )
        .await
        .expect("a pairing-failing resumed proof must re-prove, never halt");
        assert!(
            matches!(outcome, Outcome::Posted { batch: 1, .. }),
            "got {outcome:?}"
        );
        assert_eq!(
            prove_calls.load(Ordering::SeqCst),
            2,
            "a resumed proof failing only the pairing must be wiped and proved again"
        );
        assert_eq!(chain.send_calls(), 1, "posts exactly once");

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    // ===================== RED: a resumed job's history lands on
    // its own real winning attempt, never a literal `1` =====================

    /// Fails the real BN254 pairing on the FIRST call (a genuinely produced proof, retry material —
    /// never a decode/record failure), verifies clean with the real gate proof on every call after.
    /// Models attempt 1 failing and attempt 2 verifying.
    struct FlipOnceProver {
        calls: Arc<AtomicU32>,
    }
    impl Prover for FlipOnceProver {
        fn prove(
            &self,
            _elf: &Path,
            _input_bin: &Path,
            out_file: &Path,
        ) -> Result<crate::prover::ProofFile, ProveError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            let bytes = if n == 1 {
                gate_proof_bytes_with_one_flipped_proof_byte()
            } else {
                gate_proof_bytes()
            };
            std::fs::write(out_file, bytes).unwrap_or_else(|e| panic!("write fake proof: {e}"));
            Ok(crate::prover::ProofFile {
                path: out_file.to_path_buf(),
                walls: crate::prover::StageWalls {
                    verified: true,
                    proof_generated_secs: Some(1.0),
                    wrapper_snark_ms: Some(1),
                    steps: Some(1),
                },
            })
        }
    }

    /// The `continue` arm that retries a freshly produced, pairing-failing
    /// proof records `Failed` at that SAME attempt before retrying it — before this it recorded nothing at
    /// all, so attempt 1's own row was frozen at whichever transition last succeeded (`Proved`), never
    /// showing that it actually failed. `FlipOnceProver`: attempt 1 fails the pairing, attempt 2 verifies.
    /// Mutation: drop the `Failed` record from either `continue` arm → attempt 1's last status reverts to
    /// `proved` and this test goes red.
    #[tokio::test]
    async fn a_freshly_failing_attempt_ends_failed_not_stuck_at_its_last_success() {
        let chain = FakeChain::new();
        chain.open_and_finalize_inbox_batch(1);
        let work_dir = temp_work_dir("continue-arm-records-failed");
        let cfg = run_config(&chain, &work_dir);
        let prove_calls = Arc::new(AtomicU32::new(0));
        let prover = FlipOnceProver {
            calls: prove_calls.clone(),
        };
        let metrics = Metrics::new();
        let store = RecordingStore::new();

        let mut fetch = chain.clone();
        let mut verifier = chain.verifier();
        let (_anchor1, _checked, _dir, attempt, _cost) = prepare_checked_proof(
            &mut fetch,
            &prover,
            &mut verifier,
            &store,
            &cfg,
            1,
            &metrics,
        )
        .await
        .expect("must not halt")
        .expect("must reach LocallyVerified");
        assert_eq!(
            attempt, 2,
            "attempt 1 must fail the pairing, attempt 2 must verify"
        );

        let attempt1_statuses: Vec<&'static str> = store
            .events()
            .iter()
            .filter(|e| e.attempt == 1)
            .map(|e| e.kind.status())
            .collect();
        assert_eq!(
            attempt1_statuses.last(),
            Some(&"failed"),
            "attempt 1 must end failed, never stuck at its last success: {attempt1_statuses:?}"
        );

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    /// `FlipOnceProver` — attempt 1 genuinely fails the real pairing,
    /// attempt 2 genuinely verifies — so `prepare_checked_proof` returns `attempt == 2`. A restart before
    /// posting (a fresh `run_one` over the SAME candidate) resumes the on-disk attempt-2 artefact. Before
    /// the fix every resumed event (Queued/Verified/Posted/Finalized) was hardcoded to attempt `1` — a
    /// DIFFERENT `(chain_id, batch, attempt)` row than the one attempt 2's own real Queued/InputBuilt/
    /// Proving/Proved history already used, so in Postgres the real winning attempt's own row is
    /// stranded at `verified` (no signature) forever, while a fabricated attempt-1 row (which never once
    /// verified) ends up `finalized`. Mutation: hard-code `1u32` again in the resume branch → every
    /// post-restart event's `attempt` reverts to 1 and this test's own assertion goes red.
    #[tokio::test]
    async fn a_resumed_job_after_a_restart_records_every_event_under_its_real_winning_attempt() {
        let chain = FakeChain::new();
        chain.open_and_finalize_inbox_batch(1);
        let work_dir = temp_work_dir("resume-real-attempt");
        let cfg = run_config(&chain, &work_dir);
        let prove_calls = Arc::new(AtomicU32::new(0));
        let prover = FlipOnceProver {
            calls: prove_calls.clone(),
        };
        let metrics = Metrics::new();
        let store = RecordingStore::new();

        // Life 1: attempt 1 fails the pairing, attempt 2 verifies — stops at LocallyVerified (never
        // posts), modelling a restart right before the send.
        let mut fetch = chain.clone();
        let mut verifier = chain.verifier();
        let (_anchor1, _checked, _dir, attempt, _cost) = prepare_checked_proof(
            &mut fetch,
            &prover,
            &mut verifier,
            &store,
            &cfg,
            1,
            &metrics,
        )
        .await
        .expect("must not halt")
        .expect("must reach LocallyVerified");
        assert_eq!(
            attempt, 2,
            "attempt 1 must fail the pairing, attempt 2 must verify"
        );
        assert_eq!(prove_calls.load(Ordering::SeqCst), 2);
        let events_before_restart = store.events().len();

        // Life 2 (restart): `run_one` over the SAME candidate resumes the on-disk attempt-2 artefact.
        let mut fetch2 = chain.clone();
        let mut verifier2 = chain.verifier();
        let sender = chain.clone();
        let outcome = run_one(
            &mut fetch2,
            &prover,
            &sender,
            &mut verifier2,
            &store,
            &cfg,
            1,
            &metrics,
        )
        .await
        .expect("a resumed run must not halt");
        assert!(
            matches!(outcome, Outcome::Posted { batch: 1, .. }),
            "got {outcome:?}"
        );
        assert_eq!(
            prove_calls.load(Ordering::SeqCst),
            2,
            "a resume must never re-prove"
        );

        let events = store.events();
        assert!(
            events.len() > events_before_restart,
            "life 2 must have recorded something"
        );
        for ev in &events[events_before_restart..] {
            assert_eq!(
                ev.attempt, 2,
                "every resumed event must carry the real winning attempt: {ev:?}"
            );
        }
        // The resumed job's own `Verified` event carries the resume
        // gate's REAL re-verify wall time (measured inside `try_resume` itself, around the exact same
        // `poster::verify_checked` call the resume gate always ran) — never a fabricated `0.0`, which
        // would silently overwrite the real, already-recorded `wall_verify_ms` from life 1 (COALESCE
        // treats `0.0` as a value, not an absence).
        let resumed_verified = events[events_before_restart..]
            .iter()
            .find_map(|ev| match &ev.kind {
                JobEventKind::Verified { wall_ms, .. } => Some(*wall_ms),
                _ => None,
            })
            .expect("life 2 must record a Verified event");
        assert!(
            resumed_verified > 0.0,
            "the resumed Verified event's wall_ms must be a real measurement, never 0.0: got \
             {resumed_verified}"
        );

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    /// A restart between a post and its own local finalize-history record
    /// (a crash right after the on-chain send confirms — `PostRootProved` itself born the batch Final in
    /// the SAME instruction, "posted+proved final immediately") must not leave the row stuck at `posted`
    /// forever. A FRESH follower run over the SAME already-final candidate anchors straight into
    /// `HeadAhead`/`AlreadyPosted` — before the fix this path recorded NOTHING at all, so a history store with
    /// no prior row for this batch (a genuinely fresh box, or the exact stuck-at-posted shape) never
    /// learns the batch is final. Mutation: drop the new `record` call from the `AlreadyPosted`-and-final
    /// arm → `store.events()` stays empty and this test goes red.
    #[tokio::test]
    async fn a_restart_onto_an_already_final_candidate_records_finalized_with_no_signature() {
        let chain = FakeChain::new();
        chain.open_and_finalize_inbox_batch(1);
        let work_dir = temp_work_dir("restart-onto-already-final");
        let cfg = run_config(&chain, &work_dir);

        // Life 1: a normal, complete post of batch 1 — chain-side, born Final immediately — through a
        // NoopStore, so this run's own history-writing path (Posted/Finalized via `post_checked`) leaves
        // no trace at all, modelling the crash-right-after-the-send shape.
        {
            let mut fetch = chain.clone();
            let mut verifier = chain.verifier();
            let prover = FakeProver {
                calls: Arc::new(AtomicU32::new(0)),
            };
            let sender = chain.clone();
            let metrics = Metrics::new();
            let outcome = run_one(
                &mut fetch,
                &prover,
                &sender,
                &mut verifier,
                &NoopStore,
                &cfg,
                1,
                &metrics,
            )
            .await
            .expect("must not halt");
            assert!(
                matches!(outcome, Outcome::Posted { batch: 1, .. }),
                "got {outcome:?}"
            );
        }
        assert_eq!(chain.head_pending_batch(), 1);

        // Life 2 (restart): a fresh anchor for the SAME candidate finds it already posted AND already
        // final — with a store that has never recorded anything for this batch.
        let mut fetch2 = chain.clone();
        let prover2 = FakeProver {
            calls: Arc::new(AtomicU32::new(0)),
        };
        let mut verifier2 = chain.verifier();
        let store = RecordingStore::new();
        let metrics = Metrics::new();
        let outcome = prepare_checked_proof(
            &mut fetch2,
            &prover2,
            &mut verifier2,
            &store,
            &cfg,
            1,
            &metrics,
        )
        .await
        .expect("must not halt");
        assert!(
            matches!(outcome, Err(Outcome::AlreadyPosted { batch: 1 })),
            "got {outcome:?}"
        );

        let events = store.events();
        assert_eq!(events.len(), 1, "exactly one history write: {events:?}");
        assert_eq!(events[0].chain_id, cfg.chain_id);
        assert_eq!(events[0].batch, 1);
        assert!(
            matches!(
                &events[0].kind,
                JobEventKind::Finalized { finalize_sig: None }
            ),
            "got {:?}",
            events[0].kind
        );

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    /// `run_one`'s own halting path records `Failed` at the attempt-0
    /// sentinel BEFORE returning its error — not only inside `run`'s loop. This is exactly the shape
    /// `--once` (`bin/rome-zk-prover.rs`) hits: it calls `run_one` directly, never `run`, so before the fix
    /// (when only `run`'s own loop recorded this) a halting `--once` invocation recorded nothing at all.
    /// Mutation: move the `record` call back out of `run_one` (leaving it only in `run`) → this test,
    /// which calls `run_one` directly, goes red.
    #[tokio::test]
    async fn run_one_called_directly_records_failed_on_its_own_halting_error() {
        struct AlwaysFlippedProver {
            calls: Arc<AtomicU32>,
        }
        impl Prover for AlwaysFlippedProver {
            fn prove(
                &self,
                _elf: &Path,
                _input_bin: &Path,
                out_file: &Path,
            ) -> Result<crate::prover::ProofFile, ProveError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                std::fs::write(out_file, gate_proof_bytes_with_one_flipped_proof_byte()).unwrap();
                Ok(crate::prover::ProofFile {
                    path: out_file.to_path_buf(),
                    walls: crate::prover::StageWalls {
                        verified: true,
                        proof_generated_secs: Some(1.0),
                        wrapper_snark_ms: Some(1),
                        steps: Some(1),
                    },
                })
            }
        }

        let chain = FakeChain::new();
        chain.open_and_finalize_inbox_batch(1);
        let work_dir = temp_work_dir("run-one-direct-records-failed");
        let mut cfg = run_config(&chain, &work_dir);
        cfg.max_prove_attempts = 2;
        let prover = AlwaysFlippedProver {
            calls: Arc::new(AtomicU32::new(0)),
        };
        let metrics = Metrics::new();
        let store = RecordingStore::new();

        let mut fetch = chain.clone();
        let mut verifier = chain.verifier();
        let sender = chain.clone();
        let err = run_one(
            &mut fetch,
            &prover,
            &sender,
            &mut verifier,
            &store,
            &cfg,
            1,
            &metrics,
        )
        .await
        .expect_err("must halt once every attempt is exhausted");
        assert!(
            matches!(err, FollowerError::ProveAttemptsExhausted { attempts: 2 }),
            "got {err:?}"
        );

        let halt_events: Vec<_> = store
            .events()
            .into_iter()
            .filter(|e| e.attempt == 0 && matches!(e.kind, JobEventKind::Failed { .. }))
            .collect();
        assert_eq!(
            halt_events.len(),
            1,
            "run_one's own halting path must record Failed at the attempt-0 sentinel: {:?}",
            store.events()
        );

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    /// A FRESHLY produced proof that always fails only the pairing (never
    /// the decode/record check) is retry material bounded by `max_prove_attempts`, exactly like a
    /// structurally-broken proof — `ProveAttemptsExhausted`, zero sends.
    #[tokio::test]
    async fn a_fresh_proof_that_always_fails_only_the_pairing_exhausts_attempts_with_zero_sends() {
        struct AlwaysFlippedProver {
            calls: Arc<AtomicU32>,
        }
        impl Prover for AlwaysFlippedProver {
            fn prove(
                &self,
                _elf: &Path,
                _input_bin: &Path,
                out_file: &Path,
            ) -> Result<crate::prover::ProofFile, ProveError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                std::fs::write(out_file, gate_proof_bytes_with_one_flipped_proof_byte()).unwrap();
                Ok(crate::prover::ProofFile {
                    path: out_file.to_path_buf(),
                    walls: crate::prover::StageWalls {
                        verified: true,
                        proof_generated_secs: Some(1.0),
                        wrapper_snark_ms: Some(1),
                        steps: Some(1),
                    },
                })
            }
        }

        let chain = FakeChain::new();
        chain.open_and_finalize_inbox_batch(1);
        let work_dir = temp_work_dir("fresh-flip-exhausted");
        let mut cfg = run_config(&chain, &work_dir);
        cfg.max_prove_attempts = 2;
        let prove_calls = Arc::new(AtomicU32::new(0));
        let prover = AlwaysFlippedProver {
            calls: prove_calls.clone(),
        };
        let metrics = Metrics::new();

        let mut fetch = chain.clone();
        let mut verifier = chain.verifier();
        let store = NoopStore;
        let err = prepare_checked_proof(
            &mut fetch,
            &prover,
            &mut verifier,
            &store,
            &cfg,
            1,
            &metrics,
        )
        .await
        .expect_err("must halt once every attempt is exhausted");
        assert!(
            matches!(err, FollowerError::ProveAttemptsExhausted { attempts: 2 }),
            "got {err:?}"
        );
        assert_eq!(prove_calls.load(Ordering::SeqCst), 2);
        assert_eq!(chain.send_calls(), 0);

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    // ===================== RED: the fresh anchor's own changed identity
    // wipes a cached proof, honestly (a chain reset in miniature) =====

    /// After a first `prepare_checked_proof` for batch 1 succeeds (a real prove, cached to disk),
    /// the fresh anchor's OWN view of batch 1's inbox account changes (`set_inbox_identity` — applied
    /// identically to both `SnapshotFetch` and `AccountFetch`, "a chain reset in miniature": the same
    /// batch id, genuinely different on-chain content) — the second `prepare_checked_proof` for the SAME
    /// candidate must detect this against the FRESH anchor (not merely a hand-edited sidecar file) and
    /// wipe + prove again. Targets `open_unix_ts` rather than `acc` specifically: `acc` is `keccak(chain_id
    /// ‖ batch ‖ open_slot ‖ count ‖ root ‖ forced_root)` (`rome-zk-layouts::acc`) — bound to the batch id
    /// itself, so no OTHER value can honestly reproduce an arbitrary target `acc` for the SAME batch id
    /// without inverting a hash; `open_unix_ts` is a free header field (it is not part of `acc`)
    /// that exercises the exact same three-comparison block in `try_resume` just as directly. **Mutation:
    /// `if false` on the three sidecar-vs-anchor comparisons in `try_resume`** → `prove_calls` stays
    /// 1 (wrongly resumes a stale-identity artefact) — this test's own `prove_calls == 2` assertion goes
    /// red.
    #[tokio::test]
    async fn a_fresh_anchors_changed_open_unix_ts_wipes_the_cached_proof_and_reproves() {
        let chain = FakeChain::new();
        chain.open_and_finalize_inbox_batch(1);
        let work_dir = temp_work_dir("m4-fresh-identity-changed");
        let cfg = run_config(&chain, &work_dir);
        let prove_calls = Arc::new(AtomicU32::new(0));
        let prover = FakeProver {
            calls: prove_calls.clone(),
        };
        let metrics = Metrics::new();

        let mut fetch = chain.clone();
        let mut verifier = chain.verifier();
        let store = NoopStore;
        let (_anchor1, _checked, _dir, _attempt, _cost) = prepare_checked_proof(
            &mut fetch,
            &prover,
            &mut verifier,
            &store,
            &cfg,
            1,
            &metrics,
        )
        .await
        .expect("must not halt")
        .expect("must reach LocallyVerified");
        assert_eq!(prove_calls.load(Ordering::SeqCst), 1);

        chain.set_inbox_identity(1, GATE_OPEN_UNIX_TS + 1);

        let (_anchor2, _checked2, _dir2, _attempt2, _cost2) = prepare_checked_proof(
            &mut fetch,
            &prover,
            &mut verifier,
            &store,
            &cfg,
            1,
            &metrics,
        )
        .await
        .expect("must not halt")
        .expect("must reach LocallyVerified again");
        assert_eq!(
            prove_calls.load(Ordering::SeqCst),
            2,
            "the fresh anchor's changed identity must wipe the cached proof and prove again"
        );

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    /// A stale extra file left in `work_dir/1` by some earlier, unrelated life, plus a sidecar whose
    /// own `elf_sha256` no longer matches the vkey of record — after the rebuild, the directory holds
    /// EXACTLY the three files a fresh job writes, never the stale leftover. **Mutation: drop the
    /// `wipe_dir` call** → the stale file survives the rebuild, and this test's own directory-listing
    /// assertion goes red (no EXISTING test before this one inspected directory contents at all, only
    /// prove-call counts, which stay unaffected by a merely-additional stale file).
    #[tokio::test]
    async fn a_stale_leftover_file_is_wiped_leaving_exactly_the_three_expected_files() {
        let chain = FakeChain::new();
        chain.open_and_finalize_inbox_batch(1);
        let work_dir = temp_work_dir("m5-wipe-dir-contents");
        let cfg = run_config(&chain, &work_dir);
        let prover = FakeProver {
            calls: Arc::new(AtomicU32::new(0)),
        };
        let metrics = Metrics::new();

        let mut fetch = chain.clone();
        let mut verifier = chain.verifier();
        let store = NoopStore;
        let (_anchor1, _checked, batch_dir, _attempt, _cost) = prepare_checked_proof(
            &mut fetch,
            &prover,
            &mut verifier,
            &store,
            &cfg,
            1,
            &metrics,
        )
        .await
        .expect("must not halt")
        .expect("must reach LocallyVerified");

        std::fs::write(
            batch_dir.join("stale-leftover.tmp"),
            b"leftover from a different life",
        )
        .unwrap();
        let sidecar_path = batch_dir.join("sidecar.json");
        let mut sidecar_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&sidecar_path).unwrap()).unwrap();
        sidecar_json["provenance"]["elf_sha256"] =
            serde_json::json!(format!("0x{}", "ff".repeat(32)));
        std::fs::write(&sidecar_path, sidecar_json.to_string()).unwrap();

        let (_anchor2, _checked2, dir2, _attempt2, _cost2) = prepare_checked_proof(
            &mut fetch,
            &prover,
            &mut verifier,
            &store,
            &cfg,
            1,
            &metrics,
        )
        .await
        .expect("must not halt")
        .expect("must reach LocallyVerified again");

        let mut names: Vec<String> = std::fs::read_dir(&dir2)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "input.bin".to_string(),
                "proof".to_string(),
                "sidecar.json".to_string()
            ],
            "the rebuild must wipe the whole directory, not merely overwrite the expected files"
        );

        let _ = std::fs::remove_dir_all(&work_dir);
    }
}
