//! Prints compute units consumed for every signature touching a given account (in slot order) — used
//! to build a CU table by pointing it at a finalized batch's own PDA. Not part of the
//! shipped binary.
//!
//! Usage: `cargo run -p rome-zk-batcher --example cu_report -- <account>`

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
use solana_client::rpc_config::RpcTransactionConfig;
use solana_program::pubkey::Pubkey;
use solana_transaction_status_client_types::UiTransactionEncoding;
use std::str::FromStr;

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let account = Pubkey::from_str(&argv[0]).expect("bad account");
    let rpc = RpcClient::new("https://api.devnet.solana.com".to_string());

    let sigs = rpc
        .get_signatures_for_address_with_config(
            &account,
            GetConfirmedSignaturesForAddress2Config::default(),
        )
        .await
        .expect("get_signatures_for_address");
    println!(
        "{} signatures touching {account} (newest first)",
        sigs.len()
    );

    let config = RpcTransactionConfig {
        encoding: Some(UiTransactionEncoding::Json),
        max_supported_transaction_version: Some(0),
        ..Default::default()
    };
    for s in sigs.iter().rev() {
        let sig = s.signature.parse().expect("parse signature");
        match rpc.get_transaction_with_config(&sig, config).await {
            Ok(t) => {
                let cu = t
                    .transaction
                    .meta
                    .as_ref()
                    .and_then(|m| Into::<Option<u64>>::into(m.compute_units_consumed.clone()));
                let ix_count = match &t.transaction.transaction {
                    solana_transaction_status_client_types::EncodedTransaction::Json(j) => {
                        match &j.message {
                            solana_transaction_status_client_types::UiMessage::Raw(m) => {
                                m.instructions.len()
                            }
                            solana_transaction_status_client_types::UiMessage::Parsed(m) => {
                                m.instructions.len()
                            }
                        }
                    }
                    _ => 0,
                };
                println!(
                    "{} slot={} ix_count={ix_count} cu={cu:?}",
                    s.signature, t.slot
                );
            }
            Err(e) => println!("{}: fetch failed: {e}", s.signature),
        }
    }
}
