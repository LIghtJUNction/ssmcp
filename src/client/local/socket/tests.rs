use super::*;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        let mut random = [0_u8; 12];
        crate::platform::random_bytes(&mut random).unwrap();
        let name = format!("ssmcp-mcp-socket-{}", crate::hex::encode(random));
        let path = PathBuf::from("/tmp").join(name);
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }

    fn endpoint(&self) -> Endpoint {
        Endpoint::new(Some(self.0.join("mcp.sock"))).unwrap()
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        // Every file in this randomly created test directory belongs to this fixture.
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn defaults_use_runtime_directory_or_uid_fallback() {
    let runtime = Endpoint::from_environment(None, Some("/run/user/123".into()), 123).unwrap();
    assert_eq!(runtime.path, PathBuf::from("/run/user/123/ssmcp/mcp.sock"));
    let fallback = Endpoint::from_environment(None, None, 456).unwrap();
    assert_eq!(fallback.path, PathBuf::from("/tmp/ssmcp-456/mcp.sock"));
    let custom = Endpoint::from_environment(
        Some("/tmp/owned/custom.sock".into()),
        Some("relative".into()),
        123,
    )
    .unwrap();
    assert_eq!(custom.path, PathBuf::from("/tmp/owned/custom.sock"));
    assert!(custom.default_parent.is_none());
    assert!(Endpoint::from_environment(None, Some("relative".into()), 123).is_err());
}

#[test]
fn advertised_paths_must_fit_the_clients_unix_address() {
    for path in [
        "relative.sock".to_owned(),
        "/tmp/../mcp.sock".to_owned(),
        format!("/tmp/{}/mcp.sock", "x".repeat(100)),
        "/tmp/bad\0parent/mcp.sock".to_owned(),
    ] {
        assert!(Endpoint::new(Some(path.into())).is_err());
    }
    let boundary = format!("/tmp/{}/mcp.sock", "x".repeat(93));
    assert_eq!(boundary.len(), 107);
    Endpoint::new(Some(boundary.into())).unwrap();
}

#[tokio::test]
async fn default_runtime_directory_and_socket_are_private() {
    let directory = Directory::new();
    let endpoint = Endpoint::from_environment(
        None,
        Some(directory.0.clone().into_os_string()),
        super::super::effective_uid(),
    )
    .unwrap();
    let listener = endpoint.bind().await.unwrap();
    assert_eq!(
        fs::metadata(endpoint.path.parent().unwrap())
            .unwrap()
            .mode()
            & 0o777,
        0o700
    );
    let metadata = fs::symlink_metadata(&endpoint.path).unwrap();
    assert!(metadata.file_type().is_socket());
    assert_eq!(metadata.mode() & 0o777, 0o600);
    assert_eq!(metadata.uid(), super::super::effective_uid());
    drop(listener);
    assert!(!endpoint.path.exists());
    assert!(endpoint.path.with_extension("sock.lock").exists());
}

#[tokio::test]
async fn active_listener_is_preserved_and_owned_stale_socket_is_recovered() {
    if crate::test_process::isolated("client::local::socket::tests::active_listener_is_preserved_and_owned_stale_socket_is_recovered").await { return; }
    let directory = Directory::new();
    let endpoint = directory.endpoint();
    let listener = endpoint.bind().await.unwrap();
    let inode = fs::symlink_metadata(&endpoint.path).unwrap().ino();
    assert_eq!(
        endpoint.bind().await.err().unwrap().kind(),
        io::ErrorKind::AddrInUse
    );
    assert_eq!(fs::symlink_metadata(&endpoint.path).unwrap().ino(), inode);
    drop(listener);
    assert!(!endpoint.path.exists());

    // This listener has no ssmcp writer lock. A successful probe must still refuse it.
    let unmanaged = tokio::net::UnixListener::bind(&endpoint.path).unwrap();
    fs::set_permissions(&endpoint.path, fs::Permissions::from_mode(0o600)).unwrap();
    let inode = fs::symlink_metadata(&endpoint.path).unwrap().ino();
    assert_eq!(
        endpoint.bind().await.err().unwrap().kind(),
        io::ErrorKind::AddrInUse
    );
    assert_eq!(fs::symlink_metadata(&endpoint.path).unwrap().ino(), inode);
    drop(unmanaged);
    let recovered = endpoint.bind().await.unwrap();
    assert!(endpoint.path.exists());
    drop(recovered);
    assert!(!endpoint.path.exists());
}

#[tokio::test]
async fn unsafe_parent_regular_file_and_symlink_are_never_replaced() {
    let directory = Directory::new();
    let endpoint = directory.endpoint();
    fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o750)).unwrap();
    assert!(endpoint.bind().await.is_err());
    assert!(!endpoint.path.exists());
    fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(&endpoint.path, b"existing file").unwrap();
    assert!(endpoint.bind().await.is_err());
    assert_eq!(fs::read(&endpoint.path).unwrap(), b"existing file");
    fs::remove_file(&endpoint.path).unwrap();
    let target = directory.0.join("target");
    fs::write(&target, b"symlink target").unwrap();
    std::os::unix::fs::symlink(&target, &endpoint.path).unwrap();
    assert!(endpoint.bind().await.is_err());
    assert!(
        fs::symlink_metadata(&endpoint.path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(target).unwrap(), b"symlink target");
}

#[tokio::test]
async fn cleanup_preserves_a_replacement_socket() {
    let directory = Directory::new();
    let endpoint = directory.endpoint();
    let listener = endpoint.bind().await.unwrap();
    fs::rename(&endpoint.path, directory.0.join("previous.sock")).unwrap();
    let replacement = tokio::net::UnixListener::bind(&endpoint.path).unwrap();
    fs::set_permissions(&endpoint.path, fs::Permissions::from_mode(0o600)).unwrap();
    let inode = fs::symlink_metadata(&endpoint.path).unwrap().ino();
    drop(listener);
    assert_eq!(fs::symlink_metadata(&endpoint.path).unwrap().ino(), inode);
    drop(replacement);
}

async fn http(path: &std::path::Path, authority: &str, route: &str, extra: &str) -> String {
    let mut stream = UnixStream::connect(path).await.unwrap();
    stream
        .write_all(
            format!(
                "GET {route} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n{extra}\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = String::new();
    tokio::time::timeout(Duration::from_secs(3), stream.read_to_string(&mut response))
        .await
        .unwrap()
        .unwrap();
    response
}

#[tokio::test]
async fn http_socket_uses_the_same_origin_and_authenticated_mcp_router() {
    let directory = Directory::new();
    let mut settings = super::super::Settings::default();
    let path = directory.0.join("mcp.sock");
    settings.set_socket(Some(path.clone())).unwrap();
    let host = super::super::Host::start(&settings, false).await.unwrap();
    let authority = host.origin.strip_prefix("http://").unwrap();
    assert!(
        http(&path, authority, "/healthz", "")
            .await
            .starts_with("HTTP/1.1 200")
    );
    assert!(
        http(&path, "attacker.invalid", "/healthz", "")
            .await
            .starts_with("HTTP/1.1 403")
    );
    assert!(
        http(
            &path,
            authority,
            "/healthz",
            "Origin: https://attacker.invalid\r\n"
        )
        .await
        .starts_with("HTTP/1.1 403")
    );
    let unauthorized = http(&path, authority, "/socketfixture", "").await;
    assert!(unauthorized.starts_with("HTTP/1.1 401"), "{unauthorized}");
    assert!(
        unauthorized
            .to_ascii_lowercase()
            .contains("www-authenticate:")
    );
    assert!(
        http(&path, authority, "/api/client/connect", "")
            .await
            .starts_with("HTTP/1.1 404")
    );
    host.shutdown().await;
    assert!(!path.exists());
}

#[tokio::test]
async fn dropping_the_host_cleans_its_socket_without_explicit_shutdown() {
    let directory = Directory::new();
    let path = directory.0.join("mcp.sock");
    let mut settings = super::super::Settings::default();
    settings.set_socket(Some(path.clone())).unwrap();
    let host = super::super::Host::start(&settings, false).await.unwrap();
    assert!(path.exists());
    drop(host);
    tokio::time::timeout(Duration::from_secs(3), async {
        while path.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
