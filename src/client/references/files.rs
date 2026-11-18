//! Explicit file pastes become private, bounded workspace references, never commands.
//! An unaccepted import owns its copies and rolls them back; source files are untouched.

use std::path::{Path, PathBuf};

pub(crate) const MAX_FILES: usize = 8;
#[cfg(unix)]
pub(crate) const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
#[cfg(unix)]
pub(crate) const MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;
pub(crate) const MAX_PATH_LIST_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub(crate) struct SelectedFile {
    pub name: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug)]
pub(crate) struct ImportedFile {
    pub name: String,
    pub path: String,
    pub bytes: u64,
}

#[derive(Debug)]
pub(crate) struct ImportedFiles {
    pub entries: Vec<ImportedFile>,
    #[cfg(unix)]
    owned: Option<unix::Batch>,
}

impl ImportedFiles {
    /// Call only after the complete reference text and all images enter the draft.
    pub(crate) fn keep(&mut self) {
        #[cfg(unix)]
        if let Some(mut owned) = self.owned.take() {
            owned.kept = true;
        }
    }

    pub(crate) fn references(&self) -> String {
        let references = self
            .entries
            .iter()
            .map(|file| {
                format!(
                    "@\"{}\"",
                    file.path.replace('\\', "\\\\").replace('"', "\\\"")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!("\n{references}\n")
    }
}

impl Drop for ImportedFiles {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(owned) = self.owned.take() {
            // A result may be rejected while the terminal display is locked.
            // Cleanup owns its directory fds and never removes a source path.
            let _ = std::thread::Builder::new()
                .name("ssmcp-paste-cleanup".into())
                .spawn(move || drop(owned));
        }
    }
}

fn local_uri(value: &str) -> Result<PathBuf, String> {
    let url = reqwest::Url::parse(value).map_err(|_| "Invalid clipboard file URI")?;
    if url.scheme() != "file"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.host_str().is_some_and(|host| host != "localhost")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("Paste local file:// URIs without a remote host, query or fragment".into());
    }
    let path = url
        .to_file_path()
        .map_err(|_| "Clipboard URI does not identify a local file")?;
    validate_path(&path)?;
    Ok(path)
}

fn validate_path(path: &Path) -> Result<(), String> {
    let value = path.to_str().ok_or("Pasted file paths must be UTF-8")?;
    if !path.is_absolute()
        || value.len() > 4096
        || value.chars().any(char::is_control)
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(
            "Paste an absolute local file path without parent traversal or control characters"
                .into(),
        );
    }
    Ok(())
}

/// GNOME's cut selection is copied, too: a paste never moves or deletes sources.
pub(crate) fn clipboard_paths(mime: &str, text: &str) -> Result<Vec<PathBuf>, String> {
    if text.len() > MAX_PATH_LIST_BYTES {
        return Err("Clipboard file list exceeds 64 KiB".into());
    }
    let mut lines = text.lines();
    if mime == "x-special/gnome-copied-files" {
        if !matches!(lines.next().map(str::trim), Some("copy" | "cut")) {
            return Err("The desktop copied-file list is missing its copy/cut header".into());
        }
    } else if mime != "text/uri-list" {
        return Err("Unsupported clipboard file-list format".into());
    }
    let mut paths = Vec::new();
    for line in lines.map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if paths.len() == MAX_FILES {
            return Err("Paste at most eight files at once".into());
        }
        paths.push(local_uri(line)?);
    }
    if paths.is_empty() {
        return Err("The clipboard file list is empty".into());
    }
    Ok(paths)
}

fn plain_path(value: &str) -> Option<PathBuf> {
    let path = if value.starts_with("file:") {
        return local_uri(value).ok();
    } else if let Some(rest) = value.strip_prefix("~/") {
        let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })?;
        PathBuf::from(home).join(rest)
    } else {
        PathBuf::from(value)
    };
    validate_path(&path).ok()?;
    Some(path)
}

/// Recognize a complete absolute path paste, with quoting/escaped spaces and no shell expansion.
pub(crate) fn pasted_paths(text: &str) -> Option<Vec<PathBuf>> {
    let text = text.trim();
    if text.is_empty() || text.len() > MAX_PATH_LIST_BYTES {
        return None;
    }
    if text.starts_with("file:") && text.contains('\n') {
        return clipboard_paths("text/uri-list", text).ok();
    }
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut quote = None;
    let mut escaped = false;
    for ch in text.chars() {
        if escaped {
            part.push(ch);
            escaped = false;
        } else if ch == '\\' && quote != Some('\'') && cfg!(not(windows)) {
            escaped = true;
        } else if Some(ch) == quote {
            quote = None;
        } else if quote.is_none() && matches!(ch, '\'' | '"') {
            quote = Some(ch);
        } else if quote.is_none() && ch.is_whitespace() {
            if !part.is_empty() {
                parts.push(std::mem::take(&mut part));
            }
        } else {
            part.push(ch);
        }
    }
    if escaped || quote.is_some() {
        return None;
    }
    if !part.is_empty() {
        parts.push(part);
    }
    if parts.is_empty() || parts.len() > MAX_FILES {
        return None;
    }
    if let Some(paths) = parts.iter().map(|part| plain_path(part)).collect() {
        return Some(paths);
    }
    // File managers sometimes expose one unquoted absolute path containing spaces.
    // Do not interpret a multiline request or shell quoting as that single path.
    if !text.contains(['\n', '\r', '\'', '"', '\\']) {
        return plain_path(text).map(|path| vec![path]);
    }
    None
}

#[cfg(unix)]
pub(crate) fn read(paths: &[PathBuf]) -> Result<Vec<SelectedFile>, String> {
    unix::read(paths)
}

#[cfg(not(unix))]
pub(crate) fn read(_paths: &[PathBuf]) -> Result<Vec<SelectedFile>, String> {
    Err("Private file imports are not supported on this platform yet; save the file inside the workspace and use @path".into())
}

#[cfg(unix)]
pub(crate) fn import(root: &Path, files: Vec<SelectedFile>) -> Result<ImportedFiles, String> {
    if files.is_empty()
        || files.len() > MAX_FILES
        || files.iter().any(|file| file.bytes.len() > MAX_FILE_BYTES)
        || files.iter().map(|file| file.bytes.len()).sum::<usize>() > MAX_TOTAL_BYTES
    {
        return Err("Paste at most eight files: 16 MiB each and 32 MiB total".into());
    }
    for file in &files {
        if Path::new(&file.name)
            .file_name()
            .and_then(|name| name.to_str())
            != Some(file.name.as_str())
            || file.name.chars().any(char::is_control)
        {
            return Err(
                "Imported filenames must be plain UTF-8 names without control characters".into(),
            );
        }
    }
    let mut owned =
        unix::Batch::new(root).map_err(|error| format!("Cannot import files: {error}"))?;
    let mut entries = Vec::new();
    for file in files {
        let path = owned
            .write(&file.name, &file.bytes)
            .map_err(|error| format!("Cannot import {}: {error}", file.name))?;
        entries.push(ImportedFile {
            name: file.name,
            path,
            bytes: file.bytes.len() as u64,
        });
    }
    Ok(ImportedFiles {
        entries,
        owned: Some(owned),
    })
}

#[cfg(not(unix))]
pub(crate) fn import(_root: &Path, _files: Vec<SelectedFile>) -> Result<ImportedFiles, String> {
    Err("Private file imports are not supported on this platform yet; save the file inside the workspace and use @path".into())
}

#[cfg(unix)]
mod unix {
    use super::*;
    use crate::client::references::paste::{input_directory, open_at};
    use std::{
        ffi::CString,
        fs::{File, Metadata, OpenOptions},
        io::{self, Read, Write},
        os::{
            fd::AsRawFd,
            unix::{ffi::OsStrExt, fs::MetadataExt, fs::OpenOptionsExt},
        },
    };

    fn open_source(path: &Path) -> io::Result<File> {
        let mut directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open("/")?;
        let components = path.components().collect::<Vec<_>>();
        for (index, component) in components.iter().enumerate() {
            match component {
                std::path::Component::RootDir | std::path::Component::CurDir => continue,
                std::path::Component::Normal(name) => {
                    let name = CString::new(name.as_bytes())
                        .map_err(|_| io::Error::other("File path contains NUL"))?;
                    let flags = libc::O_RDONLY
                        | if index + 1 == components.len() {
                            0
                        } else {
                            libc::O_DIRECTORY
                        };
                    directory = open_at(&directory, &name, flags, 0)?;
                }
                _ => {
                    return Err(io::Error::other(
                        "Only absolute local file paths are accepted",
                    ));
                }
            }
        }
        Ok(directory)
    }

    pub(super) fn read(paths: &[PathBuf]) -> Result<Vec<SelectedFile>, String> {
        if paths.is_empty() || paths.len() > MAX_FILES {
            return Err("Paste one to eight local files at once".into());
        }
        let mut opened = Vec::new();
        let mut total = 0u64;
        for path in paths {
            validate_path(path)?;
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or("A pasted file must have a UTF-8 filename")?
                .to_owned();
            let file = open_source(path).map_err(|error| {
                format!("Cannot read {name}: {error}; choose a regular file without symlinks")
            })?;
            let metadata = file.metadata().map_err(|error| error.to_string())?;
            if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES as u64 {
                return Err(format!("{name}: choose a regular file of at most 16 MiB"));
            }
            total += metadata.len();
            if total > MAX_TOTAL_BYTES as u64 {
                return Err("Pasted files exceed the 32 MiB total limit".into());
            }
            opened.push((name, file, metadata));
        }
        let mut selected = Vec::with_capacity(opened.len());
        for (name, mut file, before) in opened {
            let mut bytes = Vec::with_capacity(before.len() as usize);
            (&mut file)
                .take((MAX_FILE_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .map_err(|error| format!("Cannot read {name}: {error}"))?;
            let after = file.metadata().map_err(|error| error.to_string())?;
            if bytes.len() as u64 != before.len()
                || before.len() != after.len()
                || (
                    before.mtime(),
                    before.mtime_nsec(),
                    before.ctime(),
                    before.ctime_nsec(),
                ) != (
                    after.mtime(),
                    after.mtime_nsec(),
                    after.ctime(),
                    after.ctime_nsec(),
                )
            {
                return Err(format!(
                    "{name} changed while being pasted; retry the paste"
                ));
            }
            selected.push(SelectedFile { name, bytes });
        }
        Ok(selected)
    }

    #[derive(Debug)]
    struct Child {
        name: CString,
        directory: File,
        files: Vec<(CString, (u64, u64))>,
        // Pin inode identity even if a rejected import was replaced before cleanup.
        opened: Vec<File>,
    }

    #[derive(Debug)]
    pub(super) struct Batch {
        parent: File,
        name: CString,
        directory: File,
        children: Vec<Child>,
        pub(super) kept: bool,
    }

    fn identity(metadata: &Metadata) -> (u64, u64) {
        (metadata.dev(), metadata.ino())
    }

    fn create_directory(parent: &File, name: &CString) -> io::Result<File> {
        // SAFETY: both directory fd and single-component name remain valid.
        if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
            return Err(io::Error::last_os_error());
        }
        open_at(parent, name, libc::O_RDONLY | libc::O_DIRECTORY, 0)
    }

    fn same_directory(parent: &File, name: &CString, expected: &File) -> bool {
        open_at(parent, name, libc::O_RDONLY | libc::O_DIRECTORY, 0)
            .and_then(|file| file.metadata())
            .and_then(|actual| Ok(identity(&actual) == identity(&expected.metadata()?)))
            .unwrap_or(false)
    }

    impl Batch {
        pub(super) fn new(root: &Path) -> io::Result<Self> {
            let mut random = [0u8; 16];
            crate::platform::random_bytes(&mut random)?;
            let id = random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let parent = input_directory(root)?;
            let name = CString::new(format!("files-{id}")).expect("generated ASCII name");
            let directory = create_directory(&parent, &name)?;
            Ok(Self {
                parent,
                name,
                directory,
                children: Vec::new(),
                kept: false,
            })
        }

        pub(super) fn write(&mut self, name: &str, bytes: &[u8]) -> io::Result<String> {
            let child_name = CString::new((self.children.len() + 1).to_string()).unwrap();
            let directory = create_directory(&self.directory, &child_name)?;
            self.children.push(Child {
                name: child_name,
                directory,
                files: Vec::new(),
                opened: Vec::new(),
            });
            let child = self.children.last_mut().unwrap();
            let temporary = CString::new(if name == ".ssmcp-partial" {
                ".ssmcp-partial-2"
            } else {
                ".ssmcp-partial"
            })
            .unwrap();
            let name = CString::new(name).map_err(|_| io::Error::other("Filename contains NUL"))?;
            let mut file = open_at(
                &child.directory,
                &temporary,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                0o600,
            )?;
            let owned_identity = identity(&file.metadata()?);
            child.files.push((temporary.clone(), owned_identity));
            let written = file.write_all(bytes).and_then(|()| file.sync_all());
            child.opened.push(file);
            written?;
            // SAFETY: publish only this complete regular file. linkat refuses an existing name.
            if unsafe {
                libc::linkat(
                    child.directory.as_raw_fd(),
                    temporary.as_ptr(),
                    child.directory.as_raw_fd(),
                    name.as_ptr(),
                    0,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            child.files.push((name.clone(), owned_identity));
            // SAFETY: this temporary name belongs to the fd just created in this private child.
            if unsafe { libc::unlinkat(child.directory.as_raw_fd(), temporary.as_ptr(), 0) } != 0 {
                return Err(io::Error::last_os_error());
            }
            child.directory.sync_all()?;
            Ok(format!(
                ".ssmcp/inputs/{}/{}/{}",
                self.name.to_str().unwrap(),
                child.name.to_str().unwrap(),
                name.to_str().unwrap()
            ))
        }
    }

    impl Drop for Batch {
        fn drop(&mut self) {
            if self.kept || !same_directory(&self.parent, &self.name, &self.directory) {
                return;
            }
            for child in &self.children {
                if !same_directory(&self.directory, &child.name, &child.directory) {
                    continue;
                }
                for (name, expected) in &child.files {
                    let ours = open_at(&child.directory, name, libc::O_RDONLY, 0)
                        .and_then(|file| file.metadata())
                        .is_ok_and(|metadata| {
                            metadata.is_file() && identity(&metadata) == *expected
                        });
                    if ours {
                        // SAFETY: identity was checked through the same private directory fd.
                        let _ = unsafe {
                            libc::unlinkat(child.directory.as_raw_fd(), name.as_ptr(), 0)
                        };
                    }
                }
                // SAFETY: remove an owned directory only when empty; unexpected entries survive.
                let _ = unsafe {
                    libc::unlinkat(
                        self.directory.as_raw_fd(),
                        child.name.as_ptr(),
                        libc::AT_REMOVEDIR,
                    )
                };
            }
            // SAFETY: this is the checked owned batch directory, not a source or symlink target.
            let _ = unsafe {
                libc::unlinkat(
                    self.parent.as_raw_fd(),
                    self.name.as_ptr(),
                    libc::AT_REMOVEDIR,
                )
            };
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn desktop_file_lists_are_local_bounded_and_preserve_names() {
        let paths = clipboard_paths(
            "text/uri-list",
            "# files\r\nfile:///tmp/a%20b.txt\r\nfile://localhost/tmp/%E4%B8%AD%E6%96%87.pdf\r\n",
        )
        .unwrap();
        assert_eq!(
            paths,
            [
                PathBuf::from("/tmp/a b.txt"),
                PathBuf::from("/tmp/中文.pdf")
            ]
        );
        for operation in ["copy", "cut"] {
            assert_eq!(
                clipboard_paths(
                    "x-special/gnome-copied-files",
                    &format!("{operation}\nfile:///tmp/a%20b.txt\n")
                )
                .unwrap(),
                [PathBuf::from("/tmp/a b.txt")]
            );
        }
        for invalid in [
            "https://example.com/x",
            "file://server/tmp/x",
            "file:///tmp/a%00b",
            "file:///tmp/a%0Ab",
            "file:///tmp/x?token=x",
        ] {
            assert!(clipboard_paths("text/uri-list", invalid).is_err());
        }
        assert!(
            clipboard_paths("text/uri-list", &"file:///tmp/a\n".repeat(MAX_FILES + 1)).is_err()
        );
        assert_eq!(
            pasted_paths("\"/tmp/a b.txt\" /tmp/c\\ d.pdf").unwrap(),
            [PathBuf::from("/tmp/a b.txt"), PathBuf::from("/tmp/c d.pdf")]
        );
        assert_eq!(
            pasted_paths("/tmp/unquoted file.txt").unwrap(),
            [PathBuf::from("/tmp/unquoted file.txt")]
        );
        assert!(pasted_paths("please read report.txt").is_none());
    }

    #[cfg(unix)]
    mod private_imports {
        use super::*;
        use std::{fs, os::unix::fs::PermissionsExt};

        struct Fixture(PathBuf);
        impl Fixture {
            fn new() -> Self {
                let mut id = [0u8; 16];
                crate::platform::random_bytes(&mut id).unwrap();
                let root = std::env::temp_dir()
                    .join(format!("ssmcp-file-paste-{:x}", u128::from_ne_bytes(id)));
                fs::create_dir(&root).unwrap();
                Self(fs::canonicalize(root).unwrap())
            }
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        #[tokio::test]
        async fn imported_names_bytes_permissions_and_real_reference_expansion_round_trip() {
            let workspace = Fixture::new();
            let source = Fixture::new();
            let name = "中文 \"quoted\" \\ notes.txt";
            let path = source.0.join(name);
            let text = "Explicit file paste\n".repeat(80);
            fs::write(&path, &text).unwrap();
            let mut imported =
                import(&workspace.0, read(std::slice::from_ref(&path)).unwrap()).unwrap();
            let entry = &imported.entries[0];
            assert_eq!(entry.name, name);
            assert_eq!(entry.bytes, text.len() as u64);
            let destination = workspace.0.join(&entry.path);
            assert_eq!(fs::read_to_string(&destination).unwrap(), text);
            assert_eq!(
                fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(destination.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::read_dir(destination.parent().unwrap()).unwrap().count(),
                1
            );
            let draft = format!("Review these notes:{}", imported.references());
            let rooted = crate::workspace::Workspace::new(workspace.0.clone()).unwrap();
            let context = crate::context::Context::new(workspace.0.clone(), vec![]).unwrap();
            let expanded = crate::client::references::expand(&draft, &rooted, &context)
                .await
                .unwrap();
            assert_eq!(crate::client::references::source(&expanded), draft);
            assert!(expanded.contains("ssmcp.fs.read_text"));
            imported.keep();
            drop(imported);
            assert_eq!(fs::read_to_string(destination).unwrap(), text);
            assert_eq!(fs::read_to_string(path).unwrap(), text);
        }

        #[test]
        fn rejected_import_rolls_back_owned_copies_but_preserves_sources_and_replacements() {
            let workspace = Fixture::new();
            let source = Fixture::new();
            let first = source.0.join("first.txt");
            let second = source.0.join("second.bin");
            fs::write(&first, "first").unwrap();
            fs::write(&second, b"\0binary").unwrap();
            let mut imported = import(
                &workspace.0,
                read(&[first.clone(), second.clone()]).unwrap(),
            )
            .unwrap();
            let first_copy = workspace.0.join(&imported.entries[0].path);
            let second_copy = workspace.0.join(&imported.entries[1].path);
            fs::remove_file(&second_copy).unwrap();
            fs::write(&second_copy, "replacement belongs to the user").unwrap();
            // Exercise the same guard synchronously so the assertion has no thread timing dependency.
            drop(imported.owned.take());
            assert!(!first_copy.exists());
            assert_eq!(
                fs::read_to_string(second_copy).unwrap(),
                "replacement belongs to the user"
            );
            assert_eq!(fs::read_to_string(first).unwrap(), "first");
            assert_eq!(fs::read(second).unwrap(), b"\0binary");
        }

        #[test]
        fn symlinks_directories_and_over_budget_selections_never_create_imports() {
            let workspace = Fixture::new();
            let source = Fixture::new();
            let ordinary = source.0.join("original.txt");
            fs::write(&ordinary, "original").unwrap();
            let link = source.0.join("link.txt");
            std::os::unix::fs::symlink(&ordinary, &link).unwrap();
            assert!(read(&[link]).is_err());
            assert!(read(std::slice::from_ref(&source.0)).is_err());
            let parent_link = source.0.join("alias");
            std::os::unix::fs::symlink(&source.0, &parent_link).unwrap();
            assert!(read(&[parent_link.join("original.txt")]).is_err());
            let huge = source.0.join("huge");
            fs::File::create(&huge)
                .unwrap()
                .set_len((MAX_FILE_BYTES + 1) as u64)
                .unwrap();
            assert!(read(&[huge]).is_err());
            let sized = (0..3)
                .map(|n| {
                    let path = source.0.join(format!("file-{n}"));
                    fs::File::create(&path)
                        .unwrap()
                        .set_len(12 * 1024 * 1024)
                        .unwrap();
                    path
                })
                .collect::<Vec<_>>();
            assert!(read(&sized).unwrap_err().contains("32 MiB"));
            assert!(!workspace.0.join(".ssmcp").exists());
        }
    }
}
