use super::*;
use rustls::{
    ProtocolVersion, ServerConfig, SupportedProtocolVersion,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
    time::timeout,
};
use tokio_rustls::TlsAcceptor;

const CA: &[u8] = include_bytes!("../../tests/fixtures/relay-trust/ca.pem");
const CERT: &[u8] = include_bytes!("../../tests/fixtures/relay-trust/server.pem");
const KEY: &[u8] = include_bytes!("../../tests/fixtures/relay-trust/server-key.pem");
const AUTH: &str = "Bearer ssmcp-http-migration-fixture";

#[test]
fn form_query_and_json_keep_the_existing_request_encoding() {
    let client = builder().build().unwrap();
    let request = client
        .post("https://example.invalid/path?existing=1")
        .query(&[("q", "a b&界"), ("q", "again")])
        .form(&[("key", "a b&界")])
        .build()
        .unwrap();
    assert_eq!(
        request.url().as_str(),
        "https://example.invalid/path?existing=1&q=a+b%26%E7%95%8C&q=again"
    );
    assert_eq!(
        request.headers()[reqwest::header::CONTENT_TYPE],
        "application/x-www-form-urlencoded"
    );
    assert_eq!(
        request.body().unwrap().as_bytes().unwrap(),
        b"key=a+b%26%E7%95%8C"
    );

    let value = serde_json::json!({"message": "界", "enabled": true});
    let request = client
        .post("https://example.invalid/json")
        .json(&value)
        .build()
        .unwrap();
    assert_eq!(
        request.headers()[reqwest::header::CONTENT_TYPE],
        "application/json"
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(request.body().unwrap().as_bytes().unwrap())
            .unwrap(),
        value
    );
}

#[derive(Default)]
struct Received {
    request: Vec<u8>,
    version: Option<ProtocolVersion>,
    alpn: Option<Vec<u8>>,
}

async fn server(
    versions: &[&'static SupportedProtocolVersion],
) -> (SocketAddr, JoinHandle<Received>) {
    let mut config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(versions)
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from_pem_slice(CERT).unwrap()],
                PrivateKeyDer::from_pem_slice(KEY).unwrap(),
            )
            .unwrap();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
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
            return Received::default();
        };
        let mut received = Received {
            version: stream.get_ref().1.protocol_version(),
            alpn: stream.get_ref().1.alpn_protocol().map(<[u8]>::to_vec),
            ..Received::default()
        };
        let mut buffer = [0; 1024];
        while received.request.len() < 8192
            && !received.request.windows(4).any(|part| part == b"\r\n\r\n")
        {
            let count = timeout(Duration::from_secs(5), stream.read(&mut buffer))
                .await
                .unwrap()
                .unwrap_or(0);
            if count == 0 {
                break;
            }
            received.request.extend_from_slice(&buffer[..count]);
        }
        if !received.request.is_empty() {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
            let _ = stream.shutdown().await;
        }
        received
    });
    (address, task)
}

async fn attempt(
    client: reqwest::ClientBuilder,
    versions: &[&'static SupportedProtocolVersion],
    host: &str,
) -> (Result<reqwest::Response, reqwest::Error>, Received) {
    let (address, task) = server(versions).await;
    let client = client
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .resolve(host, address)
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let response = client
        .get(format!("https://{host}:{}", address.port()))
        .header(reqwest::header::AUTHORIZATION, AUTH)
        .send()
        .await;
    (response, task.await.unwrap())
}

fn private_ca() -> ClientConfig {
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_slice(CA).unwrap())
        .unwrap();
    tls_config(roots)
}

#[tokio::test]
async fn bundled_roots_reject_private_ca_before_sending_credentials() {
    let (response, received) = attempt(builder(), &[&rustls::version::TLS13], "localhost").await;
    assert!(response.unwrap_err().is_connect());
    assert!(received.version.is_none());
    assert!(received.request.is_empty());
}

#[tokio::test]
async fn explicit_tls_override_keeps_tls12_tls13_http1_and_hostname_checks() {
    for (version, expected) in [
        (&rustls::version::TLS12, ProtocolVersion::TLSv1_2),
        (&rustls::version::TLS13, ProtocolVersion::TLSv1_3),
    ] {
        let (response, received) = attempt(
            builder().tls_backend_preconfigured(private_ca()),
            &[version],
            "localhost",
        )
        .await;
        let response = response.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.version(), reqwest::Version::HTTP_11);
        assert_eq!(response.text().await.unwrap(), "ok");
        assert_eq!(received.version, Some(expected));
        assert_eq!(received.alpn.as_deref(), Some(b"http/1.1".as_slice()));
        assert!(String::from_utf8_lossy(&received.request).contains(AUTH));
    }

    let (response, received) = attempt(
        builder().tls_backend_preconfigured(private_ca()),
        &[&rustls::version::TLS13],
        "different.example",
    )
    .await;
    assert!(response.unwrap_err().is_connect());
    assert!(received.version.is_none());
    assert!(received.request.is_empty());
}
