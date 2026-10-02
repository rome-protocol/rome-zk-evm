//! `vault`: the operator's vault tool — `init-vault`, `fund`, `show-vault` for `programs/zk-bridge`'s vault,
//! same shape as `release-exit.rs`/`zk-settlement-client`'s `governance.rs` (subcommands, keys always read from
//! FILE PATHS and never printed, `--dry-run` the DEFAULT where a send is possible, `--confirm` opts in). Every
//! account list comes straight from `zk_bridge_client::{init_vault_ix, fund_ix}` — never retyped here.
//!
//! This binary is WIRE ONLY: every decision — the `VaultSettlementMismatch` refusal BEFORE `fund_ix` is built, "an
//! existing vault is never re-created", `show-vault`'s view, dry-run as the DEFAULT, `send` called exactly once on
//! `--confirm` — lives in `zk_bridge_client::vault_tool` and is pinned there by tests over fakes
//! (`plan_fund_refuses_settlement_mismatch_before_building_any_instruction`,
//! `dry_run_is_the_default_and_never_calls_send`,
//! `describe_vault_reports_not_initialized_then_the_view_with_a_balance`, ...). The example only supplies the RPC
//! reads (`PrefetchedChain`), the keypair, and the real send.
//!
//! Every chain read happens before any keypair is touched and before branching on `--dry-run`/`--confirm`
//! (mirrors `release-exit.rs`): an unreachable RPC, a not-yet-created vault, or a settlement mismatch all
//! fail loud, by name, in EITHER mode.
//!
//! Usage:
//! ```text
//! cargo run -p zk-bridge-client --example vault --features devnet-driver -- \
//!   init-vault --authority-keypair /path/to/chain_authority.json \
//!   --mint <mint pubkey> --mint-decimals 6 \
//!   --settlement <settlement program pubkey> --bridge <zk-bridge program pubkey> --chain-id <u64> \
//!   [--rpc-url URL] [--confirm]
//!
//! cargo run -p zk-bridge-client --example vault --features devnet-driver -- \
//!   fund --payer-keypair /path/to/funder.json --amount <u64> \
//!   --settlement <settlement program pubkey> --bridge <zk-bridge program pubkey> --chain-id <u64> \
//!   [--rpc-url URL] [--confirm]
//!
//! cargo run -p zk-bridge-client --example vault --features devnet-driver -- \
//!   show-vault --settlement <settlement program pubkey> --bridge <zk-bridge program pubkey> \
//!   --chain-id <u64> [--rpc-url URL]
//! ```

use base64::Engine as _;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::{
    hash::Hash,
    pubkey::Pubkey,
    signature::{read_keypair_file, Signer},
    transaction::Transaction,
};
use std::str::FromStr;

use zk_bridge_client::vault_tool::{
    describe_vault, execute, plan_fund, plan_init_vault, InitVaultPlan, Mode, Outcome, VaultChain,
};

fn arg(name: &str) -> Option<String> {
    let mut it = std::env::args();
    while let Some(a) = it.next() {
        if a == name {
            return it.next();
        }
    }
    None
}
fn required_arg(name: &str) -> String {
    arg(name).unwrap_or_else(|| panic!("{name} is required"))
}
fn pubkey_arg(name: &str) -> Pubkey {
    Pubkey::from_str(&required_arg(name)).unwrap_or_else(|_| panic!("{name}: bad pubkey"))
}
fn u64_arg(name: &str) -> u64 {
    required_arg(name)
        .parse()
        .unwrap_or_else(|_| panic!("{name}: bad u64"))
}
fn u8_arg(name: &str) -> u8 {
    required_arg(name)
        .parse()
        .unwrap_or_else(|_| panic!("{name}: bad u8"))
}
/// The dry-run-default decision is `vault_tool::Mode::from_args` — pinned by its own test.
fn mode() -> Mode {
    Mode::from_args(std::env::args())
}
fn rpc_url() -> String {
    arg("--rpc-url").unwrap_or_else(|| "https://api.devnet.solana.com".to_string())
}

fn print_account(pubkey: &Pubkey, writable: bool, signer: bool) {
    println!(
        "      {pubkey}{}{}",
        if writable { "  (writable)" } else { "" },
        if signer { "  (signer)" } else { "" },
    );
}

fn print_vault_config(cfg: &zk_bridge_client::VaultConfigAccount, pda: &Pubkey) {
    println!("  vault_config   {pda}");
    println!("    chain_id             {}", cfg.chain_id);
    println!("    settlement_program   {}", cfg.settlement_program);
    println!("    mint                 {}", cfg.mint);
    println!("    mint_decimals        {}", cfg.mint_decimals);
    println!("    authority            {}", cfg.authority);
}

/// Fetches `pubkey`, returning `Ok(None)` for a genuinely missing account (a decoded on-chain fact) and
/// `Err` only when the RPC call itself failed (unreachable host, timeout, malformed response) — never
/// conflating the two the way a bare `get_account().await.ok()` would. Same pattern
/// `rome-zk-prover`'s own `RpcFetch::get_account` already uses for exactly this reason.
async fn fetch_account_optional(
    rpc: &RpcClient,
    pubkey: &Pubkey,
) -> Result<Option<solana_sdk::account::Account>, solana_client::client_error::ClientError> {
    rpc.get_account_with_commitment(pubkey, CommitmentConfig::confirmed())
        .await
        .map(|resp| resp.value)
}

/// `vault_tool::VaultChain` over the real RPC client — the planning code is sync and RPC-free, so the
/// one read it needs is done up front and handed over as bytes.
struct PrefetchedChain {
    pda: Pubkey,
    data: Result<Option<Vec<u8>>, String>,
}
impl VaultChain for PrefetchedChain {
    fn account_data(&self, pubkey: &Pubkey) -> Result<Option<Vec<u8>>, String> {
        assert_eq!(
            *pubkey, self.pda,
            "vault_tool asked for an account this example did not prefetch"
        );
        self.data.clone()
    }
}
/// The one RPC read, done up front and FAILED LOUD by name right here — before any keypair is read and
/// before `--confirm` is decided, in EITHER mode (an unreachable RPC never gets as far as a key file).
async fn prefetch(rpc: &RpcClient, pda: Pubkey, failure_name: &str) -> PrefetchedChain {
    let data = fetch_account_optional(rpc, &pda)
        .await
        .map(|acc| acc.map(|a| a.data))
        .map_err(|e| e.to_string());
    if let Err(e) = &data {
        panic!("{failure_name}: could not fetch vault_config {pda}: {e}");
    }
    PrefetchedChain { pda, data }
}

fn dry_run_print(tx: &Transaction) {
    let bytes = bincode::serialize(tx).expect("serialize tx");
    println!("-- dry run: built and signed, nothing sent to any cluster --");
    println!(
        "  tx (base64)  {}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    );
}

fn main() {
    // Sync main + one runtime (the same shape rome-zk-exit-prover's bin uses): the planning code is
    // sync, and `execute`'s send closure needs to block on the RPC without nesting runtimes.
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let sub = std::env::args()
        .nth(1)
        .expect("usage: vault <init-vault|fund|show-vault> [flags]");
    let rpc = RpcClient::new_with_commitment(rpc_url(), CommitmentConfig::confirmed());
    let bridge = pubkey_arg("--bridge");
    let settlement = pubkey_arg("--settlement");
    let chain_id = u64_arg("--chain-id");
    let (vault_config_pda, _) = zk_bridge_client::vault_config_pda(&bridge, &settlement, chain_id);

    match sub.as_str() {
        "init-vault" => {
            let mint = pubkey_arg("--mint");
            let mint_decimals = u8_arg("--mint-decimals");
            // Order: the chain read (fails loud by name on an unreachable RPC, in EITHER mode) → the
            // authority key file (its pubkey is an input to the plan; reading a key file has no side
            // effects) → the plan → the dry-run/confirm decision. Nothing is built before the plan.
            let chain = rt.block_on(prefetch(&rpc, vault_config_pda, "VaultConfigCheckFailed"));
            let authority_pubkey = read_keypair_file(required_arg("--authority-keypair")).unwrap_or_else(
                |e| panic!("read authority keypair — must equal the settlement root.authority for chain {chain_id}: {e}"),
            );
            let ix = match plan_init_vault(
                &chain,
                &bridge,
                &settlement,
                chain_id,
                mint,
                mint_decimals,
                &authority_pubkey.pubkey(),
            )
            .unwrap_or_else(|e| panic!("{e}"))
            {
                InitVaultPlan::AlreadyInitialized {
                    vault_config_pda,
                    cfg,
                } => {
                    println!("vault_config already initialized:");
                    print_vault_config(&cfg, &vault_config_pda);
                    return;
                }
                InitVaultPlan::Create {
                    vault_config_pda,
                    ix,
                } => {
                    println!(
                        "-- init-vault: chain {chain_id}, settlement {settlement}, bridge {bridge}, mint {mint}"
                    );
                    println!("  vault_config (not yet created): {vault_config_pda}");
                    println!("  instruction: InitVault");
                    for m in &ix.accounts {
                        print_account(&m.pubkey, m.is_writable, m.is_signer);
                    }
                    ix
                }
            };
            let authority = authority_pubkey;
            let mode = mode();
            if mode == Mode::DryRun {
                // Signed against an all-zero placeholder blockhash, never fetched — the same
                // sign-for-inspection-only shape release-exit.rs/governance.rs both use.
                let tx = Transaction::new_signed_with_payer(
                    std::slice::from_ref(&ix),
                    Some(&authority.pubkey()),
                    &[&authority],
                    Hash::default(),
                );
                dry_run_print(&tx);
            }
            let outcome = execute(&ix, mode, |ix| {
                let bh = rt
                    .block_on(rpc.get_latest_blockhash())
                    .map_err(|e| e.to_string())?;
                let tx = Transaction::new_signed_with_payer(
                    std::slice::from_ref(ix),
                    Some(&authority.pubkey()),
                    &[&authority],
                    bh,
                );
                rt.block_on(rpc.send_and_confirm_transaction(&tx))
                    .map(|sig| sig.to_string())
                    .map_err(|e| e.to_string())
            })
            .unwrap_or_else(|e| panic!("send InitVault: {e}"));
            if let Outcome::Sent(sig) = outcome {
                println!("-- sent: {sig}");
            }
        }
        "fund" => {
            // Order: the chain read (fails loud by name on an unreachable RPC, in EITHER mode) → the
            // payer key file (its pubkey is an input to the plan) → `plan_fund`, which refuses a missing
            // vault or a settlement mismatch by name BEFORE any instruction exists → the dry-run/confirm
            // decision. A send is never reached on any refusal.
            let amount = u64_arg("--amount");
            let chain = rt.block_on(prefetch(&rpc, vault_config_pda, "VaultConfigFetchFailed"));
            let payer = read_keypair_file(required_arg("--payer-keypair"))
                .unwrap_or_else(|e| panic!("read payer keypair: {e}"));
            let plan = plan_fund(
                &chain,
                &bridge,
                &settlement,
                chain_id,
                &payer.pubkey(),
                amount,
            )
            .unwrap_or_else(|e| panic!("{e}"));
            println!("-- fund: chain {chain_id}, settlement {settlement}, bridge {bridge}, amount {amount}");
            print_vault_config(&plan.cfg, &plan.vault_config_pda);
            println!("  funder_token_account  {}", plan.funder_token_account);
            println!("  instruction: Fund");
            for m in &plan.ix.accounts {
                print_account(&m.pubkey, m.is_writable, m.is_signer);
            }
            let mode = mode();
            if mode == Mode::DryRun {
                let tx = Transaction::new_signed_with_payer(
                    std::slice::from_ref(&plan.ix),
                    Some(&payer.pubkey()),
                    &[&payer],
                    Hash::default(),
                );
                dry_run_print(&tx);
            }
            let outcome = execute(&plan.ix, mode, |ix| {
                let bh = rt
                    .block_on(rpc.get_latest_blockhash())
                    .map_err(|e| e.to_string())?;
                let tx = Transaction::new_signed_with_payer(
                    std::slice::from_ref(ix),
                    Some(&payer.pubkey()),
                    &[&payer],
                    bh,
                );
                rt.block_on(rpc.send_and_confirm_transaction(&tx))
                    .map(|sig| sig.to_string())
                    .map_err(|e| e.to_string())
            })
            .unwrap_or_else(|e| panic!("send Fund: {e}"));
            if let Outcome::Sent(sig) = outcome {
                println!("-- sent: {sig}");
            }
        }
        "show-vault" => {
            // Every decision is `vault_tool::describe_vault`'s (refusals by name; pinned by its test) —
            // two prefetched reads, since the planning code is sync and RPC-free.
            let cfg_chain = rt.block_on(prefetch(&rpc, vault_config_pda, "VaultConfigFetchFailed"));
            let cfg_data = cfg_chain.data.clone().expect("prefetch fails loud on Err");
            let (vault_token_pda, _) = match cfg_data
                .as_deref()
                .map(zk_bridge_client::decode_vault_config_account)
            {
                Some(Ok(cfg)) => {
                    zk_bridge_client::vault_token_pda(&bridge, &settlement, chain_id, &cfg.mint)
                }
                _ => (Pubkey::default(), 0),
            };
            let token_chain = if vault_token_pda == Pubkey::default() {
                PrefetchedChain {
                    pda: vault_token_pda,
                    data: Ok(None),
                }
            } else {
                rt.block_on(prefetch(&rpc, vault_token_pda, "VaultTokenFetchFailed"))
            };
            struct TwoReads(PrefetchedChain, PrefetchedChain);
            impl VaultChain for TwoReads {
                fn account_data(&self, pubkey: &Pubkey) -> Result<Option<Vec<u8>>, String> {
                    if *pubkey == self.0.pda {
                        self.0.data.clone()
                    } else if *pubkey == self.1.pda {
                        self.1.data.clone()
                    } else {
                        Ok(None)
                    }
                }
            }
            let chain = TwoReads(cfg_chain, token_chain);
            match describe_vault(&chain, &bridge, &settlement, chain_id).unwrap_or_else(|e| panic!("{e}")) {
                Some(view) => {
                    print_vault_config(&view.cfg, &view.vault_config_pda);
                    println!("  vault_authority      {}", view.vault_authority);
                    match view.vault_token_balance {
                        Some(balance) => println!(
                            "  vault_token          {}  balance {balance} (raw units, {} decimals)",
                            view.vault_token, view.cfg.mint_decimals
                        ),
                        None => println!(
                            "  vault_token          {}  not found (InitVault should have created it)",
                            view.vault_token
                        ),
                    }
                }
                None => println!(
                    "vault_config {vault_config_pda} (chain {chain_id}, settlement {settlement}, bridge {bridge}): not initialized"
                ),
            }
        }
        other => panic!("unknown subcommand: {other} (expected init-vault, fund, or show-vault)"),
    }
}
