//! Register a chain in a deployed zk-settlement program, or print the chain id the next permissionless
//! registration will get (`chain-id`). A thin wrapper over `rome-zk-ops register` and `rome-zk-ops chain-id`,
//! kept so the scripts that call this example keep working: it takes the same flags, and it sends unless
//! `--dry-run` is given. Every transaction goes out as a V1 transaction through `rome-zk-solana-sender`.
//!
//!   cargo run -p zk-settlement-client --features devnet-driver --example register_chain -- \
//!     --keypair /path/to/authority.json --settlement <PROGRAM_ID> --inbox <PROGRAM_ID> \
//!     --evm-rpc <EVM_RPC_URL> [--reserved --chain-id N --registry-keypair PATH \
//!     --layout1-vkey-json PATH --zisk-vkey-json PATH]
//!
//!   cargo run -p zk-settlement-client --features devnet-driver --example register_chain -- \
//!     chain-id --keypair /path/to/authority.json --settlement <PROGRAM_ID> --rpc-url <SOLANA_RPC>
//!
//! The command's own documentation is `rome-zk-ops register --help`.

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let argv = rome_zk_ops::cli::example_argv(&["register"], &[("chain-id", &["chain-id"])], args)
        .expect("register_chain always forwards");
    std::process::exit(rome_zk_ops::cli::run(argv).await);
}
