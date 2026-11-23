//! Read policy through checked directory descriptors. Paths and parse errors
//! stay local. Only a trusted baseline may opt into project restrictions.
use super::rules::{self, LoadError, Rules};
use crate::workspace::open_at;
use std::{
    ffi::CString,
    fs::{File, Metadata},
    io::{self, Read},
    os::unix::fs::MetadataExt,
    path::{Component, Path},
};

const MAX_RULE_BYTES: u64 = 16 * 1024;

fn open(parent: &File, name: &str, flags: i32) -> io::Result<File> {
    let name = CString::new(name).map_err(io::Error::other)?;
    // open_at always adds O_NOFOLLOW and O_CLOEXEC.
    open_at(parent, &name, libc::O_RDONLY | libc::O_NONBLOCK | flags, 0)
}

fn safe_dir(file: &File, uid: u32, intermediate: bool) -> io::Result<()> {
    let metadata = file.metadata()?;
    let safe_owner = metadata.uid() == uid || (intermediate && metadata.uid() == 0);
    let root_sticky = intermediate && metadata.uid() == 0 && metadata.mode() & libc::S_ISVTX != 0;
    if metadata.is_dir() && safe_owner && (metadata.mode() & 0o022 == 0 || root_sticky) {
        Ok(())
    } else {
        Err(io::Error::from(io::ErrorKind::PermissionDenied))
    }
}

struct HomeDirectory {
    chain: Vec<File>,
}

impl HomeDirectory {
    fn open(path: &Path) -> io::Result<Self> {
        let mut parts = path.components();
        if parts.next() != Some(Component::RootDir) {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        let mut chain = vec![File::open("/")?];
        for part in parts {
            let Component::Normal(name) = part else {
                return Err(io::Error::from(io::ErrorKind::InvalidInput));
            };
            let name = name.to_str().ok_or(io::ErrorKind::InvalidInput)?;
            chain.push(open(chain.last().unwrap(), name, libc::O_DIRECTORY)?);
        }
        Ok(Self { chain })
    }

    fn root(&self) -> &File {
        self.chain.last().unwrap()
    }

    fn validate(&self, uid: u32) -> io::Result<()> {
        for (index, dir) in self.chain.iter().enumerate() {
            safe_dir(dir, uid, index + 1 < self.chain.len())?;
        }
        Ok(())
    }
}

/// atime is excluded because reading can update it. ctime also detects in-place
/// changes whose mtime or length was restored by the writer.
#[derive(Debug, Eq, PartialEq)]
struct Version {
    dev: u64,
    ino: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    nlink: u64,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl Version {
    fn checked(metadata: Metadata, uid: u32) -> io::Result<Self> {
        if !metadata.is_file()
            || metadata.uid() != uid
            || metadata.mode() & 0o022 != 0
            || metadata.nlink() != 1
            || metadata.len() > MAX_RULE_BYTES
        {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        Ok(Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            mode: metadata.mode(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            nlink: metadata.nlink(),
            len: metadata.len(),
            mtime: (metadata.mtime(), metadata.mtime_nsec()),
            ctime: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }
}

struct RuleFile<'a> {
    root: &'a File,
    config: File,
    directory: File,
    file: File,
    version: Version,
}

impl<'a> RuleFile<'a> {
    fn open(root: &'a File, name: &str, uid: u32) -> io::Result<Self> {
        // First discover the candidate without following links. An absent
        // baseline must not turn an otherwise unconfigured HOME into a denial.
        let config = open(root, ".ssmcp", libc::O_DIRECTORY)?;
        let directory = open(&config, "safety", libc::O_DIRECTORY)?;
        let file = open(&directory, name, 0)?;
        safe_dir(root, uid, false)?;
        safe_dir(&config, uid, false)?;
        safe_dir(&directory, uid, false)?;
        let version = Version::checked(file.metadata()?, uid)?;
        Ok(Self {
            root,
            config,
            directory,
            file,
            version,
        })
    }

    fn validate(&self, uid: u32) -> io::Result<()> {
        safe_dir(self.root, uid, false)?;
        safe_dir(&self.config, uid, false)?;
        safe_dir(&self.directory, uid, false)?;
        if Version::checked(self.file.metadata()?, uid)? != self.version {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        Ok(())
    }

    fn read(
        &mut self,
        name: &str,
        uid: u32,
        after_read: &mut impl FnMut(&str),
    ) -> io::Result<String> {
        let mut source = String::new();
        (&mut self.file)
            .take(MAX_RULE_BYTES + 1)
            .read_to_string(&mut source)?;
        after_read(name);
        self.validate(uid)?;
        if source.len() as u64 != self.version.len || source.len() as u64 > MAX_RULE_BYTES {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        Ok(source)
    }
}

pub(super) fn load(workspace: &File) -> Result<Option<Rules>, LoadError> {
    let home = std::env::var_os("HOME");
    load_at(home.as_deref().map(Path::new), workspace)
}

pub(super) fn load_at(home: Option<&Path>, workspace: &File) -> Result<Option<Rules>, LoadError> {
    load_at_with(home, workspace, &mut |_| {})
}

// The hook is a deterministic test seam after bytes have been read, before
// their descriptor version/trust is revalidated. Production supplies a no-op.
pub(super) fn load_at_with(
    home: Option<&Path>,
    workspace: &File,
    after_read: &mut impl FnMut(&str),
) -> Result<Option<Rules>, LoadError> {
    let Some(home) = home else { return Ok(None) };
    // SAFETY: geteuid has no inputs and does not modify process state.
    let uid = unsafe { libc::geteuid() };
    let home = match HomeDirectory::open(home) {
        Ok(home) => home,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(LoadError::default()),
    };
    let mut baseline = match RuleFile::open(home.root(), "baseline.toml", uid) {
        Ok(baseline) => baseline,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(LoadError::default()),
    };
    home.validate(uid).map_err(|_| LoadError::default())?;
    let source = baseline
        .read("baseline.toml", uid, after_read)
        .map_err(|_| LoadError::default())?;
    let mut project = None;
    let parsed = rules::parse(&source, || {
        let mut file = RuleFile::open(workspace, "rules.toml", uid).map_err(|_| ())?;
        let source = file.read("rules.toml", uid, after_read).map_err(|_| ())?;
        project = Some(file);
        Ok(source)
    });

    // Keep both descriptors until publication. A project read must not mask a
    // baseline changed in place while the subordinate file was being loaded.
    home.validate(uid)
        .and_then(|()| baseline.validate(uid))
        .map_err(|_| LoadError::default())?;
    if let Some(project) = project {
        project.validate(uid).map_err(|_| LoadError {
            baseline: parsed
                .as_ref()
                .ok()
                .copied()
                .map(Rules::baseline_only)
                .or_else(|| parsed.as_ref().err().and_then(|error| error.baseline)),
        })?;
    }
    parsed.map(Some)
}
