//! Observational connection updates. Only an explicit self-update installs code.
use crate::wire::versions::{Notice, PublishedClient, Version};

#[derive(Clone, Debug, serde::Serialize)]
pub(in crate::client) struct ConnectionVersions {
    client_version: String,
    server_version: String,
    published_client: Option<PublishedClient>,
    target: Option<&'static str>,
    update_available: bool,
    manifest_url: Option<String>,
}

impl ConnectionVersions {
    fn new(
        notice: Notice,
        server: &str,
        current: &str,
        target: Option<&'static str>,
    ) -> Option<Self> {
        if !notice.valid() {
            return None;
        }
        let current_version = Version::parse(current)?;
        let origin = crate::client::account::normalize_server(server).ok()?;
        let update_available = notice.published_client.as_ref().is_some_and(|published| {
            target.is_some_and(|target| {
                published
                    .targets
                    .iter()
                    .any(|candidate| candidate == target)
            }) && Version::parse(&published.version)
                .is_some_and(|version| version > current_version)
        });
        let manifest_url = notice
            .published_client
            .as_ref()
            .map(|_| format!("{origin}/client/release.json"));
        Some(Self {
            client_version: current.into(),
            server_version: notice.server_version,
            published_client: notice.published_client,
            target,
            update_available,
            manifest_url,
        })
    }

    pub(in crate::client) fn summary(&self) -> String {
        format!(
            "Client {} · server {}",
            tag(&self.client_version),
            tag(&self.server_version)
        )
    }

    pub(in crate::client) fn update_message(&self) -> Option<String> {
        if !self.update_available {
            return None;
        }
        let published = self.published_client.as_ref()?;
        Some(format!(
            "Server offers client {} · installed {} · ssmcp update --check",
            tag(&published.version),
            tag(&self.client_version),
        ))
    }
}

fn tag(value: &str) -> String {
    if value.starts_with('v') {
        value.into()
    } else {
        format!("v{value}")
    }
}

fn current_target() -> Option<&'static str> {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "linux") if cfg!(target_env = "gnu") => Some("x86_64-unknown-linux-gnu"),
        ("aarch64", "linux") if cfg!(target_env = "gnu") => Some("aarch64-unknown-linux-gnu"),
        ("x86_64", "macos") => Some("x86_64-apple-darwin"),
        ("aarch64", "macos") => Some("aarch64-apple-darwin"),
        ("x86_64", "windows") if cfg!(target_env = "msvc") => Some("x86_64-pc-windows-msvc"),
        ("aarch64", "windows") if cfg!(target_env = "msvc") => Some("aarch64-pc-windows-msvc"),
        _ => None,
    }
}

pub(in crate::client) fn report_connection(notice: Notice, server: &str) {
    let Some(versions) =
        ConnectionVersions::new(notice, server, env!("CARGO_PKG_VERSION"), current_target())
    else {
        return;
    };
    crate::client::render::status("version", "Notice", &versions.summary());
    if let Some(message) = versions.update_message() {
        // The manifest describes this server's publication. The explicit CLI
        // check uses official releases and never assumes the server is "latest".
        let details = format!(
            "{message}\nServer release: {}",
            versions.manifest_url.as_deref().unwrap_or_default()
        );
        crate::client::render::status("update", "Notice", &details);
    }
    crate::client::terminal::set_connection_versions(versions);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(current: &str, published: Option<&str>, targets: &[&str]) -> ConnectionVersions {
        ConnectionVersions::new(
            Notice {
                server_version: "99.0.0-dev".into(),
                published_client: published.map(|version| PublishedClient {
                    version: version.into(),
                    targets: targets.iter().map(|target| (*target).into()).collect(),
                }),
            },
            "https://relay.example",
            current,
            Some("x86_64-unknown-linux-gnu"),
        )
        .unwrap()
    }

    #[test]
    fn version_notice_requires_a_published_newer_client_for_the_current_target() {
        let target = &["x86_64-unknown-linux-gnu"];
        assert!(
            !state("0.10.17", None, &[]).update_available,
            "a newer running server is not evidence of a released client"
        );
        assert!(!state("0.10.17", Some("0.10.17"), target).update_available);
        assert!(!state("0.10.17+local", Some("0.10.17+published"), target).update_available);
        assert!(!state("0.10.18", Some("0.10.17"), target).update_available);
        assert!(!state("0.10.16", Some("0.10.17"), &["aarch64-apple-darwin"]).update_available);
        assert!(state("0.9.99", Some("0.10.0"), target).update_available);
        assert!(state("0.10.17-rc.2", Some("0.10.17"), target).update_available);
        let newer = state("0.10.16", Some("0.10.17"), target);
        assert_eq!(
            newer.manifest_url.as_deref(),
            Some("https://relay.example/client/release.json")
        );
        assert!(
            newer
                .update_message()
                .unwrap()
                .contains("ssmcp update --check")
        );
    }

    #[test]
    fn version_notice_rejects_untrusted_display_text_and_prerelease_offers() {
        for version in [
            "0.10.18\nrun unsafe",
            "https://other.example",
            "0.10.18-rc.1",
            "01.10.18",
        ] {
            let notice = Notice {
                server_version: "0.10.17".into(),
                published_client: Some(PublishedClient {
                    version: version.into(),
                    targets: vec!["x86_64-unknown-linux-gnu".into()],
                }),
            };
            assert!(
                ConnectionVersions::new(
                    notice,
                    "https://relay.example",
                    "0.10.17",
                    Some("x86_64-unknown-linux-gnu")
                )
                .is_none()
            );
        }
    }
}
