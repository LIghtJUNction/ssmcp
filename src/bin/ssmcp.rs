fn main() -> std::process::ExitCode {
    if let Some(exit) = ssmcp::backend::process::initialize() {
        return exit;
    }
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    if std::env::var_os("MALLOC_MMAP_THRESHOLD_").is_none()
        && std::env::var_os("GLIBC_TUNABLES").is_none()
        && std::env::var_os("LD_PRELOAD").is_none()
    {
        // Keep large transient JSON/compression buffers outside the small-object
        // heap, where long-lived catalog entries could pin their freed pages.
        // Respect explicit operator allocator settings. This runs before Tokio
        // or any client worker is started: mallopt requires single-threaded init.
        unsafe {
            libc::mallopt(libc::M_MMAP_THRESHOLD, 64 * 1024);
        }
    }
    run()
}

#[tokio::main(flavor = "current_thread")]
async fn run() -> std::process::ExitCode {
    match ssmcp::client::run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            ssmcp::client::report_error(&error.to_string());
            std::process::ExitCode::FAILURE
        }
    }
}
