//! `rome-zk-derive` binary: the full-node derivation client anyone can run. Follows the
//! inbox continuously by default; `--once` derives everything final and exits.

use clap::Parser;
use rome_zk_derive::chain_bound::chain_drift_bound;
use rome_zk_derive::config::{reconcile_drift_bound, Config};
use rome_zk_derive::engine::{self, EngineApi, EngineController};
use rome_zk_derive::pipeline::DerivePipeline;
use rome_zk_derive::reader::RpcAccountReader;
use rome_zk_derive::resume::{self, ResumeAnchor};
use rome_zk_derive::traversal::SolanaTraversal;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "config.toml")]
    config: std::path::PathBuf,
    /// Derive everything final right now and exit, instead of following the inbox forever.
    #[arg(long)]
    once: bool,
    /// Bypass the settlement-root resume anchor and start traversal at this batch id
    /// instead, walking the engine forward from its own genesis and consolidating every already-built
    /// block along the way — `--from-batch 0` is an explicit full re-derivation while DA is retained.
    #[arg(long)]
    from_batch: Option<u64>,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let config = Config::load(&args.config)?;

    tracing::info!(chain_id = config.chain_id, "starting rome-zk-derive");

    let mut engine_api = engine::connect(
        &config.reth.rpc_url,
        &config.reth.engine_url,
        &config.reth.jwt_secret_path,
    )?;

    // The chain's fee recipient is the genesis `coinbase` — read here,
    // once, off the verifier reth's own already-loaded genesis file via the SAME engine RPC connection
    // this binary already opens (`eth_getBlockByNumber(0)`'s `miner` field), rather than this crate
    // (deliberately reth-free — see this crate's own docs) carrying a second, independent genesis-file
    // loader. Real height 0 always exists on a real EL (its own genesis); a missing response here is the
    // same "engine not ready" condition `EngineController::from_engine_head` below would hit anyway.
    let genesis_block = engine_api
        .block_at(0)
        .await?
        .ok_or_else(|| eyre::eyre!("genesis (real height 0) not found on the engine"))?;
    let fee_recipient = genesis_block.beneficiary;
    tracing::info!(?fee_recipient, "chain fee recipient (genesis coinbase)");

    // Resume from the settlement root anchor when the engine itself can confirm it
    // (falling back to the engine's own real genesis, never a hardcoded height/hash, only when no root
    // account exists at all — a decoded root the engine cannot confirm is refused, not fallen back from:
    // `resume::resume_anchor` itself returns that `Critical`, propagated by the `?` above) — `--from-batch`
    // skips straight to the genesis-first fallback for an explicit re-derivation. `anchor_reader` is
    // created once, ahead of both branches: the chain_config read below needs one too, and it
    // reuses this same reader in both the resume-anchor path and the `--from-batch` path.
    let mut anchor_reader = RpcAccountReader::new(config.solana_rpc_url.clone());

    let (start_at_batch, last_design_block, engine) = if let Some(from_batch) = args.from_batch {
        tracing::info!(
            from_batch,
            "--from-batch: bypassing the settlement-root anchor"
        );
        (
            from_batch,
            None,
            EngineController::from_engine_head(engine_api).await?,
        )
    } else {
        match resume::resume_anchor(
            &mut anchor_reader,
            &config.settlement_program_id,
            config.chain_id,
            &mut engine_api,
        )
        .await?
        {
            ResumeAnchor::Confirmed {
                start_at_batch,
                last_design_block,
                number,
                block_hash,
            } => {
                tracing::info!(
                    start_at_batch,
                    ?last_design_block,
                    number,
                    "resuming from the settlement root anchor"
                );
                (
                    start_at_batch,
                    last_design_block,
                    EngineController::new(engine_api, block_hash, number),
                )
            }
            ResumeAnchor::FromGenesis => {
                tracing::info!("no confirmable settlement root — starting from genesis (batch 0)");
                (
                    0,
                    None,
                    EngineController::from_engine_head(engine_api).await?,
                )
            }
        }
    };
    tracing::info!(
        next_height = engine.next_height(),
        "seeded the engine's own position"
    );

    let traversal = SolanaTraversal::new(
        RpcAccountReader::new(config.solana_rpc_url.clone()),
        config.inbox_program_id,
        config.chain_id,
        start_at_batch,
    );
    let inbox = rome_zk_derive::inbox::InboxRetrieval::new(
        RpcAccountReader::new(config.solana_rpc_url.clone()),
        config.inbox_program_id,
    );

    // The chain's own `chain_config.max_drift_secs` is authoritative —
    // read at startup (both the resume-anchor path and `--from-batch` reach this line), over the same
    // reader the resume anchor used above. `derive.toml`'s `max_drift_secs`, if present, must equal this
    // value or startup refuses by name (`reconcile_drift_bound`) — never a silent TOML-side override.
    let chain_max_drift_secs = chain_drift_bound(
        &mut anchor_reader,
        &config.settlement_program_id,
        config.chain_id,
    )
    .await?;
    let max_drift_secs = reconcile_drift_bound(config.max_drift_secs, chain_max_drift_secs)?;
    tracing::info!(
        max_drift_secs,
        source = "chain_config",
        "drift bound enabled: block.timestamp <= batch's open_unix_ts + max_drift_secs"
    );

    // The /metrics HTTP responder — same one endpoint (`GET /metrics`) the
    // batcher and sequencer serve — spawned once here, ahead of the `--once`/follow-forever branch below,
    // so it is up for this run's whole lifetime. Bind on this task first so a bad or busy address is a
    // named fatal error, never a silently dead endpoint (mirrors the batcher's own binary).
    let metrics = rome_zk_derive::metrics::Metrics::new();
    {
        let addr = config.metrics_addr;
        let listener = rome_zk_metrics_http::bind(addr)
            .await
            .map_err(|e| eyre::eyre!("metrics responder cannot bind {addr}: {e}"))?;
        let metrics = metrics.clone();
        tokio::spawn(async move {
            if let Err(e) =
                rome_zk_metrics_http::serve_on(listener, move || metrics.render().into_bytes())
                    .await
            {
                tracing::error!("metrics responder on {addr} exited: {e}");
            }
        });
    }

    let mut pipeline = DerivePipeline::new(
        traversal,
        inbox,
        engine,
        config.chain_id,
        fee_recipient,
        config.max_open_channels,
        config.blocks_per_batch,
    )
    .with_last_design_block(last_design_block)
    .with_drift_bound(max_drift_secs)
    .with_metrics(metrics);

    if args.once {
        let derived = pipeline.run_once().await?;
        tracing::info!(derived, "run_once complete");
    } else {
        pipeline.run_forever().await?;
    }
    Ok(())
}
