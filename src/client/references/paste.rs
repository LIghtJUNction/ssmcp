//! Long clipboard inputs stay in private workspace files and use ordinary @ references.
//! Successful files are retained until the user removes .ssmcp/inputs manually.

use std::{io, path::Path};

pub(crate) const MAX_PASTE_BYTES: usize = 4 * 1024 * 1024;
/// Includes @ and quotes; callers can reserve draft space before creating a file.
pub(crate) const REFERENCE_BYTES: usize = 3 + ".ssmcp/inputs/paste-".len() + 32 + ".txt".len();

pub(crate) fn store(text: &str) -> Result<String, String> {
    let root = std::env::current_dir()
        .map_err(|error| format!("Could not locate the workspace for the long paste: {error}"))?;
    store_at(&root, text)
}

pub(crate) fn store_at(root: &Path, text: &str) -> Result<String, String> {
    let reference = store_bytes_at(root, "paste", "txt", text.as_bytes())?;
    debug_assert_eq!(reference.len(), REFERENCE_BYTES);
    Ok(reference)
}

pub(crate) fn store_markdown_at(root: &Path, text: &str) -> Result<String, String> {
    store_bytes_at(root, "handoff", "md", text.as_bytes())
}

fn store_bytes_at(
    root: &Path,
    prefix: &str,
    extension: &str,
    bytes: &[u8],
) -> Result<String, String> {
    if bytes.is_empty() {
        return Err("The pasted text is empty.".into());
    }
    if bytes.len() > MAX_PASTE_BYTES {
        return Err("The paste exceeds 4 MiB of UTF-8 text; save it as a workspace file and reference it with @path.".into());
    }
    let mut random = [0u8; 16];
    crate::platform::random_bytes(&mut random)
        .map_err(|error| format!("Could not create a unique input filename: {error}"))?;
    let id = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let name = format!("{prefix}-{id}.{extension}");
    save(root, &name, bytes)
        .map_err(|error| format!("Could not save the private input in .ssmcp/inputs: {error}"))?;
    Ok(format!("@\".ssmcp/inputs/{name}\""))
}

#[cfg(unix)]
pub(super) fn open_at(
    parent: &std::fs::File,
    name: &std::ffi::CString,
    flags: i32,
    mode: libc::mode_t,
) -> io::Result<std::fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    // SAFETY: the parent fd and NUL-terminated name remain valid for this call.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            mode,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: openat returned a new owned file descriptor.
        Ok(unsafe { std::fs::File::from_raw_fd(fd) })
    }
}

/// Shared fd-anchored, private destination for explicit local paste imports.
#[cfg(unix)]
pub(super) fn input_directory(root: &Path) -> io::Result<std::fs::File> {
    use std::{
        ffi::CString,
        fs::{File, OpenOptions},
        os::{
            fd::AsRawFd,
            unix::fs::{MetadataExt, OpenOptionsExt},
        },
    };

    fn directory(parent: &File, name: &str, private: bool) -> io::Result<File> {
        let component = CString::new(name).expect("fixed directory name");
        let flags = libc::O_RDONLY | libc::O_DIRECTORY;
        let mut opened = open_at(parent, &component, flags, 0);
        if opened
            .as_ref()
            .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
        {
            // SAFETY: the directory fd and fixed component are valid.
            if unsafe { libc::mkdirat(parent.as_raw_fd(), component.as_ptr(), 0o700) } != 0
                && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists
            {
                return Err(io::Error::last_os_error());
            }
            opened = open_at(parent, &component, flags, 0);
        }
        let directory = opened?;
        let metadata = directory.metadata()?;
        let disallowed_mode = if private { 0o077 } else { 0o022 };
        // SAFETY: geteuid has no preconditions or memory arguments.
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & disallowed_mode != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{name} must be owned by the current user and {}",
                    if private {
                        "private (permissions 0700)"
                    } else {
                        "not writable by other users"
                    }
                ),
            ));
        }
        Ok(directory)
    }

    let root = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(root)?;
    let state = directory(&root, ".ssmcp", false)?;
    directory(&state, "inputs", true)
}

#[cfg(unix)]
fn save(root: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    use std::{ffi::CString, io::Write, os::fd::AsRawFd};
    let inputs = input_directory(root)?;
    let name = CString::new(name).expect("generated ASCII filename");
    let mut file = open_at(
        &inputs,
        &name,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        0o600,
    )?;
    let result = file.write_all(bytes).and_then(|()| file.sync_all());
    if result.is_err() {
        drop(file);
        // SAFETY: remove only the newly created name through the same directory fd.
        let _ = unsafe { libc::unlinkat(inputs.as_raw_fd(), name.as_ptr(), 0) };
    }
    result
}

#[cfg(not(unix))]
fn save(_root: &Path, _name: &str, _bytes: &[u8]) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "automatic private paste files are not supported on this platform yet; save the text inside the workspace and reference it with @path",
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        ffi::CString,
        fs,
        os::unix::{ffi::OsStrExt, fs::PermissionsExt},
        path::PathBuf,
    };

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let mut id = [0u8; 16];
            crate::platform::random_bytes(&mut id).unwrap();
            let id = id
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let root = std::env::temp_dir().join(format!("ssmcp-paste-test-{id}"));
            fs::create_dir(&root).unwrap();
            Self(root)
        }
        fn path(&self, reference: &str) -> PathBuf {
            self.0.join(
                reference
                    .strip_prefix("@\"")
                    .unwrap()
                    .strip_suffix('"')
                    .unwrap(),
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn long_pastes_round_trip_as_private_non_overwriting_workspace_references() {
        let fixture = Fixture::new();
        let text = "中文 clipboard\n".repeat(1000);
        let first = store_at(&fixture.0, &text).unwrap();
        let second = store_at(&fixture.0, &text).unwrap();
        assert_ne!(first, second);
        assert_eq!(first.len(), REFERENCE_BYTES);
        assert_eq!(fs::read_to_string(fixture.path(&first)).unwrap(), text);
        assert_eq!(fs::read_to_string(fixture.path(&second)).unwrap(), text);
        assert_eq!(
            fs::metadata(fixture.path(&first))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(fixture.0.join(".ssmcp/inputs"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn invalid_sizes_create_no_paste_directory_and_exact_limit_is_preserved() {
        let fixture = Fixture::new();
        assert!(store_at(&fixture.0, "").is_err());
        assert!(
            store_at(&fixture.0, &"x".repeat(MAX_PASTE_BYTES + 1))
                .unwrap_err()
                .contains("4 MiB")
        );
        assert!(!fixture.0.join(".ssmcp").exists());
        let text = "界".repeat(MAX_PASTE_BYTES / 3) + "x";
        assert_eq!(text.len(), MAX_PASTE_BYTES);
        let reference = store_at(&fixture.0, &text).unwrap();
        assert_eq!(fs::read_to_string(fixture.path(&reference)).unwrap(), text);
    }

    #[test]
    fn symlink_and_fifo_parents_are_rejected_without_reading_or_writing_through_them() {
        let fixture = Fixture::new();
        let outside = Fixture::new();
        std::os::unix::fs::symlink(&outside.0, fixture.0.join(".ssmcp")).unwrap();
        assert!(store_at(&fixture.0, "private text").is_err());
        assert!(fs::read_dir(&outside.0).unwrap().next().is_none());
        fs::remove_file(fixture.0.join(".ssmcp")).unwrap();
        fs::create_dir(fixture.0.join(".ssmcp")).unwrap();
        std::os::unix::fs::symlink(&outside.0, fixture.0.join(".ssmcp/inputs")).unwrap();
        assert!(store_at(&fixture.0, "private text").is_err());
        assert!(fs::read_dir(&outside.0).unwrap().next().is_none());
        fs::remove_file(fixture.0.join(".ssmcp/inputs")).unwrap();
        let fifo = CString::new(fixture.0.join(".ssmcp/inputs").as_os_str().as_bytes()).unwrap();
        // SAFETY: this unique temporary path is NUL-terminated and does not yet exist.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(store_at(&fixture.0, "private text").is_err());
    }

    #[test]
    fn existing_public_input_directory_is_rejected_without_changing_its_permissions() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.0.join(".ssmcp/inputs")).unwrap();
        fs::set_permissions(
            fixture.0.join(".ssmcp/inputs"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(
            store_at(&fixture.0, "private text")
                .unwrap_err()
                .contains("0700")
        );
        assert_eq!(
            fs::metadata(fixture.0.join(".ssmcp/inputs"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert!(
            fs::read_dir(fixture.0.join(".ssmcp/inputs"))
                .unwrap()
                .next()
                .is_none()
        );
    }
    #[tokio::test]
    async fn pasted_text_is_a_readable_reference_with_a_bounded_preview_and_user_request() {
        let fixture = Fixture::new();
        let text = "长文本内容".repeat(8192);
        let reference = store_at(&fixture.0, &text).unwrap();
        let draft = format!("请从这份材料找出主要问题 {reference}，然后给出改进建议");
        let workspace = crate::workspace::Workspace::new(fixture.0.clone()).unwrap();
        let context = crate::context::Context::new(fixture.0.clone(), vec![]).unwrap();
        let expanded = super::super::expand(&draft, &workspace, &context)
            .await
            .unwrap();
        assert_eq!(super::super::source(&expanded), draft);
        assert!(expanded.contains("ssmcp.fs.read_text"));
        assert!(expanded.len() < crate::wire::MAX_USER_MESSAGE_BYTES);
        assert!(expanded.len() < text.len());
        assert_eq!(fs::read_to_string(fixture.path(&reference)).unwrap(), text);
    }
}
