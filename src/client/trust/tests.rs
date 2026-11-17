use super::*;
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
    time::timeout,
};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::{
    Connector, connect_async_tls_with_config,
    tungstenite::{client::IntoClientRequest, http::HeaderValue},
};

const CA: &[u8] = include_bytes!("../../../tests/fixtures/relay-trust/ca.pem");
const CERT: &[u8] = include_bytes!("../../../tests/fixtures/relay-trust/server.pem");
const EXPIRED: &[u8] = include_bytes!("../../../tests/fixtures/relay-trust/expired.pem");
const PRIVATE: &[u8] = include_bytes!("../../../tests/fixtures/relay-trust/server-key.pem");
const PUBLIC: &str = include_str!("../../../tests/fixtures/relay-trust/public-key.pem");
const AUTH: &str = "Bearer ssmcp-local-test-only-token";

fn test_roots() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_slice(CA).unwrap())
        .unwrap();
    roots
}

fn config(matching: bool, trusted_ca: bool) -> Arc<ClientConfig> {
    let pins = vec![if matching {
        key_pin(PUBLIC).unwrap()
    } else {
        [0; 32]
    }];
    let roots = if trusted_ca {
        test_roots()
    } else {
        RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned())
    };
    build_tls_config(pins, roots).unwrap()
}

async fn server(websocket: bool, expired: bool) -> (SocketAddr, JoinHandle<Vec<u8>>) {
    use rustls::pki_types::PrivateKeyDer;
    let certificate = CertificateDer::from_pem_slice(if expired { EXPIRED } else { CERT }).unwrap();
    let key = PrivateKeyDer::from_pem_slice(PRIVATE).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![certificate], key)
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (stream, _) = timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let accepted = timeout(
            Duration::from_secs(5),
            TlsAcceptor::from(Arc::new(config)).accept(stream),
        )
        .await
        .unwrap();
        let Ok(mut stream) = accepted else {
            return Vec::new();
        };
        if websocket {
            let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
            let captured = seen.clone();
            // Tungstenite fixes the callback's error type to an HTTP response.
            #[allow(clippy::result_large_err)]
            let callback = move |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                                 response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                if let Some(authorization) = request.headers().get("authorization") {
                    *captured.lock().unwrap() = authorization.as_bytes().to_vec();
                }
                Ok(response)
            };
            if let Ok(Ok(mut socket)) = timeout(
                Duration::from_secs(5),
                tokio_tungstenite::accept_hdr_async(stream, callback),
            )
            .await
            {
                let _ = socket.close(None).await;
            }
            return seen.lock().unwrap().clone();
        }
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        while request.len() < 8192 && !request.windows(4).any(|part| part == b"\r\n\r\n") {
            let count = timeout(Duration::from_secs(5), stream.read(&mut buffer))
                .await
                .unwrap()
                .unwrap_or(0);
            if count == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..count]);
        }
        if !request.is_empty() {
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .await;
            let _ = stream.shutdown().await;
        }
        request
    });
    (address, task)
}

async fn https_attempt(config: Arc<ClientConfig>, expired: bool, host: &str) -> (bool, Vec<u8>) {
    let (address, task) = server(false, expired).await;
    let client = reqwest::Client::builder()
        .use_preconfigured_tls((*config).clone())
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .resolve(host, address)
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let response = client
        .get(format!("https://{host}:{}", address.port()))
        .header("authorization", AUTH)
        .send()
        .await;
    let success = response.is_ok_and(|r| r.status() == reqwest::StatusCode::OK);
    (success, task.await.unwrap())
}

async fn wss_attempt(config: Arc<ClientConfig>) -> (bool, Vec<u8>) {
    let (address, task) = server(true, false).await;
    let mut request = format!("wss://{address}/api/client/connect")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", HeaderValue::from_static(AUTH));
    let response = timeout(
        Duration::from_secs(5),
        connect_async_tls_with_config(request, None, true, Some(Connector::Rustls(config))),
    )
    .await
    .unwrap();
    let success = response.is_ok();
    if let Ok((mut socket, _)) = response {
        let _ = socket.close(None).await;
    }
    (success, task.await.unwrap())
}

#[tokio::test]
async fn https_pin_match_succeeds_and_mismatch_sends_no_credentials() {
    let (success, received) = https_attempt(config(true, true), false, "localhost").await;
    assert!(success);
    assert!(String::from_utf8_lossy(&received).contains(AUTH));
    let (success, received) = https_attempt(config(false, true), false, "localhost").await;
    assert!(!success);
    assert!(
        received.is_empty(),
        "a wrong pin must fail before HTTP credentials are sent"
    );
}

#[tokio::test]
async fn matching_pin_does_not_bypass_ca_hostname_or_expiry() {
    for (trusted_ca, expired, host) in [
        (false, false, "localhost"),
        (true, false, "different.example"),
        (true, true, "localhost"),
    ] {
        let (success, received) = https_attempt(config(true, trusted_ca), expired, host).await;
        assert!(
            !success,
            "trusted_ca={trusted_ca}, expired={expired}, host={host}"
        );
        assert!(
            received.is_empty(),
            "certificate failure leaked HTTP application data"
        );
    }
}

#[tokio::test]
async fn websocket_uses_the_same_pin_verifier_before_authorization() {
    let (success, received) = wss_attempt(config(true, true)).await;
    assert!(success);
    assert_eq!(received, AUTH.as_bytes());
    let (success, received) = wss_attempt(config(false, true)).await;
    assert!(!success);
    assert!(
        received.is_empty(),
        "a wrong pin must fail before WebSocket authorization"
    );
}

#[test]
fn pins_are_scoped_to_canonical_origin_and_support_key_rotation() {
    let a = encode_pin(&[1; 32]);
    let b = encode_pin(&[2; 32]);
    let text =
        format!("[[servers]]\nserver='https://EXAMPLE.com:443/'\npins=['{a}','{b}','{a}']\n");
    let store = Store::parse(&text).unwrap();
    assert_eq!(
        store.pins("https://example.com").unwrap(),
        vec![[1; 32], [2; 32]]
    );
    assert!(store.pins("https://other.example.com").unwrap().is_empty());
    assert!(store.pins("https://example.com:8443").unwrap().is_empty());
    assert!(store.pins("http://127.0.0.1:8080").unwrap().is_empty());
    let duplicate = format!("{text}[[servers]]\nserver='https://example.com'\npins=['{a}']\n");
    assert!(Store::parse(&duplicate).is_err());
}

#[test]
fn pem_and_fingerprint_represent_the_same_tls_public_key() {
    let pem_digest = key_pin(PUBLIC).unwrap();
    let certificate = CertificateDer::from_pem_slice(CERT).unwrap();
    let expired_certificate = CertificateDer::from_pem_slice(EXPIRED).unwrap();
    for cert in [certificate, expired_certificate] {
        let cert = webpki::EndEntityCert::try_from(&cert).unwrap();
        let expected: [u8; 32] = Sha256::digest(cert.subject_public_key_info().as_ref()).into();
        assert_eq!(
            pem_digest, expected,
            "certificate renewal must preserve a pin for the same key"
        );
    }
    let entry = Server {
        server: "https://example.com".into(),
        public_keys: vec![PUBLIC.into()],
        pins: vec![encode_pin(&pem_digest)],
    };
    assert_eq!(entry.digests().unwrap(), vec![pem_digest]);
    assert_eq!(key_pin(&PUBLIC.replace('\n', "\r\n")).unwrap(), pem_digest);
}

#[test]
fn invalid_or_empty_trust_never_silently_disables_pinning() {
    let pin = encode_pin(&[1; 32]);
    for invalid in [
        "[[servers]]\nserver='https://example.com'".to_owned(),
        "[[servers]]\nserver='https://example.com'\npins=[]".to_owned(),
        format!("[[servers]]\nserver='http://127.0.0.1'\npins=['{pin}']"),
        "[[servers]]\nserver='https://example.com'\npins=['sha256//AQID']".to_owned(),
        "[[servers]]\nserver='https://example.com'\npublic_keys=['private sensitive_material']"
            .to_owned(),
        "accidentally_pasted='sensitive_material'".to_owned(),
    ] {
        let error = Store::parse(&invalid)
            .err()
            .expect("invalid trust was accepted");
        assert!(!error.to_string().contains("sensitive_material"));
    }
    for invalid in [
        std::str::from_utf8(CERT).unwrap().to_owned(),
        std::str::from_utf8(PRIVATE).unwrap().to_owned(),
        format!("{PUBLIC}\n{PUBLIC}"),
        "-----BEGIN PUBLIC KEY-----\nAQID\n-----END PUBLIC KEY-----".to_owned(),
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIexample".to_owned(),
    ] {
        assert!(key_pin(&invalid).is_err());
    }
}

struct TemporaryDirectory(PathBuf);
impl TemporaryDirectory {
    fn new() -> Self {
        let mut random = [0; 16];
        crate::platform::random_bytes(&mut random).unwrap();
        let name: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let path = std::env::temp_dir().join(format!("ssmcp-trust-test-{name}"));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn trust_file_round_trips_and_corruption_fails_closed() {
    let temporary = TemporaryDirectory::new();
    let path = temporary.0.join("trusted-servers.toml");
    assert!(load_at(&path).unwrap().servers.is_empty());
    let store = Store {
        servers: vec![Server {
            server: "https://example.com".into(),
            public_keys: vec![PUBLIC.into()],
            pins: Vec::new(),
        }],
    };
    write_at(&path, &store).unwrap();
    assert_eq!(
        load_at(&path).unwrap().pins("https://example.com").unwrap(),
        vec![key_pin(PUBLIC).unwrap()]
    );
    fs::write(&path, "[[servers]]\nserver='https://example.com'\npins=[]").unwrap();
    assert!(load_at(&path).is_err());
}

#[cfg(unix)]
#[test]
fn a_writable_or_symlinked_trust_file_cannot_define_server_identity() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let temporary = TemporaryDirectory::new();
    let path = temporary.0.join("trusted-servers.toml");
    fs::write(&path, "servers=[]").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
    assert!(load_at(&path).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let linked = temporary.0.join("linked.toml");
    symlink(&path, &linked).unwrap();
    assert!(load_at(&linked).is_err());
}

#[tokio::test]
async fn multiple_pins_allow_a_preapproved_rotation_key() {
    let config = build_tls_config(vec![[0; 32], key_pin(PUBLIC).unwrap()], test_roots()).unwrap();
    let (success, received) = https_attempt(config, false, "localhost").await;
    assert!(success);
    assert!(String::from_utf8_lossy(&received).contains(AUTH));
}
