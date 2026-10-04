//! The command line. `run` takes the whole argument list (program name first), prints, and returns the exit code,
//! so the example wrappers and the binary share it.

use crate::chain::{Chain, OfflineChain, RpcChain};
use crate::commands::{
    bridge, chain_id, chain_status, exit_config, init_cursor, migrate, pdas, refund_deposit,
    register::{self, RegisterRequest},
};
use crate::error::{Mode, OpsError, Report};
use crate::keys;
use clap::{Args, Parser, Subcommand};
use solana_program::pubkey::Pubkey;
use std::path::PathBuf;

pub const DEFAULT_RPC_URL: &str = "https://api.devnet.solana.com";

#[derive(Parser, Debug)]
#[command(
    name = "rome-zk-ops",
    about = "Operator commands for a Rome ZK chain. Every command is a dry run unless --confirm is given."
)]
pub struct Cli {
    /// Solana RPC URL.
    #[arg(long, global = true, default_value = DEFAULT_RPC_URL)]
    pub rpc_url: String,
    /// Send the transaction. Without it the command is a dry run.
    #[arg(long, global = true, conflicts_with_all = ["dry_run", "offline"])]
    pub confirm: bool,
    /// Build and print the transaction, send nothing. This is the default.
    #[arg(long, global = true)]
    pub dry_run: bool,
    /// Dry run with no RPC at all: every check that needs the chain is skipped and says so.
    #[arg(long, global = true)]
    pub offline: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Print the chain id the next permissionless registration of an authority will get.
    ChainId(ChainIdArgs),
    /// Print what a chain's settlement accounts say about its health (batch heads, reclaim deadline, verification keys).
    ChainStatus(ChainStatusArgs),
    /// Print the addresses of a chain's accounts. Reads nothing.
    Pdas(PdasArgs),
    /// Register a chain in the settlement program.
    Register(RegisterArgs),
    /// Give a chain's registration deposit back to its authority.
    RefundDeposit(RefundArgs),
    /// Propose, activate or show a chain's exit configuration.
    #[command(subcommand)]
    ExitConfig(ExitConfigCommand),
    /// Bring a chain that predates chain_config forward (MigrateChainV2).
    Migrate(MigrateArgs),
    /// Create a chain's batch cursor in the inbox program.
    InitCursor(InitCursorArgs),
    /// Create, fund or show a chain's bridge vault.
    #[command(subcommand)]
    Vault(VaultCommand),
    /// Release a proved exit from the bridge vault to its recipient.
    ReleaseExit(ReleaseExitArgs),
    /// Deposit into a chain: lock tokens in the bridge vault and queue a credit to an address on the chain.
    Deposit(DepositArgs),
}

#[derive(Subcommand, Debug)]
pub enum VaultCommand {
    /// Create the vault for a chain. A vault that already exists is reported and nothing is sent.
    Init(VaultInitArgs),
    /// Move tokens from the payer's token account into the vault.
    Fund(VaultFundArgs),
    /// Print the vault's configuration and token balance.
    Show(VaultShowArgs),
}

#[derive(Args, Debug)]
pub struct VaultInitArgs {
    /// The chain authority's keypair file (also the fee payer); it must equal the settlement root's authority.
    #[arg(long)]
    pub authority_keypair: PathBuf,
    #[arg(long)]
    pub mint: Pubkey,
    #[arg(long)]
    pub mint_decimals: u8,
    #[arg(long)]
    pub settlement: Pubkey,
    #[arg(long)]
    pub bridge: Pubkey,
    #[arg(long)]
    pub chain_id: u64,
}

#[derive(Args, Debug)]
pub struct VaultFundArgs {
    /// The funder's keypair file (also the fee payer).
    #[arg(long)]
    pub payer_keypair: PathBuf,
    /// Raw token units.
    #[arg(long)]
    pub amount: u64,
    #[arg(long)]
    pub settlement: Pubkey,
    #[arg(long)]
    pub bridge: Pubkey,
    #[arg(long)]
    pub chain_id: u64,
}

#[derive(Args, Debug)]
pub struct VaultShowArgs {
    #[arg(long)]
    pub settlement: Pubkey,
    #[arg(long)]
    pub bridge: Pubkey,
    #[arg(long)]
    pub chain_id: u64,
}

#[derive(Args, Debug)]
pub struct ReleaseExitArgs {
    #[arg(long)]
    pub settlement: Pubkey,
    #[arg(long)]
    pub bridge: Pubkey,
    #[arg(long)]
    pub chain_id: u64,
    /// The exit's message hash, 32 bytes of hex.
    #[arg(long, value_parser = parse_hex32)]
    pub message_hash: [u8; 32],
    /// The fee payer's keypair file.
    #[arg(long)]
    pub payer_keypair: PathBuf,
}

#[derive(Args, Debug)]
pub struct DepositArgs {
    #[arg(long)]
    pub settlement: Pubkey,
    #[arg(long)]
    pub bridge: Pubkey,
    #[arg(long)]
    pub chain_id: u64,
    /// Raw units of the vault's mint (lamports when the vault holds wrapped SOL).
    #[arg(long)]
    pub amount: u64,
    /// The address credited on the chain, 20 bytes of hex.
    #[arg(long, value_parser = parse_hex20)]
    pub recipient: [u8; 20],
    /// Wrap `--amount` lamports of the depositor's SOL first, in the same transaction.
    #[arg(long)]
    pub wrap_sol: bool,
    /// The depositor's keypair file; it signs and pays the record's rent and the fee.
    #[arg(long)]
    pub keypair: PathBuf,
}

#[derive(Args, Debug)]
pub struct ChainIdArgs {
    #[arg(long)]
    pub settlement: Pubkey,
    /// A keypair file; only its public key is used.
    #[arg(long, conflicts_with = "authority")]
    pub keypair: Option<PathBuf>,
    #[arg(long)]
    pub authority: Option<Pubkey>,
}

#[derive(Args, Debug)]
pub struct ChainStatusArgs {
    #[arg(long)]
    pub settlement: Pubkey,
    #[arg(long)]
    pub chain_id: u64,
}

#[derive(Args, Debug)]
pub struct PdasArgs {
    #[arg(long)]
    pub inbox: Pubkey,
    #[arg(long)]
    pub settlement: Pubkey,
    #[arg(long)]
    pub chain_id: u64,
}

#[derive(Args, Debug)]
pub struct RegisterArgs {
    /// The chain authority's keypair file (also the fee payer).
    #[arg(long)]
    pub keypair: PathBuf,
    #[arg(long)]
    pub settlement: Pubkey,
    #[arg(long)]
    pub inbox: Pubkey,
    /// The chain's own EVM RPC, read for block 0.
    #[arg(long)]
    pub evm_rpc: String,
    /// Reserved-id registration (needs --chain-id, --registry-keypair and both vkey files).
    #[arg(long, conflicts_with = "permissionless")]
    pub reserved: bool,
    /// The default path: the id is derived from the authority's nonce.
    #[arg(long)]
    pub permissionless: bool,
    #[arg(long)]
    pub chain_id: Option<u64>,
    #[arg(long)]
    pub registry_keypair: Option<PathBuf>,
    #[arg(long)]
    pub layout1_vkey_json: Option<PathBuf>,
    #[arg(long)]
    pub zisk_vkey_json: Option<PathBuf>,
    /// The nonce recorded when the chain id was derived.
    #[arg(long)]
    pub nonce: Option<String>,
    /// The chain id recorded with --nonce; the program refuses a stale pair.
    #[arg(long = "expect-chain-id")]
    pub expect_chain_id: Option<String>,
    #[arg(long, default_value_t = register::DEFAULT_CHALLENGE_WINDOW_SLOTS)]
    pub challenge_window_slots: u32,
    #[arg(long, default_value_t = register::DEFAULT_MAX_PENDING)]
    pub max_pending: u32,
    #[arg(long, default_value_t = register::DEFAULT_MAX_DRIFT_SECS)]
    pub max_drift_secs: u64,
}

#[derive(Args, Debug)]
pub struct RefundArgs {
    #[arg(long)]
    pub settlement: Pubkey,
    #[arg(long)]
    pub chain_id: u64,
    /// The fee payer's keypair file. The refund goes to the chain authority recorded on chain, whoever pays.
    #[arg(long)]
    pub keypair: PathBuf,
}

#[derive(Subcommand, Debug)]
pub enum ExitConfigCommand {
    /// The chain authority proposes new values with an activation slot.
    Propose(ProposeArgs),
    /// Copy a pending proposal into effect once its activation slot has passed.
    Activate(ActivateArgs),
    /// Print the current and pending exit configuration.
    Show(ShowArgs),
}

#[derive(Args, Debug)]
pub struct ProposeArgs {
    #[arg(long)]
    pub settlement: Pubkey,
    #[arg(long)]
    pub chain_id: u64,
    #[arg(long)]
    pub chain_authority_keypair: PathBuf,
    #[arg(long)]
    pub payer_keypair: PathBuf,
    /// The L1 exit portal, 20 bytes of hex.
    #[arg(long, value_parser = parse_hex20)]
    pub exit_portal: Option<[u8; 20]>,
    #[arg(long)]
    pub bridge_program: Option<Pubkey>,
    #[arg(long)]
    pub exit_cap: Option<u64>,
    #[arg(long)]
    pub poster_bond: Option<u64>,
    #[arg(long)]
    pub activation_slot: Option<u64>,
    #[arg(long)]
    pub activation_delay_slots: Option<u64>,
}

#[derive(Args, Debug)]
pub struct ActivateArgs {
    #[arg(long)]
    pub settlement: Pubkey,
    #[arg(long)]
    pub chain_id: u64,
    #[arg(long)]
    pub payer_keypair: PathBuf,
}

#[derive(Args, Debug)]
pub struct ShowArgs {
    #[arg(long)]
    pub settlement: Pubkey,
    #[arg(long)]
    pub chain_id: u64,
}

#[derive(Args, Debug)]
pub struct MigrateArgs {
    #[arg(long)]
    pub settlement: Pubkey,
    #[arg(long)]
    pub chain_id: u64,
    #[arg(long)]
    pub registry_keypair: PathBuf,
    #[arg(long)]
    pub payer_keypair: PathBuf,
    /// The chain's timestamp drift bound. Required: an operations value, never a program constant.
    #[arg(long)]
    pub max_drift_secs: u64,
}

#[derive(Args, Debug)]
pub struct InitCursorArgs {
    /// The chain authority's keypair file.
    #[arg(long)]
    pub keypair: PathBuf,
    #[arg(long)]
    pub inbox: Pubkey,
    #[arg(long)]
    pub settlement: Pubkey,
    #[arg(long)]
    pub chain_id: u64,
    #[arg(long, default_value_t = 1)]
    pub next_batch: u64,
    #[arg(long)]
    pub allow_zero: bool,
}

fn parse_hex20(s: &str) -> Result<[u8; 20], String> {
    let bytes = hex::decode(s.trim_start_matches("0x")).map_err(|_| "not valid hex".to_string())?;
    bytes
        .try_into()
        .map_err(|_| "expected 20 bytes (40 hex characters)".to_string())
}

fn parse_hex32(s: &str) -> Result<[u8; 32], String> {
    let bytes = hex::decode(s.trim_start_matches("0x")).map_err(|_| "not valid hex".to_string())?;
    bytes
        .try_into()
        .map_err(|_| "expected 32 bytes (64 hex characters)".to_string())
}

/// Runs one parsed command over `chain`. Separate from [`run`] so tests can hand it a fake chain.
pub async fn execute<C: Chain>(
    chain: &C,
    command: Command,
    mode: Mode,
) -> Result<Report, OpsError> {
    match command {
        Command::ChainId(a) => {
            let authority = match (a.authority, a.keypair) {
                (Some(p), _) => p,
                (None, Some(path)) => keys::pubkey(&keys::load(&path, "--keypair")?),
                (None, None) => {
                    return Err(OpsError::usage(
                        "AuthorityMissing",
                        "chain-id needs --keypair <path> or --authority <pubkey>",
                    ))
                }
            };
            let id = chain_id::run(chain, &a.settlement, authority).await?;
            let mut report = Report::default();
            report.line(id.to_string());
            Ok(report)
        }
        Command::ChainStatus(a) => chain_status::run(chain, &a.settlement, a.chain_id).await,
        Command::Pdas(a) => Ok(pdas::run(&a.inbox, &a.settlement, a.chain_id)),
        Command::Register(a) => {
            let req = RegisterRequest {
                keypair: a.keypair,
                settlement: a.settlement,
                inbox: a.inbox,
                evm_rpc: a.evm_rpc,
                reserved: a.reserved,
                chain_id: a.chain_id,
                registry_keypair: a.registry_keypair,
                layout1_vkey_json: a.layout1_vkey_json,
                zisk_vkey_json: a.zisk_vkey_json,
                nonce: a.nonce,
                expect_chain_id: a.expect_chain_id,
                challenge_window_slots: a.challenge_window_slots,
                max_pending: a.max_pending,
                max_drift_secs: a.max_drift_secs,
            };
            register::run(chain, req, mode).await
        }
        Command::RefundDeposit(a) => {
            let payer = keys::load(&a.keypair, "--keypair")?;
            refund_deposit::run(chain, &a.settlement, a.chain_id, payer, mode).await
        }
        Command::ExitConfig(ExitConfigCommand::Propose(a)) => {
            exit_config::propose(
                chain,
                exit_config::ProposeRequest {
                    settlement: a.settlement,
                    chain_id: a.chain_id,
                    chain_authority_keypair: a.chain_authority_keypair,
                    payer_keypair: a.payer_keypair,
                    exit_portal: a.exit_portal,
                    bridge_program: a.bridge_program,
                    exit_cap: a.exit_cap,
                    poster_bond: a.poster_bond,
                    activation_slot: a.activation_slot,
                    activation_delay_slots: a.activation_delay_slots,
                },
                mode,
            )
            .await
        }
        Command::ExitConfig(ExitConfigCommand::Activate(a)) => {
            exit_config::activate(
                chain,
                exit_config::ActivateRequest {
                    settlement: a.settlement,
                    chain_id: a.chain_id,
                    payer_keypair: a.payer_keypair,
                },
                mode,
            )
            .await
        }
        Command::ExitConfig(ExitConfigCommand::Show(a)) => {
            exit_config::show(chain, &a.settlement, a.chain_id).await
        }
        Command::Migrate(a) => {
            migrate::run(
                chain,
                migrate::MigrateRequest {
                    settlement: a.settlement,
                    chain_id: a.chain_id,
                    registry_keypair: a.registry_keypair,
                    payer_keypair: a.payer_keypair,
                    max_drift_secs: a.max_drift_secs,
                },
                mode,
            )
            .await
        }
        Command::InitCursor(a) => {
            init_cursor::run(
                chain,
                init_cursor::InitCursorRequest {
                    keypair: a.keypair,
                    inbox: a.inbox,
                    settlement: a.settlement,
                    chain_id: a.chain_id,
                    next_batch: a.next_batch,
                    allow_zero: a.allow_zero,
                },
                mode,
            )
            .await
        }
        Command::Vault(VaultCommand::Init(a)) => {
            bridge::vault_init(
                chain,
                bridge::VaultInitRequest {
                    authority_keypair: a.authority_keypair,
                    mint: a.mint,
                    mint_decimals: a.mint_decimals,
                    settlement: a.settlement,
                    bridge: a.bridge,
                    chain_id: a.chain_id,
                },
                mode,
            )
            .await
        }
        Command::Vault(VaultCommand::Fund(a)) => {
            bridge::vault_fund(
                chain,
                bridge::VaultFundRequest {
                    payer_keypair: a.payer_keypair,
                    amount: a.amount,
                    settlement: a.settlement,
                    bridge: a.bridge,
                    chain_id: a.chain_id,
                },
                mode,
            )
            .await
        }
        Command::Vault(VaultCommand::Show(a)) => {
            bridge::vault_show(chain, &a.settlement, &a.bridge, a.chain_id).await
        }
        Command::ReleaseExit(a) => {
            bridge::release_exit(
                chain,
                bridge::ReleaseExitRequest {
                    settlement: a.settlement,
                    bridge: a.bridge,
                    chain_id: a.chain_id,
                    message_hash: a.message_hash,
                    payer_keypair: a.payer_keypair,
                },
                mode,
            )
            .await
        }
        Command::Deposit(a) => {
            bridge::deposit(
                chain,
                bridge::DepositRequest {
                    settlement: a.settlement,
                    bridge: a.bridge,
                    chain_id: a.chain_id,
                    amount: a.amount,
                    l2_recipient: a.recipient,
                    wrap_sol: a.wrap_sol,
                    keypair: a.keypair,
                },
                mode,
            )
            .await
        }
    }
}

/// The argument list `rome-zk-ops` should run for one of the old cargo examples, which are thin wrappers over this
/// crate. `args` is the example's own argument list (program name left out).
///
/// - If the first argument is in `renames`, it is replaced by that subcommand path (`governance`'s
///   `propose-exit-config` becomes `exit-config propose`).
/// - Otherwise, if `default` is not empty, it is put in front of everything (`register_chain` with no
///   subcommand is `register`).
/// - Otherwise the example keeps the arguments for itself and this returns `None`.
///
/// The settlement examples sent unless told `--dry-run`, and scripts that call them still rely on that, so a
/// forwarded run carries `--confirm` unless the caller gave `--dry-run`, `--offline` or `--confirm` itself. The
/// bridge examples (`vault`, `release-exit`) were dry runs unless told `--confirm`; they use
/// [`example_argv_dry_by_default`], which forwards the arguments as given.
pub fn example_argv(
    default: &[&str],
    renames: &[(&str, &[&str])],
    args: Vec<String>,
) -> Option<Vec<String>> {
    forward_argv(default, renames, args, true)
}

/// [`example_argv`] for an example whose own default was a dry run: nothing is added to the arguments, so only a
/// `--confirm` the caller typed sends.
pub fn example_argv_dry_by_default(
    default: &[&str],
    renames: &[(&str, &[&str])],
    args: Vec<String>,
) -> Option<Vec<String>> {
    forward_argv(default, renames, args, false)
}

fn forward_argv(
    default: &[&str],
    renames: &[(&str, &[&str])],
    args: Vec<String>,
    send_by_default: bool,
) -> Option<Vec<String>> {
    let (path, rest): (Vec<String>, &[String]) = match args
        .first()
        .and_then(|first| renames.iter().find(|(from, _)| from == first))
    {
        Some((_, path)) => (path.iter().map(|p| p.to_string()).collect(), &args[1..]),
        None if !default.is_empty() => (default.iter().map(|p| p.to_string()).collect(), &args[..]),
        None => return None,
    };
    let mut argv = vec!["rome-zk-ops".to_string()];
    argv.extend(path);
    argv.extend(rest.iter().cloned());
    if send_by_default
        && !rest
            .iter()
            .any(|a| a == "--dry-run" || a == "--offline" || a == "--confirm")
    {
        argv.push("--confirm".to_string());
    }
    Some(argv)
}

/// Parses `args` (program name first), runs the command, prints, and returns the process exit code.
pub async fn run(args: Vec<String>) -> i32 {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            let code = e.exit_code();
            let _ = e.print();
            return code;
        }
    };
    let mode = if cli.confirm {
        Mode::Confirm
    } else {
        Mode::Dry
    };
    let result = if cli.offline {
        execute(&OfflineChain, cli.command, mode).await
    } else {
        execute(&RpcChain::new(cli.rpc_url), cli.command, mode).await
    };
    match result {
        Ok(report) => {
            for line in &report.lines {
                println!("{line}");
            }
            0
        }
        Err(e) => {
            eprintln!("{e}");
            e.exit_code
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        let mut v = vec!["rome-zk-ops"];
        v.extend_from_slice(args);
        Cli::try_parse_from(v)
    }

    const SETTLEMENT: &str = "11111111111111111111111111111111";

    #[test]
    fn the_default_is_a_dry_run() {
        let cli = parse(&[
            "exit-config",
            "show",
            "--settlement",
            SETTLEMENT,
            "--chain-id",
            "1",
        ])
        .unwrap();
        assert!(!cli.confirm);
    }

    #[test]
    fn confirm_and_dry_run_together_are_refused() {
        assert!(parse(&[
            "--confirm",
            "--dry-run",
            "exit-config",
            "show",
            "--settlement",
            SETTLEMENT,
            "--chain-id",
            "1"
        ])
        .is_err());
        assert!(parse(&[
            "--confirm",
            "--offline",
            "exit-config",
            "show",
            "--settlement",
            SETTLEMENT,
            "--chain-id",
            "1"
        ])
        .is_err());
    }

    #[test]
    fn a_bad_exit_portal_is_refused_at_parse_time() {
        let base = [
            "exit-config",
            "propose",
            "--settlement",
            SETTLEMENT,
            "--chain-id",
            "1",
            "--chain-authority-keypair",
            "a",
            "--payer-keypair",
            "p",
            "--activation-slot",
            "1",
        ];
        let mut short = base.to_vec();
        short.extend(["--exit-portal", "1234"]);
        assert!(parse(&short).is_err());
        let portal = "ab".repeat(20);
        let mut ok = base.to_vec();
        ok.extend(["--exit-portal", portal.as_str()]);
        assert!(parse(&ok).is_ok());
    }

    #[test]
    fn reserved_and_permissionless_conflict() {
        let args = [
            "register",
            "--keypair",
            "k",
            "--settlement",
            SETTLEMENT,
            "--inbox",
            SETTLEMENT,
            "--evm-rpc",
            "http://x",
            "--reserved",
            "--permissionless",
        ];
        assert!(parse(&args).is_err());
    }

    #[test]
    fn every_documented_subcommand_parses() {
        assert!(parse(&[
            "chain-id",
            "--settlement",
            SETTLEMENT,
            "--authority",
            SETTLEMENT
        ])
        .is_ok());
        assert!(parse(&[
            "refund-deposit",
            "--settlement",
            SETTLEMENT,
            "--chain-id",
            "5",
            "--keypair",
            "k"
        ])
        .is_ok());
        assert!(parse(&[
            "exit-config",
            "activate",
            "--settlement",
            SETTLEMENT,
            "--chain-id",
            "5",
            "--payer-keypair",
            "p"
        ])
        .is_ok());
        assert!(parse(&[
            "migrate",
            "--settlement",
            SETTLEMENT,
            "--chain-id",
            "5",
            "--registry-keypair",
            "r",
            "--payer-keypair",
            "p",
            "--max-drift-secs",
            "60"
        ])
        .is_ok());
        assert!(parse(&[
            "init-cursor",
            "--keypair",
            "k",
            "--inbox",
            SETTLEMENT,
            "--settlement",
            SETTLEMENT,
            "--chain-id",
            "5"
        ])
        .is_ok());
    }

    #[test]
    fn the_deposit_subcommand_parses() {
        let recipient = format!("0x{}", "ab".repeat(20));
        assert!(parse(&[
            "deposit",
            "--keypair",
            "k",
            "--amount",
            "7",
            "--recipient",
            &recipient,
            "--wrap-sol",
            "--settlement",
            SETTLEMENT,
            "--bridge",
            SETTLEMENT,
            "--chain-id",
            "5"
        ])
        .is_ok());
        assert!(parse(&[
            "deposit",
            "--keypair",
            "k",
            "--amount",
            "7",
            "--recipient",
            "0x12",
            "--settlement",
            SETTLEMENT,
            "--bridge",
            SETTLEMENT,
            "--chain-id",
            "5"
        ])
        .is_err());
    }

    #[test]
    fn the_bridge_subcommands_parse() {
        let hash = format!("0x{}", "ab".repeat(32));
        assert!(parse(&[
            "vault",
            "init",
            "--authority-keypair",
            "k",
            "--mint",
            SETTLEMENT,
            "--mint-decimals",
            "6",
            "--settlement",
            SETTLEMENT,
            "--bridge",
            SETTLEMENT,
            "--chain-id",
            "5"
        ])
        .is_ok());
        assert!(parse(&[
            "vault",
            "fund",
            "--payer-keypair",
            "k",
            "--amount",
            "7",
            "--settlement",
            SETTLEMENT,
            "--bridge",
            SETTLEMENT,
            "--chain-id",
            "5"
        ])
        .is_ok());
        assert!(parse(&[
            "vault",
            "show",
            "--settlement",
            SETTLEMENT,
            "--bridge",
            SETTLEMENT,
            "--chain-id",
            "5"
        ])
        .is_ok());
        let release = [
            "release-exit",
            "--settlement",
            SETTLEMENT,
            "--bridge",
            SETTLEMENT,
            "--chain-id",
            "5",
            "--payer-keypair",
            "k",
            "--message-hash",
        ];
        let mut ok = release.to_vec();
        ok.push(hash.as_str());
        let cli = parse(&ok).unwrap();
        assert!(!cli.confirm, "release-exit is a dry run by default");
        let mut short = release.to_vec();
        short.push("0x1234");
        assert!(
            parse(&short).is_err(),
            "a short message hash is refused at parse time"
        );
    }

    #[test]
    fn the_old_vault_example_names_forward_to_the_vault_subcommands() {
        let renames: &[(&str, &[&str])] = &[
            ("init-vault", &["vault", "init"]),
            ("fund", &["vault", "fund"]),
            ("show-vault", &["vault", "show"]),
        ];
        let argv = example_argv(
            &[],
            renames,
            vec!["init-vault".into(), "--chain-id".into(), "5".into()],
        )
        .unwrap();
        assert_eq!(&argv[..3], ["rome-zk-ops", "vault", "init"]);
        assert_eq!(argv.last().unwrap(), "--confirm");
    }

    #[test]
    fn migrate_needs_an_explicit_drift_bound() {
        assert!(parse(&[
            "migrate",
            "--settlement",
            SETTLEMENT,
            "--chain-id",
            "5",
            "--registry-keypair",
            "r",
            "--payer-keypair",
            "p"
        ])
        .is_err());
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn an_example_forwards_with_confirm_unless_told_dry_run() {
        let argv = example_argv(
            &["register"],
            &[("chain-id", &["chain-id"])],
            strings(&["--keypair", "k"]),
        )
        .unwrap();
        assert_eq!(
            argv,
            strings(&["rome-zk-ops", "register", "--keypair", "k", "--confirm"])
        );
        let argv = example_argv(
            &["register"],
            &[("chain-id", &["chain-id"])],
            strings(&["chain-id", "--keypair", "k"]),
        )
        .unwrap();
        assert_eq!(argv[1], "chain-id");
        let dry = example_argv(&["register"], &[], strings(&["--dry-run"])).unwrap();
        assert!(!dry.contains(&"--confirm".to_string()));
    }

    #[test]
    fn an_example_keeps_a_subcommand_it_does_not_forward() {
        let renames: &[(&str, &[&str])] = &[("show-exit-config", &["exit-config", "show"])];
        assert!(example_argv(&[], renames, strings(&["set-global-config", "--x"])).is_none());
        let argv = example_argv(
            &[],
            renames,
            strings(&["show-exit-config", "--chain-id", "1"]),
        )
        .unwrap();
        assert_eq!(&argv[1..3], &strings(&["exit-config", "show"])[..]);
    }

    #[tokio::test]
    async fn chain_id_needs_an_authority() {
        let chain = crate::commands::fake::FakeChain::default();
        let cmd = Command::ChainId(ChainIdArgs {
            settlement: SETTLEMENT.parse().unwrap(),
            keypair: None,
            authority: None,
        });
        let err = execute(&chain, cmd, Mode::Dry).await.unwrap_err();
        assert_eq!(err.name, "AuthorityMissing");
    }
}
