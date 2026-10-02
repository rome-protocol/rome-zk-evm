//! `rome-zk-exit-prover --config exit-prover.toml`: a thin, wire-only CLI that
//! wires the real RPC/sender implementations and drives [`rome_zk_exit_prover::follower::Follower`] over
//! every `ExitInitiated` log the L2 verifier reports. Every step — refuse-before-send (including the
//! pre-send nullifier read), classify, and retry/stuck routing — lives in
//! [`rome_zk_exit_prover::core::attempt_exit`] and [`rome_zk_exit_prover::follower::Follower`]; this
//! binary only wires the four calls together: `ingest` → `due` → `attempt_exit` → `apply`.
//!
//! `--once` runs exactly one poll (one `eth_getLogs` call, `attempt_exit` on every due message) and exits
//! — useful for a smoke check or a cron-driven run. Without it, the loop polls forever every
//! `poll_interval_ms` until the process is signalled to stop.
//!
//! The follower's own state (`scan_from_block`, `pending`, `stuck`) is never persisted: a restart starts
//! a fresh [`rome_zk_exit_prover::follower::Follower`] at `cfg.portal_from_block` and rebuilds everything
//! it needs from `eth_getLogs` plus the on-chain nullifier bit — never a disk cursor.
//!
//! Never sent to any cluster except the one `--config` names; the CI/test suite exercises
//! [`rome_zk_exit_prover::core`] and [`rome_zk_exit_prover::follower`] directly against fixtures and
//! fakes, never this binary.

use std::path::PathBuf;
use std::str::FromStr;

use clap::Parser;
use solana_program::pubkey::Pubkey;
use solana_signer::Signer;

use rome_zk_exit_prover::config::Config;
use rome_zk_exit_prover::follower::Follower;
use rome_zk_exit_prover::metrics::Metrics;
use rome_zk_exit_prover::rpc::HttpVerifierRpc;
use rome_zk_exit_prover::run::poll_once;
use rome_zk_exit_prover::settlement::{RpcSettlementReader, SettlementReader};

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
    let tuning = cfg.send_tuning();
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

    let portal_hex = {
        let exit_config = settlement.read_exit_config()?;
        format!("0x{}", hex::encode(exit_config.exit_portal))
    };

    let mut follower = Follower::new(
        cfg.portal_from_block,
        cfg.max_send_attempts,
        cfg.max_window_requeues,
    );

    loop {
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
        ));
        tracing::debug!(?report, "poll_once");

        if args.once {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(cfg.poll_interval_ms));
    }

    Ok(())
}
