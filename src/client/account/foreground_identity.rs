//! Local proof of foreground project continuity. The record selects a signing
//! key only. Every connection still goes through normal relay authorization.
use super::{create_tree, read_private_limit, verify_directory, write_private_limit};
use crate::{Result, wire::device_address};
use ring::signature::{Ed25519KeyPair, KeyPair};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::Mutex,
};

const AMBIGUOUS: &[u8] = b"multiple foreground sessions\n";

pub(crate) struct Identity {
    pub(crate) signing_key: Ed25519KeyPair,
    previous: Previous,
}

enum Previous {
    Project { record: PathBuf, _lease: File },
    // A window has a new identity and retains only its own relay reconnects.
    Frontend(Mutex<Option<String>>),
}

impl Identity {
    /// Path and ACL choose the private record. Only possession of its key
    /// can answer a fresh challenge, and the relay still enforces its ACL.
    pub(in crate::client) fn for_client(
        options: &super::super::Options,
        cwd: &Path,
    ) -> Result<Option<Self>> {
        if options.is_local()
            || options.daemon
            || options.list_tools
            || options.temporary
            || options.allow.is_empty()
        {
            return Ok(None);
        }
        let Some(origin) = options.server.as_deref() else {
            return Ok(None);
        };
        if options.frontend_owned {
            return Self::for_frontend().map(Some);
        }
        let mut allow = options.allow.clone();
        allow.sort_unstable();
        let mut roots = options
            .context_roots
            .iter()
            .map(|p| p.canonicalize())
            .collect::<std::io::Result<Vec<_>>>()?;
        roots.sort_unstable();
        roots.dedup();
        let roots = roots
            .iter()
            .map(|p| p.as_os_str().as_encoded_bytes())
            .collect::<Vec<_>>();
        let cwd = cwd.canonicalize()?;
        let mut directory = std::fs::OpenOptions::new();
        directory.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            directory
                .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS);
        }
        let object = crate::managed_files::ObjectIdentity::from_file(&directory.open(&cwd)?)?;
        let role = if options.worker.is_some() || options.worker_window_child {
            "worker"
        } else {
            "control"
        };
        let scope = serde_json::to_vec(&(
            "foreground-project-v1",
            origin,
            object,
            allow,
            cwd.as_os_str().as_encoded_bytes(),
            roots,
            role,
        ))?;
        Self::open(&super::state_root()?, &scope).map(Some)
    }

    fn for_frontend() -> Result<Self> {
        let doc = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
            .map_err(|_| "could not create frontend identity")?;
        Ok(Self {
            signing_key: Ed25519KeyPair::from_pkcs8(doc.as_ref())
                .map_err(|_| "invalid frontend identity")?,
            previous: Previous::Frontend(Mutex::new(None)),
        })
    }

    // The old and the new connection IDs are hints until the relay verifies
    // the signature and ACL. No command, grant or claim is stored here.
    pub(crate) fn replace_previous(&self, current: Option<&str>) -> Result<Option<String>> {
        if let Some(id) = current {
            device_address::decode::<32>(id)?;
        }
        let record = match &self.previous {
            Previous::Frontend(previous) => {
                let mut previous = previous.lock().unwrap_or_else(|e| e.into_inner());
                return Ok(std::mem::replace(
                    &mut *previous,
                    current.map(str::to_owned),
                ));
            }
            Previous::Project { record, .. } => record,
        };
        let _gate = crate::session::lock(&record.with_extension("lock"), false)?;
        let bytes = read_private_limit(record, 1024)?;
        let Some(stored) = bytes.as_deref().filter(|b| *b != AMBIGUOUS) else {
            return Ok(None);
        };
        let stored_key =
            Ed25519KeyPair::from_pkcs8(stored).map_err(|_| "invalid foreground identity")?;
        if stored_key.public_key().as_ref() != self.signing_key.public_key().as_ref() {
            return Ok(None);
        }
        let path = record.with_extension("previous");
        let public = device_address::encode(self.signing_key.public_key().as_ref());
        let prior = read_private_limit(&path, 512)?;
        if current.is_none() && prior.is_none() {
            return Ok(None);
        }
        let previous: Option<(String, String)> = prior
            .as_deref()
            .map(serde_json::from_slice)
            .transpose()?
            .flatten();
        let next = current.map(|id| (&public, id));
        write_private_limit(&path, &serde_json::to_vec(&next)?, 512)?;
        Ok(previous
            .filter(|(key, id)| key == &public && device_address::decode::<32>(id).is_ok())
            .map(|(_, id)| id))
    }

    fn open(root: &Path, scope: &[u8]) -> Result<Self> {
        create_tree(root)?;
        verify_directory(root)?;
        let root = root.join("foreground-identities");
        create_tree(&root)?;
        verify_directory(&root)?;
        let bucket = device_address::encode(&Sha256::digest(scope));
        // Serialize the census and key write. The shared lease on a different
        // file then witnesses process lifetime, including abnormal termination.
        let _gate = crate::session::lock(&root.join(format!("{bucket}.lock")), false)?;
        let lease = crate::session::lease_file(&root.join(format!("{bucket}.live")))?;
        let alone = match lease.try_lock() {
            Ok(()) => {
                lease.unlock()?;
                true
            }
            Err(std::fs::TryLockError::WouldBlock) => false,
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        };
        lease.lock_shared()?;
        let path = root.join(format!("{bucket}.key"));
        let previous = read_private_limit(&path, 1024)?;
        let signing_key = if alone
            && let Some(bytes) = previous.as_deref().filter(|b| *b != AMBIGUOUS)
        {
            Ed25519KeyPair::from_pkcs8(bytes).map_err(
                |_| "invalid foreground identity; inspect the private record before continuing",
            )?
        } else {
            let doc = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
                .map_err(|_| "could not create foreground identity")?;
            if alone {
                write_private_limit(&path, doc.as_ref(), 1024)?;
            } else if previous.as_deref() != Some(AMBIGUOUS) {
                // After overlap there is no unique predecessor, even once both
                // owners exit. The first later solo launch starts a fresh line.
                write_private_limit(&path, AMBIGUOUS, 1024)?;
            }
            Ed25519KeyPair::from_pkcs8(doc.as_ref()).map_err(|_| "invalid new foreground key")?
        };
        Ok(Self {
            signing_key,
            previous: Previous::Project {
                record: path,
                _lease: lease,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::KeyPair;
    use std::{fs, process::Command};

    #[test]
    fn frontend_reconnect_proofs_are_private_to_the_live_owner() {
        let one = Identity::for_frontend().unwrap();
        let two = Identity::for_frontend().unwrap();
        assert_ne!(
            one.signing_key.public_key().as_ref(),
            two.signing_key.public_key().as_ref()
        );
        let a = device_address::encode(&[1; 32]);
        let b = device_address::encode(&[2; 32]);
        assert_eq!(one.replace_previous(Some(&a)).unwrap(), None);
        assert_eq!(two.replace_previous(Some(&b)).unwrap(), None);
        assert_eq!(one.replace_previous(Some(&b)).unwrap(), Some(a));
        assert_eq!(two.replace_previous(None).unwrap(), Some(b));
        assert_eq!(two.replace_previous(None).unwrap(), None);
    }

    #[test]
    fn ordinary_foreground_restart_preserves_only_a_unique_project_identity() {
        const CHILD: &str = "SSMCP_TEST_FOREGROUND_PROJECT_CHILD";
        const TEST: &str = "client::account::foreground_identity::tests::ordinary_foreground_restart_preserves_only_a_unique_project_identity";
        if let Some(label) = std::env::var_os(CHILD) {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let args = if label == "allow" {
                vec![
                    "--allow".into(),
                    "alice@relay.example".into(),
                    "--server".into(),
                    "https://relay.example".into(),
                ]
            } else if label == "other" {
                vec!["bob@relay.example".into()]
            } else {
                vec!["alice@relay.example".into()]
            };
            if label == "allow" {
                assert!(
                    crate::client::Options::parse_new(args)
                        .err()
                        .unwrap()
                        .contains("--allow was removed")
                );
                fs::write(super::super::state_root().unwrap().join(&label), "rejected").unwrap();
                return;
            }
            let mut options = crate::client::Options::parse_new(args).unwrap().unwrap();
            rt.block_on(crate::client::apply_account(&mut options))
                .unwrap();
            let id = Identity::for_client(&options, Path::new("."))
                .unwrap()
                .unwrap();
            let public = device_address::encode(id.signing_key.public_key().as_ref());
            let current = device_address::encode(&Sha256::digest(label.as_encoded_bytes()));
            let previous = id.replace_previous(Some(&current)).unwrap();
            let proof = crate::wire::project_continuity::Proof::sign(
                &id.signing_key,
                options.server.as_deref().unwrap(),
                &current,
                previous.as_deref(),
            )
            .unwrap();
            assert!(proof.verifies(options.server.as_deref().unwrap(), &current));
            let expected = match label.to_str().unwrap() {
                "again" => Some("first"),
                "hold" => Some("again"),
                "next" => Some("after"),
                _ => None,
            }
            .map(|v| device_address::encode(&Sha256::digest(v.as_bytes())));
            assert_eq!(proof.previous, expected);
            if label == "next" {
                id.replace_previous(None).unwrap();
            }
            let root = super::super::state_root().unwrap();
            super::super::write_private(&root.join(&label), public.as_bytes()).unwrap();
            if label == "hold" {
                use std::io::Read;
                std::io::stdin().read_exact(&mut [0]).unwrap();
            }
            return;
        }

        let mut nonce = [0; 12];
        crate::platform::random_bytes(&mut nonce).unwrap();
        let root = std::env::temp_dir().join(format!(
            "ssmcp-foreground-{}",
            device_address::encode(&nonce)
        ));
        crate::platform::private_directory(&root).unwrap();
        let project = root.join("project");
        fs::create_dir(&project).unwrap();
        let auth = root.join("auth");
        crate::platform::private_directory(&auth).unwrap();
        super::super::write_private(
            &auth.join("last-connection.json"),
            &serde_json::to_vec(&super::super::LastConnection::Relay {
                username: "alice".into(),
                server: "https://relay.example".into(),
            })
            .unwrap(),
        )
        .unwrap();
        let command = |label: &str| {
            let mut cmd = Command::new(std::env::current_exe().unwrap());
            cmd.args(["--exact", TEST, "--nocapture"])
                .current_dir(&project)
                .env(CHILD, label)
                .env("SSMCP_AUTH_HOME", &auth)
                .env_remove("SSMCP_CONTEXT");
            cmd
        };
        let run = |label: &str| {
            let output = command(label).output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            fs::read_to_string(auth.join(label)).unwrap()
        };
        let first = run("first");
        // A saved controller credential for a different account on the same
        // relay must never be borrowed by the normal controlled-device path.
        super::super::save_at(
            &auth,
            &super::super::Credential {
                server: "https://relay.example".into(),
                token: "x".repeat(40),
                principal: "other@github".into(),
                subject: Some("42".into()),
                username: Some("other".into()),
                expires_at: u64::MAX,
            },
        )
        .unwrap();
        assert_eq!(first, run("again"));
        assert_eq!(run("allow"), "rejected");
        assert_ne!(first, run("other"));
        let mut holding = command("hold")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !auth.join("hold").exists() {
            assert!(holding.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(fs::read_to_string(auth.join("hold")).unwrap(), first);
        let parallel = run("overlap");
        assert_ne!(first, parallel);
        use std::io::Write;
        holding.stdin.take().unwrap().write_all(b"x").unwrap();
        assert!(holding.wait().unwrap().success());
        let clean = run("after");
        assert_ne!(clean, first);
        assert_ne!(clean, parallel);
        assert_eq!(clean, run("next"));
        assert_eq!(clean, run("after-stop"));
        fs::rename(&project, root.join("prior-directory")).unwrap();
        fs::create_dir(&project).unwrap();
        assert_ne!(clean, run("new-directory"));
        fs::remove_dir_all(root).unwrap();
    }
}
