//! Path-based native filesystem operations for macOS and Windows.
//! Linux retains its descriptor-relative implementation in the parent module.
use super::*;

impl Root {
    fn parts(path: &str) -> Result<Vec<std::ffi::OsString>, String> {
        if path.len() > 4096 || path.chars().any(char::is_control) {
            return Err("invalid workspace path".into());
        }
        Path::new(path)
            .components()
            .filter_map(|part| match part {
                Component::CurDir => None,
                Component::Normal(name) => {
                    #[cfg(windows)]
                    {
                        let name = name.to_string_lossy();
                        let stem = name
                            .split('.')
                            .next()
                            .unwrap_or_default()
                            .to_ascii_uppercase();
                        if name.contains(':')
                            || name.ends_with(['.', ' '])
                            || [
                                "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5",
                                "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4",
                                "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
                            ]
                            .contains(&stem.as_str())
                        {
                            return Some(Err("invalid Windows workspace path".into()));
                        }
                    }
                    Some(Ok(name.to_owned()))
                }
                _ => Some(Err("path must be relative without parent traversal".into())),
            })
            .collect()
    }

    fn directory_path(
        &self,
        parts: &[std::ffi::OsString],
        create: bool,
    ) -> Result<PathBuf, String> {
        let mut directory = self.path.clone();
        for part in parts {
            directory.push(part);
            if create && !directory.try_exists().map_err(error)? {
                match crate::platform::private_directory(&directory) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(error(e)),
                }
            }
            let metadata = std::fs::symlink_metadata(&directory).map_err(error)?;
            reject_link(&metadata)?;
            if !metadata.is_dir() {
                return Err("parent is not a directory".into());
            }
        }
        let resolved = directory.canonicalize().map_err(error)?;
        if !resolved.starts_with(&self.path) {
            return Err("path escaped workspace root".into());
        }
        Ok(resolved)
    }

    fn file_path(&self, path: &str) -> Result<PathBuf, String> {
        let parts = Self::parts(path)?;
        let Some((name, parents)) = parts.split_last() else {
            return Ok(self.path.clone());
        };
        let path = self.directory_path(parents, false)?.join(name);
        reject_link(&std::fs::symlink_metadata(&path).map_err(error)?)?;
        Ok(path)
    }

    pub(super) fn file(&self, path: &str) -> Result<File, String> {
        open_file(&self.file_path(path)?)
    }

    pub(super) fn stat(&self, args: PathArgs) -> Result<Value, String> {
        let metadata = std::fs::symlink_metadata(self.file_path(&args.path)?).map_err(error)?;
        #[cfg(unix)]
        let mode = metadata.mode() & 0o777;
        #[cfg(windows)]
        let mode = if metadata.permissions().readonly() {
            0o444
        } else {
            0o666
        };
        Ok(
            json!({"root":self.path,"path":args.path,"kind":kind(&metadata),"size":metadata.len(),"modified_ns":modified(&metadata),"mode":mode}),
        )
    }

    pub(super) fn list(&self, args: ListArgs) -> Result<Value, String> {
        if args.max_entries == 0 || args.max_entries > MAX_ENTRIES {
            return Err(format!("max_entries must be 1..={MAX_ENTRIES}"));
        }
        let directory = self.directory_path(&Self::parts(&args.path)?, false)?;
        let metadata = std::fs::metadata(&directory).map_err(error)?;
        let stamp = sha256(modified(&metadata).as_bytes());
        let offset = match args.cursor {
            None => 0,
            Some(cursor) => {
                let (version, index) = cursor.split_once(':').ok_or("invalid directory cursor")?;
                if version != stamp {
                    return Err("directory changed; restart without cursor".into());
                }
                index
                    .parse::<usize>()
                    .map_err(|_| "invalid directory cursor")?
            }
        };
        let mut entries = std::fs::read_dir(&directory).map_err(error)?;
        for _ in 0..offset {
            if entries.next().transpose().map_err(error)?.is_none() {
                return Err("directory cursor is beyond the entries".into());
            }
        }
        let mut page = Vec::new();
        for _ in 0..args.max_entries {
            let Some(entry) = entries.next().transpose().map_err(error)? else {
                break;
            };
            let metadata = std::fs::symlink_metadata(entry.path()).map_err(error)?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| "directory contains a non-UTF-8 filename")?;
            page.push(json!({"name":name,"kind":kind(&metadata),"size":metadata.len()}));
        }
        let end = offset + page.len();
        let more = entries.next().transpose().map_err(error)?.is_some();
        if !same_version(&metadata, &std::fs::metadata(&directory).map_err(error)?) {
            return Err("directory changed; restart without cursor".into());
        }
        let mut result = json!({"root":self.path,"path":args.path,"entries":page});
        if more {
            result["next_cursor"] = json!(format!("{stamp}:{end}"));
        }
        Ok(result)
    }

    pub(super) fn write_bytes(
        &self,
        path: &str,
        data: &[u8],
        overwrite: bool,
        create_parents: bool,
        expected_sha256: Option<&str>,
    ) -> Result<(), String> {
        validate_hash(expected_sha256)?;
        if expected_sha256.is_some() && !overwrite {
            return Err("expected_sha256 requires overwrite=true and an existing file".into());
        }
        let parts = Self::parts(path)?;
        let (name, parents) = parts.split_last().ok_or("write requires a file path")?;
        let parent = self.directory_path(parents, create_parents)?;
        let _lock = crate::platform::lock_directory(&parent)?;
        let target = parent.join(name);
        let permissions = match std::fs::symlink_metadata(&target) {
            Ok(metadata) => {
                reject_link(&metadata)?;
                if !metadata.is_file() {
                    return Err("path is not a regular file".into());
                }
                if !overwrite {
                    return Err("file already exists; set overwrite=true".into());
                }
                if let Some(expected) = expected_sha256 {
                    check_hash(expected, &sha256(&whole_file(&mut open_file(&target)?)?))?;
                }
                Some(metadata.permissions())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound && expected_sha256.is_none() => {
                None
            }
            Err(error) => return Err(error.to_string()),
        };
        replace_file(&target, data, overwrite, permissions)
    }

    pub(super) fn edit_text(&self, args: TextEditArgs) -> Result<Value, String> {
        validate_hash(args.expected_sha256.as_deref())?;
        if args.old_text.is_empty() {
            return Err("old_text must not be empty".into());
        }
        if args.old_text.len() > MAX_FILE_BYTES || args.new_text.len() > MAX_FILE_BYTES {
            return Err(format!("edit text exceeds {MAX_FILE_BYTES} UTF-8 bytes"));
        }
        let parts = Self::parts(&args.path)?;
        let (name, parents) = parts.split_last().ok_or("edit requires a file path")?;
        let parent = self.directory_path(parents, false)?;
        let _lock = crate::platform::lock_directory(&parent)?;
        let target = parent.join(name);
        let mut file = open_file(&target)?;
        let permissions = regular_metadata(&file)?.permissions();
        let bytes = whole_file(&mut file)?;
        let previous_sha256 = sha256(&bytes);
        if let Some(expected) = args.expected_sha256.as_deref() {
            check_hash(expected, &previous_sha256)?;
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| "file is not valid UTF-8; use binary fs.read instead")?;
        let index = text
            .find(&args.old_text)
            .ok_or("old_text was not found; file unchanged")?;
        let next = index + text[index..].chars().next().unwrap().len_utf8();
        if text[next..].contains(&args.old_text) {
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
        drop(file);
        replace_file(&target, replacement.as_bytes(), true, Some(permissions))?;
        Ok(
            json!({"path":args.path,"written":replacement.len(),"replacements":1,"sha256":sha256(replacement.as_bytes()),"previous_sha256":previous_sha256}),
        )
    }

    pub(super) fn mkdir(&self, args: MkdirArgs) -> Result<Value, String> {
        let parts = Self::parts(&args.path)?;
        if args.parents {
            self.directory_path(&parts, true)?;
        } else {
            let (name, parents) = parts
                .split_last()
                .ok_or("mkdir requires a directory path")?;
            crate::platform::private_directory(&self.directory_path(parents, false)?.join(name))
                .map_err(error)?;
        }
        Ok(json!({"path":args.path,"created":true}))
    }
}

fn reject_link(metadata: &std::fs::Metadata) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err("workspace reparse points are not allowed".into());
        }
    }
    if metadata.file_type().is_symlink() {
        return Err("workspace symlinks are not allowed".into());
    }
    Ok(())
}

fn open_file(path: &Path) -> Result<File, String> {
    reject_link(&std::fs::symlink_metadata(path).map_err(error)?)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path).map_err(error)?;
    reject_link(&file.metadata().map_err(error)?)?;
    regular_metadata(&file)?;
    Ok(file)
}

fn replace_file(
    path: &Path,
    data: &[u8],
    overwrite: bool,
    permissions: Option<std::fs::Permissions>,
) -> Result<(), String> {
    let parent = path.parent().ok_or("file has no parent directory")?;
    let temporary = parent.join(format!(".ssmcp-{}.tmp", random_id().map_err(error)?));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&temporary).map_err(error)?;
    struct Remove(PathBuf);
    impl Drop for Remove {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _remove = Remove(temporary.clone());
    file.write_all(data).map_err(error)?;
    if let Some(permissions) = permissions {
        file.set_permissions(permissions).map_err(error)?;
    }
    file.sync_all().map_err(error)?;
    drop(file);
    if overwrite {
        std::fs::rename(&temporary, path).map_err(error)?;
    } else {
        std::fs::hard_link(&temporary, path).map_err(error)?;
    }
    #[cfg(unix)]
    File::open(parent)
        .and_then(|file| file.sync_all())
        .map_err(error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn native_files_keep_text_cursors_preconditions_and_root_boundary() {
        let directory =
            std::env::temp_dir().join(format!("ssmcp-workspace-{}", random_id().unwrap()));
        std::fs::create_dir(&directory).unwrap();
        let workspace = Workspace::new(directory.clone()).unwrap();
        workspace
            .perform(
                "ssmcp.fs.write_text",
                json!({"path":"nested/text.txt","text":"甲乙\nhello","create_parents":true}),
            )
            .await
            .unwrap();
        let read = workspace
            .perform(
                "ssmcp.fs.read_text",
                json!({"path":"nested/text.txt","max_bytes":4}),
            )
            .await
            .unwrap();
        assert_eq!(read["text"], "甲");
        assert_eq!(read["next_offset"], 3);
        assert!(workspace.perform("ssmcp.fs.write_text", json!({"path":"nested/text.txt","text":"bad","overwrite":true,"expected_sha256":"0".repeat(64)})).await.is_err());
        workspace
            .perform(
                "ssmcp.fs.edit_text",
                json!({"path":"nested/text.txt","old_text":"hello","new_text":"world"}),
            )
            .await
            .unwrap();
        assert!(
            workspace
                .perform("ssmcp.fs.read_text", json!({"path":"../escape"}))
                .await
                .is_err()
        );
        assert!(
            std::fs::read_to_string(directory.join("nested/text.txt"))
                .unwrap()
                .ends_with("world")
        );
        workspace.shutdown().await;
        std::fs::remove_dir_all(directory).unwrap();
    }
}
