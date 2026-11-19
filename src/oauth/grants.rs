//! Hash-only account grants. The DCR writer lease also protects this companion
//! file. No write occurs on ordinary authentication; grants, refresh rotation and
//! revocation publish bounded snapshots before their operation is acknowledged.
use super::registry::{open_at, private_file, write_atomic};
use super::*;
use std::{
    ffi::CString,
    fs::File,
    io::{self, Read, Write},
    os::fd::AsRawFd,
};

const MAX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_LIFETIME_MS: u64 = 315_360_000_000;

pub(super) struct Grants {
    directory: File,
    filename: CString,
    pending: CString,
    issuer: String,
    // A duplicate of the DCR lock's open file description, not a second lock.
    _lease: File,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u32,
    issuer: String,
    written_at_ms: u64,
    families: Vec<FamilyRecord>,
    access: Vec<AccessRecord>,
    refresh: Vec<RefreshRecord>,
    used_codes: Vec<CodeRecord>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FamilyRecord {
    key: Key,
    user: String,
    resource: String,
    client_id: String,
    provider: String,
    subject: String,
    login: String,
    expires_at_ms: u64,
    absolute_expires_at_ms: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AccessRecord {
    key: Key,
    family: Key,
    expires_at_ms: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RefreshRecord {
    key: Key,
    family: Key,
    expires_at_ms: u64,
    used: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CodeRecord {
    key: Key,
    family: Key,
    challenge: Key,
    redirect_uri: String,
}

pub(super) fn wall_now_ms() -> crate::Result<u64> {
    u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())
        .map_err(|_| "OAuth wall clock is out of range".into())
}
pub(super) fn wall_deadline(deadline: Instant) -> OResult<u64> {
    // Reading the wall clock first cannot extend the monotonic deadline.
    let wall = wall_now_ms().map_err(|_| OAuthError::storage())?;
    let remaining = u64::try_from(
        deadline
            .saturating_duration_since(Instant::now())
            .as_millis(),
    )
    .map_err(|_| OAuthError::storage())?;
    wall.checked_add(remaining).ok_or_else(OAuthError::storage)
}
fn instant_deadline(now: Instant, wall: u64, end: u64) -> crate::Result<Instant> {
    now.checked_add(Duration::from_millis(end.saturating_sub(wall)))
        .ok_or_else(|| "OAuth grant deadline is out of range".into())
}

impl Grants {
    pub(super) fn open(registry: &registry::Registry, issuer: &str) -> crate::Result<Self> {
        let (directory, filename, lease) = registry.grant_storage()?;
        let mut name = filename.as_bytes().to_vec();
        name.extend_from_slice(b".pending");
        let storage = Self {
            directory,
            filename,
            pending: CString::new(name)?,
            issuer: issuer.into(),
            _lease: lease,
        };
        match open_at(
            &storage.directory,
            &storage.pending,
            libc::O_RDONLY | libc::O_NONBLOCK,
        ) {
            Ok(file) => {
                private_file(&file)?;
                return Err(
                    "incomplete OAuth grant commit; inspect the private state before restarting"
                        .into(),
                );
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(storage)
    }

    pub(super) fn restore(&self, state: &mut Store) -> crate::Result<()> {
        let file = match open_at(
            &self.directory,
            &self.filename,
            libc::O_RDONLY | libc::O_NONBLOCK,
        ) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        private_file(&file)?;
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err("OAuth grants exceed size limit".into());
        }
        // Do not expose parser diagnostics containing private account metadata.
        let doc: Document =
            serde_json::from_slice(&bytes).map_err(|_| "invalid OAuth grant state")?;
        let wall = wall_now_ms()?;
        let now = Instant::now();
        if doc.version != 1
            || doc.issuer != self.issuer
            || doc.written_at_ms > wall
            || doc.families.len() > MAX_FAMILIES
            || doc.access.len() > MAX_ACCESS
            || doc.refresh.len() > MAX_REFRESH
            || doc.used_codes.len() > MAX_CODES
        {
            return Err("OAuth grant version, issuer, clock or capacity does not match".into());
        }
        let mut families = HashMap::new();
        let mut limits = HashMap::new();
        for record in doc.families {
            if limits
                .insert(
                    record.key,
                    (record.expires_at_ms, record.absolute_expires_at_ms),
                )
                .is_some()
                || !valid_username(&record.user)
                || record.resource != format!("{}/{}", self.issuer, record.user)
                || !opaque(&record.client_id, "client_")
                || record.provider != "github"
                || !crate::management::valid_subject(&record.subject)
                || crate::wire::canonical_github_principal(&format!("{}@github", record.login))
                    .is_err()
                || record.expires_at_ms == 0
                || record.expires_at_ms > record.absolute_expires_at_ms
                || record.absolute_expires_at_ms.saturating_sub(wall) > MAX_LIFETIME_MS
            {
                return Err("invalid persisted OAuth family".into());
            }
            let Some(client) = state.clients.get(&record.client_id) else {
                continue;
            };
            if record.absolute_expires_at_ms > client.expires_unix.saturating_mul(1000) {
                return Err("OAuth grant outlives its registered client".into());
            }
            if record.expires_at_ms <= wall {
                continue;
            }
            families.insert(
                record.key,
                Family {
                    binding: Binding {
                        user: record.user,
                        resource: record.resource,
                        client_id: record.client_id,
                        authorization: Authorization::Identity(Identity {
                            provider: record.provider,
                            subject: record.subject,
                            login: record.login,
                        }),
                    },
                    expires: instant_deadline(now, wall, record.expires_at_ms)?,
                    absolute_expires: instant_deadline(now, wall, record.absolute_expires_at_ms)?,
                    expires_at_ms: record.expires_at_ms,
                    absolute_expires_at_ms: record.absolute_expires_at_ms,
                    revoked: false,
                },
            );
        }
        let mut access = HashMap::new();
        let mut seen = HashSet::new();
        for record in doc.access {
            let Some(&(_, absolute)) = limits.get(&record.family) else {
                return Err("orphaned OAuth access record".into());
            };
            if !seen.insert(record.key)
                || record.expires_at_ms == 0
                || record.expires_at_ms > absolute
            {
                return Err("invalid persisted OAuth access record".into());
            }
            if record.expires_at_ms <= wall || !families.contains_key(&record.family) {
                continue;
            }
            access.insert(
                record.key,
                AccessToken {
                    family: record.family,
                    expires: instant_deadline(now, wall, record.expires_at_ms)?,
                    expires_at_ms: record.expires_at_ms,
                },
            );
        }
        let mut refresh = HashMap::new();
        seen.clear();
        for record in doc.refresh {
            let Some(&(idle, absolute)) = limits.get(&record.family) else {
                return Err("orphaned OAuth refresh record".into());
            };
            let limit = if record.used { absolute } else { idle };
            if !seen.insert(record.key) || record.expires_at_ms == 0 || record.expires_at_ms > limit
            {
                return Err("invalid persisted OAuth refresh record".into());
            }
            if record.expires_at_ms <= wall || !families.contains_key(&record.family) {
                continue;
            }
            refresh.insert(
                record.key,
                RefreshToken {
                    family: record.family,
                    expires: instant_deadline(now, wall, record.expires_at_ms)?,
                    expires_at_ms: record.expires_at_ms,
                    used: record.used,
                },
            );
        }
        let mut codes = HashMap::new();
        seen.clear();
        for record in doc.used_codes {
            if !seen.insert(record.key) || !limits.contains_key(&record.family) {
                return Err("invalid persisted OAuth code replay record".into());
            }
            let Some(family) = families.get(&record.family) else {
                continue;
            };
            let client = &state.clients[&family.binding.client_id];
            if !valid_redirect(&record.redirect_uri, &client.application_type) {
                return Err("invalid persisted OAuth redirect".into());
            }
            codes.insert(
                record.key,
                AuthorizationCode {
                    binding: family.binding.clone(),
                    redirect_uri: record.redirect_uri,
                    challenge: record.challenge,
                    // Only consumed code hashes survive a restart. A partial login
                    // or an unredeemed one-time code can never mint a new grant.
                    redeem_until: now,
                    retain_until: family.absolute_expires,
                    used_family: Some(record.family),
                },
            );
        }
        state.families = families;
        state.access = access;
        state.refresh = refresh;
        state.codes = codes;
        Ok(())
    }

    pub(super) fn save(&self, state: &Store) -> crate::Result<()> {
        let mut families = Vec::new();
        let mut included = HashSet::new();
        for (&key, family) in &state.families {
            if family.revoked {
                continue;
            }
            // Ephemeral legacy-session authorization must not become a durable
            // account grant, and no Weak<Session> can survive a process exit.
            let Authorization::Identity(identity) = &family.binding.authorization else {
                continue;
            };
            included.insert(key);
            families.push(FamilyRecord {
                key,
                user: family.binding.user.clone(),
                resource: family.binding.resource.clone(),
                client_id: family.binding.client_id.clone(),
                provider: identity.provider.clone(),
                subject: identity.subject.clone(),
                login: identity.login.clone(),
                expires_at_ms: family.expires_at_ms,
                absolute_expires_at_ms: family.absolute_expires_at_ms,
            });
        }
        let doc = Document {
            version: 1,
            issuer: self.issuer.clone(),
            written_at_ms: wall_now_ms()?,
            families,
            access: state
                .access
                .iter()
                .filter(|(_, a)| included.contains(&a.family))
                .map(|(&key, a)| AccessRecord {
                    key,
                    family: a.family,
                    expires_at_ms: a.expires_at_ms,
                })
                .collect(),
            refresh: state
                .refresh
                .iter()
                .filter(|(_, r)| included.contains(&r.family))
                .map(|(&key, r)| RefreshRecord {
                    key,
                    family: r.family,
                    expires_at_ms: r.expires_at_ms,
                    used: r.used,
                })
                .collect(),
            used_codes: state
                .codes
                .iter()
                .filter_map(|(&key, code)| {
                    let family = code.used_family.filter(|key| included.contains(key))?;
                    Some(CodeRecord {
                        key,
                        family,
                        challenge: code.challenge,
                        redirect_uri: code.redirect_uri.clone(),
                    })
                })
                .collect(),
        };
        if doc.families.len() > MAX_FAMILIES
            || doc.access.len() > MAX_ACCESS
            || doc.refresh.len() > MAX_REFRESH
            || doc.used_codes.len() > MAX_CODES
        {
            return Err("OAuth grants exceed capacity".into());
        }
        let bytes = serde_json::to_vec(&doc).map_err(|_| "could not serialize OAuth grants")?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err("OAuth grants exceed size limit".into());
        }
        // A durable intent makes an interrupted write fail closed on restart.
        // It is deliberately retained on any later failure, never auto-repaired
        // by accepting the older snapshot or by manufacturing fresh credentials.
        let mut pending = open_at(
            &self.directory,
            &self.pending,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        )?;
        private_file(&pending)?;
        pending.write_all(b"ssmcp OAuth grant commit v1\n")?;
        pending.sync_all()?;
        self.directory.sync_all()?;
        write_atomic(&self.directory, &self.filename, &bytes)?;
        // SAFETY: this is the exact private marker created above, under the
        // shared DCR writer lease and anchored directory descriptor.
        if unsafe { libc::unlinkat(self.directory.as_raw_fd(), self.pending.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        self.directory.sync_all()?;
        Ok(())
    }
}
