//! One-time bring-up of a chain's `batch_cursor` PDA in a deployed zk-inbox program. Every `OpenBatch`
//! requires the cursor to exist (it hands out sequential batch ids), and nothing else creates it — a
//! freshly deployed program plus a freshly registered chain has no cursor until this runs once.
//!
//! Authority-gated exactly like `OpenBatch`: the signer must be the chain's `authority` on the settlement program's
//! root account, so the key passed here is the chain authority (on Tiber, the payer). A cursor that already exists is
//! reported as already initialised and the command exits 0 without sending, so a re-run after a partial failure
//! completes; an RPC failure while looking is `CursorLookupFailed` (exit 1). `--next-batch` is 1 for a fresh chain
//! (batch ids are 1-based per chain; 0 is the sentinel everywhere else in this system —
//! `head_pending_batch`/`head_final_batch` == 0 always means "none"); on a chain with prior batch history it must be
//! above every batch id ever opened (see `find_max_batch_id.rs`).
//!
//! `--next-batch 0` is refused by name unless `--allow-zero` is also passed. The inbox program's own
//! `InitBatchCursor` handler accepts any value (a shipped instruction body doesn't change shape), so this refusal
//! is tooling, not a program change — a chain whose cursor is bootstrapped at 0 can never post its first batch
//! through settlement's `PostRoot`/`PostRootProved` (`first_block == pred_last_block + 1` requires the first
//! postable batch to be 1 of a chain whose inbox also starts at 1, `programs/zk-settlement/src/settle.rs`).
//!
//! Usage:
//!   cargo run -p zk-inbox-client --features devnet-driver --example init_cursor -- \
//!     --keypair /path/to/authority.json --inbox <PROGRAM_ID> --settlement <PROGRAM_ID> \
//!     --chain-id 200101 [--next-batch 1] [--allow-zero] [--rpc-url URL]
//!
//! The keypair is read from a FILE PATH and never printed (only its pubkey is).
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_program::pubkey::Pubkey;
// `commitment_config` moved out of `solana_sdk`'s root re-export in the Agave 4.x line (API fallout) — now its own
// crate.
use solana_commitment_config::CommitmentConfig;
use solana_sdk::{
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

/// Batch ids are 1-based; 0 is the sentinel everywhere else in this system (`head_pending_batch`/`head_final_batch`
/// == 0 means "none" — `programs/zk-settlement/src/ settle.rs`). The inbox program's own `InitBatchCursor` handler
/// accepts any `next_batch` value (a shipped instruction body is immutable — the guard belongs in tooling, not the
/// program); this is that tooling guard. A cursor bootstrapped at 0 can never post its first batch through
/// settlement (`first_block == pred_last_block + 1` requires the first postable batch to be 1 of a chain whose
/// inbox also starts at 1). `allow_zero` bypasses this for a deliberate exception.
fn validate_next_batch(next_batch: u64, allow_zero: bool) -> Result<(), String> {
    if next_batch == 0 && !allow_zero {
        return Err(
            "--next-batch 0 refused (batch ids are 1-based; 0 is the sentinel \
             everywhere else in this system) — pass --allow-zero to force it anyway"
                .to_string(),
        );
    }
    Ok(())
}

/// What looking up the chain's `batch_cursor` account came back with: `Ok(None)` is a missing account,
/// `Ok(Some(data))` the account data, `Err` an RPC failure of any kind.
type CursorLookup = Result<Option<Vec<u8>>, String>;

#[derive(Debug, PartialEq, Eq)]
enum CursorState {
    /// No cursor yet: send `InitBatchCursor`.
    Missing,
    /// The cursor exists (its decoded form, for the report): nothing to send, exit 0.
    AlreadyInitialised(String),
}

/// A missing cursor is sent; an existing one is reported as already initialised (a re-run after a
/// partial failure completes); an RPC failure is a named refusal (`CursorLookupFailed`), never a send
/// or an assumption either way.
fn cursor_state(lookup: CursorLookup) -> Result<CursorState, String> {
    match lookup {
        Ok(None) => Ok(CursorState::Missing),
        Ok(Some(data)) => Ok(CursorState::AlreadyInitialised(format!(
            "{:?}",
            zk_inbox_client::decode_batch_cursor(&data)
        ))),
        Err(e) => Err(format!(
            "CursorLookupFailed: the RPC request failed, so the batch_cursor could not be read (this is not a missing account); the client said: {e}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor_account(chain_id: u64, next_batch: u64) -> Vec<u8> {
        rome_zk_layouts::cursor::write(&rome_zk_layouts::cursor::CursorFields {
            chain_id,
            next_batch,
        })
        .to_vec()
    }

    #[test]
    fn missing_cursor_is_sent() {
        assert_eq!(cursor_state(Ok(None)), Ok(CursorState::Missing));
    }

    #[test]
    fn existing_cursor_is_reported_as_already_initialised_not_refused() {
        match cursor_state(Ok(Some(cursor_account(9, 4)))) {
            Ok(CursorState::AlreadyInitialised(report)) => {
                assert!(report.contains("next_batch: 4"), "{report}");
            }
            other => panic!("expected AlreadyInitialised, got {other:?}"),
        }
    }

    #[test]
    fn rpc_error_says_the_rpc_could_not_be_read_before_the_clients_text() {
        let err = cursor_state(Err("AccountNotFound: pubkey=Abc: connection refused".into()))
            .unwrap_err();
        let ours = err.find("the RPC request failed").expect(&err);
        let theirs = err.find("AccountNotFound").expect(&err);
        assert!(ours < theirs, "{err}");
        assert!(err.contains("not a missing account"), "{err}");
        assert!(err.contains("connection refused"), "{err}");
    }

    #[test]
    fn rpc_error_is_a_named_refusal_not_a_send() {
        let err = cursor_state(Err("503 Service Unavailable".into())).unwrap_err();
        assert!(err.starts_with("CursorLookupFailed"), "{err}");
        assert!(err.contains("503"), "{err}");
    }

    #[test]
    fn next_batch_zero_is_refused_unless_allow_zero_is_set() {
        assert!(validate_next_batch(0, false).is_err());
        assert!(validate_next_batch(0, true).is_ok());
        assert!(validate_next_batch(1, false).is_ok());
    }
}

#[tokio::main]
async fn main() {
    let keypair_path = arg("--keypair").expect("--keypair <path>");
    let inbox = Pubkey::from_str(&arg("--inbox").expect("--inbox <PROGRAM_ID>")).expect("inbox id");
    let settlement = Pubkey::from_str(&arg("--settlement").expect("--settlement <PROGRAM_ID>"))
        .expect("settlement id");
    let chain_id: u64 = arg("--chain-id")
        .expect("--chain-id <u64>")
        .parse()
        .expect("chain id");
    let next_batch: u64 = arg("--next-batch")
        .unwrap_or_else(|| "1".to_string())
        .parse()
        .expect("next batch");
    let allow_zero = flag("--allow-zero");
    if let Err(msg) = validate_next_batch(next_batch, allow_zero) {
        panic!("{msg}");
    }
    let rpc_url = arg("--rpc-url").unwrap_or_else(|| "https://api.devnet.solana.com".to_string());

    let authority = read_keypair_file(&keypair_path)
        .unwrap_or_else(|e| panic!("failed to read keypair at {keypair_path}: {e}"));
    let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());

    let (cursor, _) = zk_inbox_client::cursor_pda(&inbox, chain_id);
    let lookup = rpc
        .get_account_with_commitment(&cursor, CommitmentConfig::confirmed())
        .await
        .map(|r| r.value.map(|acc| acc.data))
        .map_err(|e| e.to_string());
    match cursor_state(lookup) {
        Ok(CursorState::Missing) => {}
        Ok(CursorState::AlreadyInitialised(decoded)) => {
            println!(
                "chain {chain_id} already has a batch_cursor at {cursor} ({decoded}); already initialised, nothing sent"
            );
            return;
        }
        Err(refusal) => {
            eprintln!("{refusal}");
            std::process::exit(1);
        }
    }

    let ix = zk_inbox_client::init_batch_cursor_ix(
        &inbox,
        &authority.pubkey(),
        chain_id,
        next_batch,
        &settlement,
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
        .expect("InitBatchCursor");
    println!(
        "InitBatchCursor: chain {chain_id} cursor {cursor} next_batch {next_batch}, sig {sig}"
    );
}
