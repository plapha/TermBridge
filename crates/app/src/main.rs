#[tokio::main]
async fn main() -> anyhow::Result<()> {
    termbridge::cli::run_cli(termbridge::cli::parse()).await
}
