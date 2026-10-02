//! Register a chain in a deployed zk-settlement program (registration-and-revenue proposal). Two paths:
//!
//! - `--reserved` (`chain_id < 2^32`): the id must already have a live `AllowReservedId` marker
//!   (`allow_reserved_id_ix`, registry-authority-only, run separately beforehand), and BOTH the chain
//!   authority and the registry authority sign this transaction — pass `--registry-keypair` for the
//!   second signer.
//! - `--permissionless` (default): the program derives `chain_id` itself from the authority's current
//!   nonce; this driver reads that nonce off-chain first (`perm_nonce_pda`, `0` if the account does not
//!   exist yet) so it knows which id — and so which `root`/`registry`/`chain_config` addresses — to use.
//!   Requires `global_config.permissionless_init_enabled` (default off) and locks the configured deposit
//!   from the payer. The chain is registered with an EMPTY registry: the program refuses a permissionless
//!   chain that brings its own verifier keys, so `--layout1-vkey-json` and `--zisk-vkey-json` are refused
//!   here by name (`VkeysNotAllowedOnPermissionless`). Until Rome registers the chain's layout-1 verifier
//!   key (`SetRegistryEntry`, see the `governance` example) the chain cannot finalize a proved root.
//!
//! `--nonce <n> --expect-chain-id <id>` (permissionless only, both together; `--nonce` alone derives the id
//! from it): send the nonce and id the caller recorded (`rollup init` writes them to `chain-id.env`) instead
//! of reading the nonce again. The program refuses a nonce below the authority's current one and an id that
//! is not derived from the nonce (`BadPermissionlessChainId`) in the same transaction, so a stale id takes
//! no deposit. Without the flags the nonce is read from Solana as before; if that read fails the command
//! stops with `NonceLookupFailed` (exit 1) rather than assuming nonce 0.
//!
//! Used for Tiber (chain 200101, reserved). Reads the genesis block hash and state root from the chain's
//! own RPC so the root account starts from the real genesis. On `--reserved` it registers three genesis verifier entries
//! (`zk_settlement_client::registry_entries_for_init`): the layout-1 primary — the ZisK stateless-validator
//! guest's own vkey, from `--layout1-vkey-json` — FIRST, then the ZisK PLONK header-RLP fallback
//! (`--zisk-vkey-json`) and a zeroed Groth16 slot. Both files are required on `--reserved`: a chain
//! registered without a layout-1 entry can never finalize a proved root.
//!
//! `chain-id` subcommand: prints the chain id the next permissionless registration of an authority will get,
//! and sends nothing. It reads the authority's `perm_nonce` account over `--rpc-url` (nonce 0 when the account
//! does not exist yet) and prints three lines, `authority=`, `nonce=` and `chain_id=`
//! (`rome_zk_layouts::chainid::permissionless_chain_id`). The authority is `--keypair <path>` (only the public
//! key is used) or `--authority <pubkey>`. `rollup init` and `rollup register` call it.
//!
//!   cargo run -p zk-settlement-client --features devnet-driver --example register_chain -- \
//!     chain-id --keypair /path/to/authority.json --settlement <PROGRAM_ID> --rpc-url <SOLANA_RPC>
//!
//! Keys: every keypair is read from a FILE PATH — the Tiber deploy pipes each payer out of its secret store
//! into a private temp file and deletes it — never into the repo.
//!
//! Usage:
//!   cargo run -p zk-settlement-client --features devnet-driver --example register_chain -- \
//!     --keypair /path/to/authority.json --settlement <PROGRAM_ID> --inbox <PROGRAM_ID> \
//!     --evm-rpc <EVM_RPC_URL> \
//!     --layout1-vkey-json fixtures/vkeys/tiber-200101-layout1.json \
//!     --zisk-vkey-json fixtures/s10/block14.calldata.json \
//!     --reserved --chain-id 200101 --registry-keypair /path/to/registry_authority.json \
//!     [--challenge-window-slots N] [--max-pending N] [--max-drift-secs N]
//!
//!   # permissionless (chain id is derived, not chosen):
//!   cargo run -p zk-settlement-client --features devnet-driver --example register_chain -- \
//!     --keypair /path/to/authority.json --settlement <PROGRAM_ID> --inbox <PROGRAM_ID> \
//!     --evm-rpc <EVM_RPC_URL>
//!   # (no vkey files: Rome registers the chain's layout-1 key afterwards with SetRegistryEntry)
//!
//! `--max-drift-secs` (default 60): the chain's timestamp drift bound, written into `chain_config` v2 —
//! `PostRootProved`'s layout-1 path binds a proof's committed value to this.
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{read_keypair_file, Signer},
    transaction::Transaction,
};
use std::str::FromStr;

fn arg(name: &str) -> Option<String> {
    let mut it = std::env::args();
    while let Some(a) = it.next() {
        if a == name {
            return it.next();
        }
    }
    None
}
fn flag(name: &str) -> bool {
    std::env::args().any(|a| a == name)
}

fn hex32(s: &str) -> [u8; 32] {
    let s = s.trim_start_matches("0x");
    let v = hex::decode(s).expect("hex");
    v.try_into().expect("32 bytes")
}

async fn evm_genesis(rpc: &str) -> ([u8; 32], [u8; 32]) {
    let body = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["0x0", false]});
    let resp: serde_json::Value = reqwest::Client::new()
        .post(rpc)
        .json(&body)
        .send()
        .await
        .expect("evm rpc")
        .json()
        .await
        .expect("json");
    let r = &resp["result"];
    (
        hex32(r["hash"].as_str().expect("hash")),
        hex32(r["stateRoot"].as_str().expect("stateRoot")),
    )
}

/// What looking up the authority's `perm_nonce` account came back with: `Ok(None)` is a missing account
/// (nonce 0), `Ok(Some(data))` is the account data, `Err` is an RPC failure of any kind.
type NonceLookup = Result<Option<Vec<u8>>, String>;

/// A missing account is nonce 0; any RPC failure, or an account that does not decode, is a named refusal
/// (`NonceLookupFailed`) and never nonce 0: an unreachable or rate-limited RPC must not read as "no
/// registrations yet".
fn perm_nonce_from_lookup(lookup: NonceLookup) -> Result<u64, String> {
    match lookup {
        Ok(None) => Ok(0),
        Ok(Some(data)) => rome_zk_layouts::perm_nonce::read(&data)
            .map(|f| f.nonce)
            .map_err(|e| {
                format!("NonceLookupFailed: the perm_nonce account does not decode: {e:?}")
            }),
        Err(e) => Err(format!(
            "NonceLookupFailed: the RPC request failed, so the registration nonce could not be read (this is not a missing account); the client said: {e}"
        )),
    }
}

/// The nonce and chain id a permissionless registration sends when the caller recorded them with
/// `--nonce <n>` and `--expect-chain-id <id>`: the program checks `nonce >= current` and
/// `chain_id == derive(authority, nonce)` in the same transaction, so a stale id is refused there and no
/// deposit is taken. `Ok(None)` means neither flag was given: read the nonce from Solana and derive the id
/// (the behaviour before the flags existed). `--expect-chain-id` needs `--nonce`, because the program
/// cannot check an id without the nonce it was derived from. Refusals are named and exit 2.
fn plan_permissionless(
    reserved: bool,
    authority: &Pubkey,
    nonce_flag: Option<&str>,
    expect_flag: Option<&str>,
) -> Result<Option<(u64, u64)>, String> {
    if nonce_flag.is_none() && expect_flag.is_none() {
        return Ok(None);
    }
    if reserved {
        return Err(
            "PermissionlessFlagsOnReserved: --nonce and --expect-chain-id belong to the \
             permissionless path; --reserved takes --chain-id"
                .to_string(),
        );
    }
    let nonce_flag = match (nonce_flag, expect_flag) {
        (Some(n), _) => n,
        (None, _) => {
            return Err(
                "ExpectChainIdNeedsNonce: --expect-chain-id <id> also needs --nonce <n>, the \
                 nonce the id was derived from, so the program can check the pair"
                    .to_string(),
            )
        }
    };
    let nonce: u64 = nonce_flag
        .parse()
        .map_err(|_| format!("BadNonce: --nonce {nonce_flag:?} is not an unsigned number"))?;
    let derived = zk_settlement_client::derive_permissionless_chain_id(authority, nonce);
    if let Some(e) = expect_flag {
        let expected: u64 = e.parse().map_err(|_| {
            format!("BadExpectChainId: --expect-chain-id {e:?} is not an unsigned number")
        })?;
        if expected != derived {
            return Err(format!(
                "ExpectedChainIdMismatch: --expect-chain-id {expected} is not the id this authority \
                 gets at nonce {nonce} ({derived}); nothing was sent"
            ));
        }
    }
    Ok(Some((nonce, derived)))
}

/// The authority's `perm_nonce`: 0 when its account does not exist yet, and a named refusal
/// (`NonceLookupFailed`, exit 1) when the RPC cannot be asked, so an unreachable or rate-limited RPC
/// never reads as "no registrations yet".
async fn read_perm_nonce(rpc: &RpcClient, settlement: &Pubkey, authority: &Pubkey) -> u64 {
    let (nonce_pda, _) = zk_settlement_client::perm_nonce_pda(settlement, authority);
    let lookup = rpc
        .get_account_with_commitment(&nonce_pda, CommitmentConfig::confirmed())
        .await
        .map(|r| r.value.map(|acc| acc.data))
        .map_err(|e| e.to_string());
    match perm_nonce_from_lookup(lookup) {
        Ok(nonce) => nonce,
        Err(refusal) => {
            eprintln!("{refusal}");
            std::process::exit(1);
        }
    }
}

/// `chain-id`: print the next permissionless chain id for an authority; send nothing.
async fn print_chain_id() {
    let authority = match (arg("--authority"), arg("--keypair")) {
        (Some(a), _) => Pubkey::from_str(&a).expect("--authority is not a public key"),
        (None, Some(path)) => read_keypair_file(&path)
            .expect("read authority keypair")
            .pubkey(),
        (None, None) => {
            eprintln!("chain-id needs --keypair <path> or --authority <pubkey>");
            std::process::exit(2);
        }
    };
    let settlement = Pubkey::from_str(&arg("--settlement").expect("--settlement")).unwrap();
    let sol_rpc = arg("--rpc-url").unwrap_or_else(|| "https://api.devnet.solana.com".to_string());
    let rpc = RpcClient::new_with_commitment(sol_rpc, CommitmentConfig::confirmed());
    let nonce = read_perm_nonce(&rpc, &settlement, &authority).await;
    println!(
        "{}",
        zk_settlement_client::PermissionlessChainId::new(authority, nonce)
    );
}

#[tokio::main]
async fn main() {
    if std::env::args().nth(1).as_deref() == Some("chain-id") {
        print_chain_id().await;
        return;
    }
    let reserved = flag("--reserved");
    let vkey_json = arg("--zisk-vkey-json");
    let layout1_vkey_json = arg("--layout1-vkey-json");
    // First thing, before any key is read or any RPC is called.
    if let Err(refusal) = zk_settlement_client::check_register_chain_flags(
        reserved,
        layout1_vkey_json.as_deref(),
        vkey_json.as_deref(),
    ) {
        eprintln!("{refusal}");
        std::process::exit(2);
    }
    let keypair = arg("--keypair").expect("--keypair <path>");
    let settlement = Pubkey::from_str(&arg("--settlement").expect("--settlement")).unwrap();
    let inbox = Pubkey::from_str(&arg("--inbox").expect("--inbox")).unwrap();
    let evm_rpc = arg("--evm-rpc").expect("--evm-rpc");
    let sol_rpc = arg("--rpc-url").unwrap_or_else(|| "https://api.devnet.solana.com".to_string());
    let challenge_window_slots: u32 = arg("--challenge-window-slots")
        .map(|v| v.parse().unwrap())
        .unwrap_or(24 * 60 * 60 * 2); // default 24 h at ~2 slots/s
    let max_pending: u32 = arg("--max-pending")
        .map(|v| v.parse().unwrap())
        .unwrap_or(9_500); // window / batch_time + 10 %
    let prove_window_slots: u32 = 4 * 60 * 60 * 2; // 4 h
    let max_drift_secs: u64 = arg("--max-drift-secs")
        .map(|v| v.parse().unwrap())
        .unwrap_or(60); // default

    let authority = read_keypair_file(&keypair).expect("read authority keypair");
    let recorded = match plan_permissionless(
        reserved,
        &authority.pubkey(),
        arg("--nonce").as_deref(),
        arg("--expect-chain-id").as_deref(),
    ) {
        Ok(recorded) => recorded,
        Err(refusal) => {
            eprintln!("{refusal}");
            std::process::exit(2);
        }
    };
    let (block_hash, state_root) = evm_genesis(&evm_rpc).await;
    // Verifier keys only exist on the reserved path; a permissionless chain registers with none.
    let registry_entries = if reserved {
        let vk: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(vkey_json.expect("checked above")).expect("vkey json"),
        )
        .unwrap();
        let zisk_vkey = hex32(vk["programVK"].as_str().expect("programVK"));
        let layout1_vk: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(layout1_vkey_json.expect("checked above"))
                .expect("layout1 vkey json"),
        )
        .unwrap();
        let layout1_vkey = hex32(layout1_vk["programVK"].as_str().expect("programVK"));
        zk_settlement_client::registry_entries_for_init(layout1_vkey, zisk_vkey)
    } else {
        vec![]
    };

    let rpc = RpcClient::new_with_commitment(sol_rpc, CommitmentConfig::confirmed());
    let fields = zk_settlement_client::InitChainFields {
        number: 0,
        parent_hash: [0u8; 32],
        state_root,
        block_hash,
        profile: 0, // public
        challenge_window_slots,
        prove_window_slots,
        proving_policy: 1,      // on-challenge
        poster_bond: 0,         // no bond
        exit_cap_per_window: 0, // 0 = exits off (ProveExit refuses ExitCapUnset) until governance sets a cap
        max_pending,
        inbox_program: inbox,
        max_drift_secs,
        registry_entries,
    };

    let (tx, chain_id) = if reserved {
        let chain_id: u64 = arg("--chain-id")
            .expect("--reserved requires --chain-id")
            .parse()
            .unwrap();
        let registry_keypair_path =
            arg("--registry-keypair").expect("--reserved requires --registry-keypair");
        let registry_authority =
            read_keypair_file(&registry_keypair_path).expect("read registry authority keypair");
        let (root_pda, _) = zk_settlement_client::root_pda(&settlement, chain_id);
        if rpc.get_account(&root_pda).await.is_ok() {
            println!("chain {chain_id} already registered: root {root_pda}");
            return;
        }
        let ix = zk_settlement_client::init_chain_reserved_ix(
            &settlement,
            &authority.pubkey(),
            &authority.pubkey(),
            &registry_authority.pubkey(),
            chain_id,
            fields,
        );
        let bh = rpc.get_latest_blockhash().await.unwrap();
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&authority.pubkey()),
            &[&authority, &registry_authority],
            bh,
        );
        (tx, chain_id)
    } else {
        // The nonce and id the caller recorded are sent as given, so the program refuses a stale pair in
        // this very transaction; without the flags the nonce is read now and the id derived from it.
        let (nonce, chain_id) = match recorded {
            Some(pair) => pair,
            None => {
                let nonce = read_perm_nonce(&rpc, &settlement, &authority.pubkey()).await;
                (
                    nonce,
                    zk_settlement_client::derive_permissionless_chain_id(
                        &authority.pubkey(),
                        nonce,
                    ),
                )
            }
        };
        let ix = zk_settlement_client::init_chain_permissionless_ix(
            &settlement,
            &authority.pubkey(),
            &authority.pubkey(),
            chain_id,
            nonce,
            fields,
        );
        let bh = rpc.get_latest_blockhash().await.unwrap();
        let tx =
            Transaction::new_signed_with_payer(&[ix], Some(&authority.pubkey()), &[&authority], bh);
        (tx, chain_id)
    };

    let sig = rpc
        .send_and_confirm_transaction(&tx)
        .await
        .expect("InitChain");
    let (root_pda, _) = zk_settlement_client::root_pda(&settlement, chain_id);
    println!(
        "chain {chain_id} registered ({}): root {root_pda}, genesis block {} state_root {}, sig {sig}",
        if reserved { "reserved" } else { "permissionless" },
        hex::encode(block_hash),
        hex::encode(state_root)
    );
    if !reserved {
        println!(
            "chain {chain_id} registered; it cannot finalize a proved root until Rome registers its layout-1 verifier key (SetRegistryEntry)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authority() -> Pubkey {
        Pubkey::new_from_array([7u8; 32])
    }

    fn nonce_account(nonce: u64) -> Vec<u8> {
        let mut d = vec![0u8; rome_zk_layouts::perm_nonce::LEN];
        rome_zk_layouts::perm_nonce::write(
            &mut d,
            &rome_zk_layouts::perm_nonce::NonceFields {
                authority: authority().to_bytes(),
                nonce,
            },
        );
        d
    }

    #[test]
    fn missing_nonce_account_is_nonce_zero() {
        assert_eq!(perm_nonce_from_lookup(Ok(None)), Ok(0));
    }

    #[test]
    fn existing_nonce_account_gives_its_nonce() {
        assert_eq!(perm_nonce_from_lookup(Ok(Some(nonce_account(3)))), Ok(3));
    }

    #[test]
    fn rpc_error_is_a_named_refusal_not_nonce_zero() {
        let err = perm_nonce_from_lookup(Err("429 Too Many Requests".into())).unwrap_err();
        assert!(err.starts_with("NonceLookupFailed"), "{err}");
        assert!(err.contains("429"), "{err}");
    }

    #[test]
    fn rpc_error_says_the_rpc_could_not_be_read_before_the_clients_text() {
        // solana-rpc-client words every send error "AccountNotFound: pubkey=...": that text must not be read as a
        // missing account, so our own sentence comes first and says so.
        let err =
            perm_nonce_from_lookup(Err("AccountNotFound: pubkey=Abc: connection refused".into()))
                .unwrap_err();
        let ours = err.find("the RPC request failed").expect(&err);
        let theirs = err.find("AccountNotFound").expect(&err);
        assert!(ours < theirs, "{err}");
        assert!(err.contains("not a missing account"), "{err}");
        assert!(err.contains("connection refused"), "{err}");
    }

    #[test]
    fn undecodable_nonce_account_is_a_named_refusal() {
        let err = perm_nonce_from_lookup(Ok(Some(vec![1, 2, 3]))).unwrap_err();
        assert!(err.starts_with("NonceLookupFailed"), "{err}");
    }

    #[test]
    fn no_flags_means_read_the_nonce_as_before() {
        assert_eq!(
            plan_permissionless(false, &authority(), None, None),
            Ok(None)
        );
    }

    #[test]
    fn recorded_nonce_and_id_are_sent_as_given() {
        let id = zk_settlement_client::derive_permissionless_chain_id(&authority(), 4);
        let plan = plan_permissionless(false, &authority(), Some("4"), Some(&id.to_string()));
        assert_eq!(plan, Ok(Some((4, id))));
    }

    #[test]
    fn nonce_alone_derives_the_id_from_it() {
        let id = zk_settlement_client::derive_permissionless_chain_id(&authority(), 2);
        assert_eq!(
            plan_permissionless(false, &authority(), Some("2"), None),
            Ok(Some((2, id)))
        );
    }

    #[test]
    fn expect_chain_id_without_nonce_is_refused_by_name() {
        let err = plan_permissionless(false, &authority(), None, Some("5")).unwrap_err();
        assert!(err.starts_with("ExpectChainIdNeedsNonce"), "{err}");
    }

    #[test]
    fn expected_id_that_does_not_match_the_nonce_is_refused_by_name() {
        let err = plan_permissionless(false, &authority(), Some("1"), Some("5")).unwrap_err();
        assert!(err.starts_with("ExpectedChainIdMismatch"), "{err}");
    }

    #[test]
    fn unparsable_flag_values_are_refused_by_name() {
        let e1 = plan_permissionless(false, &authority(), Some("x"), None).unwrap_err();
        assert!(e1.starts_with("BadNonce"), "{e1}");
        let e2 = plan_permissionless(false, &authority(), Some("1"), Some("y")).unwrap_err();
        assert!(e2.starts_with("BadExpectChainId"), "{e2}");
    }

    #[test]
    fn the_flags_are_refused_on_the_reserved_path() {
        let err = plan_permissionless(true, &authority(), Some("1"), None).unwrap_err();
        assert!(err.starts_with("PermissionlessFlagsOnReserved"), "{err}");
        assert_eq!(
            plan_permissionless(true, &authority(), None, None),
            Ok(None)
        );
    }
}
