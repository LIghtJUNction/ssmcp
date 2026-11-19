#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if args.len() == 1 && args[0] == "--version" {
        println!("ssmcp-frontend {}", env!("CARGO_PKG_VERSION"));
        return std::process::ExitCode::SUCCESS;
    }
    if args.len() == 1 && matches!(args[0].to_str(), Some("--help" | "-h")) {
        println!(
            "ssmcp-frontend - independent public guide and static asset renderer\n\nUsage: ssmcp-frontend --socket /run/ssmcp-frontend/render.sock\n\nThe parent directory must already exist, be owned by the service UID and mode 0700.\nOnly known public rendering operations are accepted. No OAuth/account/traffic state is opened.\nConfigure [portal] frontend_socket in the stable core. The core serves bundled pages if this worker is unavailable."
        );
        return std::process::ExitCode::SUCCESS;
    }
    if args.len() != 2 || args[0] != "--socket" {
        eprintln!("usage: ssmcp-frontend --socket ABSOLUTE_PATH");
        return std::process::ExitCode::FAILURE;
    }
    match ssmcp::portal::frontend::serve(std::path::Path::new(&args[1])).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ssmcp-frontend: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("ssmcp-frontend runs on Linux beside the stable ssmcpd core.");
    std::process::ExitCode::FAILURE
}
