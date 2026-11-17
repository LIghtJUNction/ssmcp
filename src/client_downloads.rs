//! Public client installation and invitation bootstrap with bounded binary downloads.
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Path as RoutePath, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use std::{
    ffi::CString,
    fs::File,
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::MetadataExt,
    },
    path::{Component, Path},
    sync::Arc,
};
use tokio::io::AsyncReadExt;

const MAX_BINARY: u64 = 256 * 1024 * 1024;
const INSTALL_SH: &str = include_str!("../scripts/install.sh");
const INSTALL_POWERSHELL: &str = include_str!("../scripts/install.ps1");
const SH: &str = include_str!("client_downloads/install.sh");
const POWERSHELL: &str = include_str!("client_downloads/install.ps1");

#[derive(Clone)]
struct Downloads {
    install_shell: Arc<str>,
    install_powershell: Arc<str>,
    shell: Arc<str>,
    powershell: Arc<str>,
    directory: Option<Arc<File>>,
}

/// Bind bootstrap content to the configured public URL, never the request Host.
pub fn router(public_url: &str, directory: Option<&Path>) -> crate::Result<Router> {
    let origin = trusted_origin(public_url)?;
    let state = Downloads {
        install_shell: INSTALL_SH
            .replace("'https://ssmcp.lmm.best'", &shell_quote(&origin))
            .into(),
        install_powershell: INSTALL_POWERSHELL
            .replace("'https://ssmcp.lmm.best'", &powershell_quote(&origin))
            .into(),
        shell: SH.replace("@@ORIGIN@@", &shell_quote(&origin)).into(),
        powershell: POWERSHELL
            .replace("@@ORIGIN@@", &powershell_quote(&origin))
            .into(),
        directory: match directory.map(open_directory) {
            Some(Ok(file)) => Some(Arc::new(file)),
            Some(Err(error)) if error.kind() == io::ErrorKind::NotFound => None,
            Some(Err(error)) => return Err(error.into()),
            None => None,
        },
    };
    Ok(Router::new()
        .route("/install.sh", get(install_shell))
        .route("/install.ps1", get(install_powershell))
        .route("/client/install.sh", get(shell))
        .route("/client/install.ps1", get(powershell))
        .route("/client/bin/{target}", get(binary))
        .with_state(state))
}

fn trusted_origin(raw: &str) -> crate::Result<String> {
    let url = reqwest::Url::parse(raw)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("client downloads require the configured HTTPS public origin".into());
    }
    Ok(url.origin().ascii_serialization())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn open_directory(path: &Path) -> io::Result<File> {
    if !path.is_absolute() || path == Path::new("/") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "client_download_dir must be an absolute non-root directory",
        ));
    }
    let mut directory = File::open("/")?;
    for part in path.components() {
        let Component::Normal(name) = part else {
            if part == Component::RootDir {
                continue;
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "client_download_dir must not contain traversal components",
            ));
        };
        use std::os::unix::ffi::OsStrExt;
        let name = CString::new(name.as_bytes()).map_err(|_| io::ErrorKind::InvalidInput)?;
        // Every component is opened relative to its already-open parent.
        let descriptor = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        directory = unsafe { File::from_raw_fd(descriptor) };
    }
    let metadata = directory.metadata()?;
    if metadata.mode() & 0o022 != 0 || ![0, unsafe { libc::geteuid() }].contains(&metadata.uid()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "client_download_dir must be root/service-owned and not group/world writable",
        ));
    }
    Ok(directory)
}

fn filename(target: &str) -> Option<String> {
    let suffix = match target {
        "x86_64-unknown-linux-gnu"
        | "aarch64-unknown-linux-gnu"
        | "x86_64-apple-darwin"
        | "aarch64-apple-darwin" => "",
        "x86_64-pc-windows-msvc" | "aarch64-pc-windows-msvc" => ".exe",
        _ => return None,
    };
    Some(format!("ssmcp-{target}{suffix}"))
}

fn open_binary(directory: &File, filename: &str) -> io::Result<(File, u64)> {
    let name = CString::new(filename).map_err(|_| io::ErrorKind::InvalidInput)?;
    // A substituted FIFO must not block an HTTP worker.
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(descriptor) };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_BINARY
        || metadata.mode() & 0o022 != 0
        || ![0, unsafe { libc::geteuid() }].contains(&metadata.uid())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "client binary must be a bounded, root/service-owned regular file",
        ));
    }
    Ok((file, metadata.len()))
}

fn script(source: &str) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        source.to_owned(),
    )
        .into_response()
}
async fn install_shell(State(state): State<Downloads>) -> Response {
    script(&state.install_shell)
}
async fn install_powershell(State(state): State<Downloads>) -> Response {
    script(&state.install_powershell)
}
async fn shell(State(state): State<Downloads>) -> Response {
    script(&state.shell)
}
async fn powershell(State(state): State<Downloads>) -> Response {
    script(&state.powershell)
}

async fn binary(State(state): State<Downloads>, RoutePath(target): RoutePath<String>) -> Response {
    let Some(name) = filename(&target) else {
        return (StatusCode::NOT_FOUND, "Unsupported ssmcp client platform.").into_response();
    };
    let unavailable = || {
        (StatusCode::SERVICE_UNAVAILABLE,
        "This server has no client binary for this platform. Configure server.client_download_dir and place the matching ssmcp release executable there.").into_response()
    };
    let Some(directory) = state.directory else {
        return unavailable();
    };
    let Ok((file, length)) = open_binary(&directory, &name) else {
        return unavailable();
    };
    let stream = futures_util::stream::try_unfold(
        (tokio::fs::File::from_std(file), length),
        |(mut file, remaining)| async move {
            if remaining == 0 {
                return Ok::<_, io::Error>(None);
            }
            let mut chunk = vec![0u8; remaining.min(64 * 1024) as usize];
            let count = file.read(&mut chunk).await?;
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "client binary changed during download",
                ));
            }
            chunk.truncate(count);
            Ok(Some((Bytes::from(chunk), (file, remaining - count as u64))))
        },
    );
    let mut response = Body::from_stream(stream).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(header::CONTENT_LENGTH, length.into());
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    fn temporary() -> std::path::PathBuf {
        let mut random = [0; 16];
        crate::platform::random_bytes(&mut random).unwrap();
        let path = std::env::temp_dir().join(format!(
            "ssmcp-downloads-{}",
            random
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ));
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
    #[test]
    fn platform_names_are_fixed_and_cannot_traverse() {
        assert_eq!(
            filename("x86_64-pc-windows-msvc").unwrap(),
            "ssmcp-x86_64-pc-windows-msvc.exe"
        );
        for target in [
            "../etc/passwd",
            "x86_64/../../secret",
            "aarch64",
            "",
            "linux.exe",
        ] {
            assert!(filename(target).is_none());
        }
    }
    #[test]
    fn download_directory_and_binary_reject_symlinks_and_writable_files() {
        let root = temporary();
        let actual = root.join("assets");
        std::fs::create_dir(&actual).unwrap();
        let alias = root.join("alias");
        symlink(&actual, &alias).unwrap();
        assert!(open_directory(&alias).is_err());
        assert!(open_directory(&root.join("alias/nested")).is_err());
        let name = filename("x86_64-unknown-linux-gnu").unwrap();
        std::fs::write(actual.join(&name), b"test executable").unwrap();
        std::fs::set_permissions(actual.join(&name), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        let directory = open_directory(&actual).unwrap();
        assert_eq!(open_binary(&directory, &name).unwrap().1, 15);
        std::fs::set_permissions(actual.join(&name), std::fs::Permissions::from_mode(0o666))
            .unwrap();
        assert!(open_binary(&directory, &name).is_err());
        std::fs::remove_file(actual.join(&name)).unwrap();
        symlink(root.join("private"), actual.join(&name)).unwrap();
        assert!(open_binary(&directory, &name).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn downloads_stream_exact_bytes_and_missing_platform_returns_503() {
        let root = temporary();
        let name = filename("x86_64-unknown-linux-gnu").unwrap();
        let bytes = b"binary bytes\0\0test";
        std::fs::write(root.join(&name), bytes).unwrap();
        let state = Downloads {
            install_shell: "".into(),
            install_powershell: "".into(),
            shell: "".into(),
            powershell: "".into(),
            directory: Some(Arc::new(open_directory(&root).unwrap())),
        };
        let response = binary(
            State(state.clone()),
            RoutePath("x86_64-unknown-linux-gnu".into()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap()
                .as_ref(),
            bytes
        );
        assert_eq!(
            binary(
                State(state.clone()),
                RoutePath("aarch64-unknown-linux-gnu".into())
            )
            .await
            .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            binary(State(state), RoutePath("../private".into()))
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn http_routes_use_configured_origin_and_stream_only_whitelisted_assets() {
        let root = temporary();
        let name = filename("x86_64-unknown-linux-gnu").unwrap();
        let bytes = b"test executable bytes";
        std::fs::write(root.join(&name), bytes).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = router("https://configured.example", Some(&root)).unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();
        for path in [
            "install.sh",
            "install.ps1",
            "client/install.sh",
            "client/install.ps1",
        ] {
            let response = client
                .get(format!("http://{address}/{path}"))
                .header("Host", "injected.example")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()["cache-control"], "no-store");
            let source = response.text().await.unwrap();
            assert!(source.contains("https://configured.example"));
            assert!(!source.contains("injected.example"));
            assert!(!source.contains("@@ORIGIN@@"));
        }
        let response = client
            .get(format!(
                "http://{address}/client/bin/x86_64-unknown-linux-gnu"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.bytes().await.unwrap().as_ref(), bytes);
        let response = client
            .get(format!("http://{address}/client/bin/unknown-target"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        task.abort();
        let _ = task.await;
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bootstrap_origin_uses_configuration_and_escapes_shell_literals() {
        assert_eq!(
            trusted_origin("https://EXAMPLE.com:443/").unwrap(),
            "https://example.com"
        );
        for origin in [
            "http://example.com",
            "https://x/y",
            "https://u@x",
            "https://x?q=x",
        ] {
            assert!(trusted_origin(origin).is_err());
        }
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        assert_eq!(powershell_quote("a'b"), "'a''b'");
        assert!(SH.contains("/client/bin/$target"));
        assert!(!SH.contains("github.com"));
        assert!(!POWERSHELL.contains("github.com"));
    }
}
