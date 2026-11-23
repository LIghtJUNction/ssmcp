//! Shutdown reclamation for a live, explicitly temporary job record.
use super::{Artifacts, Identity, beneath, identity, proc_fd};
use std::{
    ffi::{CString, OsString},
    fs::{self, File},
    io,
    os::{
        fd::AsRawFd,
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::Path,
    time::{Duration, Instant},
};

pub(in super::super) const CLEANUP_TIME: Duration = Duration::from_millis(750);
const MAX_CLEANUP_ENTRIES: usize = 2048;

struct Entry {
    name: OsString,
    id: Identity,
    kind: Kind,
}

enum Kind {
    Directory(Vec<Entry>),
    File {
        bytes: u64,
        ctime: (i64, i64),
        lease: File,
    },
}

#[derive(Default)]
struct Removed {
    files: usize,
    bytes: u128,
}

fn live(deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        Err(io::ErrorKind::TimedOut.into())
    } else {
        Ok(())
    }
}

fn changed() -> io::Error {
    io::Error::other("artifact identity or ownership could not be verified")
}

// Linux asm-generic fcntl ABI on the supported x86 and ARM targets.
// Thread-directed SIGIO must not interrupt any other application thread.
#[cfg(any(
    target_arch = "x86",
    target_arch = "x86_64",
    target_arch = "arm",
    target_arch = "aarch64"
))]
fn lease(file: &File) -> io::Result<File> {
    const F_SETOWN_EX: libc::c_int = 15;
    const F_OWNER_TID: libc::c_int = 0;

    use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt};

    let lease = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(proc_fd(file))?;
    let mut signals: libc::sigset_t = unsafe { std::mem::zeroed() };
    // The detached reclamation thread never unblocks SIGIO. Pending signals
    // are discarded at thread exit after all leased file descriptions close.
    let blocked = unsafe {
        libc::sigemptyset(&mut signals);
        libc::sigaddset(&mut signals, libc::SIGIO);
        libc::pthread_sigmask(libc::SIG_BLOCK, &signals, std::ptr::null_mut())
    };
    if blocked != 0 {
        return Err(io::Error::from_raw_os_error(blocked));
    }
    // f_owner_ex is two consecutive C ints on these Linux ABIs.
    let owner = [F_OWNER_TID, unsafe { libc::gettid() }];
    let fd = lease.as_raw_fd();
    if unsafe { libc::fcntl(fd, F_SETOWN_EX, owner.as_ptr()) } == -1
        || unsafe { libc::fcntl(fd, libc::F_SETLEASE, libc::F_WRLCK) } == -1
    {
        return Err(io::Error::last_os_error());
    }
    Ok(lease)
}

#[cfg(not(any(
    target_arch = "x86",
    target_arch = "x86_64",
    target_arch = "arm",
    target_arch = "aarch64"
)))]
fn lease(_file: &File) -> io::Result<File> {
    Err(io::ErrorKind::Unsupported.into())
}

impl Entry {
    fn inspect(
        parent: &File,
        name: OsString,
        depth: usize,
        remaining: &mut usize,
        deadline: Instant,
    ) -> io::Result<Self> {
        live(deadline)?;
        if depth > super::MAX_DEPTH {
            return Err(super::bound());
        }
        *remaining = remaining.checked_sub(1).ok_or_else(super::bound)?;
        let file = beneath(parent, Path::new(&name))?;
        let meta = file.metadata()?;
        let kind = if meta.is_dir() {
            let mut children = Vec::new();
            for child in fs::read_dir(proc_fd(&file))? {
                children.push(Self::inspect(
                    &file,
                    child?.file_name(),
                    depth + 1,
                    remaining,
                    deadline,
                )?);
            }
            Kind::Directory(children)
        } else if meta.is_file() && meta.nlink() == 1 {
            Kind::File {
                bytes: meta.len(),
                ctime: (meta.ctime(), meta.ctime_nsec()),
                lease: lease(&file)?,
            }
        } else {
            return Err(changed());
        };
        Ok(Self {
            name,
            id: identity(&meta),
            kind,
        })
    }

    fn remove(
        &self,
        workspace: &File,
        parent_path: &Path,
        parent: &File,
        deadline: Instant,
        removed: &mut Removed,
    ) -> io::Result<()> {
        live(deadline)?;
        let full_path = parent_path.join(&self.name);
        let file = beneath(workspace, &full_path)?;
        let meta = file.metadata()?;
        if identity(&meta) != self.id {
            return Err(changed());
        }
        let flags = match &self.kind {
            Kind::Directory(children) if meta.is_dir() => {
                for child in children {
                    child.remove(workspace, &full_path, &file, deadline, removed)?;
                }
                libc::AT_REMOVEDIR
            }
            Kind::File {
                bytes,
                ctime,
                lease,
            } if (unsafe { libc::fcntl(lease.as_raw_fd(), libc::F_GETLEASE) }) == libc::F_WRLCK
                && meta.is_file()
                && meta.nlink() == 1
                && meta.len() == *bytes
                && (meta.ctime(), meta.ctime_nsec()) == *ctime =>
            {
                0
            }
            _ => return Err(changed()),
        };
        // Check the name again after walking descendants. Unlink is always
        // relative to its verified parent fd and cannot follow a symlink.
        if identity(&beneath(workspace, parent_path)?.metadata()?) != identity(&parent.metadata()?)
            || identity(&beneath(parent, Path::new(&self.name))?.metadata()?) != self.id
        {
            return Err(changed());
        }
        let name = CString::new(self.name.as_bytes())?;
        live(deadline)?;
        if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flags) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if let Kind::File { bytes, .. } = &self.kind {
            removed.files += 1;
            removed.bytes += u128::from(*bytes);
        }
        Ok(())
    }
}

fn rename_noreplace(directory: &File, old: &Path, new: &Path) -> io::Result<()> {
    let old = CString::new(old.as_os_str().as_bytes())?;
    let new = CString::new(new.as_os_str().as_bytes())?;
    let fd = directory.as_raw_fd();
    if unsafe { libc::renameat2(fd, old.as_ptr(), fd, new.as_ptr(), libc::RENAME_NOREPLACE) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl Artifacts {
    pub(in super::super) fn reclaim(mut self, deadline: Instant) {
        let mut removed = Removed::default();
        let result = self.reclaim_inner(deadline, &mut removed);
        match result {
            Ok(()) => eprintln!(
                "ssmcp: reclaimed temporary artifacts {} ({} files, {} logical bytes)",
                self.relative.display(),
                removed.files,
                removed.bytes
            ),
            Err(error) => eprintln!(
                "ssmcp: preserving remaining temporary artifacts {} after cleanup stopped ({} files, {} logical bytes reclaimed): {error}",
                self.relative.display(),
                removed.files,
                removed.bytes
            ),
        }
    }

    fn reclaim_inner(&mut self, deadline: Instant, removed: &mut Removed) -> io::Result<()> {
        live(deadline)?;
        let original = Path::new(self.relative.file_name().expect("task token"));
        let relative_parent = self.relative.parent().expect("managed parent");
        let parent = beneath(&self.workspace, relative_parent)?;
        let expected = identity(&self.root.metadata()?);
        if identity(&beneath(&parent, original)?.metadata()?) != expected {
            return Err(changed());
        }
        // Never recover lost coverage as part of authorization to delete.
        if self.incomplete
            || !self
                .ledger
                .as_mut()
                .is_some_and(|ledger| ledger.update(&self.root).is_ok())
        {
            return Err(io::Error::other("artifact ledger is incomplete"));
        }
        let mut tree = Entry::inspect(
            &parent,
            original.as_os_str().to_owned(),
            0,
            &mut { MAX_CLEANUP_ENTRIES },
            deadline,
        )?;
        if tree.id != expected {
            return Err(changed());
        }

        // Move the verified task out of the normal namespace before unlinking
        // anything. Never replace another name, including a stale quarantine.
        let quarantine = OsString::from(format!(".reclaim-{}", original.display()));
        let target = Path::new(&quarantine);
        live(deadline)?;
        if identity(&beneath(&self.workspace, relative_parent)?.metadata()?)
            != identity(&parent.metadata()?)
            || identity(&beneath(&parent, original)?.metadata()?) != expected
        {
            return Err(changed());
        }
        rename_noreplace(&parent, original, target)?;
        tree.name = quarantine.clone();
        if let Err(error) =
            tree.remove(&self.workspace, relative_parent, &parent, deadline, removed)
        {
            // Keep the quarantine if the original name was taken or time ran
            // out, and record the actual remaining path.
            let restored = (|| -> io::Result<()> {
                live(deadline)?;
                let current_parent = beneath(&self.workspace, relative_parent)?;
                if identity(&current_parent.metadata()?) != identity(&parent.metadata()?)
                    || identity(&beneath(&parent, target)?.metadata()?) != expected
                {
                    return Err(changed());
                }
                live(deadline)?;
                rename_noreplace(&parent, target, original)
            })();
            if restored.is_err() {
                self.relative.set_file_name(quarantine);
            }
            return Err(error);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_quarantine_can_be_retried_while_the_original_owner_is_live() {
        std::thread::spawn(retry_fixture).join().unwrap();
    }

    fn retry_fixture() {
        let workspace = std::env::temp_dir().join(format!("ssmcp-reclaim-{}", std::process::id()));
        fs::create_dir(&workspace).unwrap();
        let mut artifacts = Artifacts::new(&workspace, 20).unwrap();
        fs::write(artifacts.path.join("payload"), "bytes").unwrap();
        let quarantine = artifacts.path.with_file_name(format!(
            ".reclaim-{}",
            artifacts.path.file_name().unwrap().to_string_lossy()
        ));
        fs::create_dir(&quarantine).unwrap();
        let mut removed = Removed::default();
        let deadline = || Instant::now() + Duration::from_secs(2);
        assert_eq!(
            artifacts
                .reclaim_inner(deadline(), &mut removed)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(removed.files, 0);
        assert_eq!(fs::read(artifacts.path.join("payload")).unwrap(), b"bytes");
        assert!(quarantine.is_dir());
        fs::remove_dir(&quarantine).unwrap();
        artifacts.reclaim_inner(deadline(), &mut removed).unwrap();
        assert_eq!((removed.files, removed.bytes), (1, 5));
        assert!(!artifacts.path.exists());
        fs::remove_dir_all(workspace).unwrap();
    }
}
