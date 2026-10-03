//! One-time bring-up of a chain's `batch_cursor` PDA in a deployed zk-inbox program. A thin wrapper over
//! `rome-zk-ops init-cursor`, kept so the scripts that call this example keep working: it takes the same flags,
//! and it sends unless `--dry-run` is given. Every transaction goes out as a V1 transaction through
//! `rome-zk-solana-sender`. The rules (the signer must be the chain's authority, a cursor that already exists is
//! reported and nothing is sent, `--next-batch 0` is refused unless `--allow-zero`) are in
//! `rome-zk-ops init-cursor --help` and its README.
//!
//!   cargo run -p zk-inbox-client --features devnet-driver --example init_cursor -- \
//!     --keypair /path/to/authority.json --inbox <PROGRAM_ID> --settlement <PROGRAM_ID> \
//!     --chain-id 200101 [--next-batch 1] [--allow-zero] [--rpc-url URL]

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let argv = rome_zk_ops::cli::example_argv(&["init-cursor"], &[], args)
        .expect("init_cursor always forwards");
    std::process::exit(rome_zk_ops::cli::run(argv).await);
}
