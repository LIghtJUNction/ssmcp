//! Directory-handle-based filesystem access for macOS and Windows.
//!
//! Every path component is opened without following links. An ambient path is
//! used only when the operator grants the root; later operations stay relative
//! to that capability. Hash-precondition writes cooperate within this process.
use super::*;
#[cfg(unix)]
use cap_fs_ext::OpenOptionsSyncExt;
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsMaybeDirExt};
use cap_std::fs::{Dir, OpenOptions as CapOptions};
use std::{
    ffi::{OsStr, OsString},
    sync::{MutexGuard, TryLockError},
};

static WRITES: StdMutex<()> = StdMutex::new(());
const MAX_DIRECTORY_ENTRIES: usize = 65_536;
const MAX_DIRECTORY_NAME_BYTES: usize = 8 * 1024 * 1024;

fn lock_writes() -> Result<MutexGuard<'static, ()>, String> {
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        match WRITES.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::Poisoned(_)) => return Err("workspace write lock is poisoned".into()),
            Err(TryLockError::WouldBlock) if Instant::now() >= deadline => {
                return Err("workspace is busy; retry the file operation".into());
            }
            Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

fn lock_managed_writes(
    root: &Path,
    relative: &str,
) -> Result<Option<crate::managed_files::WorkspaceWriteLease>, String> {
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        match crate::managed_files::try_workspace_write_lock(root, Path::new(relative)) {
            Ok(guard) => return Ok(guard),
            Err(failure) if failure.kind() == io::ErrorKind::Interrupted => continue,
            Err(failure) if failure.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(
                        "workspace cleanup or file write is busy; retry the file operation".into(),
                    );
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(failure) => return Err(error(failure)),
        }
    }
}

fn open_file(parent: &Dir, name: &OsStr) -> io::Result<File> {
    let mut options = CapOptions::new();
    options
        .read(true)
        .follow(FollowSymlinks::No)
        .maybe_dir(true);
    #[cfg(unix)]
    options.nonblock(true);
    parent.open_with(name, &options).map(|file| file.into_std())
}

impl Root {
    fn components(path: &str) -> Result<Vec<OsString>, String> {
        if path.len() > 4096 || path.chars().any(char::is_control) {
            return Err("invalid or oversized workspace path".into());
        }
        Path::new(path)
            .components()
            .filter_map(|component| match component {
                Component::CurDir => None,
                Component::Normal(part) => Some(validate_component(part).map(|_| part.to_owned())),
                _ => Some(Err("path must be relative without parent traversal".into())),
            })
            .collect()
    }

    fn directory(&self, parts: &[OsString], create: bool) -> Result<Dir, String> {
        let mut directory = self.directory.try_clone().map_err(error)?;
        for part in parts {
            let mut next = directory.open_dir_nofollow(part);
            if create
                && next
                    .as_ref()
                    .is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
            {
                if let Err(e) = directory.create_dir(part)
                    && e.kind() != io::ErrorKind::AlreadyExists
                {
                    return Err(error(e));
                }
                next = directory.open_dir_nofollow(part);
            }
            directory = next.map_err(error)?;
        }
        Ok(directory)
    }

    pub(super) fn file(&self, path: &str) -> Result<File, String> {
        let parts = Self::components(path)?;
        let Some((name, parents)) = parts.split_last() else {
            return self
                .directory
                .try_clone()
                .map(|dir| dir.into_std_file())
                .map_err(error);
        };
        open_file(&self.directory(parents, false)?, name).map_err(error)
    }

    pub(super) fn stat(&self, args: PathArgs) -> Result<Value, String> {
        let metadata = self.file(&args.path)?.metadata().map_err(error)?;
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::MetadataExt;
            Some(metadata.mode() & 0o777)
        };
        #[cfg(windows)]
        let mode: Option<u32> = None;
        Ok(
            json!({"root":self.path.to_string_lossy(),"path":args.path,"kind":kind(&metadata),
            "size":metadata.len(),"modified_ns":modified(&metadata),"mode":mode,
            "readonly":metadata.permissions().readonly()}),
        )
    }

    pub(super) fn list(&self, args: ListArgs) -> Result<Value, String> {
        if args.max_entries == 0 || args.max_entries > MAX_ENTRIES {
            return Err(format!("max_entries must be 1..={MAX_ENTRIES}"));
        }
        let directory = self.directory(&Self::components(&args.path)?, false)?;
        let identity = directory.try_clone().map_err(error)?.into_std_file();
        let before = identity.metadata().map_err(error)?;
        let mut entries = Vec::new();
        let mut name_bytes = 0usize;
        for entry in directory.entries().map_err(error)? {
            let entry = entry.map_err(error)?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| "directory contains a non-UTF-8 filename")?;
            name_bytes += name.len();
            if entries.len() >= MAX_DIRECTORY_ENTRIES || name_bytes > MAX_DIRECTORY_NAME_BYTES {
                return Err("directory exceeds the bounded portable listing limit".into());
            }
            let ty = entry.file_type().map_err(error)?;
            let kind = if ty.is_symlink() {
                "symlink"
            } else if ty.is_dir() {
                "directory"
            } else if ty.is_file() {
                "file"
            } else {
                "special"
            };
            let size = if ty.is_file() {
                entry.metadata().map_err(error)?.len()
            } else {
                0
            };
            entries.push(json!({"name":name,"kind":kind,"size":size}));
        }
        if !same_version(&before, &identity.metadata().map_err(error)?) {
            return Err("directory changed; restart without cursor".into());
        }
        entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        let stamp = sha256(&serde_json::to_vec(&entries).map_err(error)?);
        let offset = if let Some(cursor) = args.cursor {
            if cursor.len() > 128 {
                return Err("invalid directory cursor".into());
            }
            let (version, index) = cursor.split_once(':').ok_or("invalid directory cursor")?;
            if version != stamp {
                return Err("directory changed; restart without cursor".into());
            }
            index
                .parse::<usize>()
                .map_err(|_| "invalid directory cursor")?
        } else {
            0
        };
        if offset > entries.len() {
            return Err("directory cursor is beyond the entries".into());
        }
        let end = offset.saturating_add(args.max_entries).min(entries.len());
        let mut result = json!({"root":self.path.to_string_lossy(),"path":args.path,"entries":entries[offset..end]});
        if end < entries.len() {
            result["next_cursor"] = json!(format!("{stamp}:{end}"));
        }
        Ok(result)
    }

    pub(super) fn write_bytes(
        &self,
        tool: &str,
        path: &str,
        data: &[u8],
        overwrite: bool,
        create_parents: bool,
        temporary: bool,
        expected_sha256: Option<&str>,
    ) -> Result<Option<String>, String> {
        crate::managed_files::validate_user_write(Path::new(path)).map_err(error)?;
        validate_temporary_write(path, overwrite, temporary)?;
        validate_hash(expected_sha256)?;
        if expected_sha256.is_some() && !overwrite {
            return Err("expected_sha256 requires overwrite=true and an existing file".into());
        }
        let parts = Self::components(path)?;
        let (name, parents) = parts.split_last().ok_or("write requires a file path")?;
        let _guard = lock_writes()?;
        let parent = self.directory(parents, create_parents)?;
        let _managed_guard = lock_managed_writes(&self.path, path)?;
        let mut permissions = None;
        let mut before = audit::Snapshot::Missing;
        match open_file(&parent, name) {
            Ok(mut existing) => {
                let metadata = regular_metadata(&existing)?;
                if !overwrite {
                    return Err("file already exists; use overwrite=true".into());
                }
                permissions = Some(metadata.permissions());
                before = if let Some(expected) = expected_sha256 {
                    let bytes = whole_file(&mut existing)?;
                    check_hash(expected, &sha256(&bytes))?;
                    audit::Snapshot::from_bytes(path, &bytes)
                } else {
                    audit::Snapshot::from_file(path, &mut existing)
                };
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound && expected_sha256.is_none() => {}
            Err(e) => return Err(error(e)),
        }
        let created = matches!(&before, audit::Snapshot::Missing);
        if let Some(guard) = &_managed_guard {
            guard.check_current().map_err(error)?;
        }
        let identity = publish(&parent, name, data, overwrite, permissions)?;
        self.audit.written(tool, path, before, data);
        Ok(if created {
            self.creation_warning(path, &identity, tool, temporary)
        } else {
            None
        })
    }

    pub(super) fn edit_text(&self, args: TextEditArgs) -> Result<Value, String> {
        crate::managed_files::validate_user_write(Path::new(&args.path)).map_err(error)?;
        validate_hash(args.expected_sha256.as_deref())?;
        if args.old_text.is_empty() {
            return Err("old_text must not be empty".into());
        }
        if args.old_text.len() > MAX_FILE_BYTES || args.new_text.len() > MAX_FILE_BYTES {
            return Err(format!("edit text exceeds {MAX_FILE_BYTES} UTF-8 bytes"));
        }
        let parts = Self::components(&args.path)?;
        let (name, parents) = parts.split_last().ok_or("edit requires a file path")?;
        let _guard = lock_writes()?;
        let parent = self.directory(parents, false)?;
        let _managed_guard = lock_managed_writes(&self.path, &args.path)?;
        let mut existing = open_file(&parent, name).map_err(error)?;
        let permissions = regular_metadata(&existing)?.permissions();
        let bytes = whole_file(&mut existing)?;
        let previous_sha256 = sha256(&bytes);
        if let Some(expected) = args.expected_sha256.as_deref() {
            check_hash(expected, &previous_sha256)?;
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| "file is not valid UTF-8; use binary fs.read instead")?;
        let index = text
            .find(&args.old_text)
            .ok_or("old_text was not found; file unchanged")?;
        let next_start = index + text[index..].chars().next().unwrap().len_utf8();
        if text[next_start..].contains(&args.old_text) {
            return Err(
                "old_text matches more than once; provide a unique literal; file unchanged".into(),
            );
        }
        let length = text.len() - args.old_text.len() + args.new_text.len();
        if length > MAX_FILE_BYTES {
            return Err(format!(
                "edited file would exceed {MAX_FILE_BYTES} UTF-8 bytes"
            ));
        }
        let mut replacement = String::with_capacity(length);
        replacement.push_str(&text[..index]);
        replacement.push_str(&args.new_text);
        replacement.push_str(&text[index + args.old_text.len()..]);
        drop(existing);
        if let Some(guard) = &_managed_guard {
            guard.check_current().map_err(error)?;
        }
        publish(
            &parent,
            name,
            replacement.as_bytes(),
            true,
            Some(permissions),
        )?;
        self.audit.written(
            "ssmcp.fs.edit_text",
            &args.path,
            audit::Snapshot::from_bytes(&args.path, &bytes),
            replacement.as_bytes(),
        );
        Ok(
            json!({"path":args.path,"written":replacement.len(),"replacements":1,
            "sha256":sha256(replacement.as_bytes()),"previous_sha256":previous_sha256}),
        )
    }

    pub(super) fn mkdir(&self, args: MkdirArgs) -> Result<Value, String> {
        let parts = Self::components(&args.path)?;
        if args.parents {
            self.directory(&parts, true)?;
        } else {
            let (name, parents) = parts
                .split_last()
                .ok_or("mkdir requires a directory path")?;
            self.directory(parents, false)?
                .create_dir(name)
                .map_err(error)?;
        }
        Ok(json!({"path":args.path,"created":true}))
    }
}

fn validate_component(part: &OsStr) -> Result<(), String> {
    #[cfg(windows)]
    {
        let text = part.to_str().ok_or("invalid workspace path")?;
        if text.contains([':', '<', '>', '"', '|', '?', '*'])
            || text.ends_with(['.', ' '])
            || text.chars().any(|c| c < ' ')
        {
            return Err(
                "Windows stream names, aliases and reserved path characters are not allowed".into(),
            );
        }
        let base = text.split('.').next().unwrap().to_ascii_uppercase();
        if matches!(
            base.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) || ["COM", "LPT"].iter().any(|prefix| {
            base.strip_prefix(prefix).is_some_and(|n| {
                matches!(
                    n,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        }) {
            return Err("Windows device names are not workspace files".into());
        }
    }
    #[cfg(not(windows))]
    let _ = part;
    Ok(())
}

fn publish(
    parent: &Dir,
    name: &OsStr,
    data: &[u8],
    overwrite: bool,
    permissions: Option<std::fs::Permissions>,
) -> Result<crate::managed_files::FileIdentity, String> {
    let temporary = format!(".ssmcp-{}.tmp", random_id().map_err(error)?);
    let mut options = CapOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    let mut file = parent
        .open_with(&temporary, &options)
        .map_err(error)?
        .into_std();
    struct Temp<'a> {
        parent: &'a Dir,
        name: &'a str,
    }
    impl Drop for Temp<'_> {
        fn drop(&mut self) {
            let _ = self.parent.remove_file(self.name);
        }
    }
    let _temporary = Temp {
        parent,
        name: &temporary,
    };
    file.write_all(data).map_err(error)?;
    if let Some(permissions) = permissions {
        file.set_permissions(permissions).map_err(error)?;
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(error)?;
        }
    }
    file.sync_all().map_err(error)?;
    let identity = crate::managed_files::FileIdentity::from_file(&file).map_err(error)?;
    drop(file);
    if overwrite {
        parent.rename(&temporary, parent, name).map_err(error)?;
    } else {
        parent.hard_link(&temporary, parent, name).map_err(error)?;
    }
    #[cfg(unix)]
    parent
        .try_clone()
        .map_err(error)?
        .into_std_file()
        .sync_all()
        .map_err(error)?;
    Ok(identity)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("ssmcp-portable-{}", random_id().unwrap()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn workspace(&self) -> Workspace {
            Workspace::new(self.0.clone()).unwrap()
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn atomic_text_edits_and_hash_preconditions_work_on_native_files() {
        let scratch = Scratch::new();
        let workspace = scratch.workspace();
        let created = workspace
            .perform(
                "ssmcp.fs.write_text",
                json!({"path":"nested/file","text":"alpha 世界\r\n","create_parents":true}),
            )
            .await
            .unwrap();
        let edited = workspace.perform("ssmcp.fs.edit_text", json!({"path":"nested/file","old_text":"alpha","new_text":"beta","expected_sha256":created["sha256"]})).await.unwrap();
        assert_eq!(edited["previous_sha256"], created["sha256"]);
        assert!(workspace.perform("ssmcp.fs.write_text", json!({"path":"nested/file","text":"stale","overwrite":true,"expected_sha256":created["sha256"]})).await.is_err());
        let read = workspace
            .perform("ssmcp.fs.read_text", json!({"path":"nested/file"}))
            .await
            .unwrap();
        assert_eq!(read["text"], "beta 世界\r\n");
        assert_eq!(read["sha256"], edited["sha256"]);
        assert_eq!(
            std::fs::read_to_string(scratch.0.join("nested/file")).unwrap(),
            "beta 世界\r\n"
        );
    }

    #[tokio::test]
    async fn traversal_and_symlink_components_are_rejected() {
        let scratch = Scratch::new();
        let outside = Scratch::new();
        let workspace = scratch.workspace();
        std::fs::write(outside.0.join("secret"), "outside").unwrap();
        for path in ["../secret", "/secret", "nested/../../secret"] {
            assert!(
                workspace
                    .perform("ssmcp.fs.read_text", json!({"path":path}))
                    .await
                    .is_err()
            );
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside.0, scratch.0.join("escape")).unwrap();
            std::os::unix::fs::symlink(outside.0.join("secret"), scratch.0.join("link")).unwrap();
            for path in ["escape/secret", "link"] {
                assert!(
                    workspace
                        .perform("ssmcp.fs.read_text", json!({"path":path}))
                        .await
                        .is_err()
                );
                assert!(
                    workspace
                        .perform(
                            "ssmcp.fs.write_text",
                            json!({"path":path,"text":"changed","overwrite":true})
                        )
                        .await
                        .is_err()
                );
            }
        }
        assert_eq!(
            std::fs::read_to_string(outside.0.join("secret")).unwrap(),
            "outside"
        );
    }

    #[tokio::test]
    async fn directory_pagination_is_stable_and_detects_changes() {
        let scratch = Scratch::new();
        let workspace = scratch.workspace();
        for name in ["one", "two", "three"] {
            std::fs::write(scratch.0.join(name), name).unwrap();
        }
        let first = workspace
            .perform("ssmcp.fs.list", json!({"max_entries":1}))
            .await
            .unwrap();
        let second = workspace
            .perform(
                "ssmcp.fs.list",
                json!({"max_entries":1,"cursor":first["next_cursor"]}),
            )
            .await
            .unwrap();
        assert_ne!(first["entries"][0]["name"], second["entries"][0]["name"]);
        std::fs::write(scratch.0.join("four"), "new").unwrap();
        assert!(
            workspace
                .perform("ssmcp.fs.list", json!({"cursor":first["next_cursor"]}))
                .await
                .is_err()
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_device_stream_and_path_aliases_are_rejected() {
        for name in [
            "NUL",
            "CON.txt",
            "LPT1",
            "COM¹",
            "file:stream",
            "trailing.",
            "space ",
            "C:relative",
        ] {
            assert!(Root::components(name).is_err(), "accepted {name}");
        }
        assert!(Root::components("folder/file.txt").is_ok());
    }
}
