//! Keep HTTP clients on the bundled WebPKI roots and ring provider.
//!
//! Reqwest 0.13 otherwise selects platform trust. TLS options must be set on
//! the config before passing it to reqwest; later TLS builder options do not
//! modify a preconfigured backend. An explicit pin config may replace it.

use rustls::{ClientConfig, RootCertStore};
use std::sync::Arc;

pub(crate) fn builder() -> reqwest::ClientBuilder {
    let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    reqwest::Client::builder().tls_backend_preconfigured(tls_config(roots))
}

fn tls_config(roots: RootCertStore) -> ClientConfig {
    let mut config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("ring supports the configured TLS 1.2 and TLS 1.3 versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
    // The previous reqwest feature set did not enable HTTP/2. Preconfigured
    // backends do not receive reqwest's automatic HTTP/1.1 ALPN configuration.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    config
}

#[cfg(test)]
mod tests;
