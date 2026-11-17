//! Bounded, private audit history owned by the gateway process.
//!
//! This journal records identity and operation metadata only. Its schema has no
//! place for credentials, public/private keys, tool arguments or tool results.
//! Callers must authorize the scope before requesting an unfiltered query.
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Component, Path, PathBuf},
    sync::{Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

pub const MAX_JOURNAL_BYTES: u64 = 16 * 1024 * 1024;
const MAX_EVENT_BYTES: usize = 4096;
const MAX_EVENTS: usize = 100_000;
const MAX_PAGE: usize = 200;
const JOURNAL: &str = "audit.jsonl";

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuditConfig {
    pub retention_days: u32,
    pub max_events: usize,
}
impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            retention_days: 7,
            max_events: 20_000,
        }
    }
}
impl AuditConfig {
    pub fn validate(&self) -> crate::Result<()> {
        if !(1..=30).contains(&self.retention_days) {
            return Err("audit retention_days must be in 1..=30".into());
        }
        if !(1..=MAX_EVENTS).contains(&self.max_events) {
            return Err("audit max_events must be in 1..=100000".into());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuditAction {
    Login,
    Logout,
    KeyAdded,
    KeyRemoved,
    AccessDenied,
    SessionConnected,
    SessionDisconnected,
    SessionDisconnectRequested,
    ToolCall,
    AdminViewed,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    /// A durable intent exists; this does not assert the action completed.
    Pending,
    Success,
    Denied,
    Error,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuditEvent {
    /// Monotonic journal cursor, assigned by the gateway.
    pub id: u64,
    /// Gateway wall-clock time, never accepted from a browser.
    pub at_unix: u64,
    pub action: AuditAction,
    pub outcome: AuditOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}
impl AuditEvent {
    pub fn new(action: AuditAction, outcome: AuditOutcome) -> Self {
        Self {
            id: 0,
            at_unix: 0,
            action,
            outcome,
            actor_subject: None,
            owner_subject: None,
            session_id: None,
            client_name: None,
            hostname: None,
            tool_name: None,
        }
    }

    fn validate_metadata(&self) -> crate::Result<()> {
        for subject in [&self.actor_subject, &self.owner_subject]
            .into_iter()
            .flatten()
        {
            validate_subject(subject)?;
        }
        if self.session_id.as_ref().is_some_and(|value| {
            value.is_empty()
                || value.len() > 128
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        }) {
            return Err("invalid audit session identifier".into());
        }
        for text in [&self.client_name, &self.hostname].into_iter().flatten() {
            if text.is_empty() || text.len() > 256 || text.chars().any(char::is_control) {
                return Err("invalid audit display metadata".into());
            }
        }
        if self.tool_name.as_ref().is_some_and(|value| {
            value.is_empty()
                || value.len() > 256
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_.:/-".contains(&byte))
        }) {
            return Err("invalid audit tool name".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuditPage {
    pub events: Vec<AuditEvent>,
    /// Pass this value as before to retrieve the next, older page.
    pub next_before: Option<u64>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Header {
    version: u8,
    next_id: u64,
}

struct State {
    events: VecDeque<AuditEvent>,
    next_id: u64,
    bytes: u64,
    file: File,
    failed: bool,
}

pub struct AuditLog {
    directory: File,
    lease: File,
    config: AuditConfig,
    state: Mutex<State>,
}

impl Drop for AuditLog {
    fn drop(&mut self) {
        // A child between fork and exec can inherit the same open description.
        // Explicitly release the lease rather than relying on last-close.
        // SAFETY: lease remains open until after this Drop implementation.
        unsafe {
            libc::flock(self.lease.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn validate_subject(subject: &str) -> crate::Result<()> {
    let id = subject
        .parse::<u64>()
        .map_err(|_| "invalid audit GitHub subject")?;
    if id == 0 || id.to_string() != subject {
        return Err("invalid audit GitHub subject".into());
    }
    Ok(())
}

fn now() -> crate::Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn private_file(file: &File) -> crate::Result<()> {
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no preconditions.
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(
            "audit files must be singly linked, service-owned regular files with mode 0600".into(),
        );
    }
    Ok(())
}

fn cleanup_temporary_files(directory: &File, anchored: &Path) -> crate::Result<()> {
    let mut changed = false;
    for entry in fs::read_dir(anchored)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(nonce) = name
            .strip_prefix(".audit-")
            .and_then(|name| name.strip_suffix(".tmp"))
        else {
            continue;
        };
        if nonce.len() != 32 || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        let path = anchored.join(name);
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(&path)?;
        private_file(&file)?;
        fs::remove_file(path)?;
        changed = true;
    }
    if changed {
        directory.sync_all()?;
    }
    Ok(())
}

fn header_bytes(next_id: u64) -> crate::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(&Header {
        version: 1,
        next_id,
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn event_bytes(event: &AuditEvent) -> crate::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(event)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_EVENT_BYTES {
        return Err("audit event exceeds size limit".into());
    }
    Ok(bytes)
}

impl AuditLog {
    /// Open an exclusive gateway-writer lease and immediately remove expired
    /// history. The parent directory must already exist; only the leaf is made.
    pub fn open(path: &Path, config: AuditConfig) -> crate::Result<Self> {
        config.validate()?;
        if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err("audit directory must be an absolute path without '..'".into());
        }
        for ancestor in path.ancestors() {
            match fs::symlink_metadata(ancestor) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err("audit directory must not contain symlinks".into());
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && ancestor == path => {}
                Err(error) => return Err(error.into()),
            }
        }
        if !path.exists() {
            fs::DirBuilder::new().mode(0o700).create(path)?;
        }
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let metadata = directory.metadata()?;
        // SAFETY: geteuid has no preconditions.
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o777 != 0o700 {
            return Err("audit directory must be service-owned with mode 0700".into());
        }
        let anchored = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(anchored.join("history.lock"))?;
        private_file(&lease)?;
        // SAFETY: lease is an open descriptor, retained for this AuditLog.
        if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("audit history is already owned by another gateway".into());
        }

        cleanup_temporary_files(&directory, &anchored)?;
        let journal_path = anchored.join(JOURNAL);
        let (mut file, created) = match OpenOptions::new()
            .read(true)
            .append(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(&journal_path)
        {
            Ok(file) => (file, false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (
                OpenOptions::new()
                    .read(true)
                    .append(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(&journal_path)?,
                true,
            ),
            Err(error) => return Err(error.into()),
        };
        private_file(&file)?;
        if created {
            file.write_all(&header_bytes(1)?)?;
            file.sync_all()?;
            directory.sync_all()?;
        }
        let bytes = file.metadata()?.len();
        if bytes > MAX_JOURNAL_BYTES {
            return Err("audit journal exceeds 16 MiB; refusing an unbounded read".into());
        }
        let mut raw = Vec::with_capacity(bytes as usize);
        file.rewind()?;
        Read::by_ref(&mut file)
            .take(MAX_JOURNAL_BYTES + 1)
            .read_to_end(&mut raw)?;
        if raw.len() as u64 > MAX_JOURNAL_BYTES || !raw.ends_with(b"\n") {
            return Err("audit journal is oversized or truncated".into());
        }
        let mut lines = raw[..raw.len() - 1].split(|byte| *byte == b'\n');
        let first = lines.next().ok_or("audit journal header is missing")?;
        if first.len() > 128 {
            return Err("audit journal header is invalid".into());
        }
        let header: Header =
            serde_json::from_slice(first).map_err(|_| "invalid audit journal header")?;
        if header.version != 1 || header.next_id == 0 {
            return Err("unsupported audit journal header".into());
        }
        let mut events = VecDeque::new();
        let mut previous = 0;
        let mut next_id = header.next_id;
        for line in lines {
            if line.is_empty() || line.len() >= MAX_EVENT_BYTES || events.len() >= MAX_EVENTS {
                return Err("audit journal record count or size limit exceeded".into());
            }
            let event: AuditEvent =
                serde_json::from_slice(line).map_err(|_| "invalid audit journal event")?;
            event.validate_metadata()?;
            if event.id <= previous || event.at_unix == 0 {
                return Err("audit journal identifiers or timestamps are invalid".into());
            }
            previous = event.id;
            next_id = next_id.max(
                event
                    .id
                    .checked_add(1)
                    .ok_or("audit identifiers exhausted")?,
            );
            events.push_back(event);
        }
        let log = Self {
            directory,
            lease,
            config,
            state: Mutex::new(State {
                events,
                next_id,
                bytes,
                file,
                failed: false,
            }),
        };
        log.prune()?;
        Ok(log)
    }

    pub fn retention_days(&self) -> u32 {
        self.config.retention_days
    }

    pub fn max_events(&self) -> usize {
        self.config.max_events
    }

    fn lock(&self) -> crate::Result<MutexGuard<'_, State>> {
        let state = self
            .state
            .lock()
            .map_err(|_| "audit storage lock is poisoned")?;
        if state.failed {
            return Err("audit storage needs recovery after a durability failure".into());
        }
        Ok(state)
    }

    /// Persist only typed metadata. Browser-supplied identifiers and timestamps
    /// cannot choose journal cursors or extend the configured retention.
    pub fn record(&self, event: AuditEvent) -> crate::Result<AuditEvent> {
        self.record_at(event, now()?)
    }

    fn record_at(&self, mut event: AuditEvent, timestamp: u64) -> crate::Result<AuditEvent> {
        event.validate_metadata()?;
        if timestamp == 0 {
            return Err("invalid audit clock".into());
        }
        let mut state = self.lock()?;
        event.id = state.next_id;
        event.at_unix = timestamp;
        let next_id = state
            .next_id
            .checked_add(1)
            .ok_or("audit identifiers exhausted")?;
        let bytes = event_bytes(&event)?;
        let mut compact = self.remove_expired(&mut state, timestamp) > 0;
        if state.events.len() >= self.config.max_events {
            // Reclaim a bounded batch so an at-capacity busy journal does not
            // rewrite its entire history for every subsequent event.
            let retain = self.config.max_events - (self.config.max_events / 10).max(1);
            while state.events.len() > retain {
                state.events.pop_front();
            }
            compact = true;
        }
        if compact {
            self.compact(&mut state)?;
        }
        // Compacting on a byte-boundary drops at most one bounded batch of the
        // oldest records, avoiding repeated full-file rewrites under load.
        if state.bytes + bytes.len() as u64 > MAX_JOURNAL_BYTES {
            let mut retained = header_bytes(state.next_id)?.len() as u64;
            let mut sizes = VecDeque::new();
            for existing in &state.events {
                let size = event_bytes(existing)?.len() as u64;
                sizes.push_back(size);
                retained += size;
            }
            let target = MAX_JOURNAL_BYTES - MAX_JOURNAL_BYTES / 10;
            while retained + bytes.len() as u64 > target {
                if state.events.pop_front().is_none() {
                    return Err("audit event cannot fit the bounded journal".into());
                }
                retained -= sizes.pop_front().expect("sizes mirror events");
            }
            self.compact(&mut state)?;
        }
        if let Err(error) = state
            .file
            .write_all(&bytes)
            .and_then(|_| state.file.sync_data())
        {
            state.failed = true;
            return Err(error.into());
        }
        state.bytes += bytes.len() as u64;
        state.next_id = next_id;
        state.events.push_back(event.clone());
        Ok(event)
    }

    /// Newest-first, exclusive cursor pagination. Scope None is administrator
    /// data; the caller must establish admin authority before calling it.
    pub fn query(
        &self,
        subject: Option<&str>,
        limit: usize,
        before: Option<u64>,
    ) -> crate::Result<AuditPage> {
        if let Some(subject) = subject {
            validate_subject(subject)?;
        }
        if !(1..=MAX_PAGE).contains(&limit) {
            return Err("audit query limit must be in 1..=200".into());
        }
        let mut state = self.lock()?;
        if self.remove_expired(&mut state, now()?) > 0 {
            self.compact(&mut state)?;
        }
        let mut matching = state.events.iter().rev().filter(|event| {
            before.is_none_or(|id| event.id < id)
                && subject.is_none_or(|subject| {
                    event.actor_subject.as_deref() == Some(subject)
                        || event.owner_subject.as_deref() == Some(subject)
                })
        });
        let events: Vec<_> = matching.by_ref().take(limit).cloned().collect();
        let next_before = if matching.next().is_some() {
            events.last().map(|event| event.id)
        } else {
            None
        };
        Ok(AuditPage {
            events,
            next_before,
        })
    }

    /// Call periodically even when there are no requests. Startup, reads and
    /// writes also prune, so stale records never leak into a history response.
    pub fn prune(&self) -> crate::Result<usize> {
        self.prune_at(now()?)
    }

    fn prune_at(&self, timestamp: u64) -> crate::Result<usize> {
        let mut state = self.lock()?;
        let mut removed = self.remove_expired(&mut state, timestamp);
        while state.events.len() > self.config.max_events {
            state.events.pop_front();
            removed += 1;
        }
        if removed > 0 {
            self.compact(&mut state)?;
        }
        Ok(removed)
    }

    fn remove_expired(&self, state: &mut State, timestamp: u64) -> usize {
        let before = state.events.len();
        let ttl = u64::from(self.config.retention_days) * 86_400;
        state
            .events
            .retain(|event| event.at_unix.saturating_add(ttl) > timestamp);
        before - state.events.len()
    }

    fn compact(&self, state: &mut State) -> crate::Result<()> {
        let directory = PathBuf::from(format!("/proc/self/fd/{}", self.directory.as_raw_fd()));
        let mut entropy = [0_u8; 16];
        if let Err(error) =
            File::open("/dev/urandom").and_then(|mut file| file.read_exact(&mut entropy))
        {
            state.failed = true;
            return Err(error.into());
        }
        let nonce: String = entropy.iter().map(|byte| format!("{byte:02x}")).collect();
        let temporary = directory.join(format!(".audit-{nonce}.tmp"));
        let result = (|| -> crate::Result<(File, u64)> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&temporary)?;
            let header = header_bytes(state.next_id)?;
            let mut total = header.len() as u64;
            file.write_all(&header)?;
            for event in &state.events {
                let bytes = event_bytes(event)?;
                total += bytes.len() as u64;
                if total > MAX_JOURNAL_BYTES {
                    return Err("audit compaction exceeds size limit".into());
                }
                file.write_all(&bytes)?;
            }
            file.sync_all()?;
            fs::rename(&temporary, directory.join(JOURNAL))?;
            self.directory.sync_all()?;
            let replacement = OpenOptions::new()
                .read(true)
                .append(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                .open(directory.join(JOURNAL))?;
            private_file(&replacement)?;
            Ok((replacement, total))
        })();
        match result {
            Ok((file, bytes)) => {
                state.file = file;
                state.bytes = bytes;
                Ok(())
            }
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                state.failed = true;
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests;
