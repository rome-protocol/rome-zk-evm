//! Fetches the latest FINALIZED batch on the dev chain, read-only (`getAccountInfo`/`getMultipleAccounts`
//! on the cursor/batch/chunk PDAs — no writes, no keys), recomputes the accumulator over the bytes
//! actually read back (same check `rome-zk-derive::inbox::InboxRetrieval` makes), and writes both a JSON
//! fixture (`fixtures/inbox/txv1-dev-batch-<id>.json`) and a ready-to-run `ziskemu` input `.bin`.
//!
//! Host-only (`host-tools` feature): uses `rome-zk-layouts`/`rome-zk-merkle` directly, and a plain blocking
//! HTTP client (`ureq`) for the three read-only JSON-RPC calls — no solana-client/async runtime needed for
//! this.

use clap::Parser;
use rome_zk_bench_decode_lib::BenchInput;
use solana_program::pubkey::Pubkey;
use std::str::FromStr;

#[derive(Parser)]
struct Cli {
    /// JSON-RPC URL of the Solana cluster to read the batch from.
    #[arg(long)]
    rpc: String,
    #[arg(long, default_value = "BcUGv24CR7SMyBo3rYkWYX8HSZj1ex4hxX5QtZutRtNm")]
    inbox_program: String,
    /// The chain's settlement program: the inbox accounts of a chain are keyed by it.
    #[arg(long)]
    settlement_program: String,
    #[arg(long, default_value_t = 200_101)]
    chain_id: u64,
    #[arg(long, default_value = "fixtures/inbox")]
    fixtures_dir: String,
    #[arg(long)]
    bin_out: Option<String>,
}

fn rpc_call(rpc: &str, method: &str, params: serde_json::Value) -> serde_json::Value {
    let body = serde_json::json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
    let resp: serde_json::Value = ureq::post(rpc)
        .set("content-type", "application/json")
        .send_json(body)
        .unwrap_or_else(|e| panic!("RPC {method} failed: {e}"))
        .into_json()
        .unwrap_or_else(|e| panic!("RPC {method}: bad JSON response: {e}"));
    if let Some(err) = resp.get("error") {
        panic!("RPC {method} returned an error: {err}");
    }
    resp["result"].clone()
}

fn get_account_data(rpc: &str, pubkey: &Pubkey) -> Option<Vec<u8>> {
    let result = rpc_call(
        rpc,
        "getAccountInfo",
        serde_json::json!([pubkey.to_string(), {"encoding": "base64", "commitment": "finalized"}]),
    );
    let value = result.get("value")?;
    if value.is_null() {
        return None;
    }
    let data_b64 = value["data"][0].as_str().expect("data[0] is base64 string");
    Some(
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data_b64)
            .expect("base64 decode account data"),
    )
}

fn get_multiple_account_data(rpc: &str, pubkeys: &[Pubkey]) -> Vec<Option<Vec<u8>>> {
    let keys: Vec<String> = pubkeys.iter().map(|p| p.to_string()).collect();
    let result = rpc_call(
        rpc,
        "getMultipleAccounts",
        serde_json::json!([keys, {"encoding": "base64", "commitment": "finalized"}]),
    );
    result["value"]
        .as_array()
        .expect("value is an array")
        .iter()
        .map(|v| {
            if v.is_null() {
                None
            } else {
                let data_b64 = v["data"][0].as_str().expect("data[0] is base64 string");
                Some(
                    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data_b64)
                        .expect("base64 decode account data"),
                )
            }
        })
        .collect()
}

fn main() {
    let cli = Cli::parse();
    let program_id = Pubkey::from_str(&cli.inbox_program).expect("valid inbox program id");
    let settlement_program =
        Pubkey::from_str(&cli.settlement_program).expect("valid settlement program id");

    let (cursor_pda, _) =
        rome_zk_layouts::cursor::pda(&program_id, &settlement_program, cli.chain_id);
    let cursor_data = get_account_data(&cli.rpc, &cursor_pda)
        .unwrap_or_else(|| panic!("batch_cursor account {cursor_pda} not found on {}", cli.rpc));
    let cursor = rome_zk_layouts::cursor::read(&cursor_data).expect("decode batch_cursor");
    eprintln!(
        "batch_cursor {cursor_pda}: chain_id={}, next_batch={}",
        cursor.chain_id, cursor.next_batch
    );
    assert_eq!(cursor.chain_id, cli.chain_id, "cursor chain_id mismatch");
    assert!(
        cursor.next_batch > 0,
        "no batch has ever been opened on this chain"
    );

    // Walk backward from the newest ever-opened id looking for a Finalized batch (the batcher's own resume-
    // anchor walk does the same).
    let mut found: Option<(u64, rome_zk_layouts::batch::BatchFields)> = None;
    let mut id = cursor.next_batch - 1;
    loop {
        let (batch_pda, _) =
            rome_zk_layouts::batch::pda(&program_id, &settlement_program, cli.chain_id, id);
        if let Some(data) = get_account_data(&cli.rpc, &batch_pda) {
            match rome_zk_layouts::batch::read(&data) {
                Ok(f) if f.finalized => {
                    eprintln!(
                        "batch {id} at {batch_pda}: FINALIZED, expected_count={}",
                        f.expected_count
                    );
                    found = Some((id, f));
                    break;
                }
                Ok(_) => eprintln!("batch {id} at {batch_pda}: open, not finalized (skipping)"),
                Err(e) => eprintln!("batch {id} at {batch_pda}: decode error {e:?} (skipping)"),
            }
        } else {
            eprintln!("batch {id} at {batch_pda}: missing (closed/never opened, skipping)");
        }
        if id == 0 {
            break;
        }
        id -= 1;
    }
    let (batch_id, batch) = found.expect("no finalized batch found on this chain");

    let chunk_pdas: Vec<Pubkey> = (0..batch.expected_count)
        .map(|idx| {
            rome_zk_layouts::chunk::pda(
                &program_id,
                &settlement_program,
                cli.chain_id,
                batch_id,
                idx,
            )
            .0
        })
        .collect();
    let chunk_datas = get_multiple_account_data(&cli.rpc, &chunk_pdas);

    let mut chunk_bodies: Vec<Vec<u8>> = Vec::with_capacity(chunk_pdas.len());
    for (idx, data) in chunk_datas.into_iter().enumerate() {
        let data = data.unwrap_or_else(|| {
            panic!(
                "chunk {idx} of finalized batch {batch_id} is missing at {}",
                chunk_pdas[idx]
            )
        });
        let header = rome_zk_layouts::chunk::read(&data).expect("decode chunk header");
        assert_eq!(header.chain_id, cli.chain_id);
        assert_eq!(header.batch, batch_id);
        assert_eq!(header.idx, idx as u32);
        assert!(
            header.sealed,
            "chunk {idx} is not sealed but batch is finalized"
        );
        let body = &data[rome_zk_layouts::chunk::HEADER_LEN
            ..rome_zk_layouts::chunk::HEADER_LEN + header.len as usize];
        chunk_bodies.push(body.to_vec());
    }

    // Recompute the commitment over the bytes actually read, compare to the batch account's own — the real producer
    // verification, not trust-on-read.
    let h = rome_zk_merkle::keccak256 as fn(&[&[u8]]) -> [u8; 32];
    let chunk_hashes: Vec<[u8; 32]> = chunk_bodies.iter().map(|b| h(&[b])).collect();
    let leaves: Vec<[u8; 32]> = chunk_hashes
        .iter()
        .enumerate()
        .map(|(i, hh)| rome_zk_merkle::indexed_leaf(&h, i as u32, hh))
        .collect();
    let root = rome_zk_merkle::root(&h, &leaves);
    let forced_root = rome_zk_layouts::forced_empty_root(&h);
    let acc = rome_zk_layouts::acc(
        &h,
        cli.chain_id,
        batch_id,
        batch.open_slot,
        batch.expected_count,
        &root,
        &forced_root,
    );
    assert_eq!(root, batch.root, "recomputed root != on-chain root");
    assert_eq!(
        forced_root, batch.forced_root,
        "recomputed forced_root != on-chain forced_root"
    );
    assert_eq!(
        acc, batch.acc,
        "recomputed acc != on-chain acc — chunk bytes read do not match what was finalized"
    );
    eprintln!("commitment verified: acc = {}", hex::encode(acc));

    std::fs::create_dir_all(&cli.fixtures_dir).expect("create fixtures dir");
    let fixture = serde_json::json!({
        "chain_id": cli.chain_id,
        "batch": batch_id,
        "open_slot": batch.open_slot,
        "open_unix_ts": batch.open_unix_ts,
        "expected_count": batch.expected_count,
        "acc": hex::encode(batch.acc),
        "root": hex::encode(batch.root),
        "forced_root": hex::encode(batch.forced_root),
        "chunk_bodies_hex": chunk_bodies.iter().map(hex::encode).collect::<Vec<_>>(),
    });
    let fixture_path = format!("{}/txv1-dev-batch-{batch_id}.json", cli.fixtures_dir);
    std::fs::write(
        &fixture_path,
        serde_json::to_string_pretty(&fixture).unwrap(),
    )
    .expect("write fixture json");
    eprintln!("wrote fixture {fixture_path}");

    if let Some(bin_out) = cli.bin_out {
        let input = BenchInput {
            chain_id: cli.chain_id,
            batch: batch_id,
            open_slot: batch.open_slot,
            expected_count: batch.expected_count,
            chunk_bodies,
        };
        let payload = bincode::serde::encode_to_vec(&input, bincode::config::standard())
            .expect("bincode encode");
        let mut file_bytes = Vec::with_capacity(8 + payload.len() + 7);
        file_bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        file_bytes.extend_from_slice(&payload);
        while file_bytes.len() % 8 != 0 {
            file_bytes.push(0);
        }
        std::fs::write(&bin_out, &file_bytes).expect("write ziskemu input bin");
        eprintln!("wrote {} bytes to {bin_out}", file_bytes.len());
    }
}
