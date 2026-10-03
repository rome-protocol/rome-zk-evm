#[tokio::main]
async fn main() {
    let code = rome_zk_ops::cli::run(std::env::args().collect()).await;
    std::process::exit(code);
}
