//! cmd.exe treats even a local verbatim drive path as a UNC current directory
//! and silently switches to the Windows directory. Adapt its launch argument,
//! keeping the canonical path for authorization, session state and receipts.
//! PowerShell also needs the DOS spelling to start on a normal filesystem drive.

fn is_powershell(name: &std::ffi::OsStr) -> bool {
    ["powershell", "powershell.exe", "pwsh", "pwsh.exe"]
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

/// Only an absolute drive spelling fits cmd's current-directory contract.
/// Leave space for the trailing separator and NUL in a MAX_PATH buffer.
fn drive_spelling(path: &[u16]) -> Option<&[u16]> {
    const VERBATIM: [u16; 4] = [BACKSLASH, BACKSLASH, b'?' as u16, BACKSLASH];
    const COLON: u16 = b':' as u16;
    const BACKSLASH: u16 = b'\\' as u16;
    let path = path.strip_prefix(&VERBATIM).unwrap_or(path);
    let [drive, COLON, BACKSLASH, ..] = path else {
        return None;
    };
    if !u8::try_from(*drive).is_ok_and(|drive| drive.is_ascii_alphabetic())
        || path.contains(&0)
        || path.len() + usize::from(path.last() != Some(&BACKSLASH)) >= 260
    {
        return None;
    }
    Some(path)
}

#[cfg(windows)]
pub(crate) use windows::child_cwd;

#[cfg(windows)]
mod windows {
    use super::drive_spelling;
    use std::{
        borrow::Cow,
        ffi::{OsStr, OsString},
        io,
        os::windows::{
            ffi::{OsStrExt, OsStringExt},
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
        },
        path::{Component, Path, PathBuf, Prefix},
        ptr,
    };
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        Storage::FileSystem::{
            CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, GetFinalPathNameByHandleW, GetShortPathNameW, OPEN_EXISTING,
        },
    };

    pub(crate) fn child_cwd<'a>(program: &Path, cwd: &'a Path) -> io::Result<Cow<'a, Path>> {
        let is_cmd = program.file_name().is_some_and(|name| {
            name.eq_ignore_ascii_case(OsStr::new("cmd"))
                || name.eq_ignore_ascii_case(OsStr::new("cmd.exe"))
        });
        let is_batch = program.extension().is_some_and(|ext| {
            ext.eq_ignore_ascii_case(OsStr::new("bat"))
                || ext.eq_ignore_ascii_case(OsStr::new("cmd"))
        });
        let powershell = program.file_name().is_some_and(super::is_powershell);
        if !is_cmd && !is_batch && !powershell {
            return Ok(Cow::Borrowed(cwd));
        }
        if matches!(cwd.components().next(), Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::UNC(..) | Prefix::VerbatimUNC(..)))
        {
            return if powershell {
                Ok(Cow::Borrowed(cwd))
            } else {
                Err(unsupported())
            };
        }
        let canonical = cwd.canonicalize()?;
        let path: Vec<_> = canonical.as_os_str().encode_wide().collect();
        if let Some(ordinary) = verified_spelling(&path, &canonical) {
            return Ok(Cow::Owned(ordinary));
        }
        // PowerShell can address UNC and extended-only paths with its provider.
        // Avoid changing their semantics (or exposing an 8.3 alias as its location).
        if powershell {
            return Ok(Cow::Borrowed(cwd));
        }
        // A short alias can represent long local paths on volumes with 8.3 names.
        // Network shares cannot become a drive by dropping their namespace prefix.
        if matches!(canonical.components().next(), Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::VerbatimDisk(_) | Prefix::Disk(_)))
            && let Some(alias) = short_path(&path)
            && let Some(ordinary) = verified_spelling(&alias, &canonical)
        {
            return Ok(Cow::Owned(ordinary));
        }
        Err(unsupported())
    }

    fn unsupported() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "cmd.exe cannot use this workspace as its current directory; use --shell powershell or a local drive path that cmd.exe can represent",
        )
    }

    fn verified_spelling(path: &[u16], canonical: &Path) -> Option<PathBuf> {
        let spelling = drive_spelling(path)?;
        let candidate = PathBuf::from(OsString::from_wide(spelling));
        let terminated: Vec<_> = spelling.iter().copied().chain([0]).collect();
        // Pass the ordinary spelling directly to Win32. Rust's file APIs may
        // attach a verbatim prefix, which would skip DOS normalization here.
        let handle = unsafe {
            CreateFileW(
                terminated.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return None;
        }
        // SAFETY: CreateFileW returned an owned handle.
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        let mut text = vec![0u16; canonical.as_os_str().encode_wide().count() + 1];
        let length = unsafe {
            GetFinalPathNameByHandleW(
                handle.as_raw_handle(),
                text.as_mut_ptr(),
                text.len() as u32,
                0,
            )
        } as usize;
        if length == 0 || length >= text.len() {
            return None;
        }
        let resolved = PathBuf::from(OsString::from_wide(&text[..length]));
        (resolved == canonical).then_some(candidate)
    }

    fn short_path(path: &[u16]) -> Option<Vec<u16>> {
        let terminated: Vec<_> = path.iter().copied().chain([0]).collect();
        let mut text = [0u16; 265]; // up to MAX_PATH plus a verbatim prefix
        let length =
            unsafe { GetShortPathNameW(terminated.as_ptr(), text.as_mut_ptr(), text.len() as u32) }
                as usize;
        (length > 0 && length < text.len()).then(|| text[..length].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::drive_spelling;

    fn ordinary(path: &str) -> Option<String> {
        drive_spelling(&path.encode_utf16().collect::<Vec<_>>()).map(String::from_utf16_lossy)
    }

    #[test]
    fn recognises_both_powershell_launchers_and_leaves_custom_programs_alone() {
        for name in ["powershell", "PowerShell.EXE", "pwsh", "PwSh.eXe"] {
            assert!(super::is_powershell(name.as_ref()));
        }
        for name in ["powershell-host.exe", "powershell.cmd", "tool.exe"] {
            assert!(!super::is_powershell(name.as_ref()));
        }
    }

    #[test]
    fn only_absolute_drive_paths_have_an_ordinary_cmd_spelling() {
        assert_eq!(ordinary(r"\\?\C:\code\演示"), Some(r"C:\code\演示".into()));
        assert_eq!(ordinary(r"d:\"), Some(r"d:\".into()));
        for path in [
            r"\\earth\share\dir",
            r"\\?\UNC\earth\share",
            r"\\.\C:\dir",
            r"\\?\Volume{guid}\dir",
            r"C:relative",
            r"\rooted",
            "X:\\nul\0tail",
        ] {
            assert_eq!(ordinary(path), None, "{path:?}");
        }
    }

    #[test]
    fn legacy_directory_limit_counts_utf16_and_a_trailing_separator() {
        let local = format!("C:\\{}x", "🦀".repeat(127));
        assert_eq!(local.encode_utf16().count(), 258);
        assert_eq!(ordinary(&local), Some(local.clone()));
        assert!(ordinary(&format!("{local}\\")).is_some());
        assert!(ordinary(&format!("{local}x")).is_none());
    }
}
