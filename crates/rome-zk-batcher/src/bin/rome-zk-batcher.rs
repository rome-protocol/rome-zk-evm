//! `rome-zk-batcher` binary: load config, preflight against the configured chain, batch the ordered log's
//! blocks in groups of at most `blocks_per_batch` consecutive blocks each, and post each group end to end
//! against the configured inbox + settlement programs.
//!
//! **One grouping path from the anchor, both modes:** `--once` and `--follow` share
//! the exact same setup — open the log AT the on-chain **chain anchor**
//! ([`rome_zk_batcher::anchor::resolve_anchor`]), verify the log's own first block matches it exactly (a
//! named `Gap` refusal otherwise — never a from-block-0 regroup-and-drop; see
//! [`rome_zk_batcher::anchor::verify_first_block_matches`]), and accumulate into one
//! [`rome_zk_batcher::grouping::SizeCappedGrouper`] seeded with `anchor.from_block - 1` (closes a group on
//! whichever comes first: `blocks_per_batch` blocks, or the channel's compressed size would exceed
//! `max_frames_per_batch`), submitting each group the moment it closes to a
//! [`pipeline::WindowedPoster`] (up to `batches_in_flight` batches post concurrently —
//! see that struct's own doc for the full bounded-window contract). The one difference: `--once` stops at end-of-log,
//! posts whatever partial group is left, drains the window, and exits — no tail-follow wait; `--follow` waits
//! for the sealer to keep appending instead.
//! Stateless across runs either way — the anchor is re-resolved live from on-chain state every invocation,
//! so a rerun over an unchanged (or grown) log posts only whatever the anchor has not yet covered.
//!
//! **Exactly one batcher per chain authority:** the run reads `batch_cursor.next_batch`
//! once, right after the startup recovery below, as its own `expected_next_batch`, and refuses by name
//! (`resolve::ResolveError::CursorAdvanced`) the moment the live on-chain cursor disagrees with it — a
//! second instance holding the same chain's authority key is destructive (it would double-post and fight over
//! the peer's in-flight batch), not merely unsupported.
//!
//! On every start (both modes), before resolving the anchor
//! or posting anything, **every** open-not-finalized batch in the pending window
//! `[root.head_final_batch, cursor.next_batch)` — not only `next_batch - 1` — is **finished under its own
//! id** (`pipeline::startup_recover`, `recover.rs`): the batcher re-derives the batch from the ordered log,
//! checks it against the leaves already on chain, sends the frames that are missing and finalizes. It never
//! abandons a batch, because settlement posts exactly the next id and an id the inbox cursor has passed
//! can never be opened again. If the log no longer matches what is on chain it stops with
//! `ResumeImpossible` and sends nothing. Running `AbandonBatch` by hand halts the chain.

use clap::Parser;
use rome_zk_batcher::anchor::{self, Anchor};
use rome_zk_batcher::config::Config;
use rome_zk_batcher::grouping::grouper_from_profile;
use rome_zk_batcher::loaded_accounts;
use rome_zk_batcher::metrics::Metrics;
use rome_zk_batcher::pipeline::{self, CuSampleConfig, FollowEvent, WindowConfig, WindowedPoster};
use rome_zk_batcher::preflight;
use rome_zk_batcher::resolve;
use rome_zk_batcher::sender::{compat, RpcSender, SendTuning};
use rome_zk_batcher::sink::{ChannelPostRootSink, PostRootSink};
use rome_zk_batcher::source::BlockSource;
use solana_client::nonblocking::rpc_client::RpcClient;
// `commitment_config` moved out of `solana_sdk`'s root re-export in the Agave
// 4.x line (API fallout) — now its own crate.
use solana_commitment_config::CommitmentConfig;
// The payer keypair is read directly as a `solana-keypair` key — `sender::RpcSender` signs
// with it; `payer_pubkey` (the `solana_program` `Pubkey` every instruction builder in this crate expects) is derived
// from it via `sender::compat::from_v1_pubkey` (the one conversion point), never read
// twice under two different key types.
use solana_keypair::read_keypair_file;
use solana_signer::Signer;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "rome-zk-batcher")]
struct Args {
    /// Path to the TOML config file (the crate README's Config section lists the keys).
    #[arg(long)]
    config: PathBuf,
    /// Batch whatever complete blocks the ordered log at this directory holds, in groups of at most
    /// `blocks_per_batch` blocks each, then exit — no tail-follow, no continuous resume loop.
    #[arg(long, conflicts_with = "follow", required_unless_present = "follow")]
    once: Option<PathBuf>,
    /// Continuously tail the ordered log at this directory, posting a batch every time a group closes
    /// (`blocks_per_batch` blocks, or the channel's compressed size would overflow), until interrupted
    /// (Ctrl-C).
    #[arg(long, conflicts_with = "once")]
    follow: Option<PathBuf>,
    /// A **verified** cross-check on the resolved chain anchor: if given, it must equal
    /// `rome_zk_batcher::anchor::resolve_anchor`'s own `from_block` exactly, or this refuses rather than trusting an
    /// operator's guess over the on-chain fact. No longer an unverified resume override (there is no such escape hatch
    /// any more — the anchor is the one resume point for both modes).
    #[arg(long)]
    from_block: Option<u64>,
}

/// Everything [`run_once`]/[`run_follow`] need, bundled so their own signatures stay readable.
/// `rpc`/`sender` are `Arc`-wrapped so [`WindowedPoster`] can hold its own clones for
/// spawned per-batch settlement tasks without borrowing this struct's lifetime.
struct RunDeps {
    rpc: Arc<RpcClient>,
    sender: Arc<RpcSender>,
    metrics: Arc<Metrics>,
    config: Config,
    payer_pubkey: solana_program::pubkey::Pubkey,
    chunk_tuning: SendTuning,
    open_tuning: SendTuning,
    finalize_tuning: SendTuning,
    poll_interval: Duration,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt::init();

    // Register tokio's ctrl-c handling for this process before anything else —
    // including preflight, well before the first post. A SIGINT delivered before any `ctrl_c()` future has
    // ever been polled gets the OS's default handling (immediate termination) instead of tokio's signal
    // driver; spawning this here, first, closes that gap for the whole process lifetime (a later
    // `tokio::signal::ctrl_c()` call, e.g. in the idle wait below, shares the same underlying registration).
    tokio::spawn(async {
        let _ = tokio::signal::ctrl_c().await;
    });

    let args = Args::parse();
    let log_dir: &Path = args
        .once
        .as_deref()
        .or(args.follow.as_deref())
        .expect("clap guarantees exactly one of --once/--follow");

    let config = match Config::load(&args.config) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("failed to load config {:?}: {e}", args.config);
            return ExitCode::FAILURE;
        }
    };

    let payer = match read_keypair_file(&config.payer_key_path) {
        Ok(k) => k,
        Err(e) => {
            tracing::error!(
                "failed to read payer keypair at {:?}: {e}",
                config.payer_key_path
            );
            return ExitCode::FAILURE;
        }
    };
    let payer_pubkey = compat::from_v1_pubkey(&payer.pubkey());

    // This read client (account/`getTransaction` reads only — every send goes
    // through `sender::RpcSender`'s own client) carries the same explicit per-request timeout as the
    // sender's `RPC_REQUEST_TIMEOUT` — the library default (`RpcClient::new_with_commitment`'s underlying
    // HTTP sender, 30 s) has no relationship to this crate's own tuning and previously left every read
    // exposed to it instead.
    let rpc = RpcClient::new_with_timeout_and_commitment(
        config.rpc_url.clone(),
        rome_zk_batcher::sender::RPC_REQUEST_TIMEOUT,
        CommitmentConfig::confirmed(),
    );

    if let Err(e) = preflight::run(
        &rpc,
        &config.settlement_program_id,
        config.chain_id,
        &payer_pubkey,
        &config.inbox_program_id,
    )
    .await
    {
        tracing::error!("preflight failed: {e}");
        return ExitCode::FAILURE;
    }
    tracing::info!(
        "preflight passed: payer {payer_pubkey} is chain {}'s root authority, inbox program matches the registry",
        config.chain_id
    );

    // Refuse to run if the configured loaded_accounts_data_size_limit
    // is not actually enough for the live inbox program (whose own ProgramData is loaded on every chunk-
    // lane, OpenBatch+Grow and FinalizeBatch transaction under SIMD-0186, even though it never appears in
    // any of their own account lists) — before this process ever sends a fee-spending transaction.
    if let Err(e) = loaded_accounts::run(
        &rpc,
        &config.inbox_program_id,
        config.loaded_accounts_data_size_limit,
        config.max_frames_per_batch as u32,
        config.max_frame_body_len,
    )
    .await
    {
        tracing::error!("loaded-accounts preflight failed: {e}");
        return ExitCode::FAILURE;
    }
    tracing::info!(
        "loaded-accounts preflight passed: loaded_accounts_data_size_limit={} covers the live inbox \
         program at max_frames_per_batch={}",
        config.loaded_accounts_data_size_limit,
        config.max_frames_per_batch
    );

    let sender = RpcSender::new(config.rpc_url.clone(), payer);
    let tuning = config.send_tuning();
    // Every chunk-lane transaction — `OpenBatch`+`GrowBatch` and the
    // one-V1-tx-per-frame `Open`+`Write`+`Seal`+`SealLeaf` plan — write-locks the fee
    // payer, so it needs its own, much tighter compute-unit limit than `tuning.compute_unit_limit`'s
    // general default: Solana's per-writable-account block cost cap charges every tx its *requested*
    // limit, so a too-high limit here caps the payer's own chunk-lane throughput regardless of `in_flight`
    // (see `config::payer_cap_frames_per_sec`'s doc for the full accounting). The batcher no longer sends
    // `AbandonBatch` or `Close`; the same limit also serves the finalize-resume sends at startup.
    let chunk_tuning = SendTuning {
        compute_unit_limit: config.chunk_compute_unit_limit,
        ..tuning
    };
    // The open-and-grow transaction has its own limit: its cost grows 4,500 CU per extra bump attempt on the
    // batch address, and it is sent once per batch, so a high limit costs nothing on the per-frame cost cap.
    let open_tuning = SendTuning {
        compute_unit_limit: config.open_compute_unit_limit,
        ..tuning
    };
    // `FinalizeBatch` alone needs a much higher CU limit than every
    // other instruction this process sends — only the `FinalizeBatch` send (`pipeline::finalize_and_verify`)
    // uses this one.
    let finalize_tuning = SendTuning {
        compute_unit_limit: config.finalize_compute_unit_limit,
        ..tuning
    };
    let poll_interval = Duration::from_millis(config.confirm_poll_interval_ms);

    // `sub_blocks_per_block`/`block_gas_limit` are read from the sequencer's own
    // `profile.json`, written beside the log this run points at — the batcher owns no parallel copy of
    // the chain's block shape.
    let profile_identity = match rome_zk_batcher::config::read_profile_identity(log_dir) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("failed to read profile identity for {:?}: {e}", log_dir);
            return ExitCode::FAILURE;
        }
    };

    // `batch_close_after_secs` below this chain's own block time is
    // refused here, once the profile identity is available — `== 0` was already refused at config load,
    // before any profile could be read.
    if let Err(e) = rome_zk_batcher::config::validate_batch_close_after(
        config.batch_close_after_secs,
        &profile_identity,
    ) {
        tracing::error!("{e}");
        return ExitCode::FAILURE;
    }

    let chain_id = config.chain_id;
    let deps = RunDeps {
        rpc: Arc::new(rpc),
        sender: Arc::new(sender),
        metrics: Metrics::new(),
        config,
        payer_pubkey,
        chunk_tuning,
        open_tuning,
        finalize_tuning,
        poll_interval,
    };

    // The /metrics HTTP responder — same one endpoint (`GET /metrics`) the sequencer
    // serves — spawned once here, ahead of the `--once`/`--follow` branch below, so it is up in BOTH
    // modes for this run's whole lifetime.
    // Bind on this task first so a bad or busy address is a named fatal error, never a silently dead
    // endpoint; only the accept loop runs spawned.
    {
        let addr = deps.config.metrics_addr;
        let listener = match rome_zk_metrics_http::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("metrics responder cannot bind {addr}: {e}");
                return ExitCode::FAILURE;
            }
        };
        let metrics = deps.metrics.clone();
        tokio::spawn(async move {
            if let Err(e) =
                rome_zk_metrics_http::serve_on(listener, move || metrics.render().into_bytes())
                    .await
            {
                tracing::error!("metrics responder on {addr} exited: {e}");
            }
        });
    }

    // The poster (a `PostRootSink` is not yet consumed by anything real,
    // so a channel-backed sink with no receiver is the whole hand-off surface needed here;
    // `sink.rs`'s own doc: a slow or absent consumer must never stall batch production).
    let (post_root_sink, _post_root_rx) = ChannelPostRootSink::new();
    let sink: Arc<dyn PostRootSink> = Arc::new(post_root_sink);

    let window_cfg = WindowConfig {
        inbox_program_id: deps.config.inbox_program_id,
        settlement_program_id: deps.config.settlement_program_id,
        payer: deps.payer_pubkey,
        chain_id,
        max_frame_body_len: deps.config.max_frame_body_len,
        chunk_tuning: deps.chunk_tuning,
        open_tuning: deps.open_tuning,
        chunk_retry_compute_unit_limit: deps.config.chunk_retry_compute_unit_limit,
        finalize_tuning: deps.finalize_tuning,
        // Matches the earlier poll shape (`FinalizePoll` in `post_one_group`): a 500 ms tick, up to
        // 120 polls (60 s) waiting for `leaves_present == expected_count` and again for `finalized`.
        finalize_poll_interval: Duration::from_millis(500),
        finalize_max_polls: 120,
        in_flight_frames: deps.config.in_flight,
        confirm_poll_interval: deps.poll_interval,
        signature_status_batch_size: deps.config.signature_status_batch_size,
        batches_in_flight: deps.config.batches_in_flight,
        // CU sampling is informational only, off the posting path — 5 s total
        // budget across both lookups (`OpenBatch+Grow`, one representative chunk-lane frame).
        cu_sample: Some(CuSampleConfig {
            rpc: deps.rpc.clone(),
            budget: Duration::from_secs(5),
        }),
        // Sample only every Nth finalized batch — see
        // `config::default_cu_sample_every`'s own doc; refused nonzero at config load.
        cu_sample_every: deps.config.cu_sample_every,
    };
    // Startup recovery (open-not-finalized batches in the pending window, then the one on-chain anchor
    // both modes resume from) lives in the library so a test drives the same code path.
    let anchor = match pipeline::startup_recover(
        deps.rpc.as_ref(),
        deps.sender.as_ref(),
        deps.metrics.as_ref(),
        sink.as_ref(),
        &pipeline::StartupRecover {
            window: &window_cfg,
            log_dir,
            sub_blocks_per_block: profile_identity.sub_blocks_per_block,
            block_gas_limit: profile_identity.block_gas_limit,
            blocks_per_batch: profile_identity.blocks_per_batch,
        },
    )
    .await
    {
        Ok(a) => a,
        Err(e) => {
            tracing::error!("{e}");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(
        "resume anchor: from_block={} prev_block_timestamp_secs={}",
        anchor.from_block,
        anchor.prev_block_timestamp_secs
    );

    if let Some(given) = args.from_block {
        if let Err(e) = anchor::verify_from_block_override(&anchor, given) {
            tracing::error!("{e}");
            return ExitCode::FAILURE;
        }
    }

    // This run's own belief about the cursor, read once — after the startup recovery
    // above (which never itself touches the cursor) — and advanced by one after every successful
    // `OpenBatch` (`WindowedPoster::submit_group`). Any later disagreement with the live on-chain cursor
    // means another writer holding this chain's authority key posted in between; `resolve::resolve_batch_id`
    // refuses by name rather than silently resolving a new id under it.
    let mut expected_next_batch = match resolve::read_cursor_next_batch(
        deps.rpc.as_ref(),
        &deps.config.inbox_program_id,
        &deps.config.settlement_program_id,
        chain_id,
    )
    .await
    {
        Ok(n) => n,
        Err(e) => {
            tracing::error!("failed to read the starting batch_cursor value: {e}");
            return ExitCode::FAILURE;
        }
    };

    // The newest block number this run has actually seen
    // appended to the ordered log, shared between the main loop (writer, on every `source.next_block()`
    // that returns a block) and every batch's own settle task (reader, at its own hand-off time) — the
    // `lag_blocks` gauge is computed from this live value, never a snapshot captured at submit time.
    let newest_block = Arc::new(AtomicU64::new(anchor.from_block.saturating_sub(1)));
    let mut poster = WindowedPoster::new(
        deps.sender.clone(),
        deps.rpc.clone(),
        deps.metrics.clone(),
        sink,
        window_cfg,
        newest_block.clone(),
    );

    if let Some(once_dir) = &args.once {
        run_once(
            &deps,
            once_dir,
            chain_id,
            &profile_identity,
            anchor,
            &mut expected_next_batch,
            &mut poster,
            &newest_block,
        )
        .await
    } else {
        let follow_dir = args
            .follow
            .as_deref()
            .expect("clap guarantees --follow is set when --once is not");
        run_follow(
            &deps,
            follow_dir,
            chain_id,
            &profile_identity,
            anchor,
            &mut expected_next_batch,
            &mut poster,
            &newest_block,
            Duration::from_secs(deps.config.batch_close_after_secs),
        )
        .await
    }
}

/// **One grouping path from the anchor, both modes:** `--once` is `--follow` without
/// the tail wait. Opens the log AT the anchor (`BlockSource::open(log_dir, ..., anchor.from_block,
/// anchor.prev_block_timestamp_secs)`), verifies the very first block the log actually hands back equals
/// the anchor exactly (else the named `Gap` refusal — no from-block-0 regrouping, no
/// `drop_already_posted_groups`: a rerun over a grown log simply has nothing new before the anchor to see
/// in the first place), accumulates into one [`SizeCappedGrouper`], submits every group that closes along
/// the way to `poster` (a bounded window, not one at a time), then — unlike `--follow`
/// — submits whatever partial group is left once the log is exhausted (no tail-follow wait), drains the
/// window (`poster.finish()`), and exits.
#[allow(clippy::too_many_arguments)]
async fn run_once(
    deps: &RunDeps,
    log_dir: &Path,
    chain_id: u64,
    profile_identity: &rome_zk_profile::ProfileIdentity,
    anchor: Anchor,
    expected_next_batch: &mut u64,
    poster: &mut WindowedPoster<RpcSender, RpcClient>,
    newest_block: &AtomicU64,
) -> ExitCode {
    let mut source = match BlockSource::open(
        log_dir,
        chain_id,
        profile_identity.block_gas_limit,
        profile_identity.sub_blocks_per_block,
        anchor.from_block,
        anchor.prev_block_timestamp_secs,
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("failed to open log at {log_dir:?}: {e}");
            return ExitCode::FAILURE;
        }
    };
    // The grouper cap is read from `profile_identity`
    // through this one tested library call — never a local `blocks_per_batch` variable that could drift
    // from it — shared by both `--once` and `--follow`.
    let mut grouper = grouper_from_profile(
        profile_identity,
        deps.config.max_frames_per_batch,
        deps.config.max_frame_body_len,
        anchor.from_block.checked_sub(1),
    );

    let mut first_block_checked = false;
    loop {
        match source.next_block() {
            Ok(Some(sourced)) => {
                if !first_block_checked {
                    if let Err(e) =
                        anchor::verify_first_block_matches(&anchor, sourced.block.number)
                    {
                        tracing::error!("log at {log_dir:?}: {e}");
                        return ExitCode::FAILURE;
                    }
                    first_block_checked = true;
                }
                newest_block.store(sourced.block.number, Ordering::Relaxed);
                if let Err(e) = pipeline::push_and_post_until_accepted(
                    &mut grouper,
                    sourced.block,
                    Instant::now(),
                    expected_next_batch,
                    poster,
                    &deps.metrics,
                )
                .await
                {
                    tracing::error!("{e} — nothing further is posted");
                    return ExitCode::FAILURE;
                }
            }
            Ok(None) => break, // end of log — no tail-follow wait, `--once`'s whole point.
            Err(e) => {
                tracing::error!("failed to read a block from the log: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    if !grouper.is_empty() {
        let group = grouper.take_group();
        tracing::info!(
            "posting final partial group at end-of-log: {} block(s), {}..={}",
            group.len(),
            group
                .first()
                .expect("a non-empty group has a first block")
                .number,
            group
                .last()
                .expect("a non-empty group has a last block")
                .number,
        );
        if poster
            .submit_group(group, expected_next_batch)
            .await
            .is_err()
        {
            return ExitCode::FAILURE;
        }
    } else if !first_block_checked {
        tracing::info!(
            "log at {log_dir:?} holds nothing past the on-chain anchor (block {}) — nothing to post",
            anchor.from_block
        );
    }

    // `--once` does not exit until every batch this run opened has actually finalized
    // and handed off — draining the window here is what makes "posts what's new, then exits" still true
    // under a bounded window (a partial window left running would silently drop that batch's own outcome
    // from this process's exit code).
    if poster.finish().await.is_err() {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Continuously tail the log, accumulating blocks into a [`SizeCappedGrouper`] (seeded
/// from the chain anchor, so cross-restart AND cross-group continuity are both enforced)
/// and submitting a group the moment it closes (cap or size) to `poster`
/// (a bounded window, not one at a time).
#[allow(clippy::too_many_arguments)]
async fn run_follow(
    deps: &RunDeps,
    log_dir: &Path,
    chain_id: u64,
    profile_identity: &rome_zk_profile::ProfileIdentity,
    anchor: Anchor,
    expected_next_batch: &mut u64,
    poster: &mut WindowedPoster<RpcSender, RpcClient>,
    newest_block: &AtomicU64,
    close_after: Duration,
) -> ExitCode {
    tracing::info!(
        "--follow resuming at block {} (prev_block_timestamp_secs={})",
        anchor.from_block,
        anchor.prev_block_timestamp_secs
    );

    let mut source = match BlockSource::open(
        log_dir,
        chain_id,
        profile_identity.block_gas_limit,
        profile_identity.sub_blocks_per_block,
        anchor.from_block,
        anchor.prev_block_timestamp_secs,
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("failed to open log at {log_dir:?}: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Same tested library call `run_once` uses — the
    // grouper cap always comes from `profile_identity`, never a local copy.
    let mut grouper = grouper_from_profile(
        profile_identity,
        deps.config.max_frames_per_batch,
        deps.config.max_frame_body_len,
        anchor.from_block.checked_sub(1),
    );

    // `--follow` verifies the log's first block against the anchor exactly as
    // `--once` does, before any grouping — a log that does not hold the anchor's own block first (pruned,
    // rotated, or genuinely diverged from the chain) fails loudly rather than silently accumulating a
    // continuity-broken group later.
    let mut first_block_checked = false;

    loop {
        match source.next_block() {
            Ok(Some(sourced)) => {
                if !first_block_checked {
                    if let Err(e) =
                        anchor::verify_first_block_matches(&anchor, sourced.block.number)
                    {
                        tracing::error!("log at {log_dir:?}: {e}");
                        return ExitCode::FAILURE;
                    }
                    first_block_checked = true;
                }
                newest_block.store(sourced.block.number, Ordering::Relaxed);
                // The age-close check runs after EVERY block,
                // not only on the idle arm below — a trickle of blocks arriving slower than the cap must
                // still close by age (see `pipeline::follow_tick`'s own doc for the full contract).
                if let Err(e) = pipeline::follow_tick(
                    &mut grouper,
                    FollowEvent::Block(sourced.block),
                    Instant::now(),
                    close_after,
                    expected_next_batch,
                    poster,
                    &deps.metrics,
                )
                .await
                {
                    tracing::error!("{e} — nothing further is posted");
                    return ExitCode::FAILURE;
                }
            }
            Ok(None) => {
                // Tail-follow: the log doesn't have the rest of the current group yet. A partial group
                // still sitting in `grouper` at shutdown is simply dropped here — it was never posted, so
                // there is nothing to lose; the next `--follow` start resumes from the same on-chain
                // anchor and re-reads those same blocks off the log.
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        tracing::info!("shutdown requested, draining the posting window before exiting");
                        if poster.finish().await.is_err() {
                            return ExitCode::FAILURE;
                        }
                        return ExitCode::SUCCESS;
                    }
                    _ = tokio::time::sleep(deps.poll_interval) => {
                        // Observe an in-flight sibling failure on every idle tick —
                        // without this, an idle/stalled chain could sit alive on a dead posting window for
                        // up to a whole batch period (or indefinitely) before the next `submit_group` ever
                        // ran the check that would have caught it.
                        if let Err(e) = poster.poll_failure().await {
                            tracing::error!(
                                "a batch in the posting window failed while idle-waiting — exiting: {e}"
                            );
                            return ExitCode::FAILURE;
                        }
                        if let Err(e) = source.refresh() {
                            tracing::error!("failed to refresh log segments: {e}");
                            return ExitCode::FAILURE;
                        }
                        // A partial group at the tail closes by age once it has
                        // waited long enough — an empty grouper never does (no batch when
                        // idle — `close_if_stale`'s own guard).
                        if let Err(e) = pipeline::follow_tick(
                            &mut grouper,
                            FollowEvent::Idle,
                            Instant::now(),
                            close_after,
                            expected_next_batch,
                            poster,
                            &deps.metrics,
                        )
                        .await
                        {
                            tracing::error!("{e} — nothing further is posted");
                            return ExitCode::FAILURE;
                        }
                    }
                }
            }
            Err(e) => {
                tracing::error!("failed to read a block from the log: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
}
