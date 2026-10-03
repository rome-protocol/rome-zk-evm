//! The operator's vault tool: `init-vault`, `fund` and `show-vault`. A thin wrapper over `rome-zk-ops vault init`,
//! `vault fund` and `vault show`, kept so the commands that call this example keep working: same flags, and the same
//! default, a dry run unless `--confirm` is given. The checks (the settlement-mismatch refusal before `Fund` is
//! built, an existing vault is never created again) are in `zk_bridge_client::vault_tool`, and every send is one V1
//! transaction through `rome-zk-solana-sender`. See `rome-zk-ops vault --help` and the `rome-zk-ops` README.
//!
//!   cargo run -p zk-bridge-client --example vault --features devnet-driver -- \
//!     init-vault --authority-keypair /path/to/chain_authority.json --mint <MINT> --mint-decimals 6 \
//!     --settlement <PROGRAM_ID> --bridge <PROGRAM_ID> --chain-id <u64> [--rpc-url URL] [--confirm]
//!
//!   cargo run -p zk-bridge-client --example vault --features devnet-driver -- \
//!     fund --payer-keypair /path/to/funder.json --amount <u64> \
//!     --settlement <PROGRAM_ID> --bridge <PROGRAM_ID> --chain-id <u64> [--rpc-url URL] [--confirm]
//!
//!   cargo run -p zk-bridge-client --example vault --features devnet-driver -- \
//!     show-vault --settlement <PROGRAM_ID> --bridge <PROGRAM_ID> --chain-id <u64> [--rpc-url URL]

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let renames: &[(&str, &[&str])] = &[
        ("init-vault", &["vault", "init"]),
        ("fund", &["vault", "fund"]),
        ("show-vault", &["vault", "show"]),
    ];
    let Some(argv) = rome_zk_ops::cli::example_argv_dry_by_default(&[], renames, args) else {
        eprintln!("usage: vault <init-vault|fund|show-vault> [flags]");
        std::process::exit(2);
    };
    std::process::exit(rome_zk_ops::cli::run(argv).await);
}
