//! `refund-deposit`: give a chain's registration deposit back to its authority. The program lets anyone send this
//! once it is due, and credits only `root.authority`, so the fee payer here can be any funded key. The command
//! checks the same rules the program does, so a refusal names the reason before a fee is spent.

use crate::chain::Chain;
use crate::commands::{execute, read, Read};
use crate::error::{Mode, OpsError, Report};
use crate::keys::Signers;
use solana_keypair::Keypair;
use solana_program::pubkey::Pubkey;

/// The program refunds once a batch has finalized, or ten batches have been posted.
pub const REFUND_AFTER_POSTED_BATCHES: u32 = 10;

pub async fn run<C: Chain>(
    chain: &C,
    settlement: &Pubkey,
    chain_id: u64,
    payer: Keypair,
    mode: Mode,
) -> Result<Report, OpsError> {
    let mut report = Report::default();
    let (cc_pda, _) = zk_settlement_client::chain_config_pda(settlement, chain_id);
    let (root_pda, _) = zk_settlement_client::root_pda(settlement, chain_id);

    let root = match read(
        chain,
        mode,
        &root_pda,
        "the root account",
        "RootLookupFailed",
        &mut report,
    )
    .await?
    {
        Read::Found(d) => Some(zk_settlement_client::decode_root_account(&d).map_err(|e| {
            OpsError::chain(
                "RootUndecodable",
                format!("the root account {root_pda} does not decode: {e}"),
            )
        })?),
        Read::Missing => {
            return Err(OpsError::chain(
                "ChainNotRegistered",
                format!(
                    "chain {chain_id} has no root account {root_pda} under this settlement program"
                ),
            ))
        }
        Read::Unavailable => None,
    };
    let cfg = match read(chain, mode, &cc_pda, "the chain_config account", "ChainConfigLookupFailed", &mut report).await? {
        Read::Found(d) => Some(zk_settlement_client::decode_chain_config_account(&d).map_err(|e| {
            OpsError::chain(
                "ChainConfigUndecodable",
                format!("the chain_config account {cc_pda} does not decode: {e}"),
            )
        })?),
        Read::Missing => {
            return Err(OpsError::chain(
                "ChainConfigMissing",
                format!("chain {chain_id} has no chain_config account {cc_pda}; a chain that predates it needs `migrate` first"),
            ))
        }
        Read::Unavailable => None,
    };

    if let Some(cfg) = &cfg {
        if cfg.deposit_lamports == 0 || cfg.deposit_refunded {
            return Err(OpsError::chain(
                "NoDepositToRefund",
                format!(
                    "chain {chain_id} has no deposit to refund (deposit_lamports {}, deposit_refunded {})",
                    cfg.deposit_lamports, cfg.deposit_refunded
                ),
            ));
        }
    }
    if let (Some(cfg), Some(root)) = (&cfg, &root) {
        let eligible =
            root.head_final_batch >= 1 || cfg.posted_batches >= REFUND_AFTER_POSTED_BATCHES;
        if !eligible {
            return Err(OpsError::chain(
                "RefundNotYetEligible",
                format!(
                    "the deposit is due once a batch has finalized or {REFUND_AFTER_POSTED_BATCHES} batches are posted; chain {chain_id} has head_final_batch {} and posted_batches {}",
                    root.head_final_batch, cfg.posted_batches
                ),
            ));
        }
    }

    // Only `root.authority` is ever credited. Without the root (an unreadable RPC in a dry run) the authority is
    // not known, so the dry run stops here rather than build a transaction with a guessed account.
    let Some(root) = root else {
        return Err(OpsError::chain(
            "RootLookupFailed",
            "the chain authority is read from the root account, which could not be read",
        ));
    };
    let ix = zk_settlement_client::refund_deposit_ix(settlement, chain_id, &root.authority);
    let signers = Signers::new(payer, vec![]);
    let amount = cfg.as_ref().map(|c| c.deposit_lamports);
    match execute(chain, mode, "RefundDeposit", ix, &signers, &mut report).await? {
        Some(sig) => report.line(format!(
            "RefundDeposit: chain {chain_id} deposit {} lamports to {}, sig {sig}",
            amount.map_or("?".to_string(), |a| a.to_string()),
            root.authority
        )),
        None => report.line(format!(
            "  would refund {} lamports to {} (the chain authority); pass --confirm to send",
            amount.map_or("?".to_string(), |a| a.to_string()),
            root.authority
        )),
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::fake::*;
    fn world(
        deposit: u64,
        refunded: bool,
        posted: u32,
        head_final: u64,
    ) -> (FakeChain, Pubkey, Keypair) {
        let (payer, path) = key_file("refund");
        remove(&path);
        let authority = Pubkey::new_from_array([4u8; 32]);
        let (cc, _) = zk_settlement_client::chain_config_pda(&program(), 77);
        let (rt, _) = zk_settlement_client::root_pda(&program(), 77);
        let chain = FakeChain::default()
            .with(
                cc,
                chain_config_bytes(77, deposit, refunded, posted, Some(60)),
            )
            .with(rt, root_bytes(77, authority, head_final, head_final));
        (chain, authority, payer)
    }

    #[tokio::test]
    async fn dry_run_sends_nothing_and_names_the_authority() {
        let (chain, authority, payer) = world(5_000_000_000, false, 0, 1);
        let report = run(&chain, &program(), 77, payer, Mode::Dry).await.unwrap();
        assert_eq!(chain.sent_count(), 0);
        assert!(!report.sent());
        let text = report.lines.join("\n");
        assert!(text.contains(&authority.to_string()), "{text}");
        assert!(text.contains("5000000000"), "{text}");
    }

    #[tokio::test]
    async fn confirm_sends_one_refund_to_the_root_authority() {
        let (chain, authority, payer) = world(5_000_000_000, false, 0, 1);
        let report = run(&chain, &program(), 77, payer, Mode::Confirm)
            .await
            .unwrap();
        assert_eq!(chain.sent_count(), 1);
        let ix = chain.sent.lock().unwrap()[0].clone();
        assert!(ix
            .accounts
            .iter()
            .any(|a| a.pubkey == authority && a.is_writable));
        assert!(report.sent());
    }

    #[tokio::test]
    async fn ten_posted_batches_make_the_refund_due_without_a_finalized_one() {
        let (chain, _, payer) = world(5_000_000_000, false, 10, 0);
        run(&chain, &program(), 77, payer, Mode::Confirm)
            .await
            .unwrap();
        assert_eq!(chain.sent_count(), 1);
    }

    #[tokio::test]
    async fn not_yet_eligible_is_refused_by_name() {
        let (chain, _, payer) = world(5_000_000_000, false, 9, 0);
        let err = run(&chain, &program(), 77, payer, Mode::Confirm)
            .await
            .unwrap_err();
        assert_eq!(err.name, "RefundNotYetEligible");
        assert_eq!(chain.sent_count(), 0);
    }

    #[tokio::test]
    async fn an_already_refunded_deposit_is_refused_by_name() {
        let (chain, _, payer) = world(0, true, 20, 3);
        let err = run(&chain, &program(), 77, payer, Mode::Confirm)
            .await
            .unwrap_err();
        assert_eq!(err.name, "NoDepositToRefund");
        assert_eq!(chain.sent_count(), 0);
    }

    #[tokio::test]
    async fn an_unknown_chain_is_refused_by_name() {
        let (payer, path) = key_file("refund-none");
        remove(&path);
        let chain = FakeChain::default();
        let err = run(&chain, &program(), 77, payer, Mode::Confirm)
            .await
            .unwrap_err();
        assert_eq!(err.name, "ChainNotRegistered");
        assert_eq!(chain.sent_count(), 0);
    }

    #[tokio::test]
    async fn a_missing_chain_config_is_refused_by_name() {
        let (payer, path) = key_file("refund-nocc");
        remove(&path);
        let (rt, _) = zk_settlement_client::root_pda(&program(), 77);
        let chain = FakeChain::default().with(rt, root_bytes(77, Pubkey::new_unique(), 1, 1));
        let err = run(&chain, &program(), 77, payer, Mode::Confirm)
            .await
            .unwrap_err();
        assert_eq!(err.name, "ChainConfigMissing");
    }

    #[tokio::test]
    async fn a_failed_read_is_a_refusal_in_confirm_mode_not_a_missing_chain() {
        let (payer, path) = key_file("refund-rpc");
        remove(&path);
        let (rt, _) = zk_settlement_client::root_pda(&program(), 77);
        let chain = FakeChain::default().failing(rt, "connection refused");
        let err = run(&chain, &program(), 77, payer, Mode::Confirm)
            .await
            .unwrap_err();
        assert_eq!(err.name, "RootLookupFailed");
        assert!(err.detail.contains("not a missing account"), "{err}");
        assert_eq!(chain.sent_count(), 0);
    }
}
