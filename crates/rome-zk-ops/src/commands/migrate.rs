//! `migrate`: `MigrateChainV2`, the one-time bring-forward for a chain that predates the `chain_config` account.
//! Registry-authority only. It creates `chain_config` v2 against the already-live `root` and `registry` accounts,
//! which stay untouched, and needs `InitGlobalConfig` to have run for the program deployment.

use crate::chain::Chain;
use crate::commands::{execute, read, Read};
use crate::error::{Mode, OpsError, Report};
use crate::keys::{self, Signers};
use solana_program::pubkey::Pubkey;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct MigrateRequest {
    pub settlement: Pubkey,
    pub chain_id: u64,
    pub registry_keypair: PathBuf,
    pub payer_keypair: PathBuf,
    /// Required, no default: an operations value, never a program constant.
    pub max_drift_secs: u64,
}

pub async fn run<C: Chain>(chain: &C, req: MigrateRequest, mode: Mode) -> Result<Report, OpsError> {
    if req.max_drift_secs == 0 {
        return Err(OpsError::usage(
            "DriftBoundZero",
            "--max-drift-secs 0 is refused: a chain with no drift bound cannot check a proof's timestamp",
        ));
    }
    let registry = keys::load(&req.registry_keypair, "--registry-keypair")?;
    let payer = keys::load(&req.payer_keypair, "--payer-keypair")?;
    let chain_id = req.chain_id;
    let mut report = Report::default();

    let (cc_pda, _) = zk_settlement_client::chain_config_pda(&req.settlement, chain_id);
    match read(
        chain,
        mode,
        &cc_pda,
        "the chain_config account",
        "ChainConfigLookupFailed",
        &mut report,
    )
    .await?
    {
        Read::Found(d) => {
            let cfg = zk_settlement_client::decode_chain_config_account(&d).map_err(|e| {
                OpsError::chain(
                    "ChainConfigUndecodable",
                    format!("the chain_config account {cc_pda} does not decode: {e}"),
                )
            })?;
            if cfg.max_drift_secs.is_some() {
                // Idempotent: a chain already on v2 prints and stops rather than send a transaction that fails.
                report.line(format!(
                    "chain {chain_id} already migrated to v2: chain_config {cc_pda}"
                ));
                return Ok(report);
            }
            report.line(format!(
                "chain {chain_id} has a v1 chain_config {cc_pda} - migrating to v2"
            ));
        }
        Read::Missing | Read::Unavailable => {}
    }

    let ix = zk_settlement_client::migrate_chain_ix(
        &req.settlement,
        &keys::pubkey(&registry),
        &keys::pubkey(&payer),
        chain_id,
        req.max_drift_secs,
    );
    let signers = Signers::new(payer, vec![registry]);
    match execute(chain, mode, "MigrateChainV2", ix, &signers, &mut report).await? {
        Some(sig) => report.line(format!("chain {chain_id} migrated: chain_config {cc_pda}, sig {sig}")),
        None => report.line(format!(
            "  would migrate chain {chain_id} to chain_config v2 with max_drift_secs {}; pass --confirm to send",
            req.max_drift_secs
        )),
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::fake::*;

    fn req(tag: &str) -> (MigrateRequest, Vec<PathBuf>) {
        let (_, reg) = key_file(&format!("{tag}-r"));
        let (_, pay) = key_file(&format!("{tag}-p"));
        (
            MigrateRequest {
                settlement: program(),
                chain_id: 200_101,
                registry_keypair: reg.clone(),
                payer_keypair: pay.clone(),
                max_drift_secs: 60,
            },
            vec![reg, pay],
        )
    }

    fn clean(paths: Vec<PathBuf>) {
        paths.iter().for_each(|p| remove(p));
    }

    #[tokio::test]
    async fn dry_run_sends_nothing_and_says_what_it_would_do() {
        let (r, files) = req("m1");
        let chain = FakeChain::default();
        let report = run(&chain, r, Mode::Dry).await.unwrap();
        assert_eq!(chain.sent_count(), 0);
        let text = report.lines.join("\n");
        assert!(text.contains("nothing sent to any cluster"), "{text}");
        assert!(text.contains("pass --confirm to send"), "{text}");
        clean(files);
    }

    #[tokio::test]
    async fn confirm_migrates_a_chain_with_no_chain_config() {
        let (r, files) = req("m2");
        let chain = FakeChain::default();
        let report = run(&chain, r, Mode::Confirm).await.unwrap();
        assert_eq!(chain.sent_count(), 1);
        assert!(
            report
                .lines
                .last()
                .unwrap()
                .contains("migrated: chain_config"),
            "{:?}",
            report.lines
        );
        clean(files);
    }

    #[tokio::test]
    async fn a_v1_chain_config_is_migrated() {
        let (r, files) = req("m3");
        let (cc, _) = zk_settlement_client::chain_config_pda(&program(), 200_101);
        let chain = FakeChain::default().with(cc, chain_config_bytes(200_101, 0, false, 0, None));
        let report = run(&chain, r, Mode::Confirm).await.unwrap();
        assert_eq!(chain.sent_count(), 1);
        assert!(
            report.lines[0].contains("has a v1 chain_config"),
            "{:?}",
            report.lines
        );
        clean(files);
    }

    #[tokio::test]
    async fn a_chain_already_on_v2_is_reported_and_nothing_is_sent() {
        let (r, files) = req("m4");
        let (cc, _) = zk_settlement_client::chain_config_pda(&program(), 200_101);
        let chain =
            FakeChain::default().with(cc, chain_config_bytes(200_101, 0, false, 0, Some(60)));
        let report = run(&chain, r, Mode::Confirm).await.unwrap();
        assert_eq!(chain.sent_count(), 0);
        assert!(
            report.lines[0].contains("already migrated to v2"),
            "{:?}",
            report.lines
        );
        clean(files);
    }

    #[tokio::test]
    async fn a_failed_lookup_is_a_refusal_not_a_guess() {
        let (r, files) = req("m5");
        let (cc, _) = zk_settlement_client::chain_config_pda(&program(), 200_101);
        let chain = FakeChain::default().failing(cc, "connection reset");
        let err = run(&chain, r, Mode::Confirm).await.unwrap_err();
        assert_eq!(err.name, "ChainConfigLookupFailed");
        assert_eq!(chain.sent_count(), 0);
        clean(files);
    }

    #[tokio::test]
    async fn a_zero_drift_bound_is_refused_by_name() {
        let (mut r, files) = req("m6");
        r.max_drift_secs = 0;
        let chain = FakeChain::default();
        let err = run(&chain, r, Mode::Confirm).await.unwrap_err();
        assert_eq!(err.name, "DriftBoundZero");
        assert_eq!(chain.sent_count(), 0);
        clean(files);
    }
}
