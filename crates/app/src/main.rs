#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = termbridge::cli::parse();
    match termbridge::cli::run_cli(cli).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            termbridge::cli::report_error(&err);
            std::process::ExitCode::FAILURE
        }
    }
}
