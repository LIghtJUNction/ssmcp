//! Private, process-locked account storage. Never reads or writes authorized_keys.
use super::{Problem, parse_key};
use crate::identity::Identity;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
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
const MAX_HANDLE_ALIASES: usize = 128;
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    /// Only an automatically assigned name may be replaced by the initial CLI claim.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub generated_handle: bool,
    /// Previous names stay bound to the same subject; they can never be reclaimed.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub handle_aliases: BTreeSet<String>,
    #[serde(default)]
    pub banned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_bps: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub has_recharged: bool,
}

/// Account directory data deliberately excludes public keys and their fingerprints.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AccountSummary {
    pub subject: String,
    pub username: String,
    pub github_login: String,
    pub key_count: usize,
    pub handle: Option<String>,
    pub banned: bool,
    pub rate_limit_bps: Option<u64>,
    pub has_recharged: bool,
    pub traffic_remaining: Option<u64>,
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
    traffic: super::traffic::TrafficLedger,
    monthly_allowances: crate::user_policy::MonthlyTrafficAllowances,
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

pub(super) fn private_file(file: &File) -> crate::Result<()> {
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
        let traffic = super::traffic::TrafficLedger::open(&directory)?;
        let mut store = Self {
            directory,
            _lock: lock,
            database,
            failed: false,
            traffic,
            monthly_allowances: Default::default(),
        };
        let mut next = store.database.clone();
        if initialize_handles(&mut next).map_err(|_| "could not initialize platform usernames")? {
            store
                .save(next)
                .map_err(|_| "could not save platform usernames")?;
        }
        Ok(store)
    }

    /// Authentication rejects a ban even when an existing token remains valid.
    pub fn account(&self, subject: &str) -> Result<Account, Problem> {
        let account = self.account_record(subject)?;
        if account.banned {
            return Err(Problem::Banned);
        }
        Ok(account)
    }

    /// Administrative reads include banned accounts so access can be restored.
    pub fn account_record(&self, subject: &str) -> Result<Account, Problem> {
        if self.failed {
            return Err(Problem::Storage);
        }
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
                handle: account.handle.clone(),
                banned: account.banned,
                rate_limit_bps: account.rate_limit_bps,
                has_recharged: account.has_recharged,
                traffic_remaining: None,
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
        if self.failed {
            return Err(Problem::Storage);
        }
        if self
            .database
            .accounts
            .get(&identity.subject)
            .is_some_and(|account| account.banned)
        {
            return Err(Problem::Banned);
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
                handle: None,
                generated_handle: false,
                handle_aliases: BTreeSet::new(),
                banned: false,
                rate_limit_bps: None,
                has_recharged: false,
            });
        account.github_login.clone_from(&identity.login);
        initialize_handles(&mut next)?;
        let account = next
            .accounts
            .get(&identity.subject)
            .ok_or(Problem::Storage)?
            .clone();
        self.save(next)?;
        Ok(account)
    }

    pub fn handle_available(&self, handle: &str) -> Result<bool, Problem> {
        Ok(self.account_by_handle(handle)?.is_none())
    }

    pub fn account_by_handle(&self, handle: &str) -> Result<Option<Account>, Problem> {
        let handle = normalize_handle(handle)?;
        if self.failed {
            return Err(Problem::Storage);
        }
        Ok(self
            .database
            .accounts
            .values()
            .find(|account| {
                account.handle.as_deref() == Some(handle.as_str())
                    || account.handle_aliases.contains(&handle)
            })
            .cloned())
    }

    /// A CLI login can choose the first explicit name, but never rename one.
    /// An old name still logs in to its owner without replacing the current name.
    pub fn claim_handle(&mut self, subject: &str, handle: &str) -> Result<Account, Problem> {
        let handle = normalize_handle(handle)?;
        let account = self.account(subject)?;
        if account.handle.as_deref() == Some(handle.as_str()) {
            return if account.generated_handle {
                self.rename_handle(subject, &handle)
            } else {
                Ok(account)
            };
        }
        if account.handle_aliases.contains(&handle) {
            return Ok(account);
        }
        if account.handle.is_some() && !account.generated_handle {
            return Err(Problem::UsernameBound);
        }
        self.rename_handle(subject, &handle)
    }

    /// The authenticated browser profile is the only explicit rename flow.
    /// Validation, namespace reservation and persistence share the Store writer lease.
    pub fn rename_handle(&mut self, subject: &str, handle: &str) -> Result<Account, Problem> {
        let handle = normalize_handle(handle)?;
        let account = self.account(subject)?;
        if account.handle.as_deref() == Some(handle.as_str()) && !account.generated_handle {
            return Ok(account);
        }
        if self
            .account_by_handle(&handle)?
            .is_some_and(|owner| owner.subject != subject)
        {
            return Err(Problem::UsernameTaken);
        }
        if account.handle.as_deref() != Some(handle.as_str())
            && account.handle_aliases.len() >= MAX_HANDLE_ALIASES
            && !account.handle_aliases.contains(&handle)
        {
            return Err(Problem::Limit);
        }
        let mut next = self.database.clone();
        let account = next
            .accounts
            .get_mut(subject)
            .ok_or(Problem::Unauthorized)?;
        account.handle_aliases.remove(&handle);
        if let Some(previous) = account.handle.replace(handle.clone())
            && previous != handle
        {
            account.handle_aliases.insert(previous);
        }
        account.generated_handle = false;
        let account = account.clone();
        self.save(next)?;
        Ok(account)
    }

    /// Partial updates preserve fields changed by another administrator.
    pub fn set_policy(
        &mut self,
        subject: &str,
        banned: Option<bool>,
        rate_limit_bps: Option<Option<u64>>,
    ) -> Result<Account, Problem> {
        self.account_record(subject)?;
        if rate_limit_bps
            .flatten()
            .is_some_and(|rate| !(1..=MAX_RATE_LIMIT_BPS).contains(&rate))
        {
            return Err(Problem::ManagementInvalid);
        }
        let mut next = self.database.clone();
        let account = next
            .accounts
            .get_mut(subject)
            .ok_or(Problem::Unauthorized)?;
        if let Some(value) = banned {
            account.banned = value;
        }
        if let Some(value) = rate_limit_bps {
            account.rate_limit_bps = value;
        }
        let account = account.clone();
        self.save(next)?;
        Ok(account)
    }

    #[cfg(test)]
    pub fn add(&mut self, owner: &str, key: PublicKey) -> Result<(), Problem> {
        self.account(owner)?;
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

    #[cfg(test)]
    pub fn remove(&mut self, owner: &str, fingerprint: &str) -> Result<(), Problem> {
        self.account(owner)?;
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

    pub(crate) fn configure_monthly_allowances(
        &mut self,
        allowances: crate::user_policy::MonthlyTrafficAllowances,
    ) {
        // Server configuration changes policy, never the persisted usage journal.
        self.monthly_allowances = allowances;
    }

    pub(crate) fn traffic_quota(
        &self,
        subject: &str,
        period: &str,
        utc_offset_minutes: i32,
        unlimited: bool,
    ) -> Result<crate::user_policy::TrafficQuota, Problem> {
        let account = self.account(subject)?;
        self.traffic.snapshot(
            subject,
            period,
            utc_offset_minutes,
            self.monthly_allowances
                .budget(account.has_recharged, unlimited),
        )
    }

    pub(crate) fn set_traffic_remaining(
        &mut self,
        subject: &str,
        period: &str,
        utc_offset_minutes: i32,
        remaining_bytes: u64,
    ) -> Result<(), Problem> {
        let account = self.account_record(subject)?;
        self.traffic.set_remaining(
            &self.directory,
            subject,
            period,
            utc_offset_minutes,
            remaining_bytes,
            self.monthly_allowances
                .budget(account.has_recharged, false)
                .free_bytes,
        )
    }

    pub(crate) fn admin_traffic_remaining(
        &self,
        subject: &str,
        period: &str,
        utc_offset_minutes: i32,
    ) -> Result<u64, Problem> {
        let account = self.account_record(subject)?;
        Ok(self
            .traffic
            .snapshot(
                subject,
                period,
                utc_offset_minutes,
                self.monthly_allowances.budget(account.has_recharged, false),
            )?
            .remaining_bytes
            .expect("finite quota"))
    }

    pub(crate) fn reserve_traffic_frame(
        &mut self,
        subject: &str,
        period: &str,
        utc_offset_minutes: i32,
        frame: crate::user_policy::TrafficFrame,
        unlimited: bool,
    ) -> Result<(), Problem> {
        let account = self.account(subject)?;
        self.traffic.reserve_frame(
            &self.directory,
            subject,
            period,
            utc_offset_minutes,
            frame,
            self.monthly_allowances
                .budget(account.has_recharged, unlimited),
        )
    }

    #[cfg(test)]
    pub(crate) fn reserve_traffic(
        &mut self,
        subject: &str,
        period: &str,
        utc_offset_minutes: i32,
        bytes: u64,
        unlimited: bool,
    ) -> Result<(), Problem> {
        let account = self.account(subject)?;
        self.traffic.reserve(
            &self.directory,
            subject,
            period,
            utc_offset_minutes,
            bytes,
            self.monthly_allowances
                .budget(account.has_recharged, unlimited),
        )
    }

    pub(crate) fn grant_paid_traffic(
        &mut self,
        subject: &str,
        period: &str,
        current_period: &str,
        order_id: &str,
        bytes: u64,
    ) -> Result<bool, Problem> {
        // An already paid order must remain reconcilable after a later account ban.
        self.account_record(subject)?;
        self.traffic.grant(
            &self.directory,
            subject,
            period,
            current_period,
            order_id,
            bytes,
        )
    }

    pub(crate) fn record_recharge(&mut self, subject: &str) -> Result<Account, Problem> {
        let account = self.account_record(subject)?;
        if account.has_recharged {
            return Ok(account);
        }
        let mut next = self.database.clone();
        let account = next
            .accounts
            .get_mut(subject)
            .ok_or(Problem::Unauthorized)?;
        account.has_recharged = true;
        let account = account.clone();
        self.save(next)?;
        Ok(account)
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

pub(crate) const MAX_RATE_LIMIT_BPS: u64 = 1024 * 1024 * 1024;

pub(crate) fn normalize_handle(raw: &str) -> Result<String, Problem> {
    crate::username::normalize(raw).map_err(|_| Problem::UsernameInvalid)
}

pub(crate) fn stable_username(subject: &str) -> Option<String> {
    let id = subject.parse::<u64>().ok()?;
    (id > 0 && id.to_string() == subject).then(|| format!("gh-{id}"))
}

/// Allocate names independently of mutable upstream login names and SSH identities.
/// Callers hold the sole writer lease and persist the complete update atomically.
fn initialize_handles(database: &mut Database) -> Result<bool, Problem> {
    let missing: Vec<_> = database
        .accounts
        .values()
        .filter(|account| account.handle.is_none())
        .map(|account| account.subject.clone())
        .collect();
    for subject in &missing {
        let mut chosen = None;
        for _ in 0..16 {
            let mut bytes = [0u8; 8];
            getrandom::fill(&mut bytes).map_err(|_| Problem::Storage)?;
            let handle = format!("member-{:016x}", u64::from_be_bytes(bytes));
            if database.accounts.values().all(|account| {
                account.handle.as_deref() != Some(handle.as_str())
                    && !account.handle_aliases.contains(&handle)
            }) {
                chosen = Some(handle);
                break;
            }
        }
        let handle = chosen.ok_or(Problem::Busy)?;
        let account = database.accounts.get_mut(subject).ok_or(Problem::Storage)?;
        account.handle = Some(handle);
        account.generated_handle = true;
    }
    Ok(!missing.is_empty())
}

fn validate(database: &Database) -> crate::Result<()> {
    if database.accounts.len() > MAX_ACCOUNTS {
        return Err("too many portal accounts".into());
    }
    let mut handles = BTreeSet::new();
    for (id, account) in &database.accounts {
        if &account.subject != id
            || stable_username(id).as_deref() != Some(&account.username)
            || account.keys.len() > MAX_KEYS
            || crate::wire::canonical_github_principal(&format!("{}@github", account.github_login))
                .is_err()
        {
            return Err("invalid portal account; refusing to discard account data".into());
        }
        if account
            .rate_limit_bps
            .is_some_and(|rate| !(1..=MAX_RATE_LIMIT_BPS).contains(&rate))
        {
            return Err("invalid account rate limit".into());
        }
        if account.handle_aliases.len() > MAX_HANDLE_ALIASES
            || (account.handle.is_none()
                && (account.generated_handle || !account.handle_aliases.is_empty()))
        {
            return Err("invalid account username aliases".into());
        }
        for handle in account.handle.iter().chain(account.handle_aliases.iter()) {
            if normalize_handle(handle).ok().as_ref() != Some(handle) || !handles.insert(handle) {
                return Err(
                    "invalid or duplicate account username; refusing to discard account data"
                        .into(),
                );
            }
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

    struct AccountDirectory(PathBuf);
    impl AccountDirectory {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!(
                "ssmcp-account-policy-{}",
                super::super::random().unwrap()
            )))
        }
    }
    impl Drop for AccountDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn identity(subject: &str, login: &str) -> Identity {
        Identity {
            provider: "github".into(),
            subject: subject.into(),
            login: login.into(),
        }
    }

    #[test]
    fn handles_are_unique_stable_canonical_and_durable() {
        let directory = AccountDirectory::new();
        let mut store = Store::open(&directory.0).unwrap();
        store.login(&identity("123", "alice")).unwrap();
        store.login(&identity("456", "bob")).unwrap();
        assert_eq!(
            store
                .claim_handle("123", "ALice-home")
                .unwrap()
                .handle
                .as_deref(),
            Some("alice-home")
        );
        assert!(matches!(
            store.claim_handle("456", "Alice-home"),
            Err(Problem::UsernameTaken)
        ));
        assert!(matches!(
            store.claim_handle("123", "other-name"),
            Err(Problem::UsernameBound)
        ));
        assert_eq!(
            store.claim_handle("123", "alice-home").unwrap().subject,
            "123"
        );
        assert_eq!(
            store
                .login(&identity("123", "renamed"))
                .unwrap()
                .handle
                .as_deref(),
            Some("alice-home")
        );
        drop(store);
        let reopened = Store::open(&directory.0).unwrap();
        assert_eq!(
            reopened
                .account_by_handle("ALICE-home")
                .unwrap()
                .unwrap()
                .subject,
            "123"
        );
        assert!(!reopened.handle_available("alice-home").unwrap());
        assert!(reopened.handle_available("other-name").unwrap());
    }

    #[test]
    fn legacy_accounts_gain_default_policy_without_losing_identity() {
        let raw = r#"{"accounts":{"123":{"subject":"123","username":"gh-123","github_login":"alice","keys":[]}}}"#;
        let database: Database = serde_json::from_str(raw).unwrap();
        validate(&database).unwrap();
        let account = &database.accounts["123"];
        assert_eq!(account.username, "gh-123");
        assert!(account.handle.is_none());
        assert!(!account.banned);
        assert!(account.rate_limit_bps.is_none());
    }

    #[test]
    fn default_username_is_persistent_and_an_explicit_claim_locks_cli_renames() {
        let directory = AccountDirectory::new();
        let mut store = Store::open(&directory.0).unwrap();
        let initial = store.login(&identity("123", "alice")).unwrap();
        let name = initial.handle.clone().unwrap();
        assert!(name.starts_with("member-"));
        assert_eq!(normalize_handle(&name).unwrap(), name);
        assert!(initial.generated_handle);
        assert_eq!(initial.username, "gh-123");
        assert_eq!(
            store.login(&identity("123", "renamed")).unwrap().handle,
            initial.handle
        );
        let confirmed = store.claim_handle("123", &name).unwrap();
        assert!(!confirmed.generated_handle);
        assert!(matches!(
            store.claim_handle("123", "another-name"),
            Err(Problem::UsernameBound)
        ));
        drop(store);
        let reopened = Store::open(&directory.0).unwrap();
        let saved = reopened.account("123").unwrap();
        assert_eq!(saved.handle.as_deref(), Some(name.as_str()));
        assert!(!saved.generated_handle);
        assert_eq!(saved.github_login, "renamed");
    }

    #[test]
    fn browser_rename_retains_names_ownership_policy_and_durable_aliases() {
        let directory = AccountDirectory::new();
        let mut store = Store::open(&directory.0).unwrap();
        let initial = store
            .login(&identity("123", "alice"))
            .unwrap()
            .handle
            .unwrap();
        store.login(&identity("456", "bob")).unwrap();
        store.claim_handle("123", "alice-home").unwrap();
        store.set_policy("123", None, Some(Some(8192))).unwrap();
        store.record_recharge("123").unwrap();
        let renamed = store.rename_handle("123", "ALice-work").unwrap();
        assert_eq!(renamed.handle.as_deref(), Some("alice-work"));
        assert_eq!(renamed.subject, "123");
        assert_eq!(renamed.username, "gh-123");
        assert_eq!(renamed.rate_limit_bps, Some(8192));
        assert!(renamed.has_recharged);
        assert!(renamed.handle_aliases.contains(&initial));
        assert!(renamed.handle_aliases.contains("alice-home"));
        for name in [&initial, "alice-home", "alice-work"] {
            assert_eq!(
                store.account_by_handle(name).unwrap().unwrap().subject,
                "123"
            );
            assert!(!store.handle_available(name).unwrap());
            assert!(matches!(
                store.rename_handle("456", name),
                Err(Problem::UsernameTaken)
            ));
        }
        let before = serde_json::to_value(&store.database).unwrap();
        assert!(matches!(
            store.rename_handle("123", "gh-456"),
            Err(Problem::UsernameInvalid)
        ));
        assert_eq!(serde_json::to_value(&store.database).unwrap(), before);
        let login = store.claim_handle("123", "alice-home").unwrap();
        assert_eq!(login.handle.as_deref(), Some("alice-work"));
        let returned = store.rename_handle("123", "alice-home").unwrap();
        assert_eq!(returned.handle.as_deref(), Some("alice-home"));
        assert!(!returned.handle_aliases.contains("alice-home"));
        assert!(returned.handle_aliases.contains("alice-work"));
        drop(store);
        let mut reopened = Store::open(&directory.0).unwrap();
        assert_eq!(
            reopened
                .account_by_handle("alice-work")
                .unwrap()
                .unwrap()
                .subject,
            "123"
        );
        reopened.set_policy("123", Some(true), None).unwrap();
        assert!(matches!(
            reopened.rename_handle("123", "alice-next"),
            Err(Problem::Banned)
        ));
        assert!(!reopened.handle_available("alice-work").unwrap());
    }

    #[test]
    fn legacy_missing_names_are_backfilled_without_unlocking_explicit_names() {
        let directory = AccountDirectory::new();
        let mut store = Store::open(&directory.0).unwrap();
        let raw = r#"{"accounts":{"123":{"subject":"123","username":"gh-123","github_login":"alice","keys":[],"banned":true,"rate_limit_bps":8192,"has_recharged":true},"456":{"subject":"456","username":"gh-456","github_login":"bob","keys":[],"handle":"bob-home"}}}"#;
        store.save(serde_json::from_str(raw).unwrap()).unwrap();
        drop(store);
        let mut reopened = Store::open(&directory.0).unwrap();
        let migrated = reopened.account_record("123").unwrap();
        let name = migrated.handle.clone().unwrap();
        assert!(migrated.generated_handle);
        assert!(migrated.banned);
        assert!(migrated.has_recharged);
        assert_eq!(migrated.username, "gh-123");
        assert_eq!(migrated.rate_limit_bps, Some(8192));
        assert!(!reopened.account("456").unwrap().generated_handle);
        assert!(matches!(
            reopened.claim_handle("456", "bob-work"),
            Err(Problem::UsernameBound)
        ));
        drop(reopened);
        assert_eq!(
            Store::open(&directory.0)
                .unwrap()
                .account_record("123")
                .unwrap()
                .handle
                .as_deref(),
            Some(name.as_str())
        );
    }

    #[test]
    fn alias_namespace_validation_and_capacity_fail_without_reassigning_names() {
        let directory = AccountDirectory::new();
        let mut store = Store::open(&directory.0).unwrap();
        store.login(&identity("123", "alice")).unwrap();
        store.login(&identity("456", "bob")).unwrap();
        store.claim_handle("123", "alice-home").unwrap();
        store.claim_handle("456", "bob-home").unwrap();
        let mut next = store.database.clone();
        let account = next.accounts.get_mut("123").unwrap();
        account.handle_aliases = (0..MAX_HANDLE_ALIASES)
            .map(|n| format!("old-name-{n}"))
            .collect();
        validate(&next).unwrap();
        store.save(next).unwrap();
        let before = serde_json::to_value(&store.database).unwrap();
        assert!(matches!(
            store.rename_handle("123", "fresh-name"),
            Err(Problem::Limit)
        ));
        assert_eq!(serde_json::to_value(&store.database).unwrap(), before);
        store.rename_handle("123", "old-name-0").unwrap();
        assert_eq!(
            store.account("123").unwrap().handle_aliases.len(),
            MAX_HANDLE_ALIASES
        );
        let mut duplicate = store.database.clone();
        duplicate
            .accounts
            .get_mut("456")
            .unwrap()
            .handle_aliases
            .insert("alice-home".into());
        assert!(validate(&duplicate).is_err());
        store.failed = true;
        assert!(matches!(
            store.rename_handle("123", "old-name-1"),
            Err(Problem::Storage)
        ));
    }

    #[test]
    fn policy_updates_are_partial_durable_and_block_authentication() {
        let directory = AccountDirectory::new();
        let mut store = Store::open(&directory.0).unwrap();
        store.login(&identity("123", "alice")).unwrap();
        store.claim_handle("123", "alice-home").unwrap();
        store
            .set_policy("123", Some(true), Some(Some(8192)))
            .unwrap();
        assert!(matches!(store.account("123"), Err(Problem::Banned)));
        assert!(matches!(
            store.login(&identity("123", "renamed")),
            Err(Problem::Banned)
        ));
        assert!(matches!(
            store.claim_handle("123", "alice-home"),
            Err(Problem::Banned)
        ));
        assert!(
            store
                .set_policy("123", None, Some(Some(16384)))
                .unwrap()
                .banned
        );
        assert!(matches!(
            store.set_policy("123", None, Some(Some(0))),
            Err(Problem::ManagementInvalid)
        ));
        assert!(matches!(
            store.set_policy("123", None, Some(Some(MAX_RATE_LIMIT_BPS + 1))),
            Err(Problem::ManagementInvalid)
        ));
        drop(store);
        let mut store = Store::open(&directory.0).unwrap();
        let saved = store.account_record("123").unwrap();
        assert!(saved.banned);
        assert_eq!(saved.rate_limit_bps, Some(16384));
        let restored = store.set_policy("123", Some(false), None).unwrap();
        assert_eq!(restored.rate_limit_bps, Some(16384));
        assert!(store.account("123").is_ok());
        assert_eq!(
            store
                .set_policy("123", None, Some(None))
                .unwrap()
                .rate_limit_bps,
            None
        );
    }

    #[test]
    fn database_rejects_duplicate_handles_and_invalid_policies() {
        let mut database = Database::default();
        for (subject, login) in [("123", "alice"), ("456", "bob")] {
            database.accounts.insert(
                subject.into(),
                Account {
                    subject: subject.into(),
                    username: format!("gh-{subject}"),
                    github_login: login.into(),
                    keys: Vec::new(),
                    handle: Some("shared-name".into()),
                    generated_handle: false,
                    handle_aliases: BTreeSet::new(),
                    banned: false,
                    rate_limit_bps: None,
                    has_recharged: false,
                },
            );
        }
        assert!(validate(&database).is_err());
        database.accounts.get_mut("456").unwrap().handle = Some("Other-name".into());
        assert!(validate(&database).is_err());
        database.accounts.get_mut("456").unwrap().handle = Some("other-name".into());
        validate(&database).unwrap();
        database.accounts.get_mut("456").unwrap().rate_limit_bps = Some(0);
        assert!(validate(&database).is_err());
    }

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

    fn monthly_allowances(free: u64, paid: u64) -> crate::user_policy::MonthlyTrafficAllowances {
        crate::user_policy::MonthlyTrafficAllowances {
            free_user_bytes: free,
            paid_user_bytes: paid,
        }
    }
    fn monthly_quota(store: &Store, subject: &str) -> crate::user_policy::TrafficQuota {
        store.traffic_quota(subject, "2026-11", 480, false).unwrap()
    }
    fn monthly_frame(
        store: &mut Store,
        uploaded: bool,
        bytes: u64,
        unlimited: bool,
    ) -> Result<(), Problem> {
        store.reserve_traffic_frame(
            "123",
            "2026-11",
            480,
            crate::user_policy::TrafficFrame { uploaded, bytes },
            unlimited,
        )
    }

    #[test]
    fn monthly_tier_uses_durable_recharge_not_login_or_handle_labels() {
        let directory = AccountDirectory::new();
        let mut store = Store::open(&directory.0).unwrap();
        store.login(&identity("123", "paid")).unwrap();
        store.claim_handle("123", "paid-user").unwrap();
        store.login(&identity("456", "ordinary")).unwrap();
        store.record_recharge("456").unwrap();
        assert_eq!(
            monthly_quota(&store, "123").limit_bytes,
            Some(1_000_000_000)
        );
        assert_eq!(
            monthly_quota(&store, "456").limit_bytes,
            Some(1_000_000_000)
        );
        store.configure_monthly_allowances(monthly_allowances(100, 1_000));
        assert_eq!(monthly_quota(&store, "123").free_bytes, 100);
        assert_eq!(monthly_quota(&store, "456").free_bytes, 1_000);
        drop(store);
        let mut store = Store::open(&directory.0).unwrap();
        store.configure_monthly_allowances(monthly_allowances(100, 1_000));
        store.login(&identity("456", "renamed")).unwrap();
        assert_eq!(monthly_quota(&store, "456").free_bytes, 1_000);
        assert_eq!(monthly_quota(&store, "123").free_bytes, 100);
        assert!(matches!(
            store.traffic_quota("999", "2026-11", 480, false),
            Err(Problem::Unauthorized)
        ));
        store.set_policy("456", Some(true), None).unwrap();
        assert!(matches!(
            store.traffic_quota("456", "2026-11", 480, true),
            Err(Problem::Banned)
        ));
        assert_eq!(
            store
                .admin_traffic_remaining("456", "2026-11", 480)
                .unwrap(),
            1_000
        );
    }

    #[test]
    fn monthly_tier_changes_preserve_directional_and_legacy_usage_across_restart() {
        let directory = AccountDirectory::new();
        let mut store = Store::open(&directory.0).unwrap();
        store.login(&identity("123", "alice")).unwrap();
        store.configure_monthly_allowances(monthly_allowances(100, 1_000));
        store
            .reserve_traffic("123", "2026-11", 480, 7, false)
            .unwrap();
        monthly_frame(&mut store, true, 40, false).unwrap();
        monthly_frame(&mut store, false, 25, false).unwrap();
        assert_eq!(monthly_quota(&store, "123").used_bytes, 47);
        store.record_recharge("123").unwrap();
        let paid = monthly_quota(&store, "123");
        assert_eq!((paid.limit_bytes, paid.used_bytes), (Some(1_000), 47));
        store.configure_monthly_allowances(monthly_allowances(20, 30));
        let reduced = monthly_quota(&store, "123");
        assert_eq!((reduced.used_bytes, reduced.remaining_bytes), (47, Some(0)));
        assert!(matches!(
            monthly_frame(&mut store, true, 1, false),
            Err(Problem::TrafficQuotaExceeded)
        ));
        monthly_frame(&mut store, true, 3, true).unwrap();
        drop(store);
        let mut store = Store::open(&directory.0).unwrap();
        store.configure_monthly_allowances(monthly_allowances(100, 200));
        let restored = monthly_quota(&store, "123");
        assert_eq!(
            (
                restored.uploaded_bytes,
                restored.downloaded_bytes,
                restored.legacy_used_bytes
            ),
            (43, 25, 7)
        );
        assert_eq!(
            (restored.used_bytes, restored.remaining_bytes),
            (50, Some(150))
        );
        let next = store.traffic_quota("123", "2026-12", 480, false).unwrap();
        assert_eq!(
            (next.used_bytes, next.free_bytes, next.paid_bytes),
            (0, 200, 0)
        );
        assert_eq!(monthly_quota(&store, "123").used_bytes, 50);
    }

    #[test]
    fn monthly_admin_override_wins_even_when_initial_edit_matches_the_default() {
        let directory = AccountDirectory::new();
        let mut store = Store::open(&directory.0).unwrap();
        store.login(&identity("123", "alice")).unwrap();
        store.configure_monthly_allowances(monthly_allowances(100, 1_000));
        monthly_frame(&mut store, true, 40, false).unwrap();
        // Explicitly pin the same total of 100, before an upgrade changes the default.
        store
            .set_traffic_remaining("123", "2026-11", 480, 60)
            .unwrap();
        store.record_recharge("123").unwrap();
        assert_eq!(monthly_quota(&store, "123").limit_bytes, Some(100));
        assert!(
            store
                .grant_paid_traffic("123", "2026-11", "2026-11", "monthly-pack", 50)
                .unwrap()
        );
        assert!(
            !store
                .grant_paid_traffic("123", "2026-11", "2026-11", "monthly-pack", 50)
                .unwrap()
        );
        drop(store);
        let mut store = Store::open(&directory.0).unwrap();
        store.configure_monthly_allowances(monthly_allowances(1, 2));
        let quota = monthly_quota(&store, "123");
        assert_eq!(
            (quota.free_bytes, quota.paid_bytes, quota.used_bytes),
            (2, 50, 40)
        );
        assert_eq!(
            (quota.limit_bytes, quota.remaining_bytes),
            (Some(150), Some(110))
        );
        assert_eq!(
            store
                .admin_traffic_remaining("123", "2026-11", 480)
                .unwrap(),
            110
        );
        let unlimited = store.traffic_quota("123", "2026-11", 480, true).unwrap();
        assert_eq!(
            (unlimited.limit_bytes, unlimited.remaining_bytes),
            (None, None)
        );
        let next = store.traffic_quota("123", "2026-12", 480, false).unwrap();
        assert_eq!(
            (next.limit_bytes, next.used_bytes, next.paid_bytes),
            (Some(2), 0, 0)
        );
    }

    #[test]
    fn monthly_zero_gift_and_paid_packs_do_not_grant_unlimited_access() {
        let directory = AccountDirectory::new();
        let mut store = Store::open(&directory.0).unwrap();
        store.login(&identity("123", "alice")).unwrap();
        store.configure_monthly_allowances(monthly_allowances(0, 0));
        monthly_frame(&mut store, true, 0, false).unwrap();
        assert!(matches!(
            monthly_frame(&mut store, true, 1, false),
            Err(Problem::TrafficQuotaExceeded)
        ));
        store.record_recharge("123").unwrap();
        assert_eq!(monthly_quota(&store, "123").limit_bytes, Some(0));
        store
            .grant_paid_traffic("123", "2026-11", "2026-11", "zero-tier-pack", 10)
            .unwrap();
        monthly_frame(&mut store, true, 10, false).unwrap();
        monthly_frame(&mut store, false, 10, false).unwrap();
        assert_eq!(monthly_quota(&store, "123").used_bytes, 10);
        assert!(matches!(
            monthly_frame(&mut store, false, 1, false),
            Err(Problem::TrafficQuotaExceeded)
        ));
        monthly_frame(&mut store, false, 1, true).unwrap();
        assert_eq!(monthly_quota(&store, "123").used_bytes, 11);
    }
}
