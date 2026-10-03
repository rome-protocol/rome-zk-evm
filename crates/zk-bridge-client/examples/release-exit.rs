//! Releases a proved exit from the bridge vault to its recipient. A thin wrapper over `rome-zk-ops release-exit`,
//! kept so the commands that call this example keep working: same flags, and the same default, a dry run unless
//! `--confirm` is given. It reads the proved `exit_record` and the vault, creates the recipient's token account if it
//! is missing, and sends `ReleaseExit` in the same V1 transaction through `rome-zk-solana-sender`. A record that does
//! not exist or is not proved is refused by name. See `rome-zk-ops release-exit --help`.
//!
//!   cargo run -p zk-bridge-client --example release-exit --features devnet-driver -- \
//!     --settlement <PROGRAM_ID> --bridge <PROGRAM_ID> --chain-id <u64> --message-hash 0x<32 bytes> \
//!     --payer-keypair /path/to/payer.json [--rpc-url URL] [--confirm]

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let argv = rome_zk_ops::cli::example_argv_dry_by_default(&["release-exit"], &[], args)
        .expect("release-exit always forwards");
    std::process::exit(rome_zk_ops::cli::run(argv).await);
}
