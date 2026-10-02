//! `rome-zk-sequencer` binary: load config, replay the ordered log, serve RPC + metrics.

use clap::{Parser, ValueEnum};
use rome_zk_sequencer::executor::{Executor, MockExecutor};
use rome_zk_sequencer::metrics::{serve_metrics, Metrics};
use rome_zk_sequencer::profile::ProfileIdentity;
use rome_zk_sequencer::recovery::{
    reconcile_profile_identity, replay_into_executor, RecoveryError,
};
use rome_zk_sequencer::rpc;
use rome_zk_sequencer::sealer::ResumePoint;
use rome_zk_sequencer::sequencer::{spawn, SpawnConfig};
use std::path::PathBuf;
use std::process::ExitCode;

/// `--executor mock|reth`. Default `mock` — existing tests/deploys are unaffected
/// unless this flag is passed (see `crate::config::RethSettings`'s doc and this crate's Cargo.toml
/// `reth` feature, default-on, gating whether `reth` is even a legal value here).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ExecutorKind {
    Mock,
    Reth,
}

#[derive(Parser)]
#[command(name = "rome-zk-sequencer")]
struct Args {
    /// Path to the TOML config file.
    #[arg(long, default_value = "config.toml")]
    config: PathBuf,
    /// If the ordered log's tail is torn (an interrupted append), truncate it and start clean instead
    /// of refusing to start.
    #[arg(long)]
    truncate_torn: bool,
    /// One-time migration: if the ordered log has existing records but no
    /// `profile.json` beside it — a log written before this check existed (e.g. Tiber's) — write one
    /// now from this config instead of refusing to start. Only needed once per log; a log that already
    /// has a `profile.json` never uses this flag's write path (a genuine mismatch is still refused).
    #[arg(long)]
    write_profile_json: bool,
    /// Execution backend. `reth` needs the `[reth]` config section (datadir + genesis_path).
    #[arg(long, value_enum, default_value_t = ExecutorKind::Mock)]
    executor: ExecutorKind,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let config = match rome_zk_sequencer::config::Config::load(&args.config) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("failed to load config {:?}: {e}", args.config);
            return ExitCode::FAILURE;
        }
    };

    let signer = match config.load_signer() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("failed to load sequencer key: {e}");
            return ExitCode::FAILURE;
        }
    };

    match args.executor {
        ExecutorKind::Mock => {
            let mut executor = MockExecutor::new();
            let resume = match replay(&config, &mut executor, signer.address(), &args).await {
                Ok(r) => r,
                Err(code) => return code,
            };
            run(executor, config, signer, resume).await
        }
        ExecutorKind::Reth => reth_main(config, signer, args).await,
    }
}

/// This chain's own fee recipient — the genesis `coinbase` the reth
/// executor's own `[reth]` config section already points at (`RethConfig::genesis_path`), read via the
/// same `genesis_from_path` `RethExecutor::new` uses, so there is one loader for the fact, not two.
/// `--executor mock` has no genesis file at all (`config.reth` is only ever consumed by the `reth`
/// executor) and stays `Address::ZERO` — unchanged from this binary's prior hardcoded behavior.
#[cfg(feature = "reth")]
fn fee_recipient_from_config(
    config: &rome_zk_sequencer::config::Config,
) -> Result<alloy::primitives::Address, ExitCode> {
    match &config.reth {
        Some(reth_settings) => {
            rome_zk_executor_reth::genesis_from_path(&reth_settings.genesis_path)
                .map(|g| g.coinbase)
                .map_err(|e| {
                    tracing::error!(
                        "failed to read genesis {:?} for the chain's fee recipient: {e}",
                        reth_settings.genesis_path
                    );
                    ExitCode::FAILURE
                })
        }
        None => Ok(alloy::primitives::Address::ZERO),
    }
}

#[cfg(not(feature = "reth"))]
fn fee_recipient_from_config(
    _config: &rome_zk_sequencer::config::Config,
) -> Result<alloy::primitives::Address, ExitCode> {
    Ok(alloy::primitives::Address::ZERO)
}

/// Replays the ordered log into `executor`, translating every recovery failure into the same
/// logged-and-`ExitCode::FAILURE` shape regardless of which `Executor` is behind it.
async fn replay<E: Executor>(
    config: &rome_zk_sequencer::config::Config,
    executor: &mut E,
    signer_address: alloy::primitives::Address,
    args: &Args,
) -> Result<ResumePoint, ExitCode> {
    // Reconcile the log's persisted chain identity BEFORE any replay work — a
    // restart under a changed profile, or pointed at the wrong chain's log, is refused right here.
    let configured_identity = ProfileIdentity::new(config.chain_id, &config.profile);
    if let Err(e) = reconcile_profile_identity(
        &config.log_dir,
        configured_identity,
        args.write_profile_json,
    ) {
        tracing::error!("profile identity check failed: {e}");
        return Err(ExitCode::FAILURE);
    }

    let fee_recipient = fee_recipient_from_config(config)?;

    match replay_into_executor(
        &config.log_dir,
        executor,
        signer_address,
        args.truncate_torn,
        config.profile.effective_block_gas_limit(),
        fee_recipient,
        config.profile.sub_blocks_per_block,
    )
    .await
    {
        Ok(r) => {
            tracing::info!(
                "replayed ordered log; resuming at block {} index {}",
                r.next_block,
                r.next_index
            );
            Ok(r)
        }
        Err(RecoveryError::TornLogRefusingStart { segment, offset }) => {
            tracing::error!(
                "log torn at {segment:?} offset {offset} — refusing to start without --truncate-torn"
            );
            Err(ExitCode::FAILURE)
        }
        Err(e) => {
            tracing::error!("failed to replay ordered log: {e}");
            Err(ExitCode::FAILURE)
        }
    }
}

/// Everything from "spawn the sequencer" to "exit with the right code", generic over the executor
/// so `--executor mock` and `--executor reth` share one startup tail (this binary's
/// startup sequence does not change when the executor is swapped behind the trait).
async fn run<E: Executor + 'static>(
    executor: E,
    config: rome_zk_sequencer::config::Config,
    signer: alloy::signers::local::PrivateKeySigner,
    resume: ResumePoint,
) -> ExitCode {
    let metrics = Metrics::new();
    tokio::spawn(serve_metrics(config.metrics_addr, metrics.clone()));

    let fee_recipient = match fee_recipient_from_config(&config) {
        Ok(v) => v,
        Err(code) => return code,
    };

    let (handle, sequencer_join) = match spawn(
        executor,
        SpawnConfig {
            admission: config.admission.to_admission_config(config.chain_id),
            log_dir: config.log_dir.clone(),
            blocks_per_segment: config.blocks_per_segment,
            signer,
            resume,
            metrics: metrics.clone(),
            // The chain's declared cadence, not a hardcoded constant.
            seal_period: std::time::Duration::from_millis(config.profile.sub_block_ms),
            preconf_feed_capacity: 1_024,
            // Configurable per-sub-block gas budget.
            sub_block_gas_limit: config.profile.sub_block_gas_limit,
            // Published into every block's BlockEnv.
            block_gas_limit: config.profile.effective_block_gas_limit(),
            fee_recipient,
            sub_blocks_per_block: config.profile.sub_blocks_per_block,
            // 0 (Tiber default) = never seal a block with no transactions.
            empty_block_interval_secs: config.profile.empty_block_interval_secs,
        },
    ) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("failed to start sequencer: {e}");
            return ExitCode::FAILURE;
        }
    };

    let (rpc_addr, _rpc_handle) =
        match rpc::serve(config.rpc_addr, handle, config.rpc.to_rpc_config(), metrics).await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("failed to start RPC server: {e}");
                return ExitCode::FAILURE;
            }
        };
    tracing::info!(
        "rome-zk-sequencer listening on {rpc_addr}, metrics on {}",
        config.metrics_addr
    );

    // The actor's JoinHandle resolves Err(SequencerFatal) on a clean
    // fatal shutdown, or Err(JoinError) if the task itself panicked (which the actor is designed never to
    // do) — log either case rather than silently discarding it.
    let join_result = sequencer_join.await;
    match &join_result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::error!("sequencer shut down: {e}"),
        Err(e) => tracing::error!("sequencer task failed: {e}"),
    }
    // A fatal shutdown or a panicked task must exit non-zero, or a
    // supervisor configured to restart on failure (systemd, compose `restart: on-failure`) would never
    // restart it.
    if run_was_fatal(&join_result) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(feature = "reth")]
async fn reth_main(
    config: rome_zk_sequencer::config::Config,
    signer: alloy::signers::local::PrivateKeySigner,
    args: Args,
) -> ExitCode {
    let Some(reth_settings) = config.reth.clone() else {
        tracing::error!("--executor reth needs a [reth] config section (datadir, genesis_path)");
        return ExitCode::FAILURE;
    };
    // Resolved before `reth_settings` is partially moved into `RethConfig`
    // below.
    let (http_addr, ws_addr) = reth_settings.resolved_rpc_addrs(config.rpc_addr);
    let mut executor =
        match rome_zk_executor_reth::RethExecutor::new(rome_zk_executor_reth::RethConfig {
            datadir: reth_settings.datadir,
            genesis_path: reth_settings.genesis_path,
            block_gas_limit: config.profile.effective_block_gas_limit(),
        }) {
            Ok(e) => e,
            Err(e) => {
                tracing::error!("failed to start reth executor: {e}");
                return ExitCode::FAILURE;
            }
        };
    let resume = match replay(&config, &mut executor, signer.address(), &args).await {
        Ok(r) => r,
        Err(code) => return code,
    };

    // `--executor reth` serves RPC via `node::serve` UNCONDITIONALLY —
    // `rpc::serve` (`run()`, below) is the mock executor's path only now. `http_addr`/`ws_addr` (above)
    // each independently fall back to `Config::rpc_addr` when `[reth]` doesn't set them
    // (`RethSettings::resolved_rpc_addrs` — see that method's doc for the one-port case this
    // produces when both are absent).
    run_with_node_rpc(executor, config, signer, resume, http_addr, ws_addr).await
}

/// The `--executor reth` tail — spawns the sequencer exactly
/// like `run()`, but serves RPC via `node::serve` (reth's own `eth_*` surface +
/// this crate's `eth_sendRawTransaction`/`rome_*` overrides/extensions) instead of `rpc::serve`.
/// `rpc_provider()`/`chain_spec()` are captured before `spawn()` moves `executor` into the actor
/// (see `RethExecutor::rpc_provider`'s doc: a clone of the executor's own `BlockchainProvider`, no
/// shared mutable access to the executor itself needed).
#[cfg(feature = "reth")]
async fn run_with_node_rpc(
    executor: rome_zk_executor_reth::RethExecutor,
    config: rome_zk_sequencer::config::Config,
    signer: alloy::signers::local::PrivateKeySigner,
    resume: ResumePoint,
    http_addr: std::net::SocketAddr,
    ws_addr: std::net::SocketAddr,
) -> ExitCode {
    let provider = executor.rpc_provider();
    let chain_spec = executor.chain_spec();

    let metrics = Metrics::new();
    tokio::spawn(serve_metrics(config.metrics_addr, metrics.clone()));

    let fee_recipient = match fee_recipient_from_config(&config) {
        Ok(v) => v,
        Err(code) => return code,
    };

    let (handle, sequencer_join) = match spawn(
        executor,
        SpawnConfig {
            admission: config.admission.to_admission_config(config.chain_id),
            log_dir: config.log_dir.clone(),
            blocks_per_segment: config.blocks_per_segment,
            signer,
            resume,
            metrics: metrics.clone(),
            seal_period: std::time::Duration::from_millis(config.profile.sub_block_ms),
            preconf_feed_capacity: 1_024,
            sub_block_gas_limit: config.profile.sub_block_gas_limit,
            block_gas_limit: config.profile.effective_block_gas_limit(),
            fee_recipient,
            sub_blocks_per_block: config.profile.sub_blocks_per_block,
            // 0 (Tiber default) = never seal a block with no transactions.
            empty_block_interval_secs: config.profile.empty_block_interval_secs,
        },
    ) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("failed to start sequencer: {e}");
            return ExitCode::FAILURE;
        }
    };

    let node_handle = match rome_zk_sequencer::node::serve(
        provider,
        chain_spec,
        handle,
        http_addr,
        ws_addr,
        config.rpc.to_rpc_config(),
    )
    .await
    {
        Ok(h) => h,
        Err(e) => {
            tracing::error!("failed to start reth node RPC: {e}");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(
        "rome-zk-sequencer (reth node RPC) listening on http {} / ws {}, metrics on {}",
        node_handle.addrs.http,
        node_handle.addrs.ws,
        config.metrics_addr
    );

    let join_result = sequencer_join.await;
    match &join_result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::error!("sequencer shut down: {e}"),
        Err(e) => tracing::error!("sequencer task failed: {e}"),
    }
    drop(node_handle);
    if run_was_fatal(&join_result) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(not(feature = "reth"))]
async fn reth_main(
    _config: rome_zk_sequencer::config::Config,
    _signer: alloy::signers::local::PrivateKeySigner,
    _args: Args,
) -> ExitCode {
    tracing::error!(
        "this binary was built without the `reth` cargo feature — --executor reth is unavailable"
    );
    ExitCode::FAILURE
}

/// Whether `sequencer_join.await`'s result represents a fatal shutdown — factored out as a plain
/// predicate over the two-layer `Result<Result<(), _>, _>` shape so it is unit-testable without
/// constructing a real `SequencerFatal` or `tokio::task::JoinError` (`std::process::ExitCode` itself is
/// deliberately not comparable, so the mapping is tested at this layer instead).
fn run_was_fatal<T, E1, E2>(result: &Result<Result<T, E1>, E2>) -> bool {
    !matches!(result, Ok(Ok(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_shutdown_is_not_fatal() {
        let result: Result<Result<(), &str>, &str> = Ok(Ok(()));
        assert!(!run_was_fatal(&result));
    }

    /// `Ok(Err(SequencerFatal))` (a clean fatal shutdown) must map to
    /// `ExitCode::FAILURE` — this is the exact bug: the old code logged this arm but then fell through
    /// to an unconditional `ExitCode::SUCCESS`.
    #[test]
    fn sequencer_fatal_error_is_fatal() {
        let result: Result<Result<(), &str>, &str> = Ok(Err("simulated SequencerFatal"));
        assert!(run_was_fatal(&result));
    }

    /// `Err(JoinError)` (the actor task itself panicked) must also map
    /// to `ExitCode::FAILURE`.
    #[test]
    fn join_error_is_fatal() {
        let result: Result<Result<(), &str>, &str> = Err("simulated JoinError");
        assert!(run_was_fatal(&result));
    }
}
