use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    termbridge::cli::run_cli(termbridge::cli::Cli::parse()).await
}
