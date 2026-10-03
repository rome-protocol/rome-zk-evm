//! `MigrateChainV2`: the one-time bring-forward for a chain that predates the `chain_config` account. A thin
//! wrapper over `rome-zk-ops migrate`, kept so the scripts that call this example keep working: it takes the same
//! flags, and it sends unless `--dry-run` is given. Every transaction goes out as a V1 transaction through
//! `rome-zk-solana-sender`.
//!
//!   cargo run -p zk-settlement-client --features devnet-driver --example migrate_chain -- \
//!     --settlement <PROGRAM_ID> --chain-id 200101 \
//!     --registry-keypair /path/to/registry_authority.json --payer-keypair /path/to/payer.json \
//!     --max-drift-secs 60 [--rpc-url URL]

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let argv = rome_zk_ops::cli::example_argv(&["migrate"], &[], args)
        .expect("migrate_chain always forwards");
    std::process::exit(rome_zk_ops::cli::run(argv).await);
}
