//! Read-only probes against the real Tiber devnet programs (no transaction sent, no SOL spent, no keys
//! read) -- one read-only pass against Tiber's programs (signature count, wall time).
//! Ignored by default (hits a real network); run manually with
//! `cargo test -p rome-zk-settlement-watcher --test devnet_probe -- --ignored --nocapture`. Mirrors
//! `rome-zk-batcher/tests/devnet_probe.rs`'s own pattern and public endpoint.

use rome_zk_settlement_watcher::{RpcSource, Source};
use solana_program::pubkey::Pubkey;
use std::str::FromStr;
use std::time::Instant;

const DEVNET_INBOX_PROGRAM_ID: &str = "EtAXw56BCmxH4Ew9JdLTjmxq21UadQjAoyLWERVNzBdL";
/// The real signature this crate's fixture recorded for batch 4005's `FinalizeBatch` call (slot
/// 496441350) -- if this probe ever finds it missing from live history, the fixture (and this file's own
/// comments about it) are stale and due for a re-record.
const KNOWN_FINALIZE_BATCH_SIG: &str =
    "21qoyNXu6jmn6JpfEE9fL7ZPMTkKhrfy4gA6zKXAEwZ6WqMKJHT6WAPZabRvY3Yqcf86Pf6BeGvHKvd5eMTBGMVo";

#[tokio::test]
#[ignore = "hits real devnet RPC; read-only, no keys, no SOL spent"]
async fn get_transaction_finds_a_real_recorded_signature_and_decodes_its_message() {
    let mut source = RpcSource::new(vec!["https://api.devnet.solana.com".to_string()]);
    let started = Instant::now();
    let tx = source
        .get_transaction(KNOWN_FINALIZE_BATCH_SIG)
        .await
        .expect("RPC call must succeed")
        .expect("this signature is real, recorded Tiber devnet history");
    println!(
        "getTransaction({KNOWN_FINALIZE_BATCH_SIG}) in {:?}: {} account keys, {} instructions",
        started.elapsed(),
        tx.message.account_keys.len(),
        tx.message.instructions.len()
    );
    let inbox_program = DEVNET_INBOX_PROGRAM_ID;
    assert!(
        tx.message.account_keys.iter().any(|k| k == inbox_program),
        "the inbox program must appear in its own FinalizeBatch transaction"
    );
}

#[tokio::test]
#[ignore = "hits real devnet RPC; read-only, no keys, no SOL spent"]
async fn get_transaction_returns_none_for_a_signature_the_chain_never_saw() {
    // The all-zero 64-byte signature -- a syntactically valid Signature (base58 of 64 zero bytes) that
    // was never a real transaction -- proves the None-on-missing path (rpc.rs's raw
    // `send::<Option<_>>` rather than the typed `get_transaction_with_config`) against a real node, not
    // just this crate's own fixture.
    let never_seen = solana_sdk::signature::Signature::default().to_string();
    let mut source = RpcSource::new(vec!["https://api.devnet.solana.com".to_string()]);
    let started = Instant::now();
    let result = source.get_transaction(&never_seen).await;
    let found = result.unwrap();
    println!(
        "getTransaction(never-seen) in {:?}: found={}",
        started.elapsed(),
        found.is_some()
    );
    assert!(found.is_none());
}

#[tokio::test]
#[ignore = "hits real devnet RPC; read-only, no keys, no SOL spent"]
async fn get_signatures_for_address_pages_the_real_inbox_program_history() {
    let program_id = Pubkey::from_str(DEVNET_INBOX_PROGRAM_ID).unwrap();
    let mut source = RpcSource::new(vec!["https://api.devnet.solana.com".to_string()]);
    let started = Instant::now();
    let page = source
        .get_signatures_for_address(&program_id, None, None, 50)
        .await
        .expect("RPC call must succeed");
    println!(
        "getSignaturesForAddress(inbox, limit=50) in {:?}: {} signatures, newest slot {:?}",
        started.elapsed(),
        page.len(),
        page.first().map(|s| s.slot)
    );
    assert_eq!(
        page.len(),
        50,
        "the real inbox program has well over 50 signatures"
    );
}
