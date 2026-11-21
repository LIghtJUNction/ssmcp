//! Resolve and download only published, digest-bearing official Zuse assets.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::Duration;

pub(super) const MAX_ARCHIVE: usize = 32 * 1024 * 1024;
const MAX_METADATA: usize = 1024 * 1024;
const REPOSITORY: &str = "https://github.com/LIghtJUNction/zuse";
const API: &str = "https://api.github.com/repos/LIghtJUNction/zuse";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Release {
    pub version: String,
    pub tag: String,
    pub target: String,
    pub release_id: u64,
    pub asset_id: u64,
    pub asset_name: String,
    pub size: u64,
    pub sha256: String,
    pub url: String,
}

pub(super) fn digest(bytes: &[u8]) -> String {
    crate::hex::encode(Sha256::digest(bytes))
}

pub(super) fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(super) fn version(value: &str) -> Result<String, String> {
    let value = value.strip_prefix('v').unwrap_or(value);
    let parts = value.split('.').collect::<Vec<_>>();
    if value.len() > 48
        || parts.len() != 3
        || parts.iter().any(|part| {
            part.is_empty()
                || !part.bytes().all(|byte| byte.is_ascii_digit())
                || (part.len() > 1 && part.starts_with('0'))
                || part.parse::<u32>().is_err()
        })
    {
        return Err("Zuse version must be a stable MAJOR.MINOR.PATCH release".into());
    }
    Ok(value.to_owned())
}

pub(super) fn target() -> Result<&'static str, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") if cfg!(target_env = "gnu") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") if cfg!(target_env = "gnu") => Ok("aarch64-unknown-linux-gnu"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc"),
        _ => Err("No official Zuse release target is configured for this platform; no fallback was selected".into()),
    }
}

pub(super) fn executable_name(target: &str) -> &'static str {
    if target.ends_with("-windows-msvc") {
        "zuse.exe"
    } else {
        "zuse"
    }
}

impl Release {
    pub(super) fn prefix(&self) -> String {
        format!("zuse-{}-{}", self.tag, self.target)
    }

    pub(super) fn validate(&self, expected_target: &str) -> Result<(), String> {
        let normalized = version(&self.version)?;
        let suffix = if expected_target.ends_with("-windows-msvc") {
            ".zip"
        } else {
            ".tar.gz"
        };
        let expected_name = format!("zuse-v{normalized}-{expected_target}{suffix}");
        if self.version != normalized
            || self.tag != format!("v{normalized}")
            || self.target != expected_target
            || self.asset_name != expected_name
            || self.url
                != format!(
                    "{REPOSITORY}/releases/download/{}/{expected_name}",
                    self.tag
                )
            || self.release_id == 0
            || self.asset_id == 0
            || self.size == 0
            || self.size > MAX_ARCHIVE as u64
            || !valid_digest(&self.sha256)
        {
            return Err(
                "The Zuse release asset identity, target, size or SHA256 is invalid".into(),
            );
        }
        Ok(())
    }

    pub(super) fn verify(&self, bytes: &[u8], expected_target: &str) -> Result<(), String> {
        self.validate(expected_target)?;
        if bytes.len() as u64 != self.size || digest(bytes) != self.sha256 {
            return Err(
                "Zuse archive size or SHA256 differs from the official release asset".into(),
            );
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct Metadata {
    id: u64,
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    id: u64,
    name: String,
    state: String,
    size: u64,
    digest: Option<String>,
    browser_download_url: String,
}

pub(super) fn select(
    bytes: &[u8],
    requested: Option<&str>,
    target: &str,
) -> Result<Release, String> {
    if bytes.len() > MAX_METADATA {
        return Err("Zuse release metadata exceeds 1 MiB".into());
    }
    let metadata: Metadata = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    let resolved = version(&metadata.tag_name)?;
    if metadata.draft
        || metadata.prerelease
        || metadata.tag_name != format!("v{resolved}")
        || requested.is_some_and(|expected| expected != resolved)
        || metadata.assets.len() > 64
    {
        return Err("GitHub did not return the requested published stable Zuse release".into());
    }
    let suffix = if target.ends_with("-windows-msvc") {
        ".zip"
    } else {
        ".tar.gz"
    };
    let name = format!("zuse-{}-{target}{suffix}", metadata.tag_name);
    let mut matches = metadata
        .assets
        .into_iter()
        .filter(|asset| asset.name == name);
    let asset = matches
        .next()
        .ok_or_else(|| format!("The official Zuse release has no exact asset {name}"))?;
    if matches.next().is_some() || asset.state != "uploaded" {
        return Err("The Zuse release asset is duplicated or not fully uploaded".into());
    }
    let sha256 = asset
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .ok_or("The official Zuse asset has no SHA256 digest; download refused")?
        .to_owned();
    let release = Release {
        version: resolved,
        tag: metadata.tag_name,
        target: target.to_owned(),
        release_id: metadata.id,
        asset_id: asset.id,
        asset_name: asset.name,
        size: asset.size,
        sha256,
        url: asset.browser_download_url,
    };
    release.validate(target)?;
    Ok(release)
}

fn allowed_url(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none_or(|port| port == 443)
        && matches!(
            url.host_str(),
            Some(
                "api.github.com"
                    | "github.com"
                    | "release-assets.githubusercontent.com"
                    | "objects.githubusercontent.com"
            )
        )
}

async fn bounded(client: &reqwest::Client, url: &str, limit: usize) -> Result<Vec<u8>, String> {
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?;
    if !allowed_url(response.url())
        || response
            .content_length()
            .is_some_and(|size| size > limit as u64)
    {
        return Err("Zuse download URL or response length is outside the release policy".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err("Zuse download exceeds its declared byte limit".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub(super) async fn download(requested: Option<&str>) -> Result<(Release, Vec<u8>), String> {
    let requested = requested.map(version).transpose()?;
    let target = target()?;
    let client = crate::http_client::builder()
        .user_agent(concat!("ssmcp/", env!("CARGO_PKG_VERSION")))
        .https_only(true)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 || !allowed_url(attempt.url()) {
                attempt.error("Zuse release redirect is outside the official HTTPS hosts")
            } else {
                attempt.follow()
            }
        }))
        .build()
        .map_err(|error| error.to_string())?;
    let metadata_url = match &requested {
        Some(version) => format!("{API}/releases/tags/v{version}"),
        None => format!("{API}/releases/latest"),
    };
    let bytes = bounded(&client, &metadata_url, MAX_METADATA).await?;
    let release = select(&bytes, requested.as_deref(), target)?;
    let archive = bounded(&client, &release.url, release.size as usize).await?;
    release.verify(&archive, target)?;
    Ok((release, archive))
}
