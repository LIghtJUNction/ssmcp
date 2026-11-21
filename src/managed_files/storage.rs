//! No-follow, handle-anchored access. No cleanup operation follows a link.
use super::{FileIdentity, ObjectIdentity, invalid};
use std::{
    ffi::OsString,
    fs::File,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};
#[cfg(unix)]
use std::{
    ffi::{CStr, CString},
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd},
        unix::{
            ffi::OsStringExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
};

pub(super) struct Directory {
    #[cfg(unix)]
    file: File,
    #[cfg(windows)]
    dir: cap_std::fs::Dir,
    #[cfg(windows)]
    path: PathBuf,
}

#[derive(Clone, Copy)]
pub(super) enum Access {
    Read,
    ReadWrite,
    Create,
}

/// The source name has been moved into a newly created private directory on
/// the same filesystem. No deletion may target the old public name afterward.
pub(super) struct Captured {
    parent: Directory,
    directory: Directory,
    directory_name: String,
    original_name: String,
}

pub(super) struct RestoreOutcome {
    pub restored: bool,
    pub recovery_remains: bool,
    pub error: Option<String>,
}

impl Directory {
    pub(super) fn workspace(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path)?;
            Ok(Self { file })
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
                FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
            };
            let file = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(path)?;
            use std::os::windows::fs::MetadataExt;
            let metadata = file.metadata()?;
            if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            {
                return Err(invalid(
                    "workspace must be a directory, not a reparse point",
                ));
            }
            Ok(Self {
                dir: cap_std::fs::Dir::from_std_file(file),
                path: path.to_path_buf(),
            })
        }
    }

    pub(super) fn child(&self, name: &str, create: bool, private: bool) -> io::Result<Self> {
        single_name(name)?;
        #[cfg(unix)]
        {
            let name = CString::new(name).map_err(io::Error::other)?;
            if create
                && unsafe { libc::mkdirat(self.file.as_raw_fd(), name.as_ptr(), 0o700) } != 0
                && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists
            {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: the parent handle is live and name is one checked component.
            let fd = unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let file = unsafe { File::from_raw_fd(fd) };
            let metadata = file.metadata()?;
            let denied = if private { 0o077 } else { 0o022 };
            if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & denied != 0 {
                return Err(invalid(
                    "managed directory must be owned by this user and not publicly writable",
                ));
            }
            Ok(Self { file })
        }
        #[cfg(windows)]
        {
            use cap_fs_ext::DirExt;
            let path = self.path.join(name);
            if create {
                match crate::platform::windows::create_private_directory(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
            }
            let dir = self.dir.open_dir_nofollow(name)?;
            let file = dir.try_clone()?.into_std_file();
            if private {
                crate::platform::windows::verify_directory(&file)?;
            } else {
                // Ordinary workspace parents may inherit their access list.
                // Do not change their ACL or require the registry's protected
                // DACL bit; require the current owner and a real directory.
                verify_owned_windows(&file, true)?;
            }
            Ok(Self { dir, path })
        }
    }

    pub(super) fn identity(&self) -> io::Result<ObjectIdentity> {
        #[cfg(unix)]
        {
            ObjectIdentity::from_file(&self.file)
        }
        #[cfg(windows)]
        {
            ObjectIdentity::from_file(&self.dir.try_clone()?.into_std_file())
        }
    }

    pub(super) fn parent_with_ids(
        &self,
        relative: &str,
    ) -> io::Result<(Self, String, Vec<ObjectIdentity>)> {
        let parts = relative.split('/').collect::<Vec<_>>();
        let (name, parents) = parts
            .split_last()
            .ok_or_else(|| invalid("missing file name"))?;
        let mut directory = self.reopen()?;
        let mut identities = vec![directory.identity()?];
        for component in parents {
            directory = directory.child(component, false, false)?;
            identities.push(directory.identity()?);
        }
        Ok((directory, (*name).to_owned(), identities))
    }

    pub(super) fn reopen(&self) -> io::Result<Self> {
        #[cfg(unix)]
        {
            // A new open description is intentional: dup shares directory offsets.
            let dot = c".";
            let fd = unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    dot.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self {
                file: unsafe { File::from_raw_fd(fd) },
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                dir: self.dir.try_clone()?,
                path: self.path.clone(),
            })
        }
    }

    #[cfg(target_os = "linux")]
    pub(super) fn publication_lock(&self) -> io::Result<super::LockedFile> {
        super::LockedFile::acquire(self.reopen()?.file)
    }

    pub(super) fn open(&self, name: &str, access: Access, private: bool) -> io::Result<File> {
        single_name(name)?;
        #[cfg(unix)]
        let file = {
            let name = CString::new(name).map_err(io::Error::other)?;
            let flags = match access {
                Access::Read => libc::O_RDONLY,
                Access::ReadWrite => libc::O_RDWR,
                Access::Create => libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            };
            let fd = unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                    0o600,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            unsafe { File::from_raw_fd(fd) }
        };
        #[cfg(windows)]
        let file = {
            use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
            let mut options = cap_std::fs::OpenOptions::new();
            options.follow(FollowSymlinks::No).read(true);
            match access {
                Access::Read => {}
                Access::ReadWrite => {
                    options.write(true);
                }
                Access::Create => {
                    options.write(true).create_new(true);
                }
            }
            self.dir.open_with(name, &options)?.into_std()
        };
        verify_file(&file, private)?;
        Ok(file)
    }

    pub(super) fn read(&self, name: &str, limit: usize, private: bool) -> io::Result<Vec<u8>> {
        self.read_identified(name, limit, private)
            .map(|(bytes, _)| bytes)
    }

    pub(super) fn read_identified(
        &self,
        name: &str,
        limit: usize,
        private: bool,
    ) -> io::Result<(Vec<u8>, FileIdentity)> {
        let mut file = self.open(name, Access::Read, private)?;
        let before = FileIdentity::from_file(&file)?;
        if file.metadata()?.len() > limit as u64 {
            return Err(invalid("managed metadata exceeds its size limit"));
        }
        let mut bytes = Vec::new();
        (&mut file).take(limit as u64 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > limit {
            return Err(invalid("managed metadata grew beyond its size limit"));
        }
        if FileIdentity::from_file(&file)? != before {
            return Err(invalid("managed metadata changed while reading"));
        }
        verify_file(&file, private)?;
        Ok((bytes, before))
    }

    pub(super) fn write_new(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let mut file = self.open(name, Access::Create, true)?;
        let result = file.write_all(bytes).and_then(|()| file.sync_all());
        if result.is_err() {
            let _ = self.remove(name);
        }
        result
    }

    pub(super) fn write_atomic(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        single_name(name)?;
        let temporary = format!(".write-{}.tmp", super::random_id()?);
        self.write_new(&temporary, bytes)?;
        let result = self.rename(&temporary, name);
        if result.is_err() {
            let _ = self.remove(&temporary);
        }
        result
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        #[cfg(unix)]
        {
            let from = CString::new(from).map_err(io::Error::other)?;
            let to = CString::new(to).map_err(io::Error::other)?;
            if unsafe {
                libc::renameat(
                    self.file.as_raw_fd(),
                    from.as_ptr(),
                    self.file.as_raw_fd(),
                    to.as_ptr(),
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            self.file.sync_all()
        }
        #[cfg(windows)]
        {
            self.dir.rename(from, &self.dir, to)
        }
    }

    pub(super) fn remove(&self, name: &str) -> io::Result<()> {
        single_name(name)?;
        #[cfg(unix)]
        {
            let name = CString::new(name).map_err(io::Error::other)?;
            if unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), 0) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(windows)]
        {
            self.dir.remove_file(name)
        }
    }

    fn private_child_new(&self, name: &str) -> io::Result<Self> {
        single_name(name)?;
        #[cfg(unix)]
        {
            let name = CString::new(name).map_err(io::Error::other)?;
            if unsafe { libc::mkdirat(self.file.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        #[cfg(windows)]
        crate::platform::windows::create_private_directory(&self.path.join(name))?;
        self.child(name, false, true)
    }

    fn move_to(&self, from: &str, to_directory: &Self, to: &str) -> io::Result<()> {
        single_name(from)?;
        single_name(to)?;
        #[cfg(unix)]
        {
            let from = CString::new(from).map_err(io::Error::other)?;
            let to = CString::new(to).map_err(io::Error::other)?;
            if unsafe {
                libc::renameat(
                    self.file.as_raw_fd(),
                    from.as_ptr(),
                    to_directory.file.as_raw_fd(),
                    to.as_ptr(),
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(windows)]
        {
            self.dir.rename(from, &to_directory.dir, to)
        }
    }

    fn remove_directory(&self, name: &str) -> io::Result<()> {
        single_name(name)?;
        #[cfg(unix)]
        {
            let name = CString::new(name).map_err(io::Error::other)?;
            if unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) }
                != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(windows)]
        {
            self.dir.remove_dir(name)
        }
    }

    /// Used only for a recovery receipt, never as authority for a file mutation.
    fn actual_path(&self) -> io::Result<PathBuf> {
        #[cfg(target_os = "linux")]
        {
            std::fs::read_link(format!("/proc/self/fd/{}", self.file.as_raw_fd()))
        }
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        {
            let mut bytes = [0u8; libc::PATH_MAX as usize];
            if unsafe { libc::fcntl(self.file.as_raw_fd(), libc::F_GETPATH, bytes.as_mut_ptr()) }
                < 0
            {
                return Err(io::Error::last_os_error());
            }
            let end = bytes
                .iter()
                .position(|byte| *byte == 0)
                .ok_or_else(|| invalid("directory recovery path was not terminated"))?;
            Ok(PathBuf::from(OsString::from_vec(bytes[..end].to_vec())))
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;
            let file = self.dir.try_clone()?.into_std_file();
            let mut text = vec![0u16; 32_768];
            let length = unsafe {
                GetFinalPathNameByHandleW(
                    file.as_raw_handle(),
                    text.as_mut_ptr(),
                    text.len() as u32,
                    0,
                )
            };
            if length == 0 {
                return Err(io::Error::last_os_error());
            }
            if length as usize >= text.len() {
                return Err(invalid("directory recovery path is too long"));
            }
            Ok(PathBuf::from(
                String::from_utf16(&text[..length as usize]).map_err(io::Error::other)?,
            ))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "ios", windows)))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "directory handle path unavailable",
            ))
        }
    }

    pub(super) fn capture(self, name: &str) -> io::Result<Captured> {
        single_name(name)?;
        let directory_name = format!("{}{}", super::RECOVERY_PREFIX, super::random_id()?);
        let directory = self.private_child_new(&directory_name)?;
        let capture = Captured {
            parent: self,
            directory,
            directory_name,
            original_name: name.to_owned(),
        };
        // The destination is inside a new EXCL-created, private, empty directory.
        // rename is atomic and both directories are on the same filesystem.
        if let Err(error) = capture.parent.move_to(name, &capture.directory, "captured") {
            let location = capture.directory.actual_path().ok();
            if let Err(cleanup) = capture.remove_empty() {
                return Err(io::Error::other(format!(
                    "{error}; empty recovery directory retained at {location:?}: {cleanup}"
                )));
            }
            return Err(error);
        }
        Ok(capture)
    }

    pub(super) fn names(&self, limit: usize) -> io::Result<(Vec<OsString>, bool)> {
        #[cfg(unix)]
        {
            let file = self.reopen()?.file;
            let fd = file.into_raw_fd();
            let stream = unsafe { libc::fdopendir(fd) };
            if stream.is_null() {
                let error = io::Error::last_os_error();
                unsafe {
                    libc::close(fd);
                }
                return Err(error);
            }
            struct Stream(*mut libc::DIR);
            impl Drop for Stream {
                fn drop(&mut self) {
                    unsafe {
                        libc::closedir(self.0);
                    }
                }
            }
            let stream = Stream(stream);
            let mut names = Vec::new();
            loop {
                // readdir uses null for both EOF and error; retain that distinction.
                unsafe {
                    *errno_location() = 0;
                }
                let entry = unsafe { libc::readdir(stream.0) };
                if entry.is_null() {
                    let code = unsafe { *errno_location() };
                    return if code == 0 {
                        Ok((names, false))
                    } else {
                        Err(io::Error::from_raw_os_error(code))
                    };
                }
                let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
                if bytes == b"." || bytes == b".." {
                    continue;
                }
                if names.len() == limit {
                    return Ok((names, true));
                }
                names.push(OsString::from_vec(bytes.to_vec()));
            }
        }
        #[cfg(windows)]
        {
            let mut names = Vec::new();
            for entry in self.dir.entries()? {
                if names.len() == limit {
                    return Ok((names, true));
                }
                names.push(entry?.file_name());
            }
            Ok((names, false))
        }
    }
}

impl Captured {
    pub(super) fn remove_matching(&self, expected: &FileIdentity, private: bool) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        let _publication = self.directory.publication_lock()?;
        let file = self.directory.open("captured", Access::Read, private)?;
        if FileIdentity::from_file(&file)? != *expected
            || !same_at(&self.directory, "captured", expected)?
        {
            return Err(invalid(
                "isolated object does not match the registered file",
            ));
        }
        // Only the verified object in the private isolation directory is unlinked.
        // A new file at original_name is never touched.
        self.directory.remove("captured")
    }

    pub(super) fn recovery_path(&self) -> io::Result<PathBuf> {
        Ok(self.directory.actual_path()?.join("captured"))
    }

    pub(super) fn original_path(&self) -> io::Result<PathBuf> {
        Ok(self.parent.actual_path()?.join(&self.original_name))
    }

    pub(super) fn directory_name(&self) -> &str {
        &self.directory_name
    }

    pub(super) fn parent_identity(&self) -> io::Result<ObjectIdentity> {
        self.parent.identity()
    }

    pub(super) fn restore_no_replace(&self) -> RestoreOutcome {
        #[cfg(target_os = "linux")]
        let result = {
            let from = c"captured";
            let to = CString::new(self.original_name.as_str()).expect("checked single component");
            let result = unsafe {
                libc::syscall(
                    libc::SYS_renameat2,
                    self.directory.file.as_raw_fd(),
                    from.as_ptr(),
                    self.parent.file.as_raw_fd(),
                    to.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            if result == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        };
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        let result = {
            let from = c"captured";
            let to = CString::new(self.original_name.as_str()).expect("checked single component");
            let result = unsafe {
                libc::renameatx_np(
                    self.directory.file.as_raw_fd(),
                    from.as_ptr(),
                    self.parent.file.as_raw_fd(),
                    to.as_ptr(),
                    libc::RENAME_EXCL,
                )
            };
            if result == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        };
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "ios")))]
        {
            // On these platforms an exclusive hard link restores regular files.
            // No replace/copy fallback is allowed. Nonregular objects or a
            // filesystem without hard links remain intact in recovery storage.
            let linked = self.restore_link();
            return match linked {
                Ok(()) => match self.directory.remove("captured") {
                    Ok(()) => RestoreOutcome {
                        restored: true,
                        recovery_remains: false,
                        error: None,
                    },
                    Err(error) => RestoreOutcome {
                        restored: true,
                        recovery_remains: true,
                        error: Some(format!("restored link; isolated link remains: {error}")),
                    },
                },
                Err(error) => RestoreOutcome {
                    restored: false,
                    recovery_remains: true,
                    error: Some(error.to_string()),
                },
            };
        }
        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios"))]
        match result {
            Ok(()) => RestoreOutcome {
                restored: true,
                recovery_remains: false,
                error: None,
            },
            Err(error) => RestoreOutcome {
                restored: false,
                recovery_remains: true,
                error: Some(error.to_string()),
            },
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "ios")))]
    fn restore_link(&self) -> io::Result<()> {
        // This refuses symlinks and special files; their recovery copy stays put.
        let _file = self.directory.open("captured", Access::Read, false)?;
        #[cfg(windows)]
        {
            self.directory
                .dir
                .hard_link("captured", &self.parent.dir, &self.original_name)
        }
        #[cfg(unix)]
        {
            let to = CString::new(self.original_name.as_str()).expect("checked single component");
            if unsafe {
                libc::linkat(
                    self.directory.file.as_raw_fd(),
                    c"captured".as_ptr(),
                    self.parent.file.as_raw_fd(),
                    to.as_ptr(),
                    0,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }

    pub(super) fn remove_empty(self) -> io::Result<()> {
        let expected = self.directory.identity()?;
        let current = self.parent.child(&self.directory_name, false, true)?;
        if current.identity()? != expected {
            return Err(invalid("recovery directory identity changed"));
        }
        drop(current);
        drop(self.directory);
        // On Windows the open directory handle must be closed before rmdir.
        self.parent.remove_directory(&self.directory_name)
    }
}

#[cfg(target_os = "linux")]
unsafe fn errno_location() -> *mut libc::c_int {
    unsafe { libc::__errno_location() }
}

#[cfg(target_os = "android")]
unsafe fn errno_location() -> *mut libc::c_int {
    unsafe { libc::__errno() }
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
unsafe fn errno_location() -> *mut libc::c_int {
    unsafe { libc::__error() }
}

pub(super) fn verify_file(file: &File, private: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        let metadata = file.metadata()?;
        let denied = if private { 0o077 } else { 0o022 };
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & denied != 0
        {
            return Err(invalid(
                "managed file must be a singly linked, current-user regular file with safe permissions",
            ));
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        if private {
            crate::platform::windows::verify_file(file)
        } else {
            verify_owned_windows(file, false)
        }
    }
}

#[cfg(windows)]
fn verify_owned_windows(file: &File, directory: bool) -> io::Result<()> {
    use std::{
        ffi::c_void,
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    };
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            Authorization::{GetSecurityInfo, SE_FILE_OBJECT},
            EqualSid, GetTokenInformation, OWNER_SECURITY_INFORMATION, TOKEN_OWNER, TOKEN_QUERY,
            TOKEN_USER, TokenOwner, TokenUser,
        },
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
            GetFileInformationByHandle,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
        || (!directory && (!file.metadata()?.is_file() || information.nNumberOfLinks != 1))
    {
        return Err(invalid(
            "managed path must have its expected type, without reparse points or multiple links",
        ));
    }
    let mut owner = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    let result = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    struct SecurityMemory(*mut c_void);
    impl Drop for SecurityMemory {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
    let _descriptor = SecurityMemory(descriptor);
    if owner.is_null() {
        return Err(invalid("managed path has no Windows owner"));
    }
    let mut raw_token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = unsafe { OwnedHandle::from_raw_handle(raw_token) };
    for class in [TokenUser, TokenOwner] {
        let mut needed = 0;
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                class,
                std::ptr::null_mut(),
                0,
                &mut needed,
            );
        }
        if needed == 0 {
            return Err(io::Error::last_os_error());
        }
        // usize storage provides the alignment needed by TOKEN_USER/TOKEN_OWNER.
        let mut buffer = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                class,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let sid = if class == TokenUser {
            unsafe { (*(buffer.as_ptr().cast::<TOKEN_USER>())).User.Sid }
        } else {
            unsafe { (*(buffer.as_ptr().cast::<TOKEN_OWNER>())).Owner }
        };
        if unsafe { EqualSid(owner, sid) } != 0 {
            return Ok(());
        }
    }
    Err(invalid(
        "managed path is not owned by the current Windows user",
    ))
}

pub(super) fn same_at(parent: &Directory, name: &str, expected: &FileIdentity) -> io::Result<bool> {
    let file = parent.open(name, Access::Read, false)?;
    Ok(FileIdentity::from_file(&file)? == *expected)
}

fn single_name(name: &str) -> io::Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0', ':']) {
        Err(invalid("expected one ordinary path component"))
    } else {
        Ok(())
    }
}
