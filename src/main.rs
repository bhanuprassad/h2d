#[tokio::main]
async fn main() {
    std::process::exit(h2d::cli::run().await);
}
