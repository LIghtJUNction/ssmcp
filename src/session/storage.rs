//! Admission for a new saved definition while its caller holds manage.lock.
//! Existing definitions and active workers never pass through this check.
use super::{Result, executable, private_open, verify_directory};
use std::{fs, io, path::Path};

pub(super) const MAX_DIRECTORIES: usize = 1024;
pub(super) const MAX_EXECUTABLE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub(super) const BYTE_LIMIT_ERROR: &str = "saved-session executable storage exceeds the 4 GiB limit; remove an unused saved session before creating another; existing records were preserved";

pub(super) fn remaining_bytes(root: &Path) -> Result<u64> {
    let mut count = 0;
    let mut remaining = MAX_EXECUTABLE_BYTES;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // A process crash can leave a prepared image before its atomic rename.
        // Include that reserved storage, without deleting or following it.
        if !(name.starts_with("session-") || name.starts_with(".prepare-"))
            || !entry.file_type()?.is_dir()
            || entry.file_type()?.is_symlink()
        {
            continue;
        }
        count += 1;
        if count >= MAX_DIRECTORIES {
            return Err(format!("saved-session storage already contains {MAX_DIRECTORIES} definitions or interrupted preparations; remove an unused entry before creating another; existing records were preserved").into());
        }
        let directory = entry.path();
        verify_directory(&directory)?;
        let image = executable(&directory);
        match fs::symlink_metadata(&image) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        }
        // Validate the same private regular-file boundary as the rest of the
        // session store. Count logical bytes without hashing/reading the image.
        let image = private_open(&image, false, false, false)?;
        remaining = remaining
            .checked_sub(image.metadata()?.len())
            .ok_or(BYTE_LIMIT_ERROR)?;
    }
    Ok(remaining)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{
        Manifest, SessionSpec, load, lock, manager_id, path_text, prepare, random_suffix,
        session_directory, snapshot_executable, write_json,
    };
    use sha2::{Digest, Sha256};
    use std::{collections::BTreeMap, path::PathBuf};

    struct Temporary(PathBuf);
    impl Temporary {
        fn new() -> Self {
            let root = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("ssmcp-saved-quota-{}", random_suffix().unwrap()));
            crate::platform::private_directory(&root).unwrap();
            Self(root)
        }
        fn spec(&self, name: &str) -> SessionSpec {
            SessionSpec {
                name: name.into(),
                cwd: self.0.clone(),
                args: vec![
                    "--name".into(),
                    name.into(),
                    "--ssh".into(),
                    "ssmcp@example".into(),
                ],
                watchdog: false,
                reconnect_delay_secs: 5,
            }
        }
    }
    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn saved_quota_rejects_full_directory_count_without_creating_or_removing_state() {
        let temporary = Temporary::new();
        for index in 0..MAX_DIRECTORIES {
            let prefix = if index % 2 == 0 {
                "session-"
            } else {
                ".prepare-"
            };
            crate::platform::private_directory(&temporary.0.join(format!("{prefix}{index:064x}")))
                .unwrap();
        }
        let _guard = lock(&temporary.0.join("manage.lock"), false).unwrap();
        let before = fs::read_dir(&temporary.0).unwrap().count();
        let error = prepare(&temporary.0, temporary.spec("refused"), true)
            .err()
            .unwrap();
        assert!(error.to_string().contains("1024"), "{error}");
        assert_eq!(fs::read_dir(&temporary.0).unwrap().count(), before);
        assert!(!session_directory(&temporary.0, "refused").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn saved_quota_counts_interrupted_images_and_preserves_existing_over_budget() {
        use std::io::Write;
        use std::os::unix::fs::MetadataExt;
        let temporary = Temporary::new();
        let kept = session_directory(&temporary.0, "kept");
        crate::platform::private_directory(&kept).unwrap();
        let bytes = b"unchanged saved image";
        private_open(&executable(&kept), true, true, false)
            .unwrap()
            .write_all(bytes)
            .unwrap();
        let manifest = Manifest {
            format: super::super::FORMAT,
            supervised: true,
            manager_id: manager_id(&temporary.0, "kept"),
            spec: temporary.spec("kept"),
            executable_sha256: format!("{:x}", Sha256::digest(bytes)),
            version: "quota-fixture".into(),
            created_at: 0,
            environment: BTreeMap::from([(
                "SSMCP_SESSION_HOME".into(),
                path_text(&temporary.0).unwrap().into(),
            )]),
        };
        write_json(&kept.join("manifest.json"), &manifest).unwrap();
        let before = fs::read(kept.join("manifest.json")).unwrap();
        let _running = lock(&kept.join("run.lock"), true).unwrap();
        let interrupted = temporary.0.join(".prepare-interrupted");
        crate::platform::private_directory(&interrupted).unwrap();
        let image = private_open(&executable(&interrupted), true, true, false).unwrap();
        // Sparse metadata exercises the actual 4 GiB boundary without writing
        // or allocating a multi-gigabyte test body on the Linux filesystem.
        image.set_len(MAX_EXECUTABLE_BYTES).unwrap();
        assert!(image.metadata().unwrap().blocks() * 512 < 1024 * 1024);
        let _guard = lock(&temporary.0.join("manage.lock"), false).unwrap();
        let count = fs::read_dir(&temporary.0).unwrap().count();
        let error = prepare(&temporary.0, temporary.spec("refused"), true)
            .err()
            .unwrap();
        assert!(error.to_string().contains("4 GiB"), "{error}");
        assert_eq!(fs::read_dir(&temporary.0).unwrap().count(), count);
        assert_eq!(
            load(&temporary.0, "kept").unwrap().1.executable_sha256,
            manifest.executable_sha256
        );
        assert_eq!(fs::read(kept.join("manifest.json")).unwrap(), before);
        assert_eq!(fs::read(executable(&kept)).unwrap(), bytes);
        assert_eq!(image.metadata().unwrap().len(), MAX_EXECUTABLE_BYTES);
    }

    #[test]
    fn saved_quota_snapshot_accepts_exact_remaining_bytes_and_rejects_oversize_before_write() {
        let temporary = Temporary::new();
        let source = temporary.0.join("source");
        fs::write(&source, b"12345").unwrap();
        let exact = temporary.0.join("exact");
        assert_eq!(
            snapshot_executable(&source, &exact, 5).unwrap(),
            format!("{:x}", Sha256::digest(b"12345"))
        );
        assert_eq!(fs::read(&exact).unwrap(), b"12345");
        let refused = temporary.0.join("refused");
        assert!(
            snapshot_executable(&source, &refused, 4)
                .unwrap_err()
                .to_string()
                .contains("4 GiB")
        );
        assert!(!refused.exists());
    }

    #[test]
    fn saved_quota_management_lock_serializes_two_creators_at_the_last_slot() {
        let temporary = Temporary::new();
        for index in 0..MAX_DIRECTORIES - 1 {
            crate::platform::private_directory(&temporary.0.join(format!("session-{index:064x}")))
                .unwrap();
        }
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let workers: Vec<_> = ["one", "two"]
            .into_iter()
            .map(|name| {
                let root = temporary.0.clone();
                let spec = temporary.spec(name);
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let _guard = lock(&root.join("manage.lock"), false).unwrap();
                    prepare(&root, spec, true)
                        .map(|_| ())
                        .map_err(|error| error.to_string())
                })
            })
            .collect();
        barrier.wait();
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(
            results.iter().filter(|result| result.is_ok()).count(),
            1,
            "{results:?}"
        );
        assert!(
            results
                .iter()
                .filter_map(|result| result.as_ref().err())
                .all(|error| error.contains("1024")),
            "{results:?}"
        );
        assert_eq!(
            fs::read_dir(&temporary.0)
                .unwrap()
                .filter(|entry| entry.as_ref().unwrap().file_type().unwrap().is_dir())
                .count(),
            MAX_DIRECTORIES
        );
        assert_eq!(
            ["one", "two"]
                .iter()
                .filter(|name| session_directory(&temporary.0, name).exists())
                .count(),
            1
        );
    }
}
