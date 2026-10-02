//! Scans a chain's accounts (`getProgramAccounts` + memcmp, both account kinds — chunk `ZKIB` and batch `ZKBT`,
//! `zk_inbox_client::scan`) to find the highest batch id ever touched there, and prints the `next_batch`
//! `InitBatchCursor` must be bootstrapped with — one past the highest id found, so a chain with prior batch history
//! (e.g. Tiber, which already has batches opened under the pre-cursor program) can never have its cursor start
//! below an id that already exists on chain (which would let `OpenBatch` re-open it).
//!
//! The earlier id-by-id `batch_pda(chain, 0)`, `batch_pda(chain, 1)`, ... scan had two bugs, both closed here. (1)
//! It only ever probed batch PDAs — `AbandonBatch`/ `CloseBatch` delete the *batch* account (never the chunk PDAs
//! opened under it), so the highest id it ever missed is exactly the one a crashed prior run left chunk PDAs for;
//! the first batcher run at that id would then hard-error `CursorBatchAlreadyTouched`. (2) `Err(_)` from
//! `get_account` (a transient RPC failure, not "no such account") was silently folded into "missing", so 50
//! transient errors in a row ended the scan early and `--submit` made that permanent. Both are gone now:
//! `getProgramAccounts` enumerates every matching account directly (no per-id probing, no early-termination
//! heuristic to get wrong), and a genuine RPC error propagates as a hard failure rather than being read as "nothing
//! here".
//!
//! Read-only by default — pass `--submit --keypair PATH` to also send `InitBatchCursor` at the computed
//! `next_batch` (the keypair must be the chain's root `authority`; never printed or logged). Findings are
//! always printed before `--submit` ever sends anything.
//!
//! Usage:
//!   cargo run --features devnet-driver -p zk-inbox-client --example find_max_batch_id -- \
//!     [--rpc-url URL] [--program-id ID] --chain-id ID \
//!     [--submit --keypair PATH --settlement-program ID [--next-batch N]]

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::RpcProgramAccountsConfig;
use solana_program::pubkey::Pubkey;
// `commitment_config` moved out of `solana_sdk`'s root re-export in the Agave 4.x line (API fallout) — now its own
// crate.
use solana_commitment_config::CommitmentConfig;
use solana_sdk::{
    signature::{read_keypair_file, Signer},
    transaction::Transaction,
};
use std::{path::PathBuf, str::FromStr};
use zk_inbox_client::scan;

const DEVNET_PROGRAM_ID: &str = "EtAXw56BCmxH4Ew9JdLTjmxq21UadQjAoyLWERVNzBdL";

struct Args {
    rpc_url: String,
    program_id: Pubkey,
    chain_id: u64,
    submit: bool,
    keypair: Option<PathBuf>,
    settlement_program: Option<Pubkey>,
    next_batch_override: Option<u64>,
}

fn parse_args() -> Args {
    let mut rpc_url = "https://api.devnet.solana.com".to_string();
    let mut program_id = Pubkey::from_str(DEVNET_PROGRAM_ID).unwrap();
    let mut chain_id = None;
    let mut submit = false;
    let mut keypair = None;
    let mut settlement_program = None;
    let mut next_batch_override = None;
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let flag = argv[i].as_str();
        if flag == "--submit" {
            submit = true;
            i += 1;
            continue;
        }
        let val = || {
            argv.get(i + 1)
                .unwrap_or_else(|| panic!("{flag} needs a value"))
                .clone()
        };
        match flag {
            "--rpc-url" => rpc_url = val(),
            "--program-id" => program_id = Pubkey::from_str(&val()).expect("bad --program-id"),
            "--chain-id" => chain_id = Some(val().parse().expect("bad --chain-id")),
            "--keypair" => keypair = Some(PathBuf::from(val())),
            "--settlement-program" => {
                settlement_program =
                    Some(Pubkey::from_str(&val()).expect("bad --settlement-program"))
            }
            "--next-batch" => next_batch_override = Some(val().parse().expect("bad --next-batch")),
            other => panic!("unknown flag: {other}"),
        }
        i += 2;
    }
    Args {
        rpc_url,
        program_id,
        chain_id: chain_id.expect("--chain-id is required"),
        submit,
        keypair,
        settlement_program,
        next_batch_override,
    }
}

/// Runs one `getProgramAccounts` call with the given filters and returns the max `batch` id
/// `extract` finds across every returned account (`None` if nothing matched). A real RPC error (node
/// down, rate-limited, malformed response) propagates as `Err` — never silently read as "no accounts".
/// The `getProgramAccounts` config this scan sends to the RPC node.
///
/// `RpcProgramAccountsConfig::default()` leaves `account_config.encoding` at `None`; `solana-rpc-client` 2.1.6
/// fills in only `commitment` before sending, so the node falls back to base58 and rejects any account over 128
/// bytes with `-32602 INVALID_PARAMS_WITH_MESSAGE` before this scan ever sees an account — dead on arrival against
/// the real RPC (both chunk and batch accounts here are well over 128 B). `Base64` makes the node return the real
/// bytes regardless of account size; `get_program_accounts_with_config` decodes base64 the same as base58, so
/// nothing else about the scan changes.
fn program_accounts_config(
    filters: Vec<solana_client::rpc_filter::RpcFilterType>,
) -> RpcProgramAccountsConfig {
    RpcProgramAccountsConfig {
        filters: Some(filters),
        account_config: solana_client::rpc_config::RpcAccountInfoConfig {
            encoding: Some(solana_account_decoder_client_types::UiAccountEncoding::Base64),
            ..solana_client::rpc_config::RpcAccountInfoConfig::default()
        },
        ..RpcProgramAccountsConfig::default()
    }
}

async fn max_batch_id(
    rpc: &RpcClient,
    program_id: &Pubkey,
    filters: Vec<solana_client::rpc_filter::RpcFilterType>,
    extract: impl Fn(&[u8]) -> Option<u64>,
) -> Result<Option<u64>, String> {
    // `get_program_accounts_with_config` (returning `Vec<(Pubkey, Account)>` directly) no longer exists in the
    // Agave 4.x line (API fallout) — only `get_program_accounts` (no config) and
    // `get_program_ui_accounts_with_config` (returns `UiAccount`, needing `.to_account()` to decode back to a plain
    // `Account`).
    let accounts = rpc
        .get_program_ui_accounts_with_config(program_id, program_accounts_config(filters))
        .await
        .map_err(|e| e.to_string())?;
    highest_batch_id(&accounts, extract)
}

/// The highest batch id over the fetched accounts. An account that cannot be decoded is an ERROR that
/// stops the scan, never a silent skip: the 2.1.6 client failed the whole call with a parse error in
/// that case, and a maximum computed over fewer accounts than the node returned could hand the
/// operator a too-low `next_batch`.
fn highest_batch_id(
    accounts: &[(Pubkey, solana_account_decoder_client_types::UiAccount)],
    extract: impl Fn(&[u8]) -> Option<u64>,
) -> Result<Option<u64>, String> {
    let mut highest: Option<u64> = None;
    for (address, ui_account) in accounts {
        let account = ui_account.to_account().ok_or_else(|| {
            format!("account {address} could not be decoded; refusing to scan past it")
        })?;
        highest = highest.max(extract(&account.data));
    }
    Ok(highest)
}

#[cfg(test)]
mod config_tests {
    use super::*;

    /// `RpcProgramAccountsConfig::default()` leaves `account_config.encoding` at `None`; `solana-rpc-client` 2.1.6
    /// fills in only `commitment` before sending, so the node falls back to base58 and rejects any account over 128
    /// bytes with `-32602 INVALID_PARAMS_WITH_MESSAGE` before this scan ever sees an account — dead on arrival
    /// against the real RPC (both chunk and batch accounts here are well over 128 B). The request this scan
    /// actually sends must carry `encoding: Some(Base64)`.
    #[test]
    fn program_accounts_config_requests_base64_encoding() {
        let config = program_accounts_config(vec![]);
        assert_eq!(
            config.account_config.encoding,
            Some(solana_account_decoder_client_types::UiAccountEncoding::Base64),
            "getProgramAccounts config must request base64 encoding, else the node applies base58 \
             and rejects any account over 128 bytes before this scan sees it"
        );
    }
    fn ui_account(
        data: solana_account_decoder_client_types::UiAccountData,
    ) -> solana_account_decoder_client_types::UiAccount {
        solana_account_decoder_client_types::UiAccount {
            lamports: 1,
            data,
            owner: Pubkey::new_unique().to_string(),
            executable: false,
            rent_epoch: 0,
            space: None,
        }
    }

    fn encoded(bytes: &[u8]) -> solana_account_decoder_client_types::UiAccount {
        use solana_account_decoder_client_types::{UiAccountData, UiAccountEncoding};
        let b64 = {
            // minimal standard base64 (no extra dependency for one test)
            const T: &[u8; 64] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut out = String::new();
            for c in bytes.chunks(3) {
                let n = (c[0] as u32) << 16
                    | (*c.get(1).unwrap_or(&0) as u32) << 8
                    | *c.get(2).unwrap_or(&0) as u32;
                out.push(T[(n >> 18) as usize & 63] as char);
                out.push(T[(n >> 12) as usize & 63] as char);
                out.push(if c.len() > 1 {
                    T[(n >> 6) as usize & 63] as char
                } else {
                    '='
                });
                out.push(if c.len() > 2 {
                    T[n as usize & 63] as char
                } else {
                    '='
                });
            }
            out
        };
        ui_account(UiAccountData::Binary(b64, UiAccountEncoding::Base64))
    }

    /// An account that fails to decode used to be dropped by a `filter_map`, so the maximum was computed over fewer
    /// accounts than the node returned. It must stop the scan.
    #[test]
    fn an_undecodable_account_stops_the_scan() {
        use solana_account_decoder_client_types::{UiAccountData, UiAccountEncoding};
        let good = (Pubkey::new_unique(), encoded(&[7]));
        let bad_key = Pubkey::new_unique();
        let bad = (
            bad_key,
            ui_account(UiAccountData::Binary(
                "!!! not base64 !!!".into(),
                UiAccountEncoding::Base64,
            )),
        );
        let err = highest_batch_id(&[good, bad], |d| d.first().map(|b| *b as u64))
            .expect_err("a decode failure must be an error, not a skipped account");
        assert!(
            err.contains(&bad_key.to_string()),
            "the error names the account: {err}"
        );
    }

    #[test]
    fn the_highest_batch_id_over_decodable_accounts() {
        let accounts = [
            (Pubkey::new_unique(), encoded(&[3])),
            (Pubkey::new_unique(), encoded(&[9])),
            (Pubkey::new_unique(), encoded(&[5])),
        ];
        assert_eq!(
            highest_batch_id(&accounts, |d| d.first().map(|b| *b as u64)),
            Ok(Some(9))
        );
        assert_eq!(highest_batch_id(&[], |_| Some(1)), Ok(None));
    }
}

#[tokio::main]
async fn main() {
    let args = parse_args();
    let rpc = RpcClient::new_with_commitment(args.rpc_url.clone(), CommitmentConfig::confirmed());

    let highest_chunk = max_batch_id(
        &rpc,
        &args.program_id,
        scan::chunk_account_filters(args.chain_id),
        scan::chunk_batch_id,
    )
    .await
    .unwrap_or_else(|e| panic!("getProgramAccounts (chunk accounts) failed: {e}"));
    let highest_batch = max_batch_id(
        &rpc,
        &args.program_id,
        scan::batch_account_filters(args.chain_id),
        scan::batch_batch_id,
    )
    .await
    .unwrap_or_else(|e| panic!("getProgramAccounts (batch accounts) failed: {e}"));

    let highest = [highest_chunk, highest_batch].into_iter().flatten().max();
    let next_batch = args
        .next_batch_override
        .unwrap_or_else(|| highest.map(|h| h + 1).unwrap_or(0));

    // Findings are always printed before --submit ever sends anything.
    println!(
        "chain {}: highest chunk-account batch id = {:?}, highest batch-account batch id = {:?}",
        args.chain_id, highest_chunk, highest_batch
    );
    match highest {
        Some(h) => println!(
            "chain {}: highest touched batch id (either account kind) = {h}; InitBatchCursor next_batch >= {next_batch}",
            args.chain_id
        ),
        None => println!(
            "chain {}: no existing batch or chunk accounts found; InitBatchCursor next_batch = {next_batch}",
            args.chain_id
        ),
    }

    if !args.submit {
        return;
    }
    let keypair_path = args.keypair.expect("--submit requires --keypair");
    let settlement_program = args
        .settlement_program
        .expect("--submit requires --settlement-program");
    let authority = read_keypair_file(&keypair_path)
        .unwrap_or_else(|e| panic!("reading {keypair_path:?}: {e}"));

    // Final sanity check, using the same "Ok(None) = missing, Err propagates" discipline the scan
    // itself follows: the computed next_batch's own batch PDA must not already exist. This is a
    // best-effort belt-and-suspenders check on top of the scan above, not a substitute for it (the scan
    // already covers chunk PDAs the batch-PDA-only check below cannot see).
    let (candidate_pda, _) =
        zk_inbox_client::batch_pda(&args.program_id, args.chain_id, next_batch);
    match rpc
        .get_account_with_commitment(&candidate_pda, CommitmentConfig::confirmed())
        .await
    {
        Ok(resp) if resp.value.is_none() => {}
        Ok(resp) => panic!(
            "computed next_batch {next_batch}'s own batch PDA {candidate_pda} already exists \
             ({:?} bytes) — refusing to submit InitBatchCursor at a value the scan itself should have \
             already excluded",
            resp.value.map(|a| a.data.len())
        ),
        Err(e) => panic!("sanity-check read of {candidate_pda} failed: {e} — refusing to submit"),
    }

    let ix = zk_inbox_client::init_batch_cursor_ix(
        &args.program_id,
        &authority.pubkey(),
        args.chain_id,
        next_batch,
        &settlement_program,
    );
    let blockhash = rpc
        .get_latest_blockhash()
        .await
        .expect("get_latest_blockhash");
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&authority.pubkey()),
        &[&authority],
        blockhash,
    );
    let sig = rpc
        .send_and_confirm_transaction(&tx)
        .await
        .expect("InitBatchCursor send_and_confirm_transaction");
    println!("InitBatchCursor sent: {sig}");
}
