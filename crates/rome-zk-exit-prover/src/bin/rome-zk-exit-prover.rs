//! `rome-zk-exit-prover --config exit-prover.toml`: a thin, wire-only CLI that
//! wires the real RPC/sender implementations and drives [`rome_zk_exit_prover::follower::Follower`] over
//! every `ExitInitiated` log the L2 verifier reports. Every step — refuse-before-send (including the
//! pre-send nullifier read), classify, and retry/stuck routing — lives in
//! [`rome_zk_exit_prover::core::attempt_exit`] and [`rome_zk_exit_prover::follower::Follower`]; this
//! binary only wires the four calls together: `ingest` → `due` → `attempt_exit` → `apply`.
//!
//! `--once` runs exactly one poll (the log scan up to the node's newest block, `attempt_exit` on every due message) and exits
//! — useful for a smoke check or a cron-driven run. Without it, the loop polls forever every
//! `poll_interval_ms` until the process is signalled to stop.
//!
//! A `ProveExit` loads the settlement program's own data, which is hundreds of kilobytes, so the loaded-accounts limit
//! is worked out from the live account sizes on every poll unless the config sets it; a configured value below the
//! requirement stops the process with an error that names the setting.
//!
//! The follower's own state (`scan_from_block`, `pending`, `stuck`) is never persisted: a restart starts
//! a fresh [`rome_zk_exit_prover::follower::Follower`] at `cfg.portal_from_block` and rebuilds everything
//! it needs from the logs plus the on-chain nullifier bit — never a disk cursor.
//!
//! A chain that has not switched exits on (no exit config, no portal, a cap of zero) leaves the process idle and
//! healthy: it asks again every poll and starts scanning once the chain activates exits.
//!
//! Never sent to any cluster except the one `--config` names. The unit and integration tests exercise
//! [`rome_zk_exit_prover::core`], [`rome_zk_exit_prover::follower`] and [`rome_zk_exit_prover::run`] against
//! fixtures and fakes; `tests/binary_once.rs` also runs this binary itself, with `--once`, against fake Solana and L2
//! nodes on a loopback port.

use std::path::PathBuf;
use std::str::FromStr;

use clap::Parser;
use solana_program::pubkey::Pubkey;
use solana_signer::Signer;

use rome_zk_exit_prover::config::Config;
use rome_zk_exit_prover::follower::Follower;
use rome_zk_exit_prover::metrics::Metrics;
use rome_zk_exit_prover::release::{ReleaseContext, Releaser};
use rome_zk_exit_prover::rpc::HttpVerifierRpc;
use rome_zk_exit_prover::run::{
    exit_gate_with_tuning, poll_once, resolve_tuning, ExitGate, TuningError, WaitingLog,
};
use rome_zk_exit_prover::settlement::RpcSettlementReader;

#[derive(Parser)]
#[command(name = "rome-zk-exit-prover")]
struct Args {
    #[arg(long)]
    config: PathBuf,
    /// Run exactly one poll and exit — never a live loop.
    #[arg(long)]
    once: bool,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let cfg = Config::load(&args.config)?;
    let program_id = Pubkey::from_str(&cfg.settlement_program_id)?;

    let v1_payer = solana_keypair::read_keypair_file(&cfg.payer_key_path)
        .map_err(|e| anyhow::anyhow!("read payer key {}: {e}", cfg.payer_key_path.display()))?;
    let payer = rome_zk_solana_sender::compat::from_v1_pubkey(&v1_payer.pubkey());

    let sender = rome_zk_solana_sender::RpcSender::new(cfg.settlement_rpc.clone(), v1_payer);
    let settlement = RpcSettlementReader {
        client: solana_client::rpc_client::RpcClient::new(cfg.settlement_rpc.clone()),
        program_id,
        chain_id: cfg.chain_id,
    };
    let verifier = HttpVerifierRpc {
        url: cfg.verifier_rpc.clone(),
    };

    let metrics = Metrics::new();
    let max_tx_bytes = cfg.effective_max_proof_bytes();

    let rt = tokio::runtime::Runtime::new()?;

    let metrics_addr: std::net::SocketAddr = cfg.metrics_addr.parse()?;
    rt.block_on(async {
        let listener = rome_zk_metrics_http::bind(metrics_addr)
            .await
            .unwrap_or_else(|e| panic!("metrics responder cannot bind {metrics_addr}: {e}"));
        let m = metrics.clone();
        tokio::spawn(async move {
            let _ = rome_zk_metrics_http::serve_on(listener, move || m.render()).await;
        });
    });

    let mut follower = Follower::new(
        cfg.portal_from_block,
        cfg.max_send_attempts,
        cfg.max_window_requeues,
    );

    let release_ctx = ReleaseContext {
        program_id,
        chain_id: cfg.chain_id,
        payer,
        create_account_min_lamports: cfg.release_create_account_min_lamports,
    };
    let mut releaser = Releaser::new(cfg.auto_release);

    let mut logged_limit = None;
    let mut waiting_log = WaitingLog::default();
    loop {
        // What a ProveExit loads depends on the live program size, so the loaded-accounts limit is worked out
        // here on every poll. A configured value that is too small stops the process; accounts that cannot be
        // read yet only keep it idle.
        let tuning = match resolve_tuning(&cfg, &settlement.client, &program_id) {
            Ok(tuning) => {
                if logged_limit != Some(tuning.loaded_accounts_data_size_limit) {
                    logged_limit = Some(tuning.loaded_accounts_data_size_limit);
                    tracing::info!(
                        loaded_accounts_data_size_limit = tuning.loaded_accounts_data_size_limit,
                        "sending with this loaded-accounts limit"
                    );
                }
                Some(tuning)
            }
            Err(e @ TuningError::LimitTooLow(_)) => return Err(e.into()),
            Err(e) => {
                tracing::warn!(error = %e, "idle until the program accounts can be read");
                None
            }
        };
        // Asked on every poll: a chain that has not switched exits on leaves this process idle and healthy,
        // and an activation on chain is picked up without a restart.
        let (portal_hex, tuning) = match (
            exit_gate_with_tuning(&settlement, tuning.as_ref(), &metrics),
            tuning,
        ) {
            (ExitGate::Active { portal_hex }, Some(tuning)) => {
                waiting_log.active();
                (portal_hex, tuning)
            }
            (gate, _) => {
                let reason = match gate {
                    ExitGate::Idle(reason) => reason,
                    ExitGate::Active { .. } => "the send settings could not be worked out",
                };
                if waiting_log.should_log(reason) {
                    tracing::info!(reason, "exits are not active, waiting");
                }
                metrics.record_follower(&follower);
                if args.once {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(cfg.poll_interval_ms));
                continue;
            }
        };
        let report = rt.block_on(poll_once(
            &settlement,
            &verifier,
            &sender,
            &mut follower,
            &metrics,
            &portal_hex,
            &program_id,
            &payer,
            max_tx_bytes,
            tuning,
            cfg.max_log_range,
        ));
        tracing::debug!(?report, "poll_once");

        // Pay out what is proved. Proved exits found by this poll join the ones still open from earlier polls.
        for proved in &report.proved {
            releaser.note(proved);
        }
        rt.block_on(releaser.run(&settlement, &sender, &release_ctx, tuning, &metrics));

        if args.once {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(cfg.poll_interval_ms));
    }

    Ok(())
}
