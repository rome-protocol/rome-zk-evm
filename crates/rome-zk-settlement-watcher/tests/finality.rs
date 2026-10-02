//! `finality::track_finality` ("confirmed -> finalized by re-checking Solana's
//! own status; a None status past a 150-slot finalized horizon -> dropped, terminal"). Uses a small
//! hand-built `Source` (not the Tiber fixture, which is already all-finalized real history and so cannot
//! exercise a transition) so both transitions are actually observed happening.

mod support;

use rome_zk_settlement_watcher::cursor::ProgramKind;
use rome_zk_settlement_watcher::finality::{track_finality, DROPPED_HORIZON_SLOTS};
use rome_zk_settlement_watcher::ingest::{run_once, WatcherConfig};
use rome_zk_settlement_watcher::{RawTx, SignatureInfo, Source, SourceError};
use solana_program::pubkey::Pubkey;
use solana_transaction_status_client_types::{TransactionConfirmationStatus, UiRawMessage};
use std::collections::HashMap;
use support::TestPg;

/// Two fixed signatures, both reported `Confirmed` at ingest time; `statuses` controls what
/// `get_signature_statuses` reports for each afterward (the finality pass's own read).
/// `current_slot_value` controls what `current_slot` reports (the `dropped`-horizon reference point).
struct TwoTxSource {
    program_id: Pubkey,
    sigs: [String; 2],
    statuses: HashMap<String, TransactionConfirmationStatus>,
    current_slot_value: u64,
}

fn empty_message() -> UiRawMessage {
    UiRawMessage {
        header: solana_sdk::message::MessageHeader {
            num_required_signatures: 1,
            num_readonly_signed_accounts: 0,
            num_readonly_unsigned_accounts: 0,
        },
        account_keys: vec![Pubkey::new_unique().to_string()],
        recent_blockhash: solana_program::hash::Hash::default().to_string(),
        instructions: vec![],
        address_table_lookups: None,
        transaction_config: None,
    }
}

impl Source for TwoTxSource {
    async fn get_signatures_for_address(
        &mut self,
        program_id: &Pubkey,
        _before: Option<String>,
        until: Option<String>,
        _limit: usize,
    ) -> Result<Vec<SignatureInfo>, SourceError> {
        if *program_id != self.program_id || until.is_some() {
            return Ok(vec![]);
        }
        Ok(self
            .sigs
            .iter()
            .enumerate()
            .map(|(i, sig)| SignatureInfo {
                signature: sig.clone(),
                slot: 1_000 + i as u64,
                block_time: Some(1_700_000_000 + i as i64),
                err: false,
                confirmation_status: Some(TransactionConfirmationStatus::Confirmed),
            })
            .collect())
    }

    async fn get_transaction(&mut self, signature: &str) -> Result<Option<RawTx>, SourceError> {
        if self.sigs.iter().any(|s| s == signature) {
            Ok(Some(RawTx {
                message: empty_message(),
                err: false,
            }))
        } else {
            Ok(None)
        }
    }

    async fn get_signature_statuses(
        &mut self,
        signatures: &[String],
    ) -> Result<Vec<Option<TransactionConfirmationStatus>>, SourceError> {
        Ok(signatures
            .iter()
            .map(|s| self.statuses.get(s).cloned())
            .collect())
    }

    async fn current_slot(&mut self) -> Result<u64, SourceError> {
        Ok(self.current_slot_value)
    }
}

#[tokio::test]
async fn track_finality_upgrades_only_the_signatures_solana_now_reports_finalized() {
    let pg = TestPg::start().await;
    let program_id = Pubkey::new_unique();
    let sigs = [
        "1111111111111111111111111111111111111111111111111111111111".to_string(),
        "2222222222222222222222222222222222222222222222222222222222".to_string(),
    ];

    let mut statuses = HashMap::new();
    statuses.insert(sigs[0].clone(), TransactionConfirmationStatus::Finalized);
    statuses.insert(sigs[1].clone(), TransactionConfirmationStatus::Confirmed);

    let mut source = TwoTxSource {
        program_id,
        sigs: sigs.clone(),
        statuses,
        current_slot_value: 1_000,
    };

    run_once(
        &pg.pool,
        &mut source,
        &program_id,
        ProgramKind::Inbox,
        WatcherConfig::default(),
    )
    .await
    .unwrap();

    let before: Vec<(String, String)> =
        sqlx::query_as("SELECT sig, status FROM settlement_tx ORDER BY sig")
            .fetch_all(&pg.pool)
            .await
            .unwrap();
    assert!(before.iter().all(|(_, s)| s == "confirmed"), "{before:?}");

    let outcome = track_finality(&pg.pool, &mut source, 10).await.unwrap();
    assert_eq!(outcome.checked, 2);
    assert_eq!(outcome.upgraded, 1);
    assert_eq!(outcome.dropped, 0);

    let after: HashMap<String, String> =
        sqlx::query_as::<_, (String, String)>("SELECT sig, status FROM settlement_tx")
            .fetch_all(&pg.pool)
            .await
            .unwrap()
            .into_iter()
            .collect();
    assert_eq!(after[&sigs[0]], "finalized");
    assert_eq!(after[&sigs[1]], "confirmed");

    // Idempotent: running it again with the same statuses upgrades nothing further.
    let outcome2 = track_finality(&pg.pool, &mut source, 10).await.unwrap();
    assert_eq!(
        outcome2.checked, 1,
        "only the still-not-finalized row is re-checked"
    );
    assert_eq!(outcome2.upgraded, 0);
    assert_eq!(outcome2.dropped, 0);
}

/// A signature `getSignatureStatuses` reports **no status at all** for (pruned, or never landed) stays
/// `confirmed` until it has been behind the node's own finalized slot for `DROPPED_HORIZON_SLOTS` slots,
/// then becomes the terminal `dropped` -- never re-checked again (without a terminal state this row would be
/// re-scanned on every pass forever).
#[tokio::test]
async fn track_finality_terminates_a_none_status_row_past_the_dropped_horizon() {
    let pg = TestPg::start().await;
    let program_id = Pubkey::new_unique();
    let sigs = [
        "3333333333333333333333333333333333333333333333333333333333".to_string(),
        "4444444444444444444444444444444444444444444444444444444444".to_string(),
    ];

    // Neither signature has any status Solana will report -- both are "pruned/never landed" from
    // `get_signature_statuses`'s point of view.
    let mut source = TwoTxSource {
        program_id,
        sigs: sigs.clone(),
        statuses: HashMap::new(),
        current_slot_value: 1_000 + DROPPED_HORIZON_SLOTS - 1, // sig[0] is at slot 1_000, still inside the horizon
    };
    run_once(
        &pg.pool,
        &mut source,
        &program_id,
        ProgramKind::Inbox,
        WatcherConfig::default(),
    )
    .await
    .unwrap();

    // Still inside the horizon: neither row moves.
    let outcome = track_finality(&pg.pool, &mut source, 10).await.unwrap();
    assert_eq!(outcome.checked, 2);
    assert_eq!(outcome.upgraded, 0);
    assert_eq!(outcome.dropped, 0);
    let statuses_now: HashMap<String, String> =
        sqlx::query_as::<_, (String, String)>("SELECT sig, status FROM settlement_tx")
            .fetch_all(&pg.pool)
            .await
            .unwrap()
            .into_iter()
            .collect();
    assert!(statuses_now.values().all(|s| s == "confirmed"));

    // Now past the horizon for both (sigs[0] at slot 1_000, sigs[1] at slot 1_001 -- +2 clears the
    // horizon for the later of the two as well, not just the earlier).
    source.current_slot_value = 1_000 + DROPPED_HORIZON_SLOTS + 2;
    let outcome2 = track_finality(&pg.pool, &mut source, 10).await.unwrap();
    assert_eq!(outcome2.upgraded, 0);
    assert_eq!(outcome2.dropped, 2);
    let statuses_after: HashMap<String, String> =
        sqlx::query_as::<_, (String, String)>("SELECT sig, status FROM settlement_tx")
            .fetch_all(&pg.pool)
            .await
            .unwrap()
            .into_iter()
            .collect();
    assert!(statuses_after.values().all(|s| s == "dropped"));

    // Terminal: a third pass re-checks nothing.
    let outcome3 = track_finality(&pg.pool, &mut source, 10).await.unwrap();
    assert_eq!(outcome3.checked, 0, "dropped rows must never be re-checked");
}
