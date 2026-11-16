#[cfg(target_os = "linux")]
include!("../server_main.rs");

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("ssmcpd runs on Linux. Use ssmcp to connect this computer to a Linux gateway.");
    std::process::ExitCode::FAILURE
}
