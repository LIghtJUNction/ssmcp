//! Private, process-locked account storage. Never reads or writes authorized_keys.
use super::{Problem, parse_key};
use crate::identity::Identity;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

pub(super) const MAX_KEYS: usize = 20;
const MAX_ACCOUNTS: usize = 4096;
const MAX_DATABASE: u64 = 8 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicKey {
    pub fingerprint: String,
    pub label: String,
    pub public_key: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Account {
    pub subject: String,
    pub username: String,
    pub github_login: String,
    pub keys: Vec<PublicKey>,
}

/// Account directory data deliberately excludes public keys and their fingerprints.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AccountSummary {
    pub subject: String,
    pub username: String,
    pub github_login: String,
    pub key_count: usize,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Database {
    accounts: BTreeMap<String, Account>,
}

pub(crate) struct Store {
    directory: File,
    _lock: File,
    database: Database,
    failed: bool,
}

impl Drop for Store {
    fn drop(&mut self) {
        // A concurrent fork can briefly inherit this open file description even
        // with O_CLOEXEC. Closing our descriptor alone would then keep the lock
        // alive until that child execs. Store owns the writer lease, so release
        // it explicitly before its fields close.
        // SAFETY: the descriptor remains open for this Drop implementation.
        unsafe {
            libc::flock(self._lock.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn private_file(file: &File) -> crate::Result<()> {
    let m = file.metadata()?;
    // SAFETY: geteuid has no preconditions.
    if !m.is_file()
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o777 != 0o600
        || m.nlink() != 1
    {
        return Err(
            "portal files must be regular, singly linked, service-owned files with mode 0600"
                .into(),
        );
    }
    Ok(())
}

impl Store {
    pub fn open(path: &Path) -> crate::Result<Self> {
        if !path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err("portal state_dir must be an absolute path without '..'".into());
        }
        // Reject symlink components; do not follow a configured path into a user's home.
        for ancestor in path.ancestors() {
            match fs::symlink_metadata(ancestor) {
                Ok(m) if m.file_type().is_symlink() => {
                    return Err("portal state_dir must not contain symlinks".into());
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && ancestor == path => {}
                Err(e) => return Err(e.into()),
            }
        }
        if !path.exists() {
            fs::DirBuilder::new().mode(0o700).create(path)?;
        }
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let m = directory.metadata()?;
        // SAFETY: geteuid has no preconditions.
        if m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o777 != 0o700 {
            return Err("portal state_dir must be owned by the service user with mode 0700".into());
        }
        // Anchor all operations to the opened directory, even if its pathname changes.
        let anchored = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(anchored.join("accounts.lock"))?;
        private_file(&lock)?;
        // SAFETY: flock receives an open file descriptor; the file is held for Store's lifetime.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("portal storage is already locked by another daemon".into());
        }
        let database = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(anchored.join("accounts.json"))
        {
            Ok(file) => {
                private_file(&file)?;
                if file.metadata()?.len() > MAX_DATABASE {
                    return Err("portal database exceeds 8 MiB".into());
                }
                let mut bytes = Vec::new();
                file.take(MAX_DATABASE + 1).read_to_end(&mut bytes)?;
                if bytes.len() as u64 > MAX_DATABASE {
                    return Err("portal database exceeds 8 MiB".into());
                }
                serde_json::from_slice::<Database>(&bytes)
                    .map_err(|_| "invalid portal database; refusing to discard account data")?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Database::default(),
            Err(e) => return Err(e.into()),
        };
        validate(&database)?;
        Ok(Self {
            directory,
            _lock: lock,
            database,
            failed: false,
        })
    }

    pub fn account(&self, subject: &str) -> Result<Account, Problem> {
        self.database
            .accounts
            .get(subject)
            .cloned()
            .ok_or(Problem::Unauthorized)
    }

    pub fn summaries(&self) -> Result<Vec<AccountSummary>, Problem> {
        if self.failed {
            return Err(Problem::Storage);
        }
        Ok(self
            .database
            .accounts
            .values()
            .map(|account| AccountSummary {
                subject: account.subject.clone(),
                username: account.username.clone(),
                github_login: account.github_login.clone(),
                key_count: account.keys.len(),
            })
            .collect())
    }

    pub fn login(&mut self, identity: &Identity) -> Result<Account, Problem> {
        let username = stable_username(&identity.subject).ok_or(Problem::Unauthorized)?;
        if identity.provider != "github"
            || crate::wire::canonical_github_principal(&format!("{}@github", identity.login))
                .is_err()
        {
            return Err(Problem::Unauthorized);
        }
        let mut next = self.database.clone();
        if !next.accounts.contains_key(&identity.subject) && next.accounts.len() >= MAX_ACCOUNTS {
            return Err(Problem::Busy);
        }
        let account = next
            .accounts
            .entry(identity.subject.clone())
            .or_insert_with(|| Account {
                subject: identity.subject.clone(),
                username,
                github_login: identity.login.clone(),
                keys: Vec::new(),
            });
        account.github_login.clone_from(&identity.login);
        let account = account.clone();
        self.save(next)?;
        Ok(account)
    }

    pub fn add(&mut self, owner: &str, key: PublicKey) -> Result<(), Problem> {
        let mut next = self.database.clone();
        let account = next.accounts.get_mut(owner).ok_or(Problem::Unauthorized)?;
        if account
            .keys
            .iter()
            .any(|k| k.fingerprint == key.fingerprint)
        {
            return Err(Problem::Duplicate);
        }
        if account.keys.len() >= MAX_KEYS {
            return Err(Problem::Limit);
        }
        account.keys.push(key);
        self.save(next)
    }

    pub fn remove(&mut self, owner: &str, fingerprint: &str) -> Result<(), Problem> {
        let mut next = self.database.clone();
        let account = next.accounts.get_mut(owner).ok_or(Problem::Unauthorized)?;
        let index = account
            .keys
            .iter()
            .position(|k| k.fingerprint == fingerprint)
            .ok_or(Problem::NotFound)?;
        account.keys.remove(index);
        self.save(next)
    }

    fn save(&mut self, next: Database) -> Result<(), Problem> {
        if self.failed {
            return Err(Problem::Storage);
        }
        let data = serde_json::to_vec(&next).map_err(|_| Problem::Storage)?;
        if data.len() as u64 > MAX_DATABASE {
            return Err(Problem::Limit);
        }
        let dir = PathBuf::from(format!("/proc/self/fd/{}", self.directory.as_raw_fd()));
        let temporary = dir.join(format!(".accounts-{}.tmp", super::random()?));
        let result = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&temporary)?;
            file.write_all(&data)?;
            file.sync_all()?;
            fs::rename(&temporary, dir.join("accounts.json"))?;
            self.directory.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
            // Fail closed after an ambiguous durability failure; restart reloads the disk state.
            self.failed = true;
            return Err(Problem::Storage);
        }
        self.database = next;
        Ok(())
    }
}

pub(crate) fn stable_username(subject: &str) -> Option<String> {
    let id = subject.parse::<u64>().ok()?;
    (id > 0 && id.to_string() == subject).then(|| format!("gh-{id}"))
}

fn validate(database: &Database) -> crate::Result<()> {
    if database.accounts.len() > MAX_ACCOUNTS {
        return Err("too many portal accounts".into());
    }
    for (id, account) in &database.accounts {
        if &account.subject != id
            || stable_username(id).as_deref() != Some(&account.username)
            || account.keys.len() > MAX_KEYS
            || crate::wire::canonical_github_principal(&format!("{}@github", account.github_login))
                .is_err()
        {
            return Err("invalid portal account; refusing to discard account data".into());
        }
        let mut seen = std::collections::HashSet::new();
        for key in &account.keys {
            let parsed =
                parse_key(&key.public_key, &key.label).map_err(|_| "invalid stored public key")?;
            if parsed.fingerprint != key.fingerprint
                || parsed.public_key != key.public_key
                || !seen.insert(&key.fingerprint)
            {
                return Err("invalid stored public key fingerprint".into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_store_releases_lock_despite_an_inherited_descriptor() {
        struct Temporary(PathBuf);
        impl Drop for Temporary {
            fn drop(&mut self) {
                fs::remove_dir_all(&self.0).unwrap();
            }
        }
        let temporary = Temporary(std::env::temp_dir().join(format!(
            "ssmcp-portal-lock-{}",
            super::super::random().unwrap()
        )));
        let store = Store::open(&temporary.0).unwrap();
        // dup and fork share the same open file description and flock lease.
        // Keep a duplicate alive to model a child between fork and exec.
        let inherited = store._lock.try_clone().unwrap();
        assert!(Store::open(&temporary.0).is_err());
        drop(store);
        let reopened = Store::open(&temporary.0).unwrap();
        assert!(Store::open(&temporary.0).is_err());
        drop(inherited);
        assert!(Store::open(&temporary.0).is_err());
        drop(reopened);
        assert!(Store::open(&temporary.0).is_ok());
    }
}
