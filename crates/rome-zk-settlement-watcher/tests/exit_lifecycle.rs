//! `ProveExit`/`ConsumeExit` decoded by name, and the `exit` table's proved -> released
//! lifecycle. A hand-built `Source` (not the Tiber fixture — Tiber has never posted
//! either instruction yet, so there is no real recorded history to replay) serving two real, builder-
//! compiled instructions, run through the real `ingest::run_once` + `lifecycle::derive_exit_once` against
//! a throwaway postgres container — same shape as `tests/finality.rs`'s own `TwoTxSource`.

mod support;

use rome_zk_settlement_watcher::cursor::ProgramKind;
use rome_zk_settlement_watcher::ingest::{run_once, WatcherConfig};
use rome_zk_settlement_watcher::lifecycle::{derive_exit_once, DeriveOutcome};
use rome_zk_settlement_watcher::{RawTx, SignatureInfo, Source, SourceError};
use solana_program::pubkey::Pubkey;
use solana_sdk::message::Message;
use solana_transaction_status_client_types::{TransactionConfirmationStatus, UiRawMessage};
use support::TestPg;

/// Compiles real builder `Instruction`s into a `UiRawMessage` — the exact shape a real `getTransaction`
/// call returns (mirrors `decode.rs`'s own private test helper of the same name and purpose).
fn raw_message(payer: &Pubkey, ixs: &[solana_program::instruction::Instruction]) -> UiRawMessage {
    let message = Message::new(ixs, Some(payer));
    UiRawMessage {
        header: solana_sdk::message::MessageHeader {
            num_required_signatures: message.header.num_required_signatures,
            num_readonly_signed_accounts: message.header.num_readonly_signed_accounts,
            num_readonly_unsigned_accounts: message.header.num_readonly_unsigned_accounts,
        },
        account_keys: message.account_keys.iter().map(|k| k.to_string()).collect(),
        recent_blockhash: message.recent_blockhash.to_string(),
        instructions: message
            .instructions
            .iter()
            .map(
                |ci| solana_transaction_status_client_types::UiCompiledInstruction {
                    program_id_index: ci.program_id_index,
                    accounts: ci.accounts.clone(),
                    data: bs58::encode(&ci.data).into_string(),
                    stack_height: None,
                },
            )
            .collect(),
        address_table_lookups: None,
        transaction_config: None,
    }
}

struct TwoTxSource {
    program_id: Pubkey,
    prove_sig: String,
    consume_sig: String,
    prove_msg: UiRawMessage,
    consume_msg: UiRawMessage,
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
        // Newest-first, matching the real RPC contract: ConsumeExit (slot 101) before ProveExit (slot 100).
        Ok(vec![
            SignatureInfo {
                signature: self.consume_sig.clone(),
                slot: 101,
                block_time: Some(1_700_000_101),
                err: false,
                confirmation_status: Some(TransactionConfirmationStatus::Finalized),
            },
            SignatureInfo {
                signature: self.prove_sig.clone(),
                slot: 100,
                block_time: Some(1_700_000_100),
                err: false,
                confirmation_status: Some(TransactionConfirmationStatus::Finalized),
            },
        ])
    }

    async fn get_transaction(&mut self, signature: &str) -> Result<Option<RawTx>, SourceError> {
        if signature == self.prove_sig {
            Ok(Some(RawTx {
                message: self.prove_msg.clone(),
                err: false,
            }))
        } else if signature == self.consume_sig {
            Ok(Some(RawTx {
                message: self.consume_msg.clone(),
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
            .map(|_| Some(TransactionConfirmationStatus::Finalized))
            .collect())
    }

    async fn current_slot(&mut self) -> Result<u64, SourceError> {
        Ok(u64::MAX)
    }
}

/// Builds one `TwoTxSource` around a real `ProveExit` (28) and its matching `ConsumeExit` (29) —
/// `message_hash` is re-derived the same way `decode::exit_message_hash` (and the on-chain program) does,
/// so the two instructions genuinely refer to the same exit.
fn build_source() -> (TwoTxSource, Pubkey, u64, [u8; 32], String, String) {
    let settlement_program = Pubkey::new_unique();
    let chain_id = 200_101u64;
    let batch = 5u64;
    let payer = Pubkey::new_unique();

    let message = zk_settlement_client::ExitMessageArg {
        nonce: 3,
        l2_sender: [0x11u8; 20],
        sol_recipient: [0x22u8; 32],
        asset: [0u8; 20],
        amount: 123_456_789_012_345u128,
    };
    let message_hash = rome_zk_layouts::exit::ExitMessage {
        nonce: message.nonce,
        l2_sender: message.l2_sender,
        sol_recipient: message.sol_recipient,
        asset: message.asset,
        amount: message.amount,
    }
    .message_hash();

    let prove_ix = zk_settlement_client::prove_exit_ix(
        &settlement_program,
        &payer,
        chain_id,
        batch,
        message,
        rome_zk_mpt::ExitProof {
            account_nodes: vec![],
            storage_nodes: vec![],
        },
        0,
        0,
    );
    let bridge_signer = Pubkey::new_unique();
    let exit_config = Pubkey::new_unique();
    let exit_record = Pubkey::new_unique();
    let payer_refund = Pubkey::new_unique();
    let consume_ix = zk_settlement_client::consume_exit_ix(
        &settlement_program,
        chain_id,
        message_hash,
        &bridge_signer,
        &exit_config,
        &exit_record,
        &payer_refund,
    );

    let prove_sig = "1".repeat(88);
    let consume_sig = "2".repeat(88);
    let source = TwoTxSource {
        program_id: settlement_program,
        prove_sig: prove_sig.clone(),
        consume_sig: consume_sig.clone(),
        prove_msg: raw_message(&payer, &[prove_ix]),
        consume_msg: raw_message(&payer, &[consume_ix]),
    };
    (
        source,
        settlement_program,
        chain_id,
        message_hash,
        prove_sig,
        consume_sig,
    )
}

#[tokio::test]
async fn watcher_decodes_prove_and_consume_exit_by_name() {
    let pg = TestPg::start().await;
    let (mut source, settlement_program, _chain_id, _message_hash, prove_sig, consume_sig) =
        build_source();

    run_once(
        &pg.pool,
        &mut source,
        &settlement_program,
        &settlement_program,
        ProgramKind::Root,
        WatcherConfig::default(),
    )
    .await
    .expect("ingest must succeed");

    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT st.sig, stp.ix_kind FROM settlement_tx st
         JOIN settlement_tx_program stp ON stp.settlement_tx_id = st.id
         WHERE st.sig = ANY($1)",
    )
    .bind(vec![prove_sig.clone(), consume_sig.clone()])
    .fetch_all(&pg.pool)
    .await
    .expect("query settlement_tx_program");

    let prove_kind = rows
        .iter()
        .find(|(sig, _)| *sig == prove_sig)
        .map(|(_, k)| k.clone());
    let consume_kind = rows
        .iter()
        .find(|(sig, _)| *sig == consume_sig)
        .map(|(_, k)| k.clone());
    assert_eq!(prove_kind.as_deref(), Some("ProveExit"));
    assert_eq!(consume_kind.as_deref(), Some("ConsumeExit"));
}

#[tokio::test]
async fn exit_row_lifecycle_proved_then_released() {
    let pg = TestPg::start().await;
    let (mut source, settlement_program, chain_id, message_hash, prove_sig, consume_sig) =
        build_source();

    run_once(
        &pg.pool,
        &mut source,
        &settlement_program,
        &settlement_program,
        ProgramKind::Root,
        WatcherConfig::default(),
    )
    .await
    .expect("ingest must succeed");

    // First derive pass: only ProveExit's effect should be visible if applied alone -- but since both
    // signatures land in the same ingest walk (one page), a single `derive_exit_once` call processes both
    // rows in true (slot, id) order in one pass. Call it in a loop, same as the bin's own main loop, so
    // this test does not depend on how many rows happen to fit under one internal LIMIT.
    while let DeriveOutcome::Processed { .. } = derive_exit_once(&pg.pool, 1_000)
        .await
        .expect("derive_exit_once must succeed")
    {}

    let row: (i64, Vec<u8>, String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT chain_id, message_hash, status, proved_sig, released_sig FROM exit WHERE message_hash = $1",
    )
    .bind(message_hash.to_vec())
    .fetch_one(&pg.pool)
    .await
    .expect("exactly one exit row must exist after both events are derived");

    assert_eq!(row.0, chain_id as i64);
    assert_eq!(row.1, message_hash.to_vec());
    assert_eq!(
        row.2, "released",
        "ProveExit then ConsumeExit must leave status=released"
    );
    assert_eq!(row.3.as_deref(), Some(prove_sig.as_str()));
    assert_eq!(row.4.as_deref(), Some(consume_sig.as_str()));

    let count: (i64,) = sqlx::query_as("SELECT count(*) FROM exit WHERE message_hash = $1")
        .bind(message_hash.to_vec())
        .fetch_one(&pg.pool)
        .await
        .unwrap();
    assert_eq!(
        count.0, 1,
        "one row, updated in place, never a second insert"
    );
}
