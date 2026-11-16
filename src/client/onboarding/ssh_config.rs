//! Idempotent, atomic OpenSSH configuration owned by interactive setup.
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub(super) struct Saved {
    pub path: PathBuf,
    pub alias: String,
    pub backup: Option<PathBuf>,
    pub changed: bool,
}

pub(super) fn home() -> Result<PathBuf, String> {
    let value = if cfg!(windows) {
        std::env::var_os("USERPROFILE")
    } else {
        std::env::var_os("HOME")
    };
    value
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| "Cannot find an absolute user home directory.".into())
}

pub(super) fn ensure_private_directory(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                // SAFETY: geteuid has no preconditions.
                if m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o022 != 0 {
                    return Err(format!(
                        "Directory is not safely owned by this user: {}",
                        path.display()
                    ));
                }
            }
            Ok(())
        }
        Ok(_) => Err(format!(
            "Expected an owned directory, not a link or file: {}",
            path.display()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            crate::platform::private_directory(path).map_err(|e| format!("{}: {e}", path.display()))
        }
        Err(e) => Err(e.to_string()),
    }
}

fn private_open(path: &Path) -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let _ = path;
    options
}

fn read_existing(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::symlink_metadata(path) {
        Ok(m) => {
            if !m.is_file() || m.file_type().is_symlink() || m.len() > 1024 * 1024 {
                return Err("SSH config must be a regular file of at most 1 MiB; a linked config is left unchanged.".into());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                // SAFETY: geteuid has no preconditions.
                if m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o022 != 0 || m.nlink() != 1
                {
                    return Err(
                        "SSH config must be owned by this user and not writable by other users."
                            .into(),
                    );
                }
            }
            let mut file = fs::File::open(path).map_err(|e| e.to_string())?;
            let mut data = Vec::new();
            std::io::Read::by_ref(&mut file)
                .take(1024 * 1024 + 1)
                .read_to_end(&mut data)
                .map_err(|e| e.to_string())?;
            if data.len() > 1024 * 1024 {
                return Err("SSH config grew beyond 1 MiB.".into());
            }
            Ok(Some(data))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

fn quote(value: &str) -> Result<String, String> {
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(
            "SSH configuration values cannot be empty or contain control characters.".into(),
        );
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

pub(super) fn alias(host: &str, port: u16) -> String {
    let stem: String = host
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(32)
        .collect();
    let hash = Sha256::digest(format!("{host}:{port}").as_bytes());
    format!(
        "ssmcp-{}-{:02x}{:02x}{:02x}",
        if stem.is_empty() { "server" } else { &stem },
        hash[0],
        hash[1],
        hash[2]
    )
}

fn merge(existing: &str, alias: &str, block: &str) -> Result<String, String> {
    let start = format!("# >>> ssmcp managed {alias}");
    let end = format!("# <<< ssmcp managed {alias}");
    let mut offset = 0;
    let mut starts = Vec::new();
    let mut ends = Vec::new();
    for line in existing.split_inclusive('\n') {
        let label = line.trim_end_matches(['\r', '\n']);
        if label == start {
            starts.push(offset);
        }
        if label == end {
            ends.push(offset + line.len());
        }
        offset += line.len();
    }
    let rest = match (starts.as_slice(), ends.as_slice()) {
        ([], []) => existing.to_owned(),
        ([a], [b]) if a < b => format!("{}{}", &existing[..*a], &existing[*b..]),
        _ => return Err("The managed SSH block has duplicate or incomplete markers; the file was left unchanged.".into()),
    };
    Ok(format!("{block}{rest}"))
}

pub(super) fn save(
    path: &Path,
    host: &str,
    port: u16,
    user: &str,
    identity: &Path,
) -> Result<Saved, String> {
    if port == 0
        || user.is_empty()
        || user.len() > 64
        || !user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    {
        return Err("Invalid account SSH username or port.".into());
    }
    if host.is_empty()
        || host.len() > 253
        || host.starts_with('-')
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".:-_".contains(&b))
    {
        return Err("Invalid SSH hostname.".into());
    }
    let parent = path.parent().ok_or("SSH config has no parent directory")?;
    ensure_private_directory(parent)?;
    let lock_path = parent.join(".ssmcp-setup.lock");
    if let Ok(m) = fs::symlink_metadata(&lock_path)
        && (!m.is_file() || m.file_type().is_symlink())
    {
        return Err("SSH setup lock is not a regular file.".into());
    }
    let mut lock_options = OpenOptions::new();
    lock_options
        .read(true)
        .write(true)
        .create(true)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        lock_options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let lock = lock_options.open(&lock_path).map_err(|e| e.to_string())?;
    crate::platform::lock_file(&lock, true)
        .map_err(|_| "Another SSH setup is editing this config; retry after it finishes.")?;
    #[cfg(unix)]
    let _lock = ConfigLock(lock);
    let original = read_existing(path)?;
    let original_text = std::str::from_utf8(original.as_deref().unwrap_or_default())
        .map_err(|_| "SSH config is not UTF-8; it was left unchanged.")?;
    let identity = identity.to_str().ok_or("Identity path is not UTF-8")?;
    let identity = if cfg!(windows) {
        identity.replace('\\', "/")
    } else {
        identity.to_owned()
    };
    let identity = identity.replace('%', "%%");
    let host_alias = alias(host, port);
    let block = format!(
        "# >>> ssmcp managed {host_alias}\nHost {host_alias}\n    HostName {}\n    User {}\n    Port {port}\n    IdentityFile {}\n    IdentitiesOnly yes\n    PreferredAuthentications publickey\n    ConnectTimeout 8\n    ConnectionAttempts 1\nHost *\n# <<< ssmcp managed {host_alias}\n",
        quote(host)?,
        quote(user)?,
        quote(&identity)?
    );
    let merged = merge(original_text, &host_alias, &block)?;
    if original.as_deref() == Some(merged.as_bytes()) {
        return Ok(Saved {
            path: path.into(),
            alias: host_alias,
            backup: None,
            changed: false,
        });
    }
    let mut random = [0; 8];
    crate::platform::random_bytes(&mut random).map_err(|e| e.to_string())?;
    let nonce = u64::from_ne_bytes(random);
    let temp = parent.join(format!(".ssmcp-config-{nonce:x}.tmp"));
    let mut pending = PendingFile(temp.clone());
    let mut file = private_open(&temp).open(&temp).map_err(|e| e.to_string())?;
    file.write_all(merged.as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())?;
    drop(file);
    if read_existing(path)? != original {
        return Err(
            "SSH config changed during setup; the new edits were preserved. Retry setup.".into(),
        );
    }
    let backup = if let Some(bytes) = original.as_ref() {
        let backup = parent.join(format!("config.ssmcp-backup-{nonce:x}"));
        let mut f = private_open(&backup)
            .open(&backup)
            .map_err(|e| e.to_string())?;
        f.write_all(bytes)
            .and_then(|_| f.sync_all())
            .map_err(|e| e.to_string())?;
        Some(backup)
    } else {
        None
    };
    fs::rename(&temp, path).map_err(|e| format!("Could not atomically save SSH config: {e}"))?;
    pending.0.clear();
    Ok(Saved {
        path: path.into(),
        alias: host_alias,
        backup,
        changed: true,
    })
}

#[cfg(unix)]
struct ConfigLock(fs::File);

#[cfg(unix)]
impl Drop for ConfigLock {
    fn drop(&mut self) {
        // An unrelated fork may retain this open-file-description until exec.
        // End the critical section now rather than waiting for every copy to close.
        let _ = self.0.unlock();
    }
}

struct PendingFile(PathBuf);
impl Drop for PendingFile {
    fn drop(&mut self) {
        if !self.0.as_os_str().is_empty() {
            let _ = fs::remove_file(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn managed_block_precedes_user_wildcards_and_replacement_is_idempotent() {
        let old = "Host *\n  User original\nHost project\n  HostName private.example\n";
        let alias = "ssmcp-demo";
        let block = "# >>> ssmcp managed ssmcp-demo\nHost ssmcp-demo\n User account\nHost *\n# <<< ssmcp managed ssmcp-demo\n";
        let once = merge(old, alias, block).unwrap();
        assert!(once.starts_with(block));
        assert!(once.ends_with(old));
        assert_eq!(merge(&once, alias, block).unwrap(), once);
    }
    #[test]
    fn replacement_preserves_crlf_and_missing_final_newline_exactly() {
        let block = "# >>> ssmcp managed demo\nHost demo\nHost *\n# <<< ssmcp managed demo\n";
        for old in [
            "Host existing\r\n  User original\r\n",
            "Host existing\n  User original",
            "Host existing\r\n  User original",
        ] {
            let once = merge(old, "demo", block).unwrap();
            assert!(once.ends_with(old));
            assert_eq!(merge(&once, "demo", block).unwrap(), once);
        }
    }
    #[test]
    fn duplicate_and_unclosed_managed_markers_do_not_overwrite_user_text() {
        for old in [
            "# >>> ssmcp managed a\n",
            "# <<< ssmcp managed a\n",
            "# >>> ssmcp managed a\n# >>> ssmcp managed a\n# <<< ssmcp managed a\n",
        ] {
            assert!(merge(old, "a", "").is_err());
        }
    }
    #[test]
    fn quoting_rejects_newline_injection_and_preserves_spaces() {
        assert!(quote("key\nProxyCommand injected").is_err());
        assert_eq!(quote("a b\"c\\d").unwrap(), "\"a b\\\"c\\\\d\"");
        assert_ne!(alias("a.example", 22), alias("a.example", 2222));
        assert!(
            alias("::1", 22)
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        );
    }
    #[test]
    fn save_preserves_unmanaged_configuration_and_does_not_duplicate_block() {
        let root = std::env::temp_dir().join(format!(
            "ssmcp-config-{}-{}",
            std::process::id(),
            crate::platform::hostname().unwrap()
        ));
        let _ = fs::remove_dir_all(&root);
        crate::platform::private_directory(&root).unwrap();
        let path = root.join("config");
        let old = b"Host existing\n  HostName example.net\n";
        fs::write(&path, old).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let key = root.join("id with space");
        let first = save(&path, "server.example", 2222, "ssmcp-gh-42", &key).unwrap();
        assert!(first.changed);
        assert_eq!(fs::read(first.backup.unwrap()).unwrap(), old);
        let second = save(&path, "server.example", 2222, "ssmcp-gh-42", &key).unwrap();
        assert!(!second.changed);
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .ends_with(std::str::from_utf8(old).unwrap())
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn setup_lock_stays_exclusive_then_releases_with_a_live_duplicate() {
        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let mut random = [0_u8; 16];
        crate::platform::random_bytes(&mut random).unwrap();
        let nonce: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let root = std::env::temp_dir().join(format!("ssmcp-config-lock-{nonce}"));
        crate::platform::private_directory(&root).unwrap();
        let fixture = Fixture(root);
        let path = fixture.0.join("config");
        let old = b"Host existing\n  HostName example.net\n";
        private_open(&path)
            .open(&path)
            .unwrap()
            .write_all(old)
            .unwrap();
        let lock_path = fixture.0.join(".ssmcp-setup.lock");
        let lock = private_open(&lock_path).open(&lock_path).unwrap();
        crate::platform::lock_file(&lock, true).unwrap();
        let guard = ConfigLock(lock);
        // A duplicate keeps the same open-file-description alive, as an
        // unrelated fork does before exec closes its CLOEXEC descriptors.
        let inherited = guard.0.try_clone().unwrap();
        let key = fixture.0.join("key");
        let blocked = save(&path, "server.example", 22, "account", &key)
            .err()
            .expect("a live setup guard must retain exclusive ownership");
        assert!(blocked.contains("Another SSH setup is editing this config"));
        assert_eq!(fs::read(&path).unwrap(), old);
        drop(guard);
        assert!(inherited.metadata().unwrap().is_file());
        assert!(
            save(&path, "server.example", 22, "account", &key)
                .unwrap()
                .changed
        );
        assert!(
            !save(&path, "server.example", 22, "account", &key)
                .unwrap()
                .changed
        );
        assert!(fs::read(&path).unwrap().ends_with(old));
        drop(inherited);
    }

    #[cfg(unix)]
    #[test]
    fn linked_config_is_never_replaced() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("ssmcp-config-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        crate::platform::private_directory(&root).unwrap();
        fs::write(root.join("real"), "unchanged").unwrap();
        symlink("real", root.join("config")).unwrap();
        assert!(
            save(
                &root.join("config"),
                "server",
                22,
                "user",
                &root.join("key")
            )
            .is_err()
        );
        assert_eq!(fs::read_to_string(root.join("real")).unwrap(), "unchanged");
        fs::remove_dir_all(root).unwrap();
    }
}
