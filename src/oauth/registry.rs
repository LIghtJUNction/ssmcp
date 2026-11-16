//! Private, single-writer DCR registry. Tokens and authorization grants are not
//! persisted: a daemon restart still requires a new user authorization.
use super::{AuthMethod, Client, Key, MAX_CLIENTS, opaque, valid_redirect};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ffi::{CString, OsStr},
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_LIFETIME: u64 = 315_360_000;

pub(super) struct Registry {
    dir: File,
    filename: CString,
    issuer: String,
    // Keep the advisory lock for the entire OAuth store lifetime. Never unlink it.
    _lock: File,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u32,
    issuer: String,
    clients: BTreeMap<String, Record>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    name: String,
    redirects: Vec<String>,
    application_type: String,
    method: String,
    secret_hash: Option<Key>,
    refresh: bool,
    expires_unix: u64,
}

fn unix_now() -> crate::Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn cstring(value: &OsStr) -> crate::Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| "invalid OAuth registry path".into())
}

fn open_at(dir: &File, name: &CString, flags: i32) -> io::Result<File> {
    // SAFETY: dir and name live for the call; on success the new FD is owned here.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a fresh, valid file descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn private_file(file: &File) -> crate::Result<()> {
    let meta = file.metadata()?;
    // SAFETY: geteuid has no preconditions.
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o777 != 0o600
        || meta.nlink() != 1
        || meta.len() > MAX_BYTES
    {
        return Err("OAuth registry files must be regular, singly linked, owned by the daemon user, and mode 0600".into());
    }
    Ok(())
}

impl Registry {
    pub(super) fn open(
        path: &Path,
        issuer: &str,
    ) -> crate::Result<(Self, HashMap<String, Client>)> {
        if !path.is_absolute() {
            return Err("OAuth client registry needs an absolute path".into());
        }
        let parent = path
            .parent()
            .ok_or("OAuth client registry needs a parent directory")?;
        let filename = cstring(path.file_name().ok_or("OAuth registry needs a filename")?)?;
        // Walk with directory descriptors, never following a symlink. All writes
        // below are relative to the verified directory even if its path changes.
        let mut dir = File::open("/")?;
        for part in parent.components() {
            match part {
                Component::RootDir => {}
                Component::Normal(name) => {
                    dir = open_at(&dir, &cstring(name)?, libc::O_RDONLY | libc::O_DIRECTORY)?;
                    let meta = dir.metadata()?;
                    // Root-owned sticky /tmp is allowed as an ancestor in tests.
                    // SAFETY: geteuid has no preconditions.
                    if (meta.uid() != 0 && meta.uid() != unsafe { libc::geteuid() })
                        || (meta.mode() & 0o022 != 0
                            && !(meta.uid() == 0 && meta.mode() & 0o1000 != 0))
                    {
                        return Err("OAuth registry parent directories must be trusted and not writable by other users".into());
                    }
                }
                _ => {
                    return Err(
                        "OAuth registry path must not contain dot or parent components".into(),
                    );
                }
            }
        }
        let meta = dir.metadata()?;
        // SAFETY: geteuid has no preconditions.
        if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o777 != 0o700 {
            return Err(
                "OAuth registry directory must be owned by the daemon user with mode 0700".into(),
            );
        }
        let lockname = CString::new(format!("{}.lock", filename.to_string_lossy()))?;
        let lock = open_at(
            &dir,
            &lockname,
            libc::O_RDWR | libc::O_CREAT | libc::O_NONBLOCK,
        )?;
        private_file(&lock)?;
        // CLOEXEC descriptors can briefly survive in a concurrently forked child
        // until exec. Bound that transient wait, while rejecting a live writer.
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            // SAFETY: lock owns a valid FD; closing it releases the flock.
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::WouldBlock || Instant::now() >= deadline {
                return Err("OAuth client registry is already locked or cannot be locked".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let registry = Self {
            dir,
            filename,
            issuer: issuer.into(),
            _lock: lock,
        };
        let clients = match open_at(
            &registry.dir,
            &registry.filename,
            libc::O_RDONLY | libc::O_NONBLOCK,
        ) {
            Ok(file) => registry.load(file)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let empty = HashMap::new();
                registry.save(&empty)?;
                empty
            }
            Err(error) => return Err(error.into()),
        };
        Ok((registry, clients))
    }

    fn load(&self, file: File) -> crate::Result<HashMap<String, Client>> {
        private_file(&file)?;
        let mut text = String::new();
        file.take(MAX_BYTES + 1).read_to_string(&mut text)?;
        if text.len() as u64 > MAX_BYTES {
            return Err("OAuth registry exceeds size limit".into());
        }
        // Never return the parser diagnostic: it can include the secret verifier.
        let doc: Document = toml::from_str(&text).map_err(|_| "invalid OAuth registry TOML")?;
        if doc.version != 1 || doc.issuer != self.issuer || doc.clients.len() > MAX_CLIENTS {
            return Err("OAuth registry version, issuer, or capacity does not match".into());
        }
        let wall = unix_now()?;
        let now = Instant::now();
        let mut clients = HashMap::new();
        for (id, r) in doc.clients {
            let method = match r.method.as_str() {
                "none" if r.secret_hash.is_none() => AuthMethod::None,
                "client_secret_basic" if r.secret_hash.is_some() => AuthMethod::Basic,
                "client_secret_post" if r.secret_hash.is_some() => AuthMethod::Post,
                _ => return Err("invalid OAuth registry client authentication method".into()),
            };
            if !matches!(r.application_type.as_str(), "web" | "native")
                || !opaque(&id, "client_")
                || r.name.trim().is_empty()
                || r.name.len() > 200
                || r.name.chars().any(char::is_control)
                || r.redirects.is_empty()
                || r.redirects.len() > 8
                || r.redirects.iter().collect::<HashSet<_>>().len() != r.redirects.len()
                || r.redirects
                    .iter()
                    .any(|uri| !valid_redirect(uri, &r.application_type))
                || r.expires_unix.saturating_sub(wall) > MAX_LIFETIME
            {
                return Err("invalid OAuth registry client metadata".into());
            }
            if r.expires_unix <= wall {
                continue;
            }
            let expires = now
                .checked_add(Duration::from_secs(r.expires_unix - wall))
                .ok_or("OAuth registry deadline is out of range")?;
            clients.insert(
                id,
                Client {
                    name: r.name,
                    redirects: r.redirects,
                    application_type: r.application_type,
                    method,
                    secret_hash: r.secret_hash,
                    refresh: r.refresh,
                    expires,
                    expires_unix: r.expires_unix,
                },
            );
        }
        Ok(clients)
    }

    pub(super) fn save(&self, clients: &HashMap<String, Client>) -> crate::Result<()> {
        if clients.len() > MAX_CLIENTS {
            return Err("OAuth registry capacity exceeded".into());
        }
        // Validate an existing destination too. A symlink or permissions change
        // is an error, not a reason to silently replace administrator data.
        match open_at(&self.dir, &self.filename, libc::O_RDONLY | libc::O_NONBLOCK) {
            Ok(file) => private_file(&file)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let doc = Document {
            version: 1,
            issuer: self.issuer.clone(),
            clients: clients
                .iter()
                .map(|(id, c)| {
                    (
                        id.clone(),
                        Record {
                            name: c.name.clone(),
                            redirects: c.redirects.clone(),
                            application_type: c.application_type.clone(),
                            method: c.method.name().into(),
                            secret_hash: c.secret_hash,
                            refresh: c.refresh,
                            expires_unix: c.expires_unix,
                        },
                    )
                })
                .collect(),
        };
        let text =
            toml::to_string(&doc).map_err(|_| "could not serialize OAuth client registry")?;
        if text.len() as u64 > MAX_BYTES {
            return Err("OAuth registry exceeds size limit".into());
        }
        let nonce =
            super::random("tmp_").map_err(|_| "could not generate registry temporary filename")?;
        let temp = CString::new(format!(".{}.{nonce}", self.filename.to_string_lossy()))?;
        let result = (|| -> crate::Result<()> {
            let mut file = open_at(
                &self.dir,
                &temp,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            )?;
            private_file(&file)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            // SAFETY: both names and the held directory FD are valid. renameat
            // atomically publishes only a completely written same-directory file.
            if unsafe {
                libc::renameat(
                    self.dir.as_raw_fd(),
                    temp.as_ptr(),
                    self.dir.as_raw_fd(),
                    self.filename.as_ptr(),
                )
            } != 0
            {
                return Err(io::Error::last_os_error().into());
            }
            self.dir.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            // SAFETY: unlinkat is confined to our unpredictable temporary name.
            unsafe {
                libc::unlinkat(self.dir.as_raw_fd(), temp.as_ptr(), 0);
            }
        }
        result
    }
}
