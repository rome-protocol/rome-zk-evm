//! `rome-zk-prover --config prover.toml [--once --batch N | --follow] [--dry-run] [--iterations N]`
//! is a thin CLI over [`rome_zk_prover::follower`] — every step (anchor, input,
//! prove, decode, verify, post, finalize, close) lives in that module now; this binary only wires the
//! real RPC/subprocess implementations and drives either a single job (`--once`) or the always-on loop
//! (`--follow`) until stopped.
//!
//! `--once --batch N` runs [`rome_zk_prover::follower::run_one`] exactly once and exits — the
//! degenerate single-iteration case of the same state machine `--follow` runs forever. `--dry-run`
//! means different things in each mode (both never send a real transaction): under `--once`, it stops
//! after the local re-verify and prints the built `PostRootFields`, the signed V1 tx's serialized size,
//! and a read-only `simulateTransaction` result; under `--follow`, it prints, every iteration, the
//! anchor's own head/`batches_behind` and the job it WOULD prove, never calling `Prover::prove` at all
//! — the read-only live gate (`--follow --dry-run --iterations N`).

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use solana_program::pubkey::Pubkey;
// `solana-client-v1` is now the identical crate+version as the plain `solana-client`
// (Cargo.toml's own doc) — this alias keeps every existing `solana_client_v1::` call site below
// unchanged.
use solana_client as solana_client_v1;

use rome_zk_prover::anchor::SnapshotFetch;
use rome_zk_prover::follower::{self, Deps, FollowerError, Outcome, RunConfig, RunUntil};
use rome_zk_prover::metrics::Metrics;
use rome_zk_prover::prover::LocalCargoZisk;
use rome_zk_prover::store::{AnyStore, NoopStore, PgStore};
use rome_zk_prover_input::verifier::RemoteVerifier;

#[derive(Parser)]
#[command(name = "rome-zk-prover")]
struct Args {
    #[arg(long)]
    config: PathBuf,
    /// Prove exactly one batch and exit — mutually exclusive with `--follow`.
    #[arg(long)]
    once: bool,
    /// Required with `--once`: the id `anchor()`'s own `candidate_batch` must equal
    /// `root.head_pending_batch + 1`.
    #[arg(long)]
    batch: Option<u64>,
    /// Run the always-on follower loop until SIGTERM/SIGINT (graceful: finishes the current stage's
    /// send, never kills mid-post) or `--iterations` is reached — mutually exclusive with `--once`.
    #[arg(long)]
    follow: bool,
    /// Stop this many loop iterations after starting, then exit 0 (`--follow` only). Required with
    /// `--follow --dry-run` (the live gate); optional otherwise.
    #[arg(long)]
    iterations: Option<u64>,
    /// `--once`: stop after the local re-verify and print the fields/tx size/simulateTransaction result,
    /// never sending. `--follow`: print every iteration's anchor/batches_behind/would-prove job, never
    /// calling `Prover::prove` at all.
    #[arg(long)]
    dry_run: bool,
}

/// The real RPC-backed fetch — implements BOTH traits the follower's `Deps::fetch` needs
/// ([`SnapshotFetch`] for the chain anchor, [`rome_zk_prover_input::inbox::AccountFetch`] for the
/// inbox batch/chunk reads) over ONE FINALIZED client, distinct from the sender's CONFIRMED client.
struct RpcFetch<'a> {
    rt: &'a tokio::runtime::Runtime,
    client: solana_client::nonblocking::rpc_client::RpcClient,
}
impl SnapshotFetch for RpcFetch<'_> {
    fn get_multiple_accounts(
        &mut self,
        keys: &[Pubkey],
    ) -> Result<(u64, Vec<Option<Vec<u8>>>), rome_zk_prover::anchor::FetchError> {
        self.rt.block_on(async {
            let resp = self
                .client
                .get_multiple_accounts_with_commitment(
                    keys,
                    solana_commitment_config::CommitmentConfig::finalized(),
                )
                .await
                .map_err(|e| rome_zk_prover::anchor::FetchError(e.to_string()))?;
            Ok((
                resp.context.slot,
                resp.value.into_iter().map(|a| a.map(|a| a.data)).collect(),
            ))
        })
    }

    /// A plain `getBalance` — reporting only, never folded into the decision
    /// snapshot above (see the trait's own doc).
    fn payer_lamports(
        &mut self,
        payer: &Pubkey,
    ) -> Result<u64, rome_zk_prover::anchor::FetchError> {
        self.rt.block_on(async {
            self.client
                .get_balance(payer)
                .await
                .map_err(|e| rome_zk_prover::anchor::FetchError(e.to_string()))
        })
    }
}
impl rome_zk_prover_input::inbox::AccountFetch for RpcFetch<'_> {
    /// `get_account_with_commitment` reports a genuinely missing
    /// account as `Ok(None)` directly (a decoded on-chain fact) rather than an error a caller would
    /// otherwise have to string-match — never conflated with the RPC call itself failing (rate limit,
    /// dropped connection, timeout), which is `Err(FetchError)` and never a panic.
    fn get_account(
        &mut self,
        pubkey: &Pubkey,
    ) -> Result<Option<Vec<u8>>, rome_zk_prover_input::inbox::FetchError> {
        self.rt.block_on(async {
            self.client
                .get_account_with_commitment(pubkey, self.client.commitment())
                .await
                .map(|resp| resp.value.map(|a| a.data))
                .map_err(|e| rome_zk_prover_input::inbox::FetchError(e.to_string()))
        })
    }
    fn get_multiple_accounts(
        &mut self,
        pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Vec<u8>>>, rome_zk_prover_input::inbox::FetchError> {
        self.rt.block_on(async {
            let mut out = Vec::with_capacity(pubkeys.len());
            for page in pubkeys.chunks(rome_zk_prover_input::inbox::MAX_ACCOUNTS_PER_GET_MULTIPLE) {
                let accounts = self
                    .client
                    .get_multiple_accounts(page)
                    .await
                    .map_err(|e| rome_zk_prover_input::inbox::FetchError(e.to_string()))?;
                out.extend(accounts.into_iter().map(|a| a.map(|a| a.data)));
            }
            Ok(out)
        })
    }
}

/// The real, tokio-backed sleep [`rome_zk_prover::follower::run`] uses between iterations.
fn real_sleep(d: Duration) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(tokio::time::sleep(d))
}

fn print_outcome(outcome: &Outcome) {
    match outcome {
        Outcome::Posted { batch, sig } => println!("posted: batch {batch} sig {sig}"),
        Outcome::AlreadyPosted { batch } => println!("already posted: batch {batch}"),
        Outcome::Superseded {
            batch,
            head_pending_batch,
        } => println!("superseded: batch {batch}, chain head is now {head_pending_batch}"),
        Outcome::Idle { batches_behind } => println!("idle: batches_behind={batches_behind}"),
        Outcome::Retry {
            batches_behind,
            reason,
        } => println!("retry: batches_behind={batches_behind} reason={reason:?}"),
    }
}

fn run_config(
    cfg: &rome_zk_prover::config::Config,
    vkey: rome_zk_prover::config::VkeyOfRecord,
    settlement_program: Pubkey,
    inbox_program: Pubkey,
    authority: Pubkey,
) -> RunConfig {
    RunConfig {
        settlement_program,
        inbox_program,
        chain_id: cfg.chain_id,
        vkey,
        elf_path: cfg.elf_path.clone(),
        work_dir: cfg.work_dir.clone(),
        keep_work_dirs: cfg.keep_work_dirs,
        authority,
        tuning: cfg.send_tuning(),
        loaded_accounts_data_size_limit: cfg.loaded_accounts_data_size_limit,
        finalize_walk: cfg.finalize_walk,
        close_pending_after_batches: cfg.close_pending_after_batches,
        poll_interval: Duration::from_millis(cfg.poll_interval_ms),
        verifier_behind_alarm_polls: cfg.verifier_behind_alarm_polls,
        stale_anchor_alarm_polls: cfg.stale_anchor_alarm_polls,
        max_prove_attempts: cfg.max_prove_attempts,
        fetch_alarm_polls: cfg.fetch_alarm_polls,
        solana_rpc_url: cfg.solana_rpc_url.clone(),
        verifier_rpc_url: cfg.verifier_rpc_url.clone(),
        gpu_hourly_usd: cfg.gpu_hourly_usd.unwrap_or(0.0),
        stale_send_pending: std::sync::Mutex::new(None),
    }
}

/// The `--follow --dry-run` live gate's own print loop: anchors, prints
/// `batches_behind` and the job it WOULD prove, never touching `Prover::prove`. Self-corrects its own
/// candidate guess from `AnchorError::HeadAhead`/`InboxNotFinalizedYet`, exactly like
/// [`rome_zk_prover::follower::run`]'s own bookkeeping, but stops well before any input is built.
fn dry_run_follow_loop(
    fetch: &mut impl SnapshotFetch,
    cfg: &RunConfig,
    iterations: u64,
) -> anyhow::Result<()> {
    let mut candidate = 1u64;
    for i in 0..iterations {
        let anchor_result = rome_zk_prover::anchor::anchor(
            fetch,
            &cfg.settlement_program,
            &cfg.inbox_program,
            cfg.chain_id,
            candidate,
            &cfg.vkey,
        );
        match anchor_result {
            Ok(a) => {
                let batches_behind =
                    follower::batches_behind(a.cursor_next_batch, a.root.head_pending_batch);
                println!(
                    "[{i}] anchored at slot {}: head_pending_batch={} head_final_batch={} \
                     cursor_next_batch={} batches_behind={batches_behind} -> would prove batch {}",
                    a.slot,
                    a.root.head_pending_batch,
                    a.root.head_final_batch,
                    a.cursor_next_batch,
                    candidate
                );
            }
            Err(rome_zk_prover::anchor::AnchorError::HeadAhead {
                head_pending_batch, ..
            }) => {
                println!("[{i}] head ahead: chain head is now {head_pending_batch}, re-deriving candidate");
                candidate = head_pending_batch + 1;
                continue;
            }
            Err(rome_zk_prover::anchor::AnchorError::InboxNotFinalizedYet {
                head_pending_batch,
                cursor_next_batch,
                ..
            }) => {
                let batches_behind =
                    follower::batches_behind(cursor_next_batch, head_pending_batch);
                println!(
                    "[{i}] head_pending_batch={head_pending_batch} cursor_next_batch={cursor_next_batch} \
                     batches_behind={batches_behind} -> {}",
                    if batches_behind == 0 { "idle" } else { "candidate not finalized yet" }
                );
            }
            Err(e) => {
                println!("[{i}] anchor refused: {e}");
            }
        }
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if args.once == args.follow {
        anyhow::bail!("exactly one of --once or --follow must be given");
    }
    if args.once && args.batch.is_none() {
        anyhow::bail!("--once requires --batch N");
    }
    if args.follow && args.dry_run && args.iterations.is_none() {
        anyhow::bail!("--follow --dry-run requires --iterations N (the live gate runs a bounded read-only walk)");
    }

    let cfg = rome_zk_prover::config::Config::load(&args.config)?;
    let vkey = cfg.load_vkey_of_record()?;
    let settlement_program = Pubkey::from_str(&cfg.settlement_program_id)?;
    let inbox_program = Pubkey::from_str(&cfg.inbox_program_id)?;

    let rt = tokio::runtime::Runtime::new()?;
    let v1_payer = solana_keypair::read_keypair_file(&cfg.payer_key_path)
        .map_err(|e| anyhow::anyhow!("read payer key {}: {e}", cfg.payer_key_path.display()))?;
    let authority = rome_zk_solana_sender::compat::from_v1_pubkey(&{
        use solana_signer::Signer;
        v1_payer.pubkey()
    });

    let run_cfg = run_config(&cfg, vkey, settlement_program, inbox_program, authority);
    let metrics = Metrics::new();

    // `database_url` set means the operator asked for history — a failure to connect
    // or migrate is a named, fail-closed refusal to start (`HistoryUnreachable`), never a silent
    // `NoopStore` fallback. Unset means history stays off, as it always has.
    let store: AnyStore = match &cfg.database_url {
        Some(url) => AnyStore::Pg(rt.block_on(PgStore::connect(url))?),
        None => AnyStore::Noop(NoopStore::new()),
    };

    // The `/metrics` HTTP responder — bound on this task first (a bad/busy address is a named fatal
    // error, never a silently dead endpoint), then served on a spawned task for this run's whole
    // lifetime, in every mode.
    let metrics_addr: std::net::SocketAddr = cfg.metrics_addr.parse()?;
    rt.block_on(async {
        let listener = rome_zk_metrics_http::bind(metrics_addr)
            .await
            .unwrap_or_else(|e| panic!("metrics responder cannot bind {metrics_addr}: {e}"));
        let m = metrics.clone();
        tokio::spawn(async move {
            let _ = rome_zk_metrics_http::serve_on(listener, move || m.render().into_bytes()).await;
        });
    });

    let mut fetch = RpcFetch {
        rt: &rt,
        client: solana_client::nonblocking::rpc_client::RpcClient::new_with_commitment(
            cfg.solana_rpc_url.clone(),
            solana_commitment_config::CommitmentConfig::finalized(),
        ),
    };

    if args.follow && args.dry_run {
        return dry_run_follow_loop(
            &mut fetch,
            &run_cfg,
            args.iterations.expect("checked above"),
        );
    }

    let prover = LocalCargoZisk {
        zisk_home: cfg.zisk_home.clone(),
        gpu: cfg.gpu,
        timeout: Duration::from_secs(cfg.prove_timeout_secs),
    };
    let mut verifier = RemoteVerifier {
        url: &cfg.verifier_rpc_url,
    };

    if args.once {
        let batch = args.batch.expect("checked above");
        if args.dry_run {
            let (anchor1, checked, _dir, _attempt, _cost_usd) = rt
                .block_on(follower::prepare_for_dry_run(
                    &mut fetch,
                    &prover,
                    &mut verifier,
                    &run_cfg,
                    batch,
                    &metrics,
                ))?
                .map_err(|early| anyhow::anyhow!("cannot dry-run: {early:?}"))?;

            let pv = rome_zk_prover::poster::derive_public_values(&checked)?;
            let params = rome_zk_prover::poster::PostParams {
                anchor: &anchor1,
                vkey: &run_cfg.vkey,
                checked: &checked,
                settlement_program: &settlement_program,
                inbox_program: &inbox_program,
                authority: &authority,
                treasury: &anchor1.global_config.treasury,
            };
            let ix = rome_zk_prover::poster::build_post_ix(&params)?;
            let loaded_accounts_data_size_limit =
                rome_zk_prover::poster::resolve_loaded_accounts_data_size_limit(
                    run_cfg.loaded_accounts_data_size_limit,
                    &anchor1.account_data_lens,
                )?;
            let tx = rome_zk_solana_sender::build_v1_tx(
                &v1_payer,
                &[ix],
                run_cfg.tuning.compute_unit_limit,
                loaded_accounts_data_size_limit,
                run_cfg.tuning.priority_fee_micro_lamports,
                solana_hash::Hash::default(),
            )?;
            let bytes = wincode::serialize(&tx)?;
            println!("dry-run: batch {batch} fields:");
            println!(
                "  chain_id={} first_block={} last_block={}",
                pv.chain_id, pv.first_number, pv.last_number
            );
            println!("  state_root=0x{}", hex::encode(pv.state_root));
            println!("  parent_hash=0x{}", hex::encode(pv.parent_hash));
            println!("  last_block_hash=0x{}", hex::encode(pv.last_block_hash));
            println!("  gas_used={}", pv.gas_used);
            println!(
                "V1 tx size: {} bytes (<= 4096: {})",
                bytes.len(),
                bytes.len() <= 4096
            );
            println!("loaded_accounts_data_size_limit: {loaded_accounts_data_size_limit} bytes");

            let sim_client = solana_client_v1::nonblocking::rpc_client::RpcClient::new(
                cfg.solana_rpc_url.clone(),
            );
            let sim = rt.block_on(sim_client.simulate_transaction_with_config(
                &tx,
                solana_client_v1::rpc_config::RpcSimulateTransactionConfig {
                    sig_verify: false,
                    replace_recent_blockhash: true,
                    commitment: Some(solana_commitment_config::CommitmentConfig::confirmed()),
                    ..Default::default()
                },
            ));
            match sim {
                Ok(resp) => {
                    println!("simulateTransaction err: {:?}", resp.value.err);
                    println!(
                        "simulateTransaction units_consumed: {:?}",
                        resp.value.units_consumed
                    );
                    if let Some(logs) = resp.value.logs {
                        for l in logs {
                            println!("  log: {l}");
                        }
                    }
                }
                Err(e) => println!("simulateTransaction RPC error: {e}"),
            }
            return Ok(());
        }

        let sender = rome_zk_solana_sender::RpcSender::new(cfg.solana_rpc_url.clone(), v1_payer);
        let outcome = rt.block_on(follower::run_one(
            &mut fetch,
            &prover,
            &sender,
            &mut verifier,
            &store,
            &run_cfg,
            batch,
            &metrics,
        ))?;
        print_outcome(&outcome);
        if matches!(outcome, Outcome::Retry { .. }) {
            std::process::exit(2);
        }
        return Ok(());
    }

    // `--follow` (real): runs until SIGTERM/SIGINT or `--iterations` is reached.
    let sender = rome_zk_solana_sender::RpcSender::new(cfg.solana_rpc_url.clone(), v1_payer);
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        rt.spawn(async move {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("install SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = sigterm.recv() => {}
            }
            tracing::info!("stop signal received: finishing the current stage, then exiting");
            stop.store(true, Ordering::Relaxed);
        });
    }

    let mut deps = Deps {
        fetch,
        prover,
        sender,
        verifier,
        store,
    };
    let until = match args.iterations {
        Some(n) => RunUntil::Iterations(n),
        None => RunUntil::Forever,
    };
    let result = rt.block_on(follower::run(
        &mut deps,
        &run_cfg,
        until,
        stop.as_ref(),
        &metrics,
        real_sleep,
    ));
    match result {
        Ok(()) => Ok(()),
        Err(e @ FollowerError::AbandonedInboxBatch { .. }) => anyhow::bail!("{e}"),
        Err(e @ FollowerError::VerifierBehindAlarm { .. }) => anyhow::bail!("{e}"),
        Err(e) => anyhow::bail!("{e}"),
    }
}
