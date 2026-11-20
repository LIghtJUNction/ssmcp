use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TARGET: &str = "x86_64-unknown-linux-gnu";
const RELEASE: &str = r#"{"tag_name":"v1.2.4","draft":false,"prerelease":false,"assets":[{"name":"ssmcp-x86_64-unknown-linux-gnu.tar.gz","state":"uploaded"},{"name":"SHA256SUMS","state":"uploaded"}]}"#;

fn installed(value: &str) -> InstalledVersion {
    InstalledVersion::parse(value).unwrap()
}

struct Server {
    root: PathBuf,
    url: String,
    requests: Arc<AtomicUsize>,
    worker: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn start(status: &str, body: &str) -> Self {
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce).unwrap();
        let root = std::env::temp_dir().join(format!(
            "ssmcp-startup-update-{:x}",
            u128::from_le_bytes(nonce)
        ));
        std::fs::create_dir(&root).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let count = requests.clone();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let worker = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let size = stream.read(&mut buffer).await.unwrap();
                    if size == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..size]);
                    assert!(request.len() <= 8192);
                }
                count.fetch_add(1, Ordering::SeqCst);
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        Self {
            root,
            url,
            requests,
            worker,
        }
    }
    fn directory(&self) -> PathBuf {
        self.root.join("cache")
    }
    fn hits(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
    fn client(&self) -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .pool_max_idle_per_host(0)
            .build()
            .unwrap()
    }
    async fn check(&self, current: &str, now: u64) -> Option<String> {
        notice_at(
            self.client(),
            &self.url,
            self.directory(),
            installed(current),
            TARGET,
            now,
        )
        .await
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.worker.abort();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn startup_notice_uses_cached_release_and_semver_without_repeated_http() {
    let server = Server::start("200 OK", RELEASE).await;
    let expected = Some("ssmcp update --check · v1.2.4 available".to_owned());
    assert_eq!(server.check("1.2.3", 1000).await, expected);
    assert_eq!(server.check("1.2.4-rc.2", 1001).await, expected);
    assert_eq!(server.check("1.2.4+local", 1001).await, None);
    assert_eq!(server.check("2.0.0", 1001).await, None);
    assert_eq!(server.check("1.2.3", 1000 + NOTICE_TTL - 1).await, expected);
    assert_eq!(server.hits(), 1);
    assert_eq!(server.check("1.2.3", 1000 + NOTICE_TTL).await, expected);
    assert_eq!(server.hits(), 2);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(server.directory().join(NOTICE_CACHE_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn startup_notice_deduplicates_each_release_without_disabling_future_checks() {
    let mut shown = std::collections::VecDeque::new();
    assert!(claim_notice(&mut shown, "v1.2.4"));
    assert!(!claim_notice(&mut shown, "v1.2.4"));
    assert!(claim_notice(&mut shown, "v1.2.5"));
    for version in 6..40 {
        assert!(claim_notice(&mut shown, &format!("v1.2.{version}")));
    }
    assert_eq!(shown.len(), 16);
    assert!(!claim_notice(&mut shown, "v1.2.39"));
}

#[tokio::test]
async fn startup_notice_failure_backoff_preserves_only_a_verified_target_cache() {
    let server = Server::start("503 Unavailable", "offline").await;
    assert_eq!(server.check("1.2.3", 1000).await, None);
    assert_eq!(server.check("1.2.3", 1000 + NOTICE_BACKOFF - 1).await, None);
    assert_eq!(server.hits(), 1);
    assert_eq!(server.check("1.2.3", 1000 + NOTICE_BACKOFF).await, None);
    assert_eq!(server.hits(), 2);
    let directory = SafeDirectory::at(server.directory()).unwrap();
    write_notice_cache(
        &directory,
        TARGET,
        1000,
        true,
        Some(Version::parse("1.2.4").unwrap()),
    )
    .unwrap();
    assert!(
        server
            .check("1.2.3", 1000 + NOTICE_TTL)
            .await
            .unwrap()
            .contains("v1.2.4")
    );
    assert_eq!(server.hits(), 3);
    assert!(server.check("1.2.3", 1001 + NOTICE_TTL).await.is_some());
    assert_eq!(server.hits(), 3);
}

#[tokio::test]
async fn startup_notice_revalidates_old_corrupt_future_and_other_target_caches() {
    for content in [
        b"not JSON".to_vec(),
        vec![b'x'; NOTICE_CACHE_LIMIT as usize + 1],
        br#"{"schema":1,"checked_at":1000,"succeeded":true,"latest":"v9.9.9"}"#.to_vec(),
        br#"{"schema":2,"checked_at":1001,"succeeded":true,"target":"x86_64-unknown-linux-gnu","latest":"v9.9.9"}"#.to_vec(),
        br#"{"schema":2,"checked_at":1000,"succeeded":true,"target":"aarch64-apple-darwin","latest":"v9.9.9"}"#.to_vec(),
        br#"{"schema":2,"checked_at":1000,"succeeded":true,"target":"x86_64-unknown-linux-gnu","latest":"v9.9.9\u001b[31m"}"#.to_vec(),
    ] {
        let server = Server::start("200 OK", RELEASE).await;
        let directory = SafeDirectory::at(server.directory()).unwrap();
        directory.write_atomic(NOTICE_CACHE_FILE, &content).unwrap();
        assert_eq!(server.check("1.2.3", 1000).await,
            Some("ssmcp update --check · v1.2.4 available".into()));
        assert_eq!(server.hits(), 1);
    }
}

#[tokio::test]
async fn startup_notice_skips_busy_cache_and_unsupported_targets_without_http() {
    let server = Server::start("200 OK", RELEASE).await;
    let directory = SafeDirectory::at(server.directory()).unwrap();
    let lock = directory.lock().unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), server.check("1.2.3", 1000))
            .await
            .unwrap(),
        None
    );
    assert_eq!(server.hits(), 0);
    drop(lock);
    assert_eq!(
        notice_at(
            server.client(),
            &server.url,
            server.directory(),
            installed("1.2.3"),
            "unknown-platform",
            1000
        )
        .await,
        None
    );
    assert_eq!(server.hits(), 0);
    assert!(server.check("1.2.3", 1000).await.is_some());
    assert_eq!(server.hits(), 1);
}

#[test]
fn startup_notice_requires_stable_release_and_uploaded_platform_artifacts() {
    assert_eq!(
        published_version(RELEASE.as_bytes(), TARGET).unwrap().tag(),
        "v1.2.4"
    );
    let base: serde_json::Value = serde_json::from_str(RELEASE).unwrap();
    for field in ["draft", "prerelease"] {
        let mut release = base.clone();
        release[field] = true.into();
        assert!(published_version(&serde_json::to_vec(&release).unwrap(), TARGET).is_err());
    }
    for tag in ["v1.2.5-rc.1", "v9.9.9\u{1b}[31m", "v01.2.5", "v1.2.5+other"] {
        let mut release = base.clone();
        release["tag_name"] = tag.into();
        assert!(published_version(&serde_json::to_vec(&release).unwrap(), TARGET).is_err());
    }
    for index in [0, 1] {
        let mut release = base.clone();
        release["assets"][index]["state"] = "uploading".into();
        assert!(published_version(&serde_json::to_vec(&release).unwrap(), TARGET).is_err());
    }
    assert!(published_version(RELEASE.as_bytes(), "aarch64-apple-darwin").is_err());
    let mut windows = base;
    windows["assets"][0]["name"] = "ssmcp-x86_64-pc-windows-msvc.zip".into();
    assert!(
        published_version(
            &serde_json::to_vec(&windows).unwrap(),
            "x86_64-pc-windows-msvc"
        )
        .is_ok()
    );
}

#[tokio::test]
async fn startup_notice_http_timeout_leaves_reconnect_backoff() {
    let fixture = Server::start("200 OK", RELEASE).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let size = stream.read(&mut buffer).await.unwrap();
            assert_ne!(size, 0);
            request.extend_from_slice(&buffer[..size]);
            assert!(request.len() < 8192);
        }
        let closed = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer))
            .await
            .unwrap();
        assert!(matches!(closed, Ok(0)) || closed.is_err(), "{closed:?}");
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .pool_max_idle_per_host(0)
        .timeout(Duration::from_millis(200))
        .build()
        .unwrap();
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(5),
            notice_at(
                client,
                &url,
                fixture.directory(),
                installed("1.2.3"),
                TARGET,
                1000
            )
        )
        .await
        .unwrap(),
        None
    );
    server.await.unwrap();
    assert_eq!(fixture.check("1.2.3", 1001).await, None);
    assert_eq!(fixture.hits(), 0);
}
