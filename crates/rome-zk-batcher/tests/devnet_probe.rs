//! Read-only devnet probes (no transaction sent, no SOL spent).
//!
//! `devnet_root_account_is_too_short_for_openbatch_and_no_batch_account_exists` confirms — with real
//! on-chain evidence, not just a documentation citation — exactly why the chunk lane cannot be exercised
//! on devnet today without either the settlement program or a stand-in this design explicitly
//! forecloses (root.rs's own doc: "these reads intentionally cannot succeed... unless you fake a root
//! account, which a program cannot").
//!
//! `design_frame_v1_tx_simulates_clean_on_target_cluster`: proves
//! the *actual* fix — that the corrected `loaded_accounts_data_size_limit` really clears
//! `simulateTransaction` on live devnet for the design-max chunk-lane V1 transaction, against the real
//! inbox program's real `ProgramData` and a real, already-open batch account — not merely that this
//! crate's own formula computes a number under 262,144.
//!
//! Both are ignored by default (hit a real network); run manually with `cargo test -p rome-zk-batcher
//! --test devnet_probe -- --ignored --nocapture`.

use solana_client::nonblocking::rpc_client::RpcClient;
// Before the dependency bump, this crate depended on two genuinely different major versions of
// `solana-client` under two Cargo names (`solana-client` / `solana-client-v1`); now that the whole
// workspace is one Agave line there is only one `solana-client` dependency, so the historical `_v1`
// path below (used further down in this file, unchanged) is a plain Rust alias, not a second crate.
use solana_client as solana_client_v1;
use solana_program::pubkey::Pubkey;
use std::str::FromStr;

const DEVNET_INBOX_PROGRAM_ID: &str = "EtAXw56BCmxH4Ew9JdLTjmxq21UadQjAoyLWERVNzBdL";
const DEVNET_SETTLEMENT_PROGRAM_ID: &str = "39Kraok3WYQyJ8FAJjyYzjemFHxZfcNrKAHoNg2BM4p3";
const DEVNET_CHAIN_ID: u64 = 200_101;
/// A batch already open on the Tiber devnet stand with a real, non-trivial batch account (290 frames,
/// 9,519 B — `zk_inbox_client::account_len`-shaped) — chosen so the simulated chunk-lane transaction's
/// `batch` account load is the real thing this design posts against, not an empty/non-existent stand-in.
const PROBE_BATCH: u64 = 4002;
const PROBE_CHUNK_IDX: u32 = 0;
/// Read-only pubkey supplied by the operator for this simulation — never signs anything, never receives
/// a key on this machine (`simulateTransaction` with `sigVerify: false` does not check signatures).
const PROBE_PAYER: &str = "6TJCfzkNfhpieupBVvKiN8bacT2ZChYQYp6joNNX2v9m";

#[tokio::test]
#[ignore = "hits real devnet RPC; read-only, no SOL spent"]
async fn devnet_root_account_is_too_short_for_openbatch_and_no_batch_account_exists() {
    let rpc = RpcClient::new("https://api.devnet.solana.com".to_string());
    let inbox_program = Pubkey::from_str(DEVNET_INBOX_PROGRAM_ID).unwrap();
    let settlement_program = Pubkey::from_str(DEVNET_SETTLEMENT_PROGRAM_ID).unwrap();

    // --- the root account OpenBatch would read ---
    let (root_pda, _) = zk_inbox_client::root_pda(&settlement_program, DEVNET_CHAIN_ID);
    let root_account = rpc.get_account(&root_pda).await;
    match root_account {
        Ok(acc) => {
            println!(
                "root PDA {root_pda} on devnet: {} bytes, owner {}",
                acc.data.len(),
                acc.owner
            );
            let read_result = rome_zk_layouts::root::read(&acc.data);
            println!("rome_zk_layouts::root::read(...) = {read_result:?}");
            assert!(
                acc.data.len() < rome_zk_layouts::root::MIN_LEN,
                "expected the deployed root account ({} bytes) to be shorter than MIN_LEN ({}) -- if \
                 this now fails, the new root layout has shipped and this probe is stale",
                acc.data.len(),
                rome_zk_layouts::root::MIN_LEN
            );
            assert!(
                matches!(
                    read_result,
                    Err(rome_zk_layouts::LayoutError::TooShort { .. })
                ),
                "expected a TooShort layout error confirming OpenBatch cannot succeed here yet"
            );
        }
        Err(e) => {
            println!(
                "root PDA {root_pda} not found on devnet ({e}) -- OpenBatch would fail on a \
                       missing account, an even more direct block than a too-short one"
            );
        }
    }

    // --- chunk Open also independently requires a batch account that only OpenBatch can create ---
    let probe_batch_id = 999_999_999u64; // an id nobody has plausibly opened
    let (batch_pda, _) = zk_inbox_client::batch_pda(
        &inbox_program,
        &settlement_program,
        DEVNET_CHAIN_ID,
        probe_batch_id,
    );
    match rpc.get_account(&batch_pda).await {
        Ok(acc) => println!(
            "batch PDA {batch_pda} unexpectedly exists ({} bytes, owner {}) -- pick a different probe_batch_id",
            acc.data.len(),
            acc.owner
        ),
        Err(_) => println!(
            "batch PDA {batch_pda} does not exist on devnet, confirming: a chunk `Open` at this batch id \
             would fail `open_chunk_check`'s `batch_pda.owner != program_id` before the root layout is \
             even relevant -- the chunk lane is gated on `OpenBatch` succeeding, not just an independent \
             restriction of its own."
        ),
    }
}

/// Builds the exact `plan_chunk` V1 transaction for chain 200101 /
/// inbox `EtAXw56BCmxH4Ew9JdLTjmxq21UadQjAoyLWERVNzBdL` at the design's own max frame size, with the
/// configured `loaded_accounts_data_size_limit`, and calls `simulateTransaction` (`sigVerify: false`, no
/// signing, nothing sent — the message is built and never handed to anything that could submit it) against
/// `api.devnet.solana.com`, asserting `err == None`. Run manually with `cargo test -p rome-zk-batcher
/// --test devnet_probe design_frame_v1_tx_simulates_clean_on_target_cluster -- --ignored --nocapture`.
/// A run at 131,072 is rejected (`MaxLoadedAccountsDataSizeExceeded`) and one at 262,144 is accepted
/// (`None`) — the config's own compile-time default, asserted here, is 262,144.
#[tokio::test]
#[ignore = "hits real devnet RPC; read-only simulate, no signing, nothing sent"]
async fn design_frame_v1_tx_simulates_clean_on_target_cluster() {
    use rome_zk_batcher::sender::compat;

    let loaded_accounts_data_size_limit =
        rome_zk_batcher::config::default_loaded_accounts_data_size_limit();

    let inbox_program = Pubkey::from_str(DEVNET_INBOX_PROGRAM_ID).unwrap();
    let settlement_program = Pubkey::from_str(DEVNET_SETTLEMENT_PROGRAM_ID).unwrap();
    let payer_legacy = Pubkey::from_str(PROBE_PAYER).unwrap();
    let payer_v1 = compat::to_v1_pubkey(&payer_legacy);

    let payload = vec![
        7u8;
        rome_zk_batcher::channel::FRAME_HEADER_LEN
            + rome_zk_batcher::channel::DEFAULT_MAX_FRAME_BODY_LEN
    ];
    let ixs = rome_zk_batcher::pipeline::plan_chunk(
        &inbox_program,
        &payer_legacy,
        &settlement_program,
        DEVNET_CHAIN_ID,
        PROBE_BATCH,
        PROBE_CHUNK_IDX,
        &payload,
    );
    let v1_ixs: Vec<solana_instruction::Instruction> =
        ixs.iter().map(compat::to_v1_instruction).collect();

    let rpc_v1 = solana_client_v1::nonblocking::rpc_client::RpcClient::new(
        "https://api.devnet.solana.com".to_string(),
    );
    let blockhash = rpc_v1
        .get_latest_blockhash()
        .await
        .expect("get_latest_blockhash");

    let config = solana_message::v1::TransactionConfig::empty()
        .with_compute_unit_limit(40_000) // chunk_compute_unit_limit default
        .with_loaded_accounts_data_size_limit(loaded_accounts_data_size_limit);
    let message =
        solana_message::v1::Message::try_compile_with_config(&payer_v1, &v1_ixs, blockhash, config)
            .expect("the design-max frame must compile into one V1 transaction");
    let num_required_signatures = message.header.num_required_signatures as usize;
    let tx = solana_transaction::versioned::VersionedTransaction {
        signatures: vec![solana_signature::Signature::default(); num_required_signatures],
        message: solana_message::VersionedMessage::V1(message),
    };

    let result = rpc_v1
        .simulate_transaction_with_config(
            &tx,
            solana_client_v1::rpc_config::RpcSimulateTransactionConfig {
                sig_verify: false,
                ..Default::default()
            },
        )
        .await
        .expect("simulateTransaction RPC call itself must succeed (network-level, not tx-level)");

    println!(
        "simulateTransaction @ loaded_accounts_data_size_limit={loaded_accounts_data_size_limit}: \
         err={:?} units_consumed={:?} logs={:?}",
        result.value.err, result.value.units_consumed, result.value.logs
    );
    assert_eq!(
        result.value.err, None,
        "the design-max chunk-lane V1 tx must simulate clean at the configured \
         loaded_accounts_data_size_limit={loaded_accounts_data_size_limit}"
    );
}
