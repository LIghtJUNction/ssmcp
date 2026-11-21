//! No-follow, handle-anchored access. No cleanup operation follows a link.
use super::{FileIdentity, ObjectIdentity, invalid};
#[cfg(windows)]
use std::path::PathBuf;
use std::{
    ffi::OsString,
    fs::File,
    io::{self, Read, Write},
    path::Path,
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
            // Windows uses the existing current-user, protected-DACL contract.
            // This is deliberately stricter than accepting a public directory.
            let _ = private;
            crate::platform::windows::verify_directory(&file)?;
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

    fn reopen(&self) -> io::Result<Self> {
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
        Ok(bytes)
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
        let _ = private;
        crate::platform::windows::verify_file(file)
    }
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
