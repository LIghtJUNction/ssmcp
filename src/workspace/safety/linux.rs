//! Read policy through checked directory descriptors. Paths and parse errors
//! stay local. Only a trusted baseline may opt into project restrictions.
use super::rules::{self, Rules};
use crate::workspace::open_at;
use std::{
    ffi::CString,
    fs::File,
    io::{self, Read},
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::{Component, Path},
};

const MAX_RULE_BYTES: u64 = 16 * 1024;

fn open(parent: &File, name: &str, flags: i32) -> io::Result<File> {
    let name = CString::new(name).map_err(io::Error::other)?;
    open_at(parent, &name, libc::O_RDONLY | libc::O_NONBLOCK | flags, 0)
}

fn safe_dir(file: &File, uid: u32, intermediate: bool) -> io::Result<()> {
    let metadata = file.metadata()?;
    let safe_owner = metadata.uid() == uid || (intermediate && metadata.uid() == 0);
    let root_sticky = intermediate && metadata.uid() == 0 && metadata.mode() & libc::S_ISVTX != 0;
    if safe_owner && (metadata.mode() & 0o022 == 0 || root_sticky) {
        Ok(())
    } else {
        Err(io::Error::from(io::ErrorKind::PermissionDenied))
    }
}

fn home_directory(path: &Path, uid: u32) -> io::Result<(File, bool)> {
    let mut parts = path.components().peekable();
    if parts.next() != Some(Component::RootDir) {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    let mut dir = File::open("/")?;
    let mut trusted_ancestors = true;
    while let Some(part) = parts.next() {
        let Component::Normal(name) = part else {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        };
        let name = name.to_str().ok_or(io::ErrorKind::InvalidInput)?;
        dir = open(&dir, name, libc::O_DIRECTORY)?;
        if parts.peek().is_some() {
            trusted_ancestors &= safe_dir(&dir, uid, true).is_ok();
        }
    }
    safe_dir(&dir, uid, false)?;
    Ok((dir, trusted_ancestors))
}

fn read_rule(root: &File, file: &str, uid: u32) -> io::Result<String> {
    safe_dir(root, uid, false)?;
    let config = open(root, ".ssmcp", libc::O_DIRECTORY)?;
    safe_dir(&config, uid, false)?;
    let directory = open(&config, "safety", libc::O_DIRECTORY)?;
    safe_dir(&directory, uid, false)?;
    // Pin the directory entry without opening it for I/O. In particular a
    // special file must not acquire device or FIFO open semantics.
    let name = CString::new(file).map_err(io::Error::other)?;
    let pinned = open_at(&directory, &name, libc::O_PATH, 0)?;
    let metadata = pinned.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.mode() & 0o022 != 0
        || metadata.nlink() != 1
        || metadata.len() > MAX_RULE_BYTES
    {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    // Reopen the inspected inode, rather than looking up the pathname
    // again. Keep the O_PATH descriptor alive through the bounded read.
    let file = File::open(format!("/proc/self/fd/{}", pinned.as_raw_fd()))?;
    let mut source = String::new();
    file.take(MAX_RULE_BYTES + 1).read_to_string(&mut source)?;
    if source.len() as u64 > MAX_RULE_BYTES {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    Ok(source)
}

pub(super) fn load(workspace: &File) -> Result<Option<Rules>, ()> {
    let home = std::env::var_os("HOME").ok_or(())?;
    load_at(Path::new(&home), workspace)
}

pub(super) fn load_at(home: &Path, workspace: &File) -> Result<Option<Rules>, ()> {
    // SAFETY: geteuid has no inputs and does not modify process state.
    let uid = unsafe { libc::geteuid() };
    let (home, trusted_ancestors) = home_directory(home, uid).map_err(|_| ())?;
    let baseline = match read_rule(&home, "baseline.toml", uid) {
        Ok(baseline) => baseline,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    if !trusted_ancestors {
        return Err(());
    }
    rules::parse(&baseline, || {
        read_rule(workspace, "rules.toml", uid).map_err(|_| ())
    })
    .map(Some)
}
