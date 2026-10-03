//! `init-cursor`: the one-time bring-up of a chain's `batch_cursor` in the inbox program. Every `OpenBatch`
//! needs the cursor, and nothing else creates it.
//!
//! Authority-gated like `OpenBatch`: the signer must be the chain's authority on the settlement program's root
//! account. A cursor that already exists is reported and nothing is sent, so a re-run after a partial failure
//! completes. `--next-batch` is 1 for a fresh chain (batch ids are 1-based; 0 is the sentinel everywhere else); on
//! a chain with batch history it must be `root.head_pending_batch + 1`, never above it, because a higher cursor
//! halts the chain. `--next-batch 0` is refused by name unless `--allow-zero` is also passed.

use crate::chain::Chain;
use crate::commands::{execute, read, Read};
use crate::error::{Mode, OpsError, Report};
use crate::keys::{self, Signers};
use solana_program::pubkey::Pubkey;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct InitCursorRequest {
    pub keypair: PathBuf,
    pub inbox: Pubkey,
    pub settlement: Pubkey,
    pub chain_id: u64,
    pub next_batch: u64,
    pub allow_zero: bool,
}

/// Batch ids are 1-based; 0 is the sentinel everywhere else in this system (`head_pending_batch` and
/// `head_final_batch` equal to 0 mean "none"). The inbox program's `InitBatchCursor` accepts any value, and a
/// shipped instruction body does not change, so the guard belongs here. A cursor bootstrapped at 0 can never post
/// its first batch through settlement.
pub fn validate_next_batch(next_batch: u64, allow_zero: bool) -> Result<(), OpsError> {
    if next_batch == 0 && !allow_zero {
        return Err(OpsError::usage(
            "NextBatchZero",
            "--next-batch 0 refused (batch ids are 1-based; 0 is the sentinel everywhere else in this system); pass --allow-zero to force it anyway",
        ));
    }
    Ok(())
}

pub async fn run<C: Chain>(
    chain: &C,
    req: InitCursorRequest,
    mode: Mode,
) -> Result<Report, OpsError> {
    validate_next_batch(req.next_batch, req.allow_zero)?;
    let authority = keys::load(&req.keypair, "--keypair")?;
    let mut report = Report::default();
    let chain_id = req.chain_id;

    let (cursor, _) = zk_inbox_client::cursor_pda(&req.inbox, &req.settlement, chain_id);
    match read(
        chain,
        mode,
        &cursor,
        "the batch_cursor",
        "CursorLookupFailed",
        &mut report,
    )
    .await?
    {
        Read::Found(d) => {
            report.line(format!(
                "chain {chain_id} already has a batch_cursor at {cursor} ({:?}); already initialised, nothing sent",
                zk_inbox_client::decode_batch_cursor(&d)
            ));
            return Ok(report);
        }
        Read::Missing | Read::Unavailable => {}
    }

    let ix = zk_inbox_client::init_batch_cursor_ix(
        &req.inbox,
        &keys::pubkey(&authority),
        chain_id,
        req.next_batch,
        &req.settlement,
    );
    let signers = Signers::new(authority, vec![]);
    match execute(chain, mode, "InitBatchCursor", ix, &signers, &mut report).await? {
        Some(sig) => report.line(format!(
            "InitBatchCursor: chain {chain_id} cursor {cursor} next_batch {}, sig {sig}",
            req.next_batch
        )),
        None => report.line(format!(
            "  would create the batch_cursor {cursor} with next_batch {}; pass --confirm to send",
            req.next_batch
        )),
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::fake::*;

    fn req(path: PathBuf) -> InitCursorRequest {
        InitCursorRequest {
            keypair: path,
            inbox: inbox(),
            settlement: program(),
            chain_id: 7,
            next_batch: 1,
            allow_zero: false,
        }
    }

    fn cursor_account(chain_id: u64, next_batch: u64) -> Vec<u8> {
        rome_zk_layouts::cursor::write(&rome_zk_layouts::cursor::CursorFields {
            chain_id,
            next_batch,
            deposit: None,
        })
        .to_vec()
    }

    #[tokio::test]
    async fn dry_run_sends_nothing() {
        let (_, path) = key_file("c1");
        let chain = FakeChain::default();
        let report = run(&chain, req(path.clone()), Mode::Dry).await.unwrap();
        assert_eq!(chain.sent_count(), 0);
        assert!(report
            .lines
            .join("\n")
            .contains("nothing sent to any cluster"));
        remove(&path);
    }

    #[tokio::test]
    async fn a_missing_cursor_is_created_on_confirm() {
        let (_, path) = key_file("c2");
        let chain = FakeChain::default();
        let report = run(&chain, req(path.clone()), Mode::Confirm).await.unwrap();
        assert_eq!(chain.sent_count(), 1);
        assert!(
            report
                .lines
                .last()
                .unwrap()
                .starts_with("InitBatchCursor: chain 7 cursor"),
            "{:?}",
            report.lines
        );
        remove(&path);
    }

    #[tokio::test]
    async fn an_existing_cursor_is_reported_as_initialised_not_refused() {
        let (_, path) = key_file("c3");
        let (cursor, _) = zk_inbox_client::cursor_pda(&inbox(), &program(), 7);
        let chain = FakeChain::default().with(cursor, cursor_account(7, 4));
        let report = run(&chain, req(path.clone()), Mode::Confirm).await.unwrap();
        assert_eq!(chain.sent_count(), 0);
        let text = report.lines.join("\n");
        assert!(text.contains("already initialised, nothing sent"), "{text}");
        assert!(text.contains("next_batch: 4"), "{text}");
        remove(&path);
    }

    #[tokio::test]
    async fn a_failed_read_says_it_is_not_a_missing_account_and_sends_nothing() {
        let (_, path) = key_file("c4");
        let (cursor, _) = zk_inbox_client::cursor_pda(&inbox(), &program(), 7);
        let chain =
            FakeChain::default().failing(cursor, "AccountNotFound: pubkey=Abc: connection refused");
        let err = run(&chain, req(path.clone()), Mode::Confirm)
            .await
            .unwrap_err();
        assert_eq!(err.name, "CursorLookupFailed");
        let ours = err
            .detail
            .find("the RPC request failed")
            .expect(&err.detail);
        let theirs = err.detail.find("AccountNotFound").expect(&err.detail);
        assert!(ours < theirs, "{err}");
        assert!(err.detail.contains("not a missing account"), "{err}");
        assert_eq!(chain.sent_count(), 0);
        remove(&path);
    }

    #[tokio::test]
    async fn next_batch_zero_is_refused_unless_allow_zero_is_set() {
        let (_, path) = key_file("c5");
        let chain = FakeChain::default();
        let mut r = req(path.clone());
        r.next_batch = 0;
        let err = run(&chain, r.clone(), Mode::Confirm).await.unwrap_err();
        assert_eq!(err.name, "NextBatchZero");
        assert_eq!(chain.sent_count(), 0);
        r.allow_zero = true;
        run(&chain, r, Mode::Confirm).await.unwrap();
        assert_eq!(chain.sent_count(), 1);
        remove(&path);
    }
}
