//! Persist the public WebAuthn credential and its verified GitHub account together.
//! The separate file lets earlier binaries continue to use accounts.json.
use super::{Database, Problem, Store, private_file};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::{ErrorKind, Read},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};
use webauthn_rs::prelude::{AuthenticationResult, Passkey, Uuid};

const FILE: &str = "passkeys-v1.json";
const MAX_PER_ACCOUNT: usize = 8;
const MAX_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    subject: String,
    user_handle: Uuid,
    passkey: Passkey,
}

// The wire credential ID is the primary key, independent of changeable account names.
#[derive(Default)]
pub(super) struct Credentials(BTreeMap<Vec<u8>, Binding>);

impl Credentials {
    pub(super) fn open(directory: &Path, accounts: &Database) -> crate::Result<Self> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(directory.join(FILE))
        {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error.into()),
        };
        private_file(&file)?;
        if file.metadata()?.len() > MAX_BYTES {
            return Err("portal passkey file exceeds its size limit".into());
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err("portal passkey file exceeds its size limit".into());
        }
        let bindings: Vec<Binding> = serde_json::from_slice(&bytes)?;
        let mut subjects = BTreeMap::new();
        let mut owners = BTreeMap::new();
        let mut credentials = Self::default();
        for binding in bindings {
            let (handle, count) = subjects
                .entry(binding.subject.clone())
                .or_insert((binding.user_handle, 0_usize));
            *count += 1;
            let owner = owners
                .entry(binding.user_handle)
                .or_insert_with(|| binding.subject.clone());
            let id = binding.passkey.cred_id();
            if !accounts.accounts.contains_key(&binding.subject)
                || binding.user_handle.is_nil()
                || *handle != binding.user_handle
                || *owner != binding.subject
                || *count > MAX_PER_ACCOUNT
                || !(1..=1023).contains(&id.len())
                || credentials.0.insert(id.clone(), binding).is_some()
            {
                return Err("invalid or duplicate portal passkey binding".into());
            }
        }
        Ok(credentials)
    }
}

impl Store {
    pub fn registration_handle(&self, subject: &str) -> Result<Uuid, Problem> {
        self.account(subject)?;
        Ok(self
            .passkeys
            .0
            .values()
            .find(|binding| binding.subject == subject)
            .map(|binding| binding.user_handle)
            .unwrap_or_else(Uuid::new_v4))
    }

    pub fn passkeys_for(&self, subject: &str) -> Result<Vec<Passkey>, Problem> {
        self.account(subject)?;
        Ok(self
            .passkeys
            .0
            .values()
            .filter(|binding| binding.subject == subject)
            .map(|binding| binding.passkey.clone())
            .collect())
    }

    pub fn bind_passkey(
        &mut self,
        subject: &str,
        user_handle: Uuid,
        passkey: Passkey,
    ) -> Result<(), Problem> {
        self.account(subject)?;
        let id = passkey.cred_id();
        if !(1..=1023).contains(&id.len()) {
            return Err(Problem::Unauthorized);
        }
        if user_handle.is_nil()
            || self.passkeys.0.contains_key(id)
            || self.passkeys.0.values().any(|binding| {
                // One account owns exactly one handle, and a handle belongs to one account.
                (binding.subject == subject) != (binding.user_handle == user_handle)
            })
        {
            return Err(Problem::PasskeyConflict);
        }
        if self
            .passkeys
            .0
            .values()
            .filter(|item| item.subject == subject)
            .count()
            >= MAX_PER_ACCOUNT
        {
            return Err(Problem::Limit);
        }
        self.save_passkey(Binding {
            subject: subject.to_owned(),
            user_handle,
            passkey,
        })
    }

    pub fn passkey_owner(&self, id: &[u8]) -> Result<(String, Uuid, Passkey), Problem> {
        if self.failed {
            return Err(Problem::Storage);
        }
        let binding = self.passkeys.0.get(id).ok_or(Problem::Unauthorized)?;
        self.account(&binding.subject)?;
        Ok((
            binding.subject.clone(),
            binding.user_handle,
            binding.passkey.clone(),
        ))
    }

    // The caller verifies the assertion against the current credential under this same Store lock.
    pub fn update_passkey(
        &mut self,
        subject: &str,
        result: &AuthenticationResult,
    ) -> Result<(), Problem> {
        self.account(subject)?;
        let binding = self
            .passkeys
            .0
            .get(result.cred_id())
            .filter(|binding| binding.subject == subject)
            .ok_or(Problem::Unauthorized)?;
        let mut passkey = binding.passkey.clone();
        match passkey.update_credential(result) {
            Some(false) => Ok(()),
            Some(true) => self.save_passkey(Binding {
                subject: subject.to_owned(),
                user_handle: binding.user_handle,
                passkey,
            }),
            None => Err(Problem::Unauthorized),
        }
    }

    fn save_passkey(&mut self, binding: Binding) -> Result<(), Problem> {
        let id = binding.passkey.cred_id();
        let entries: Vec<_> = self
            .passkeys
            .0
            .values()
            .filter(|item| item.passkey.cred_id() != id)
            .chain(std::iter::once(&binding))
            .collect();
        let bytes = serde_json::to_vec(&entries).map_err(|_| Problem::Storage)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(Problem::Limit);
        }
        self.replace_private(FILE, &bytes)?;
        self.passkeys.0.insert(id.clone(), binding);
        Ok(())
    }
}
