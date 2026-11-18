//! Public, script-free documentation. It never creates an account session.
use crate::broker::Broker;
use axum::{Router, extract::State, http::header, response::Html, routing::get};
use std::sync::Arc;

const ENGLISH: &str = include_str!("guide/en.html");
const CHINESE: &str = include_str!("guide/zh.html");
const CSS: &str = include_str!("guide/style.css");

#[derive(Clone, Copy)]
enum Language {
    English,
    Chinese,
}

pub(super) fn routes() -> Router<Arc<Broker>> {
    // The trailing slash avoids shadowing the existing single-segment /{user}
    // MCP route. "guide" has never been a reserved legacy account name.
    Router::new()
        .route("/guide/", get(english))
        .route("/guide/en", get(english))
        .route("/guide/zh", get(chinese))
        .route(
            "/guide/style.css",
            get(|| async { ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], CSS) }),
        )
}

async fn english(State(broker): State<Arc<Broker>>) -> Html<String> {
    Html(page(Language::English, &broker.public_url))
}

async fn chinese(State(broker): State<Arc<Broker>>) -> Html<String> {
    Html(page(Language::Chinese, &broker.public_url))
}

fn page(language: Language, public_url: &str) -> String {
    let origin = reqwest::Url::parse(public_url)
        .ok()
        .filter(|url| {
            matches!(url.scheme(), "https" | "http")
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.host_str().is_some()
        })
        .map(|url| url.origin().ascii_serialization())
        .unwrap_or_else(|| "https://relay.example".into());
    let authority = origin
        .split_once("://")
        .map_or("relay.example", |(_, host)| host);
    let template = match language {
        Language::English => ENGLISH,
        Language::Chinese => CHINESE,
    };
    template
        .replace("{{gateway_origin}}", &super::view::escape(&origin))
        .replace("{{gateway_authority}}", &super::view::escape(authority))
        .replace("{{version}}", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn localized_pages_keep_examples_and_exclude_credentials() {
        for (language, locale) in [(Language::English, "en"), (Language::Chinese, "zh-CN")] {
            let html = page(language, "https://relay.example:8443");
            assert!(html.contains(&format!("<html lang=\"{locale}\">")));
            for expected in [
                "href=\"/guide/en\"",
                "href=\"/guide/zh\"",
                "ssmcp yourname@relay.example:8443",
                "https://relay.example:8443/mcp",
                "@src/main.rs",
                "ssmcp.fs.read_image",
                "tools.search",
                "/handoff",
                "/resume",
            ] {
                assert!(html.contains(expected), "missing {expected}");
            }
            assert!(!html.contains("{{"));
            assert!(!html.contains("<script"));
        }
        for origin in [
            "https://user:secret@relay.example",
            "https://relay.example/?token=secret",
            "https://relay.example/#secret",
            "https://relay.example/\"><script>secret</script>",
        ] {
            let html = page(Language::English, origin);
            assert!(!html.contains("secret"));
            assert!(!html.contains("<script"));
        }
    }

    #[tokio::test]
    async fn guide_routes_are_public_without_displacing_the_guide_account_endpoint() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let public_origin = "https://relay.example";
        let broker = Arc::new(Broker::new(public_origin.into()));
        let router = crate::http::router(broker);
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = reqwest::Client::new();
        for (path, language) in [
            ("/guide/", "en"),
            ("/guide/en", "en"),
            ("/guide/zh", "zh-CN"),
        ] {
            let response = client
                .get(format!("{base}{path}"))
                .header(header::ACCEPT_LANGUAGE, "zh-CN")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            assert!(
                response.headers()[header::CONTENT_TYPE]
                    .to_str()
                    .unwrap()
                    .starts_with("text/html")
            );
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            assert!(!response.headers().contains_key(header::SET_COOKIE));
            assert!(
                response.headers()["content-security-policy"]
                    .to_str()
                    .unwrap()
                    .contains("default-src 'none'")
            );
            let html = response.text().await.unwrap();
            assert!(html.contains(&format!("<html lang=\"{language}\">")));
            assert!(html.contains(&format!("{public_origin}/mcp")));
        }
        let css = client
            .get(format!("{base}/guide/style.css"))
            .send()
            .await
            .unwrap();
        assert_eq!(css.status(), reqwest::StatusCode::OK);
        assert!(
            css.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/css")
        );
        let legacy = client.post(format!("{base}/guide")).send().await.unwrap();
        assert_eq!(legacy.status(), reqwest::StatusCode::UNAUTHORIZED);
        assert!(legacy.headers().contains_key(header::WWW_AUTHENTICATE));
        let missing = client
            .get(format!("{base}/guide/not-a-language"))
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);
        server.abort();
        let _ = server.await;
    }
}
