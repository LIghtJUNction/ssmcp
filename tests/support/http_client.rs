//! HTTP fixture clients for standalone integration crates.

use rustls::{ClientConfig, RootCertStore};
use std::sync::Arc;

pub(super) fn builder() -> reqwest::ClientBuilder {
    let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let mut tls =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("ring supports TLS 1.2 and TLS 1.3")
            .with_root_certificates(roots)
            .with_no_client_auth();
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    reqwest::Client::builder().tls_backend_preconfigured(tls)
}
