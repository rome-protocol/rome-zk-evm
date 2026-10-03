//! Proposes the `next_batch` `InitBatchCursor` must be bootstrapped with: `head_pending_batch + 1`, read from the
//! chain's root account under the configured settlement program. That is the id settlement's `PostRoot` accepts
//! next, so a cursor there lets the chain start posting, and a cursor ABOVE it halts the chain for good.
//!
//! Anyone can create inbox accounts for any chain id under their own settlement program, so a scan of the inbox
//! program's accounts by chain id is never an input to the proposal. The scan below (batch accounts only, `ZKBT`)
//! keeps only accounts that sit at this settlement program's own `batch_pda` address AND record this settlement
//! program; it is a safety check, not a source: if a batch of this chain under this settlement program already
//! exists at `head_pending_batch + 1` or above, the tool refuses to propose, because a cursor there would collide
//! with it. A foreign batch (another settlement program's, however large its id) is ignored.
//!
//! `--next-batch N` may lower the value; a value above `head_pending_batch + 1` is refused.
//!
//! Read-only by default; pass `--submit --keypair PATH` to also send `InitBatchCursor` at the proposed
//! `next_batch` (the keypair must be the chain's root `authority`; never printed or logged). Findings are always
//! printed before `--submit` ever sends anything.
//!
//! Usage:
//!   cargo run --features devnet-driver -p zk-inbox-client --example find_max_batch_id -- \
//!     [--rpc-url URL] [--program-id ID] --chain-id ID --settlement-program ID \
//!     [--submit --keypair PATH [--next-batch N]]

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::RpcProgramAccountsConfig;
use solana_program::pubkey::Pubkey;
// `commitment_config` moved out of `solana_sdk`'s root re-export in the Agave 4.x line (API fallout) — now its own
// crate.
use solana_commitment_config::CommitmentConfig;
use solana_sdk::signature::{read_keypair_file, Signer};
use std::{path::PathBuf, str::FromStr};
use zk_inbox_client::{cursor_proposal, scan};

const DEVNET_PROGRAM_ID: &str = "EtAXw56BCmxH4Ew9JdLTjmxq21UadQjAoyLWERVNzBdL";

struct Args {
    rpc_url: String,
    program_id: Pubkey,
    chain_id: u64,
    submit: bool,
    keypair: Option<PathBuf>,
    settlement_program: Pubkey,
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
        settlement_program: settlement_program.expect("--settlement-program is required"),
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

/// Fetches every account matching `filters` under `program_id`, decoded to plain bytes. A real RPC error (node
/// down, rate-limited, malformed response) propagates as `Err` — never silently read as "no accounts".
async fn fetch_accounts(
    rpc: &RpcClient,
    program_id: &Pubkey,
    filters: Vec<solana_client::rpc_filter::RpcFilterType>,
) -> Result<Vec<(Pubkey, Vec<u8>)>, String> {
    // `get_program_accounts_with_config` (returning `Vec<(Pubkey, Account)>` directly) no longer exists in the
    // Agave 4.x line (API fallout) — only `get_program_accounts` (no config) and
    // `get_program_ui_accounts_with_config` (returns `UiAccount`, needing `.to_account()` to decode back to a plain
    // `Account`).
    let accounts = rpc
        .get_program_ui_accounts_with_config(program_id, program_accounts_config(filters))
        .await
        .map_err(|e| e.to_string())?;
    decode_accounts(&accounts)
}

/// An account that cannot be decoded is an ERROR that stops the scan, never a silent skip: a check computed
/// over fewer accounts than the node returned could miss a colliding batch.
fn decode_accounts(
    accounts: &[(Pubkey, solana_account_decoder_client_types::UiAccount)],
) -> Result<Vec<(Pubkey, Vec<u8>)>, String> {
    accounts
        .iter()
        .map(|(address, ui_account)| {
            ui_account
                .to_account()
                .map(|a| (*address, a.data))
                .ok_or_else(|| {
                    format!("account {address} could not be decoded; refusing to scan past it")
                })
        })
        .collect()
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
        let err = decode_accounts(&[good, bad])
            .expect_err("a decode failure must be an error, not a skipped account");
        assert!(
            err.contains(&bad_key.to_string()),
            "the error names the account: {err}"
        );
    }

    #[test]
    fn decodable_accounts_come_back_with_their_bytes() {
        let a = (Pubkey::new_unique(), encoded(&[3, 4]));
        let decoded = decode_accounts(&[a.clone()]).unwrap();
        assert_eq!(decoded, vec![(a.0, vec![3, 4])]);
        assert_eq!(decode_accounts(&[]), Ok(vec![]));
    }
}

#[tokio::main]
async fn main() {
    let args = parse_args();
    let rpc = RpcClient::new_with_commitment(args.rpc_url.clone(), CommitmentConfig::confirmed());
    let settlement_program = args.settlement_program;

    // The root account under the configured settlement program is the source of truth: settlement accepts only
    // `head_pending_batch + 1` as the next batch.
    let (root_address, _) = zk_inbox_client::root_pda(&settlement_program, args.chain_id);
    let root_data = rpc
        .get_account_with_commitment(&root_address, CommitmentConfig::confirmed())
        .await
        .unwrap_or_else(|e| panic!("reading the root account {root_address} failed: {e}"))
        .value
        .unwrap_or_else(|| {
            panic!(
                "no root account at {root_address}: chain {} is not registered under settlement program {settlement_program}",
                args.chain_id
            )
        })
        .data;
    let root = rome_zk_layouts::root::read(&root_data)
        .unwrap_or_else(|e| panic!("root account {root_address} could not be decoded: {e:?}"));

    // Safety check only: batch accounts of this chain under THIS settlement program (settlement-keyed address and
    // recorded settlement program). Foreign accounts are ignored however large their ids.
    let scanned = fetch_accounts(
        &rpc,
        &args.program_id,
        scan::batch_account_filters(args.chain_id),
    )
    .await
    .unwrap_or_else(|e| panic!("getProgramAccounts (batch accounts) failed: {e}"));
    let own_ids: Vec<u64> = scanned
        .iter()
        .filter_map(|(address, data)| {
            cursor_proposal::settlement_keyed_batch_id(
                &args.program_id,
                &settlement_program,
                args.chain_id,
                address,
                data,
            )
        })
        .collect();

    println!(
        "chain {}: root.head_pending_batch = {}, batch accounts returned by the scan = {}, of which under settlement program {settlement_program} = {}",
        args.chain_id,
        root.head_pending_batch,
        scanned.len(),
        own_ids.len()
    );
    let proposed = cursor_proposal::propose_next_batch(root.head_pending_batch, &own_ids)
        .unwrap_or_else(|e| panic!("{e}"));
    let next_batch = match args.next_batch_override {
        Some(n) if n > proposed => panic!(
            "--next-batch {n} is above head_pending_batch + 1 = {proposed}: a cursor there halts the chain \
             (settlement posts only head_pending_batch + 1)"
        ),
        Some(n) => n,
        None => proposed,
    };
    println!(
        "chain {}: InitBatchCursor next_batch = {next_batch} (head_pending_batch + 1 = {proposed})",
        args.chain_id
    );

    if !args.submit {
        return;
    }
    let keypair_path = args.keypair.expect("--submit requires --keypair");
    let authority = read_keypair_file(&keypair_path)
        .unwrap_or_else(|e| panic!("reading {keypair_path:?}: {e}"));

    // Final sanity check: the computed next_batch's own batch PDA must not already exist.
    let (candidate_pda, _) = zk_inbox_client::batch_pda(
        &args.program_id,
        &settlement_program,
        args.chain_id,
        next_batch,
    );
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
    // One V1 transaction through `rome-zk-solana-sender`, via the operator CLI's chain seam.
    let sig = {
        use rome_zk_ops::chain::{Chain, RpcChain};
        let signers = rome_zk_ops::keys::Signers::new(rome_zk_ops::keys::copy(&authority), vec![]);
        RpcChain::new(rpc.url())
            .send(&[ix], &signers)
            .await
            .expect("InitBatchCursor send")
    };
    println!("InitBatchCursor sent: {sig}");
}
