#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    match ssmcp::client::run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ssmcp: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "linux"))]
compile_error!("ssmcp requires Linux");
