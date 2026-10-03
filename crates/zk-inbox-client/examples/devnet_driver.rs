//! Devnet driver for the batch accumulator: OpenBatch -> 8 parallel SealLeaf (sent in one burst, out of
//! order) -> FinalizeBatch -> reads `acc` back and checks it against the client-side reference. Prints
//! CU per transaction from `getTransaction`.
//!
//! Usage:
//!   cargo run --features devnet-driver -p zk-inbox-client --example devnet_driver -- \
//!     [--keypair PATH] [--rpc-url URL] [--program-id ID] [--settlement-program ID]
//!
//! Defaults: `--keypair ~/.config/solana/id.json`, `--rpc-url https://api.devnet.solana.com`, `--program-id` /
//! `--settlement-program` = the dev deployment's program ids. The keypair is never read from or written into
//! the repo; a throwaway program keypair (only generated if the deployed dev program doesn't yet recognize the
//! new instructions) is written under `/tmp` and its id is printed so it can be recorded and later closed.

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_program::{instruction::Instruction, pubkey::Pubkey};
// `commitment_config` moved out of `solana_sdk`'s root re-export in the Agave 4.x line (API fallout) — now its own
// crate.
use solana_commitment_config::CommitmentConfig;
use solana_sdk::{
    signature::{read_keypair_file, write_keypair_file, Keypair, Signature, Signer},
    transaction::Transaction,
};
use solana_transaction_status_client_types::UiTransactionEncoding;
use std::{path::PathBuf, str::FromStr, time::Duration};

const DEVNET_PROGRAM_ID: &str = "EtAXw56BCmxH4Ew9JdLTjmxq21UadQjAoyLWERVNzBdL";
const DEVNET_SETTLEMENT_PROGRAM_ID: &str = "39Kraok3WYQyJ8FAJjyYzjemFHxZfcNrKAHoNg2BM4p3";
const DEVNET_CHAIN_ID: u64 = 200101;

struct Args {
    keypair: PathBuf,
    rpc_url: String,
    program_id: Pubkey,
    settlement_program: Pubkey,
}

fn parse_args() -> Args {
    let mut keypair = dirs_home().join(".config/solana/id.json");
    let mut rpc_url = "https://api.devnet.solana.com".to_string();
    let mut program_id = Pubkey::from_str(DEVNET_PROGRAM_ID).unwrap();
    let mut settlement_program = Pubkey::from_str(DEVNET_SETTLEMENT_PROGRAM_ID).unwrap();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let flag = argv[i].as_str();
        let val = || {
            argv.get(i + 1)
                .unwrap_or_else(|| panic!("{flag} needs a value"))
                .clone()
        };
        match flag {
            "--keypair" => keypair = PathBuf::from(val()),
            "--rpc-url" => rpc_url = val(),
            "--program-id" => program_id = Pubkey::from_str(&val()).expect("bad --program-id"),
            "--settlement-program" => {
                settlement_program = Pubkey::from_str(&val()).expect("bad --settlement-program")
            }
            other => panic!("unknown flag: {other}"),
        }
        i += 2;
    }
    Args {
        keypair,
        rpc_url,
        program_id,
        settlement_program,
    }
}

fn dirs_home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME must be set"))
}

fn sbf_so_path() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/deploy/zk_inbox.so"
    ))
}

async fn send(rpc: &RpcClient, payer: &Keypair, ixs: &[Instruction]) -> Signature {
    let blockhash = rpc
        .get_latest_blockhash()
        .await
        .expect("get_latest_blockhash");
    let tx = Transaction::new_signed_with_payer(ixs, Some(&payer.pubkey()), &[payer], blockhash);
    rpc.send_and_confirm_transaction(&tx)
        .await
        .expect("send_and_confirm_transaction")
}

/// `getTransaction` can return a null result for a signature moments after
/// `send_and_confirm_transaction` returns — the RPC node's history store lags its own confirmation by a
/// beat, not a correctness issue — so this retries briefly before giving up.
async fn print_cu(rpc: &RpcClient, label: &str, sig: &Signature) {
    for attempt in 0..5 {
        match rpc.get_transaction(sig, UiTransactionEncoding::Json).await {
            Ok(t) => {
                let cu = t
                    .transaction
                    .meta
                    .as_ref()
                    .and_then(|m| Into::<Option<u64>>::into(m.compute_units_consumed.clone()));
                println!("{label}: sig={sig} cu={cu:?}");
                return;
            }
            Err(_) if attempt < 4 => tokio::time::sleep(Duration::from_millis(500)).await,
            Err(e) => {
                println!("{label}: sig={sig} (could not fetch CU after retries: {e})");
                return;
            }
        }
    }
}

/// Probes whether the deployed program at `program_id` recognizes `OpenBatch` by simulating it — the
/// original measurement program (discriminants 0-3 only) fails borsh deserialization on discriminant 4 with a native
/// `InvalidInstructionData` error, distinct from any *our* program's own error paths (which are all
/// `Custom(_)`).
async fn supports_accumulator(rpc: &RpcClient, program_id: &Pubkey, payer: &Pubkey) -> bool {
    let probe = zk_inbox_client::open_batch_ix(
        program_id,
        payer,
        u64::MAX,
        u64::MAX,
        1,
        &Pubkey::new_unique(),
    );
    let blockhash = rpc
        .get_latest_blockhash()
        .await
        .expect("get_latest_blockhash");
    let tx = Transaction::new_unsigned(solana_sdk::message::Message::new_with_blockhash(
        &[probe],
        Some(payer),
        &blockhash,
    ));
    match rpc.simulate_transaction(&tx).await {
        Ok(res) => {
            let Some(err) = res.value.err else {
                return true;
            };
            // `simulateTransaction`'s `.value.err` is now `UiTransactionError`, a thin wrapper around the same
            // `TransactionError` this match always expected (API fallout) — convert back via its own `From` impl
            // rather than rewriting the match pattern.
            let err: solana_sdk::transaction::TransactionError = err.into();
            let is_native_invalid_ix_data = matches!(
                err,
                solana_sdk::transaction::TransactionError::InstructionError(
                    _,
                    solana_sdk::instruction::InstructionError::InvalidInstructionData
                )
            );
            !is_native_invalid_ix_data
        }
        Err(_) => false,
    }
}

fn deploy_throwaway(rpc_url: &str, payer_path: &std::path::Path) -> Pubkey {
    let program_keypair = Keypair::new();
    let program_id = program_keypair.pubkey();
    let path = std::env::temp_dir().join(format!("zk-inbox-throwaway-{program_id}.json"));
    write_keypair_file(&program_keypair, &path).expect("write throwaway program keypair");
    println!(
        "deploying throwaway program {program_id} (keypair at {})",
        path.display()
    );
    let so = sbf_so_path();
    assert!(
        so.exists(),
        "missing {} — run cargo build-sbf first",
        so.display()
    );
    let status = std::process::Command::new("solana")
        .args([
            "program",
            "deploy",
            "--program-id",
            path.to_str().unwrap(),
            "--keypair",
            payer_path.to_str().unwrap(),
            "--url",
            rpc_url,
            so.to_str().unwrap(),
        ])
        .status()
        .expect("failed to run `solana program deploy` — is the solana CLI installed?");
    assert!(status.success(), "solana program deploy failed");
    program_id
}

#[tokio::main]
async fn main() {
    let args = parse_args();
    let payer = read_keypair_file(&args.keypair)
        .unwrap_or_else(|e| panic!("failed to read keypair at {}: {e}", args.keypair.display()));
    let rpc = RpcClient::new_with_commitment(args.rpc_url.clone(), CommitmentConfig::confirmed());

    let balance = rpc.get_balance(&payer.pubkey()).await.expect("get_balance");
    println!("payer {} balance {} lamports", payer.pubkey(), balance);
    assert!(
        balance > 100_000_000,
        "payer needs at least 0.1 SOL on devnet to run this driver"
    );

    let mut program_id = args.program_id;
    if !supports_accumulator(&rpc, &program_id, &payer.pubkey()).await {
        println!(
            "devnet program {program_id} does not yet recognize OpenBatch (the deployed program predates this branch) \
             — deploying a throwaway program instead"
        );
        program_id = deploy_throwaway(&args.rpc_url, &args.keypair);
    } else {
        println!("program {program_id} already recognizes the accumulator instructions");
    }

    let chain_id = DEVNET_CHAIN_ID;
    // Batch ids are sequential and never reused — this driver must post under the chain's own
    // `batch_cursor.next_batch`, not a `SystemTime` guess that could easily collide with (or skip below)
    // whatever the cursor already expects.
    let (cursor_pda, _) =
        zk_inbox_client::cursor_pda(&program_id, &args.settlement_program, chain_id);
    let cursor_data = rpc
        .get_account(&cursor_pda)
        .await
        .unwrap_or_else(|e| {
            panic!(
                "chain {chain_id} has no batch_cursor account ({cursor_pda}): {e} — run \
                 InitBatchCursor for this chain first, at root.head_pending_batch + 1 (see examples/find_max_batch_id.rs)"
            )
        })
        .data;
    let batch = zk_inbox_client::decode_batch_cursor(&cursor_data)
        .expect("decode_batch_cursor")
        .next_batch;
    let n: u32 = 8;
    println!("chain_id={chain_id} batch={batch} expected_count={n} program_id={program_id}");

    // --- 8 chunks: Open + Write + Seal, one transaction per chunk ---
    let bodies: Vec<Vec<u8>> = (0..n)
        .map(|i| format!("rome-zk devnet chunk #{i}").into_bytes())
        .collect();
    for (idx, body) in bodies.iter().enumerate() {
        let idx = idx as u32;
        let ixs = vec![
            zk_inbox_client::open_chunk_ix(
                &program_id,
                &payer.pubkey(),
                &args.settlement_program,
                chain_id,
                batch,
                idx,
                body.len() as u32,
            ),
            zk_inbox_client::write_chunk_ix(
                &program_id,
                &payer.pubkey(),
                &args.settlement_program,
                chain_id,
                batch,
                idx,
                0,
                body.clone(),
            ),
            zk_inbox_client::seal_chunk_ix(
                &program_id,
                &payer.pubkey(),
                &args.settlement_program,
                chain_id,
                batch,
                idx,
                body.len() as u32,
                zk_inbox_client::chunk_body_hash(body),
            ),
        ];
        let sig = send(&rpc, &payer, &ixs).await;
        print_cu(&rpc, &format!("chunk[{idx}] open+write+seal"), &sig).await;
    }

    // --- OpenBatch (+ however many GrowBatch calls `n` needs), one transaction — `open_batch_ix` alone only
    // reaches `account_len(n)` up to the single-CPI `MAX_PERMITTED_DATA_INCREASE` ceiling (~312 leaves);
    // `open_and_grow_batch_ixs` builds the whole plan.
    let open_batch_sig = send(
        &rpc,
        &payer,
        &zk_inbox_client::open_and_grow_batch_ixs(
            &program_id,
            &payer.pubkey(),
            chain_id,
            batch,
            n,
            &args.settlement_program,
        ),
    )
    .await;
    print_cu(&rpc, "OpenBatch(+Grow)", &open_batch_sig).await;

    // --- 8 SealLeaf, sent in one burst, out of order ---
    let mut order: Vec<u32> = (0..n).collect();
    order.reverse(); // out of order, deliberately: idx 7 first, idx 0 last
    let handles: Vec<_> = order
        .iter()
        .map(|&idx| {
            let rpc_url = args.rpc_url.clone();
            let payer_bytes = payer.to_bytes();
            let program_id = program_id;
            let settlement_program = args.settlement_program;
            tokio::spawn(async move {
                let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
                // `Keypair::from_bytes` was removed in the Agave 4.x line (API fallout) — `TryFrom<&[u8]>` is the
                // same reconstruction (64 bytes, secret+public, validates the public key matches the derived one),
                // just via a different trait.
                let payer = Keypair::try_from(payer_bytes.as_slice()).unwrap();
                let ix = zk_inbox_client::seal_leaf_ix(
                    &program_id,
                    &settlement_program,
                    chain_id,
                    batch,
                    idx,
                );
                let sig = send(&rpc, &payer, &[ix]).await;
                (idx, sig)
            })
        })
        .collect();
    for h in handles {
        let (idx, sig) = h.await.expect("seal_leaf task panicked");
        print_cu(&rpc, &format!("SealLeaf[{idx}]"), &sig).await;
    }

    // --- FinalizeBatch (authority-gated; `payer` opened this batch so it is the authority) ---
    let finalize_sig = send(
        &rpc,
        &payer,
        &[zk_inbox_client::finalize_batch_ix(
            &program_id,
            &payer.pubkey(),
            &args.settlement_program,
            chain_id,
            batch,
            0,
        )],
    )
    .await;
    print_cu(&rpc, "FinalizeBatch", &finalize_sig).await;

    // --- read back and verify acc against the client-side reference ---
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (batch_pda, _) =
        zk_inbox_client::batch_pda(&program_id, &args.settlement_program, chain_id, batch);
    let acct = rpc
        .get_account(&batch_pda)
        .await
        .expect("get_account(batch_pda)");
    let decoded = zk_inbox_client::decode_batch_account(&acct.data).expect("decode_batch_account");
    assert!(
        decoded.finalized,
        "batch must be finalized after FinalizeBatch"
    );

    use solana_program::keccak;
    let chunk_hashes: Vec<[u8; 32]> = bodies
        .iter()
        .map(|b| keccak::hashv(&[b]).to_bytes())
        .collect();
    let (expected_root, expected_forced_root, expected_acc) =
        zk_inbox_client::reference_commitment(chain_id, batch, decoded.open_slot, &chunk_hashes);

    println!("on-chain root  = {:02x?}", decoded.root);
    println!("reference root = {:02x?}", expected_root);
    println!("on-chain acc   = {:02x?}", decoded.acc);
    println!("reference acc  = {:02x?}", expected_acc);
    assert_eq!(
        decoded.root, expected_root,
        "on-chain root must match the client-side reference"
    );
    assert_eq!(decoded.forced_root, expected_forced_root);
    assert_eq!(
        decoded.acc, expected_acc,
        "on-chain acc must match the client-side reference"
    );
    println!("OK: on-chain acc matches the client-side reference for {n} leaves");
}
