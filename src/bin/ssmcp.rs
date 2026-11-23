fn main() -> std::process::ExitCode {
    let environment = match ssmcp::client::startup_environment() {
        Ok(environment) => environment,
        Err(error) => {
            ssmcp::client::report_run_error(error.as_ref());
            return std::process::ExitCode::FAILURE;
        }
    };
    for (name, value) in environment {
        // SAFETY: main has not created Tokio or started any client, native
        // worker, or other thread. The complete plan was validated first.
        unsafe { std::env::set_var(name, value) };
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
            ssmcp::client::report_run_error(error.as_ref());
            std::process::ExitCode::FAILURE
        }
    }
}
