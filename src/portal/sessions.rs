//! Restart-safe browser sessions. The account Store holds the exclusive writer lease.
//! Only hashes of browser authentication cookies are written to this private file.
use super::{DigestKey, MAX_SESSIONS, Problem, SESSION_TTL, Session, store::private_file};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const FILE_NAME: &str = "browser-sessions.json";
const MAX_FILE: u64 = 2 * 1024 * 1024;
const RENEW_INTERVAL: u64 = 24 * 3600;
const ABSOLUTE_TTL: u64 = 90 * 24 * 3600;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    subject: String,
    csrf: String,
    renewed_at: u64,
    expires_at: u64,
    absolute_expires_at: u64,
}

pub(super) struct Sessions {
    directory: File,
    records: BTreeMap<String, Record>,
    failed: bool,
}

impl Sessions {
    pub(super) fn open(path: &Path) -> crate::Result<Self> {
        // Portal::open first acquires Store's validated, exclusive directory lease.
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let dir = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        let records = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(dir.join(FILE_NAME))
        {
            Ok(file) => {
                private_file(&file)?;
                if file.metadata()?.len() > MAX_FILE {
                    return Err("browser session file exceeds its storage limit".into());
                }
                let mut data = Vec::new();
                file.take(MAX_FILE + 1).read_to_end(&mut data)?;
                if data.len() as u64 > MAX_FILE {
                    return Err("browser session file exceeds its storage limit".into());
                }
                serde_json::from_slice::<BTreeMap<String, Record>>(&data)
                    .map_err(|_| "invalid browser session file")?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error.into()),
        };
        if records.len() > MAX_SESSIONS
            || records.iter().any(|(key, record)| {
                URL_SAFE_NO_PAD
                    .decode(key)
                    .map_or(true, |bytes| bytes.len() != 32)
                    || super::store::stable_username(&record.subject).is_none()
                    || !super::valid_secret(&record.csrf)
                    || record.renewed_at >= record.expires_at
                    || record.expires_at > record.absolute_expires_at
                    || record.expires_at - record.renewed_at > SESSION_TTL.as_secs()
                    || record.absolute_expires_at - record.renewed_at > ABSOLUTE_TTL
            })
        {
            return Err("invalid browser session record".into());
        }
        Ok(Self {
            directory,
            records,
            failed: false,
        })
    }

    pub(super) fn healthy(&self) -> Result<(), Problem> {
        if self.failed {
            Err(Problem::Storage)
        } else {
            Ok(())
        }
    }

    pub(super) fn restore(&self, now: u64) -> HashMap<DigestKey, Session> {
        let instant = Instant::now();
        self.records
            .iter()
            .filter_map(|(key, record)| {
                if record.expires_at <= now || record.renewed_at > now {
                    return None;
                }
                let key = URL_SAFE_NO_PAD.decode(key).ok()?.try_into().ok()?;
                Some((
                    key,
                    Session {
                        subject: Some(record.subject.clone()),
                        csrf: record.csrf.clone(),
                        expires: instant + Duration::from_secs(record.expires_at - now),
                    },
                ))
            })
            .collect()
    }

    pub(super) fn issue(
        &mut self,
        key: DigestKey,
        subject: String,
        csrf: String,
        now: u64,
    ) -> Result<(), Problem> {
        self.healthy()?;
        let mut next = self.records.clone();
        next.retain(|_, record| record.expires_at > now);
        if next.len() >= MAX_SESSIONS {
            return Err(Problem::Busy);
        }
        next.insert(
            URL_SAFE_NO_PAD.encode(key),
            Record {
                subject,
                csrf,
                renewed_at: now,
                expires_at: now + SESSION_TTL.as_secs(),
                absolute_expires_at: now + ABSOLUTE_TTL,
            },
        );
        self.save(next)
    }

    pub(super) fn renew(&mut self, key: DigestKey, now: u64) -> Result<Option<u64>, Problem> {
        self.healthy()?;
        let key = URL_SAFE_NO_PAD.encode(key);
        let Some(record) = self.records.get(&key) else {
            return Ok(None);
        };
        if record.expires_at <= now || record.renewed_at > now {
            return Ok(None);
        }
        let expires = now
            .saturating_add(SESSION_TTL.as_secs())
            .min(record.absolute_expires_at);
        if now.saturating_sub(record.renewed_at) >= RENEW_INTERVAL && expires > record.expires_at {
            let mut next = self.records.clone();
            next.retain(|_, record| record.expires_at > now);
            let record = next.get_mut(&key).ok_or(Problem::Unauthorized)?;
            record.renewed_at = now;
            record.expires_at = expires;
            self.save(next)?;
        }
        Ok(self.records.get(&key).map(|record| record.expires_at - now))
    }

    pub(super) fn remove(&mut self, key: DigestKey) -> Result<(), Problem> {
        self.healthy()?;
        let key = URL_SAFE_NO_PAD.encode(key);
        if !self.records.contains_key(&key) {
            return Ok(());
        }
        let mut next = self.records.clone();
        next.remove(&key);
        self.save(next)
    }

    pub(super) fn revoke(&mut self, subject: &str) -> Result<(), Problem> {
        self.healthy()?;
        if !self
            .records
            .values()
            .any(|record| record.subject == subject)
        {
            return Ok(());
        }
        let mut next = self.records.clone();
        next.retain(|_, record| record.subject != subject);
        self.save(next)
    }

    fn save(&mut self, next: BTreeMap<String, Record>) -> Result<(), Problem> {
        self.healthy()?;
        let data = serde_json::to_vec(&next).map_err(|_| Problem::Storage)?;
        if data.len() as u64 > MAX_FILE {
            return Err(Problem::Limit);
        }
        let dir = PathBuf::from(format!("/proc/self/fd/{}", self.directory.as_raw_fd()));
        let temporary = dir.join(format!(".browser-sessions-{}.tmp", super::random()?));
        let result = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&temporary)?;
            file.write_all(&data)?;
            file.sync_all()?;
            fs::rename(&temporary, dir.join(FILE_NAME))?;
            self.directory.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
            self.failed = true;
            return Err(Problem::Storage);
        }
        self.records = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::portal::{digest, random};
    use std::os::unix::fs::PermissionsExt;

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("ssmcp-browser-sessions-{}", random().unwrap()));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn authenticated_sessions_restore_without_storing_cookie_secrets() {
        let dir = Temp::new();
        let raw = random().unwrap();
        let key = digest(&raw);
        let csrf = random().unwrap();
        let mut sessions = Sessions::open(&dir.0).unwrap();
        sessions
            .issue(key, "123".into(), csrf.clone(), 1_000)
            .unwrap();
        let stored = fs::read_to_string(dir.0.join(FILE_NAME)).unwrap();
        assert!(!stored.contains(&raw));
        assert!(stored.contains(&URL_SAFE_NO_PAD.encode(key)));
        assert_eq!(
            fs::metadata(dir.0.join(FILE_NAME))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(sessions);
        let sessions = Sessions::open(&dir.0).unwrap();
        let restored = sessions.restore(2_000);
        assert_eq!(restored[&key].subject.as_deref(), Some("123"));
        assert_eq!(restored[&key].csrf, csrf);
    }

    #[test]
    fn active_sessions_renew_daily_and_stop_at_absolute_deadline() {
        let dir = Temp::new();
        let key = digest("active");
        let start = 1_000;
        let mut sessions = Sessions::open(&dir.0).unwrap();
        sessions
            .issue(key, "123".into(), random().unwrap(), start)
            .unwrap();
        let ttl = SESSION_TTL.as_secs();
        assert_eq!(sessions.renew(key, start + 60).unwrap(), Some(ttl - 60));
        assert_eq!(
            sessions.records[&URL_SAFE_NO_PAD.encode(key)].renewed_at,
            start
        );
        assert_eq!(
            sessions.renew(key, start + RENEW_INTERVAL).unwrap(),
            Some(ttl)
        );
        for day in [29, 58, 87] {
            assert!(
                sessions
                    .renew(key, start + day * RENEW_INTERVAL)
                    .unwrap()
                    .is_some()
            );
        }
        assert_eq!(
            sessions.renew(key, start + 89 * RENEW_INTERVAL).unwrap(),
            Some(RENEW_INTERVAL)
        );
        assert_eq!(sessions.renew(key, start + ABSOLUTE_TTL).unwrap(), None);
        assert!(sessions.restore(start + ABSOLUTE_TTL).is_empty());
    }

    #[test]
    fn idle_sessions_expire_and_cannot_be_revived() {
        let dir = Temp::new();
        let key = digest("idle");
        let mut sessions = Sessions::open(&dir.0).unwrap();
        sessions
            .issue(key, "123".into(), random().unwrap(), 1_000)
            .unwrap();
        let expired = 1_000 + SESSION_TTL.as_secs();
        assert_eq!(sessions.renew(key, expired).unwrap(), None);
        assert!(Sessions::open(&dir.0).unwrap().restore(expired).is_empty());
    }

    #[test]
    fn logout_and_account_revocation_survive_restart() {
        let dir = Temp::new();
        let mut sessions = Sessions::open(&dir.0).unwrap();
        for (name, subject) in [("one", "123"), ("two", "123"), ("other", "456")] {
            sessions
                .issue(digest(name), subject.into(), random().unwrap(), 1_000)
                .unwrap();
        }
        sessions.remove(digest("one")).unwrap();
        drop(sessions);
        let mut sessions = Sessions::open(&dir.0).unwrap();
        assert!(!sessions.restore(2_000).contains_key(&digest("one")));
        sessions.revoke("123").unwrap();
        let restored = Sessions::open(&dir.0).unwrap().restore(2_000);
        assert_eq!(restored.len(), 1);
        assert!(restored.contains_key(&digest("other")));
    }

    #[test]
    fn unsafe_files_and_invalid_records_are_rejected() {
        let dir = Temp::new();
        let mut sessions = Sessions::open(&dir.0).unwrap();
        sessions
            .issue(digest("one"), "123".into(), random().unwrap(), 1_000)
            .unwrap();
        let file = dir.0.join(FILE_NAME);
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Sessions::open(&dir.0).is_err());
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&file, b"{broken").unwrap();
        assert!(Sessions::open(&dir.0).is_err());
    }

    #[test]
    fn failed_durable_revocation_closes_the_store() {
        let dir = Temp::new();
        let key = digest("one");
        let mut sessions = Sessions::open(&dir.0).unwrap();
        sessions
            .issue(key, "123".into(), random().unwrap(), 1_000)
            .unwrap();
        fs::remove_file(dir.0.join(FILE_NAME)).unwrap();
        fs::create_dir(dir.0.join(FILE_NAME)).unwrap();
        assert_eq!(sessions.remove(key), Err(Problem::Storage));
        assert_eq!(sessions.healthy(), Err(Problem::Storage));
        assert_eq!(sessions.renew(key, 2_000), Err(Problem::Storage));
    }
}
