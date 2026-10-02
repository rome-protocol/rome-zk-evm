//! Devnet driver: deploys a THROWAWAY zk-settlement program, then runs `InitChain` -> (real
//! Tiber zk-inbox) `OpenBatch` + 3 `SealLeaf` + `FinalizeBatch` -> `PostRoot` (reading `acc` from that
//! real inbox batch account) -> wait for the challenge window -> `FinalizeBatch` (settlement) ->
//! `RootView` -> closes the throwaway program. Prints CU per instruction and every id/signature.
//!
//! Usage:
//!   cargo run --features devnet-driver -p zk-settlement-client --example devnet_driver -- \
//!     [--keypair PATH] [--rpc-url URL] [--inbox-program ID]
//!
//! Defaults: `--keypair ~/.config/solana/id.json`, `--rpc-url https://api.devnet.solana.com`,
//! `--inbox-program` = Tiber's deployed zk-inbox (`EtAXw56BCmxH4Ew9JdLTjmxq21UadQjAoyLWERVNzBdL`). Chain
//! id is fixed at 200199 (a value not used by any live chain) with `challenge_window_slots = 20` — real
//! devnet has no slot-warp, so the window has to be short enough to actually wait out.

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_program::{instruction::Instruction, keccak, pubkey::Pubkey};
use solana_sdk::{
    signature::{read_keypair_file, write_keypair_file, Keypair, Signature, Signer},
    transaction::Transaction,
};
use solana_transaction_status_client_types::UiTransactionEncoding;
use std::{path::PathBuf, str::FromStr, time::Duration};

const DEFAULT_INBOX_PROGRAM_ID: &str = "EtAXw56BCmxH4Ew9JdLTjmxq21UadQjAoyLWERVNzBdL";
const TEST_CHAIN_ID: u64 = 200199;
const CHALLENGE_WINDOW_SLOTS: u32 = 20;

struct Args {
    keypair: PathBuf,
    rpc_url: String,
    inbox_program: Pubkey,
}

fn parse_args() -> Args {
    let mut keypair = PathBuf::from(std::env::var("HOME").expect("HOME must be set"))
        .join(".config/solana/id.json");
    let mut rpc_url = "https://api.devnet.solana.com".to_string();
    let mut inbox_program = Pubkey::from_str(DEFAULT_INBOX_PROGRAM_ID).unwrap();
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
            "--inbox-program" => {
                inbox_program = Pubkey::from_str(&val()).expect("bad --inbox-program")
            }
            other => panic!("unknown flag: {other}"),
        }
        i += 2;
    }
    Args {
        keypair,
        rpc_url,
        inbox_program,
    }
}

fn sbf_so_path() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/deploy/zk_settlement.so"
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

/// Like `send`, but returns the error instead of panicking — for a call this driver expects might be
/// rejected by a real program-level check (not a client-side mistake), so the driver can report exactly
/// which on-chain rule fired instead of crashing.
async fn try_send(
    rpc: &RpcClient,
    payer: &Keypair,
    ixs: &[Instruction],
) -> Result<Signature, solana_client::client_error::ClientError> {
    let blockhash = rpc
        .get_latest_blockhash()
        .await
        .expect("get_latest_blockhash");
    let tx = Transaction::new_signed_with_payer(ixs, Some(&payer.pubkey()), &[payer], blockhash);
    rpc.send_and_confirm_transaction(&tx).await
}

/// See `zk-inbox-client`'s driver for why this retries: `getTransaction` can lag `send_and_confirm`.
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

fn deploy_throwaway(rpc_url: &str, payer_path: &std::path::Path) -> (Pubkey, PathBuf) {
    let program_keypair = Keypair::new();
    let program_id = program_keypair.pubkey();
    let path = std::env::temp_dir().join(format!("zk-settlement-throwaway-{program_id}.json"));
    write_keypair_file(&program_keypair, &path).expect("write throwaway program keypair");
    println!(
        "deploying throwaway settlement program {program_id} (keypair at {})",
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
    (program_id, path)
}

fn close_program(rpc_url: &str, payer_path: &std::path::Path, program_id: &Pubkey) {
    println!("closing throwaway settlement program {program_id}");
    let status = std::process::Command::new("solana")
        .args([
            "program",
            "close",
            &program_id.to_string(),
            "--keypair",
            payer_path.to_str().unwrap(),
            "--url",
            rpc_url,
            "--bypass-warning",
        ])
        .status()
        .expect("failed to run `solana program close`");
    if !status.success() {
        println!("warning: `solana program close` reported failure for {program_id} — reclaim it manually");
    }
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
        balance > 200_000_000,
        "payer needs at least 0.2 SOL on devnet to run this driver"
    );

    let (program_id, program_keypair_path) = deploy_throwaway(&args.rpc_url, &args.keypair);
    let chain_id = TEST_CHAIN_ID;
    // Batch ids are sequential and never reused — read the real inbox program's own
    // `batch_cursor.next_batch` for this chain rather than a hardcoded `1`, which would collide with (or
    // wrongly skip past) whatever the cursor already expects.
    let (cursor_pda, _) = zk_inbox_client::cursor_pda(&args.inbox_program, chain_id);
    let cursor_data = rpc
        .get_account(&cursor_pda)
        .await
        .unwrap_or_else(|e| {
            panic!(
                "chain {chain_id} has no batch_cursor account on inbox program {} ({cursor_pda}): {e} \
                 — run InitBatchCursor for this chain first (see zk-inbox-client's \
                 examples/find_max_batch_id.rs)",
                args.inbox_program
            )
        })
        .data;
    let batch = zk_inbox_client::decode_batch_cursor(&cursor_data)
        .expect("decode_batch_cursor")
        .next_batch;
    println!(
        "chain_id={chain_id} batch={batch} settlement_program={program_id} inbox_program={}",
        args.inbox_program
    );

    // --- registration-and-revenue proposal: bootstrap the global config, then allow this (reserved,
    // < 2^32) chain id — this throwaway program's payer plays every governance role for the driver ---
    let init_global_sig = send(
        &rpc,
        &payer,
        &[zk_settlement_client::init_global_config_ix(
            &program_id,
            &payer.pubkey(),
            &payer.pubkey(), // `solana program deploy --keypair payer` makes payer the upgrade authority
            zk_settlement_client::GlobalConfigFields {
                registry_authority: payer.pubkey(),
                treasury: payer.pubkey(),
                permissionless_init_enabled: false,
                reclaim_window_slots: 5_184_000,
                deposit_lamports: 25_000_000_000,
                default_fee_base_lamports: 1_000_000,
                default_fee_bps: 0,
            },
        )],
    )
    .await;
    print_cu(&rpc, "InitGlobalConfig", &init_global_sig).await;
    let allow_sig = send(
        &rpc,
        &payer,
        &[zk_settlement_client::allow_reserved_id_ix(
            &program_id,
            &payer.pubkey(),
            &payer.pubkey(),
            chain_id,
        )],
    )
    .await;
    print_cu(&rpc, "AllowReservedId", &allow_sig).await;

    // --- InitChain: genesis root + registry (registry entries are placeholders here — this driver
    // exercises PostRoot, not PostRootProved, so no vkey is actually checked) ---
    let genesis_state_root =
        keccak::hashv(&[b"rome-zk devnet driver genesis", &chain_id.to_le_bytes()]).to_bytes();
    let init_ix = zk_settlement_client::init_chain_reserved_ix(
        &program_id,
        &payer.pubkey(),
        &payer.pubkey(),
        &payer.pubkey(),
        chain_id,
        zk_settlement_client::InitChainFields {
            number: 0,
            parent_hash: [0u8; 32],
            state_root: genesis_state_root,
            block_hash: [0u8; 32],
            profile: 0,
            challenge_window_slots: CHALLENGE_WINDOW_SLOTS,
            prove_window_slots: 4 * 60 * 60 * 2, // ~4h at 2 slots/s — not exercised by this driver
            proving_policy: 1,                   // on-challenge
            poster_bond: 0,                      // no bond
            exit_cap_per_window: 0,              // 0 = exits off (ProveExit refuses ExitCapUnset)
            max_pending: 16,
            inbox_program: args.inbox_program,
            max_drift_secs: 60, // the default — this driver exercises PostRoot only
            registry_entries: vec![
                zk_settlement_client::RegistryEntry {
                    curve: rome_zk_layouts::registry::CURVE_BN254,
                    scheme: rome_zk_layouts::registry::SCHEME_PLONK,
                    vkey_hash: [0u8; 32],
                    layout_id: rome_zk_layouts::registry::LAYOUT_HEADER_FALLBACK,
                },
                zk_settlement_client::RegistryEntry {
                    curve: rome_zk_layouts::registry::CURVE_BN254,
                    scheme: rome_zk_layouts::registry::SCHEME_GROTH16,
                    vkey_hash: [0u8; 32],
                    layout_id: rome_zk_layouts::registry::LAYOUT_HEADER_FALLBACK,
                },
            ],
        },
    );
    let sig = send(&rpc, &payer, &[init_ix]).await;
    print_cu(&rpc, "InitChain", &sig).await;

    // --- real Tiber zk-inbox: OpenBatch + 3×(Open+Write+Seal) + FinalizeBatch ---
    let n: u32 = 3;
    let bodies: Vec<Vec<u8>> = (0..n)
        .map(|i| format!("rome-zk devnet chunk #{i}").into_bytes())
        .collect();
    let open_batch_sig = send(
        &rpc,
        &payer,
        &zk_inbox_client::open_and_grow_batch_ixs(
            &args.inbox_program,
            &payer.pubkey(),
            chain_id,
            batch,
            n,
            &program_id,
        ),
    )
    .await;
    print_cu(&rpc, "inbox OpenBatch(+Grow)", &open_batch_sig).await;

    for (idx, body) in bodies.iter().enumerate() {
        let idx = idx as u32;
        let ixs = vec![
            zk_inbox_client::open_chunk_ix(
                &args.inbox_program,
                &payer.pubkey(),
                chain_id,
                batch,
                idx,
                body.len() as u32,
            ),
            zk_inbox_client::write_chunk_ix(
                &args.inbox_program,
                &payer.pubkey(),
                chain_id,
                batch,
                idx,
                0,
                body.clone(),
            ),
            zk_inbox_client::seal_chunk_ix(
                &args.inbox_program,
                &payer.pubkey(),
                chain_id,
                batch,
                idx,
                body.len() as u32,
                zk_inbox_client::chunk_body_hash(body),
            ),
            zk_inbox_client::seal_leaf_ix(&args.inbox_program, chain_id, batch, idx),
        ];
        let sig = send(&rpc, &payer, &ixs).await;
        print_cu(
            &rpc,
            &format!("inbox chunk[{idx}] open+write+seal+SealLeaf"),
            &sig,
        )
        .await;
    }

    let finalize_inbox_sig = send(
        &rpc,
        &payer,
        &[zk_inbox_client::finalize_batch_ix(
            &args.inbox_program,
            &payer.pubkey(),
            chain_id,
            batch,
            0,
        )],
    )
    .await;
    print_cu(&rpc, "inbox FinalizeBatch", &finalize_inbox_sig).await;

    tokio::time::sleep(Duration::from_millis(500)).await;
    let (inbox_batch_pda, _) = zk_inbox_client::batch_pda(&args.inbox_program, chain_id, batch);
    let inbox_acct = rpc
        .get_account(&inbox_batch_pda)
        .await
        .expect("get_account(inbox batch pda)");
    let inbox_decoded =
        zk_inbox_client::decode_batch_account(&inbox_acct.data).expect("decode_batch_account");
    assert!(
        inbox_decoded.finalized,
        "inbox batch must be finalized before PostRoot"
    );
    println!("inbox acc = {:02x?}", inbox_decoded.acc);

    // --- PostRoot: reads the real inbox batch account's `acc` as inbox_commitment ---
    let forced_outcome_commitment =
        rome_zk_layouts::forced_empty_root(&(keccak_sw as fn(&[&[u8]]) -> [u8; 32]));
    let post_root_args = zk_settlement_client::PostRootFields {
        chain_id,
        batch,
        prev_batch: 0,
        pre_state_root: genesis_state_root,
        first_block: 1,
        last_block: 1,
        state_root: keccak::hashv(&[b"rome-zk devnet driver batch-1 state root"]).to_bytes(),
        block_roots_merkle: keccak::hashv(&[b"rome-zk devnet driver block roots merkle"])
            .to_bytes(),
        inbox_commitment: inbox_decoded.acc,
        forced_outcome_commitment,
        parent_hash: keccak::hashv(&[b"rome-zk devnet driver parent hash"]).to_bytes(),
        last_block_hash: keccak::hashv(&[b"rome-zk devnet driver block hash"]).to_bytes(),
        gas_in_batch: 0,
    };
    let post_root_sig = send(
        &rpc,
        &payer,
        &[zk_settlement_client::post_root_ix(
            &program_id,
            &payer.pubkey(),
            &args.inbox_program,
            &payer.pubkey(),
            post_root_args.clone(),
        )
        .expect("reserved chain")],
    )
    .await;
    print_cu(&rpc, "PostRoot", &post_root_sig).await;

    let (pending_pda, _) = zk_settlement_client::pending_pda(&program_id, chain_id, batch);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let pending_acct = rpc
        .get_account(&pending_pda)
        .await
        .expect("get_account(pending pda)");
    let pending_decoded = zk_settlement_client::decode_pending_account(&pending_acct.data)
        .expect("decode_pending_account");
    println!(
        "pending batch {batch}: posted_slot={} deadline_slot={} status={}",
        pending_decoded.posted_slot, pending_decoded.deadline_slot, pending_decoded.status
    );

    // --- wait for the challenge window to elapse (real devnet — no slot warp) ---
    loop {
        let slot = rpc.get_slot().await.expect("get_slot");
        if slot >= pending_decoded.deadline_slot {
            println!(
                "slot {slot} >= deadline_slot {} — window elapsed",
                pending_decoded.deadline_slot
            );
            break;
        }
        println!(
            "slot {slot} < deadline_slot {} — waiting",
            pending_decoded.deadline_slot
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // --- FinalizeBatch (settlement) ---
    let finalize_settlement_sig = send(
        &rpc,
        &payer,
        &[zk_settlement_client::finalize_batch_ix(
            &program_id,
            chain_id,
            batch,
            &[],
        )],
    )
    .await;
    print_cu(&rpc, "settlement FinalizeBatch", &finalize_settlement_sig).await;

    // --- RootView ---
    let root_view_ix = zk_settlement_client::root_view_ix(&program_id, chain_id, batch);
    let blockhash = rpc
        .get_latest_blockhash()
        .await
        .expect("get_latest_blockhash");
    let tx = Transaction::new_signed_with_payer(
        &[root_view_ix],
        Some(&payer.pubkey()),
        &[&payer],
        blockhash,
    );
    let sim = rpc
        .simulate_transaction(&tx)
        .await
        .expect("simulate_transaction(RootView)");
    println!(
        "RootView simulate: err={:?} return_data={:?}",
        sim.value.err, sim.value.return_data
    );
    let root_view_sig = send(
        &rpc,
        &payer,
        &[zk_settlement_client::root_view_ix(
            &program_id,
            chain_id,
            batch,
        )],
    )
    .await;
    print_cu(&rpc, "RootView", &root_view_sig).await;

    let (root_pda, _) = zk_settlement_client::root_pda(&program_id, chain_id);
    let root_acct = rpc
        .get_account(&root_pda)
        .await
        .expect("get_account(root pda)");
    let root_decoded =
        zk_settlement_client::decode_root_account(&root_acct.data).expect("decode_root_account");
    println!(
        "root after finalize: number={} head_final_batch={} state_root={:02x?}",
        root_decoded.number, root_decoded.head_final_batch, root_decoded.state_root
    );
    assert_eq!(root_decoded.head_final_batch, batch, "batch must be final");
    assert_eq!(
        root_decoded.state_root, post_root_args.state_root,
        "root state_root must match the posted batch"
    );

    // --- ClosePending: this driver posts only batch 1, so it is still `head_pending_batch` — the program
    // refuses to recycle the head pending batch's rent (the next PostRoot may still need to read it as its
    // predecessor). Attempted and reported rather than assumed either way.
    let close_pending_ix =
        zk_settlement_client::close_pending_ix(&program_id, &payer.pubkey(), chain_id, batch);
    match try_send(&rpc, &payer, &[close_pending_ix]).await {
        Ok(sig) => print_cu(&rpc, "ClosePending", &sig).await,
        Err(e) => println!(
            "ClosePending on batch {batch}: rejected — {e} (expected: batch {batch} is still \
             head_pending_batch, IsHeadPendingBatch; a chain with a later batch posted \
             would let this succeed)"
        ),
    }

    // --- inbox chunk Close against the now-final root ---
    let close_chunk_ix = zk_inbox_client::close_chunk_ix(
        &args.inbox_program,
        &payer.pubkey(),
        &program_id,
        chain_id,
        batch,
        0,
    );
    let close_chunk_sig = send(&rpc, &payer, &[close_chunk_ix]).await;
    print_cu(&rpc, "inbox chunk Close (final root)", &close_chunk_sig).await;

    close_program(&args.rpc_url, &args.keypair, &program_id);
    let _ = program_keypair_path; // kept on disk in case close needs a retry; not deleted here
    println!("OK: devnet flow complete for chain {chain_id} batch {batch}");
}

fn keccak_sw(parts: &[&[u8]]) -> [u8; 32] {
    keccak::hashv(parts).to_bytes()
}
