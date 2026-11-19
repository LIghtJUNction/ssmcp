//! Bundled portal assets use content-derived URLs under the portal proxy prefix.
//! Legacy top-level URLs remain available for previously rendered pages.
use axum::{
    Router,
    http::{HeaderName, HeaderValue, header},
    response::{IntoResponse, Response},
    routing::get,
};
use sha2::{Digest, Sha256};
use std::sync::LazyLock;

struct Asset {
    name: &'static str,
    content_type: &'static str,
    body: &'static str,
    fingerprint: String,
    path: String,
    legacy_path: String,
}

impl Asset {
    fn new(name: &'static str, content_type: &'static str, body: &'static str) -> Self {
        let fingerprint = format!("{:x}", Sha256::digest(body.as_bytes()));
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

static ASSETS: LazyLock<[Asset; 6]> = LazyLock::new(|| {
    [
        Asset::new("favicon.svg", "image/svg+xml", include_str!("favicon.svg")),
        Asset::new("portal.css", "text/css; charset=utf-8", super::view::CSS),
        Asset::new(
            "portal-refresh.css",
            "text/css; charset=utf-8",
            include_str!("portal-refresh.css"),
        ),
        Asset::new(
            "traffic-meter.js",
            "text/javascript; charset=utf-8",
            include_str!("traffic-meter.js"),
        ),
        Asset::new(
            "client-install-copy.js",
            "text/javascript; charset=utf-8",
            include_str!("client-install-copy.js"),
        ),
        Asset::new(
            "copy-command.js",
            "text/javascript; charset=utf-8",
            include_str!("copy-command.js"),
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

pub(super) fn routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let mut router = Router::new();
    for asset in ASSETS.iter() {
        // Register exact content/name pairs. Unknown names and stale fingerprints
        // never reach a file lookup or a permissive fallback handler.
        router = router
            .route(
                url(asset.name),
                get(move || async move { asset.response() }),
            )
            .route(
                &asset.legacy_path,
                get(move || async move { asset.response() }),
            );
    }
    router
}

#[cfg(test)]
pub(super) fn preview_files() -> impl Iterator<Item = (&'static str, &'static str)> {
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
            axum::serve(listener, routes::<()>()).await.unwrap();
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
                assert_eq!(bytes.as_ref(), asset.body.as_bytes());
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
            axum::serve(listener, routes::<()>()).await.unwrap();
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
