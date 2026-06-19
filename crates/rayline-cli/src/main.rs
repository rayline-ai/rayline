use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    rayline_cli::run().await
}
