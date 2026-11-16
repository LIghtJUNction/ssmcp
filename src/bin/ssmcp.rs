#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    match ssmcp::client::run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            ssmcp::client::report_error(&error.to_string());
            std::process::ExitCode::FAILURE
        }
    }
}
