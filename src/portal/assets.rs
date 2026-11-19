//! Bundled portal assets use content-derived URLs under the portal proxy prefix.
//! Legacy top-level URLs remain available for previously rendered pages.
use crate::broker::Broker;
use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use sha2::{Digest, Sha256};
use std::sync::{Arc, LazyLock};

struct Asset {
    name: &'static str,
    content_type: &'static str,
    body: &'static [u8],
    fingerprint: String,
    path: String,
    legacy_path: String,
}

impl Asset {
    fn new(name: &'static str, content_type: &'static str, body: &'static [u8]) -> Self {
        let fingerprint = format!("{:x}", Sha256::digest(body));
        let path = format!("/account/assets/{fingerprint}/{name}");
        let legacy_path = format!("/{name}");
        Self {
            name,
            content_type,
            body,
            fingerprint,
            path,
            legacy_path,
        }
    }

    fn response(&self) -> Response {
        let mut response = (
            [
                (header::CONTENT_TYPE, self.content_type),
                (header::CACHE_CONTROL, "no-store"),
            ],
            self.body,
        )
            .into_response();
        response.headers_mut().insert(
            HeaderName::from_static("x-ssmcp-asset-sha256"),
            HeaderValue::from_str(&self.fingerprint).expect("SHA-256 contains only ASCII hex"),
        );
        response
    }
}

static ASSETS: LazyLock<[Asset; 7]> = LazyLock::new(|| {
    [
        Asset::new(
            "favicon.svg",
            "image/svg+xml",
            include_bytes!("favicon.svg"),
        ),
        Asset::new(
            "brand-mark.png",
            "image/png",
            include_bytes!("brand-mark.png"),
        ),
        Asset::new(
            "portal.css",
            "text/css; charset=utf-8",
            super::view::CSS.as_bytes(),
        ),
        Asset::new(
            "portal-refresh.css",
            "text/css; charset=utf-8",
            include_bytes!("portal-refresh.css"),
        ),
        Asset::new(
            "traffic-meter.js",
            "text/javascript; charset=utf-8",
            include_bytes!("traffic-meter.js"),
        ),
        Asset::new(
            "client-install-copy.js",
            "text/javascript; charset=utf-8",
            include_bytes!("client-install-copy.js"),
        ),
        Asset::new(
            "copy-command.js",
            "text/javascript; charset=utf-8",
            include_bytes!("copy-command.js"),
        ),
    ]
});

/// Templates and route registration use only these known bundled asset names.
pub(super) fn url(name: &str) -> &'static str {
    &ASSETS
        .iter()
        .find(|asset| asset.name == name)
        .expect("portal template referenced an unknown bundled asset")
        .path
}

pub(super) fn public_bytes(name: &str) -> Option<(&'static str, &'static [u8])> {
    ASSETS
        .iter()
        .find(|asset| asset.name == name)
        .map(|asset| (asset.content_type, asset.body))
}

async fn respond(asset: &'static Asset, broker: &Broker, expected: Option<&str>) -> Response {
    if let Some(endpoint) = &broker.portal.frontend
        && let Some(rendered) = endpoint
            .render(
                super::frontend::Request::Asset {
                    name: asset.name.into(),
                },
                asset.content_type,
                expected,
            )
            .await
    {
        let mut response = (
            [
                (header::CONTENT_TYPE, asset.content_type),
                (header::CACHE_CONTROL, "no-store"),
            ],
            rendered.body,
        )
            .into_response();
        response.headers_mut().insert(
            HeaderName::from_static("x-ssmcp-asset-sha256"),
            HeaderValue::from_str(&rendered.fingerprint).expect("computed SHA-256 is ASCII hex"),
        );
        return response;
    }
    // Old core templates may retain an old fingerprint after a worker upgrade.
    // Serve only the exact bundled bytes for that URL; never lie about a hash.
    if expected.is_none_or(|hash| hash == asset.fingerprint) {
        asset.response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

async fn fingerprinted(
    State(broker): State<Arc<Broker>>,
    Path((fingerprint, name)): Path<(String, String)>,
) -> Response {
    if fingerprint.len() != 64
        || !fingerprint
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(asset) = ASSETS.iter().find(|asset| asset.name == name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    respond(asset, &broker, Some(&fingerprint)).await
}

pub(super) fn routes() -> Router<Arc<Broker>> {
    let mut router =
        Router::new().route("/account/assets/{fingerprint}/{name}", get(fingerprinted));
    for asset in ASSETS.iter() {
        router = router.route(
            &asset.legacy_path,
            get(move |State(broker): State<Arc<Broker>>| async move {
                respond(asset, &broker, None).await
            }),
        );
    }
    router
}

#[cfg(test)]
pub(super) fn preview_files() -> impl Iterator<Item = (&'static str, &'static [u8])> {
    ASSETS.iter().flat_map(|asset| {
        [
            (asset.path.as_str(), asset.body),
            (asset.legacy_path.as_str(), asset.body),
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[tokio::test]
    async fn fingerprinted_and_legacy_assets_return_the_same_bundled_bytes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                routes().with_state(Arc::new(Broker::new("https://ssmcp.test".into()))),
            )
            .await
            .unwrap();
        });
        let client = reqwest::Client::new();
        for asset in ASSETS.iter() {
            for path in [url(asset.name), asset.legacy_path.as_str()] {
                let response = client.get(format!("{base}{path}")).send().await.unwrap();
                assert_eq!(response.status(), StatusCode::OK, "{path}");
                assert_eq!(response.headers()[header::CONTENT_TYPE], asset.content_type);
                assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
                let advertised = response.headers()["x-ssmcp-asset-sha256"]
                    .to_str()
                    .unwrap()
                    .to_owned();
                let bytes = response.bytes().await.unwrap();
                assert_eq!(bytes.as_ref(), asset.body);
                let actual = format!("{:x}", Sha256::digest(&bytes));
                assert_eq!(advertised, actual);
                assert_eq!(
                    url(asset.name),
                    format!("/account/assets/{actual}/{}", asset.name)
                );
            }
        }
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn asset_routes_reject_unknown_names_and_mismatched_fingerprints() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                routes().with_state(Arc::new(Broker::new("https://ssmcp.test".into()))),
            )
            .await
            .unwrap();
        });
        let client = reqwest::Client::new();
        let hash = &ASSETS[0].fingerprint;
        for path in [
            format!("/account/assets/{}/portal.css", "0".repeat(64)),
            format!("/account/assets/{hash}/not-bundled.js"),
            format!("/account/assets/{hash}/portal.css"),
            "/account/assets/not-a-fingerprint/portal.css".to_owned(),
        ] {
            let response = client.get(format!("{base}{path}")).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            assert!(!response.headers().contains_key("x-ssmcp-asset-sha256"));
        }
        server.abort();
        let _ = server.await;
    }
}
