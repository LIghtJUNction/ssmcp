//! Bounded, cached checks for an interactive session; never installs anything.
use super::{API, Version, release_metadata, release_version};
use crate::client::interface::history::{HistoryLock, SafeDirectory};
use crate::wire::versions::Version as InstalledVersion;
use std::{path::PathBuf, time::Duration};

const NOTICE_TTL: u64 = 24 * 60 * 60;
const NOTICE_BACKOFF: u64 = 60 * 60;
const NOTICE_CACHE_LIMIT: u64 = 4096;
const NOTICE_CACHE_FILE: &str = "release.json";

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct NoticeCache {
    schema: u8,
    checked_at: u64,
    succeeded: bool,
    target: String,
    latest: Option<String>,
}

struct NoticeCheck {
    directory: SafeDirectory,
    _lock: HistoryLock,
    previous: Option<Version>,
}

enum NoticePreparation {
    Cached(Option<Version>),
    Check(NoticeCheck),
}

fn notice_cache_directory() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .map(|path| path.join("ssmcp/cache/update"))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .filter(|path| path.is_absolute())
                    .map(|path| path.join(".cache"))
            })
            .map(|path| path.join("ssmcp/update"))
    }
}

fn read_notice_cache(directory: &SafeDirectory, target: &str, now: u64) -> Option<NoticeCache> {
    use std::io::Read;
    let mut bytes = Vec::new();
    directory
        .open(NOTICE_CACHE_FILE, false, false, false)
        .ok()?
        .take(NOTICE_CACHE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > NOTICE_CACHE_LIMIT {
        return None;
    }
    let cache: NoticeCache = serde_json::from_slice(&bytes).ok()?;
    if cache.schema != 2
        || cache.target != target
        || cache.checked_at > now
        || (cache.succeeded && cache.latest.is_none())
        || cache
            .latest
            .as_ref()
            .is_some_and(|tag| Version::parse(tag).is_err())
    {
        return None;
    }
    Some(cache)
}

fn write_notice_cache(
    directory: &SafeDirectory,
    target: &str,
    now: u64,
    succeeded: bool,
    latest: Option<Version>,
) -> Option<()> {
    let bytes = serde_json::to_vec(&NoticeCache {
        schema: 2,
        checked_at: now,
        succeeded,
        target: target.into(),
        latest: latest.map(Version::tag),
    })
    .ok()?;
    directory.write_atomic(NOTICE_CACHE_FILE, &bytes).ok()
}

fn prepare_notice(directory: PathBuf, target: &str, now: u64) -> Option<NoticePreparation> {
    let directory = SafeDirectory::at(directory).ok()?;
    // Contention skips an optional check; it never waits for another client.
    let lock = directory.lock().ok()?;
    let cache = read_notice_cache(&directory, target, now);
    let previous = cache
        .as_ref()
        .and_then(|cache| cache.latest.as_deref())
        .and_then(|tag| Version::parse(tag).ok());
    if cache.as_ref().is_some_and(|cache| {
        now - cache.checked_at
            < if cache.succeeded {
                NOTICE_TTL
            } else {
                NOTICE_BACKOFF
            }
    }) {
        return Some(NoticePreparation::Cached(previous));
    }
    // Save the attempt before HTTP so cancellation/reconnect cannot hammer GitHub.
    write_notice_cache(&directory, target, now, false, previous)?;
    Some(NoticePreparation::Check(NoticeCheck {
        directory,
        _lock: lock,
        previous,
    }))
}

fn published_version(bytes: &[u8], target: &str) -> crate::Result<Version> {
    let version = release_version(bytes)?;
    if !crate::wire::versions::TARGETS.contains(&target) {
        return Err("unsupported client release target".into());
    }
    #[derive(serde::Deserialize)]
    struct Asset {
        name: String,
        state: String,
    }
    #[derive(serde::Deserialize)]
    struct Assets {
        assets: Vec<Asset>,
    }
    let assets: Assets = serde_json::from_slice(bytes)?;
    let suffix = if target.ends_with("windows-msvc") {
        ".zip"
    } else {
        ".tar.gz"
    };
    let archive = format!("ssmcp-{target}{suffix}");
    if ![archive.as_str(), "SHA256SUMS"].iter().all(|name| {
        assets
            .assets
            .iter()
            .any(|asset| asset.name == *name && asset.state == "uploaded")
    }) {
        return Err("release has no completed client download for this platform".into());
    }
    Ok(version)
}

fn available_text(current: &InstalledVersion, latest: Option<Version>) -> Option<String> {
    let latest = latest?;
    (InstalledVersion::parse(&latest.tag())? > *current)
        .then(|| format!("ssmcp update --check · {} available", latest.tag()))
}

async fn notice_at(
    client: reqwest::Client,
    url: &str,
    directory: PathBuf,
    current: InstalledVersion,
    target: &'static str,
    now: u64,
) -> Option<String> {
    if !crate::wire::versions::TARGETS.contains(&target) {
        return None;
    }
    let prepared = tokio::task::spawn_blocking(move || prepare_notice(directory, target, now))
        .await
        .ok()??;
    let check = match prepared {
        NoticePreparation::Cached(latest) => return available_text(&current, latest),
        NoticePreparation::Check(check) => check,
    };
    // This future owns the HTTP request; cancelling its Session drops the socket.
    let result = match release_metadata(&client, url).await {
        Ok(bytes) => published_version(&bytes, target),
        Err(error) => Err(error),
    };
    let succeeded = result.is_ok();
    let latest = result.ok().or(check.previous);
    let notice = available_text(&current, latest);
    let _ = tokio::task::spawn_blocking(move || {
        let owned = check;
        write_notice_cache(&owned.directory, target, now, succeeded, latest)
    })
    .await;
    notice
}

pub(in crate::client) async fn available_notice() -> Option<String> {
    let current = InstalledVersion::parse(env!("CARGO_PKG_VERSION"))?;
    let target = super::notice::current_target()?;
    let directory = notice_cache_directory()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let client = crate::http_client::builder()
        .user_agent(concat!("ssmcp/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(1))
        .timeout(Duration::from_secs(3))
        .pool_max_idle_per_host(0)
        .build()
        .ok()?;
    notice_at(client, API, directory, current, target, now).await
}

#[cfg(test)]
mod tests;
