//! Linux job-local logical byte accounting, sampled by the owning command caller.
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    ffi::CString,
    fs::{self, File, Metadata},
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Path, PathBuf},
};

const MAX_ENTRIES: usize = 8192;
const MAX_DEPTH: usize = 24;
const MAX_BATCH_BYTES: usize = 256 * 1024;
const WATCH: u32 = libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_MODIFY
    | libc::IN_ATTRIB
    | libc::IN_CLOSE_WRITE
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF
    | libc::IN_ONLYDIR;

type Identity = (u64, u64);
fn identity(metadata: &Metadata) -> Identity {
    (metadata.dev(), metadata.ino())
}

struct Entry {
    identity: Identity,
    // Directories own watches. Regular files contribute logical length.
    kind: Kind,
}

enum Kind {
    Directory(i32),
    File(u64),
}

struct Ledger {
    watcher: File,
    entries: HashMap<PathBuf, Entry>,
    watches: HashMap<i32, PathBuf>,
    used: u128,
}

pub(super) struct Artifacts {
    workspace: File,
    root: File,
    relative: PathBuf,
    pub(super) path: PathBuf,
    budget: u64,
    armed: bool,
    incomplete: bool,
    ledger: Option<Ledger>,
}

// Resolve entire paths in a single syscall. Never traverse a symlink, magic
// link or mount (including bind mounts) below the opened directory.
fn beneath(directory: &File, path: &Path) -> io::Result<File> {
    open_path(
        directory.as_raw_fd(),
        path,
        libc::RESOLVE_BENEATH | libc::RESOLVE_NO_XDEV,
    )
}

fn open_path(directory: i32, path: &Path, resolve: u64) -> io::Result<File> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    let mut how: libc::open_how = unsafe { std::mem::zeroed() };
    how.flags = (libc::O_PATH | libc::O_CLOEXEC) as u64;
    how.resolve = resolve | libc::RESOLVE_NO_SYMLINKS;
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            directory,
            path.as_ptr(),
            &how,
            std::mem::size_of_val(&how),
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd as i32) })
}

fn proc_fd(file: &File) -> String {
    format!("/proc/self/fd/{}", file.as_raw_fd())
}

impl Artifacts {
    pub(super) fn new(workspace: &Path, id: &str, budget: u64) -> io::Result<Self> {
        let relative = PathBuf::from(format!(".ssmcp-artifacts-{id}"));
        let path = workspace.join(&relative);
        // The caller's workspace path was canonicalized. Refuse any symlink
        // inserted into that path since authorization.
        let workspace = open_path(libc::AT_FDCWD, workspace, 0)?;
        let mut ledger = Ledger::new()?;
        let name = CString::new(relative.as_os_str().as_bytes())?;
        if unsafe { libc::mkdirat(workspace.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let root = beneath(&workspace, &relative)?;
        // Watch before spawning, even when the directory is empty.
        ledger.refresh(&root, Path::new(""), &mut { MAX_ENTRIES })?;
        Ok(Self {
            workspace,
            root,
            relative,
            path,
            budget,
            armed: true,
            incomplete: false,
            ledger: Some(ledger),
        })
    }

    pub(super) fn description(&self) -> Value {
        json!({
            "directory": self.relative.to_string_lossy(),
            "soft_limit_bytes": self.budget,
        })
    }

    pub(super) fn sample(&mut self) -> Value {
        let still_owned = (|| -> io::Result<bool> {
            let current = beneath(&self.workspace, &self.relative)?;
            Ok(identity(&current.metadata()?) == identity(&self.root.metadata()?))
        })()
        .unwrap_or(false);

        if !still_owned {
            self.incomplete = true;
            self.ledger = None;
        } else if !self
            .ledger
            .as_mut()
            .is_some_and(|ledger| ledger.update(&self.root).is_ok())
        {
            // A truncated event batch, unknown watch state or overflow loses
            // coverage. Recover once per sample, and keep the gap visible.
            self.incomplete = true;
            self.ledger = (|| {
                let mut rebuilt = Ledger::new()?;
                rebuilt.refresh(&self.root, Path::new(""), &mut { MAX_ENTRIES })?;
                Ok::<_, io::Error>(rebuilt)
            })()
            .ok();
        }

        let mut result = self.description();
        result["incomplete"] = json!(self.incomplete);
        result["logical_bytes"] = json!(
            self.ledger
                .as_ref()
                .map(|ledger| ledger.used.min(u64::MAX as u128) as u64)
        );
        if let Some(ledger) = &self.ledger {
            let used = ledger.used;
            if used <= u128::from(self.budget) * 4 / 5 {
                self.armed = true;
            } else if used > u128::from(self.budget) && self.armed {
                self.armed = false;
                result["notice"] = json!({
                    "type": "ssmcp.exec.artifact_budget_exceeded",
                    "logical_bytes": result["logical_bytes"],
                    "soft_limit_bytes": self.budget,
                    "largest_files": ledger.largest(),
                    "advice": "Review only artifacts you have confirmed can be rebuilt."
                });
            }
        }
        result
    }
}

impl Ledger {
    fn new() -> io::Result<Self> {
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            watcher: unsafe { File::from_raw_fd(fd) },
            entries: HashMap::new(),
            watches: HashMap::new(),
            used: 0,
        })
    }

    fn remove_one(&mut self, path: &Path) {
        if let Some(entry) = self.entries.remove(path) {
            match entry.kind {
                Kind::File(bytes) => self.used -= u128::from(bytes),
                Kind::Directory(watch) => {
                    self.watches.remove(&watch);
                    unsafe { libc::inotify_rm_watch(self.watcher.as_raw_fd(), watch) };
                }
            }
        }
    }

    fn remove(&mut self, path: &Path) {
        if self
            .entries
            .get(path)
            .is_some_and(|entry| matches!(entry.kind, Kind::Directory(_)))
        {
            let descendants: Vec<_> = self
                .entries
                .keys()
                .filter(|entry| entry.starts_with(path))
                .cloned()
                .collect();
            for entry in descendants {
                self.remove_one(&entry);
            }
        } else {
            self.remove_one(path);
        }
    }

    fn refresh(&mut self, root: &File, path: &Path, remaining: &mut usize) -> io::Result<()> {
        *remaining = remaining.checked_sub(1).ok_or_else(bound)?;
        if path.components().count() > MAX_DEPTH {
            return Err(bound());
        }
        let file = if path.as_os_str().is_empty() {
            root.try_clone()
        } else {
            beneath(root, path)
        };
        let file = match file {
            Ok(file) => file,
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP | libc::EXDEV)
                ) =>
            {
                self.remove(path);
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        let id = identity(&metadata);
        if metadata.is_dir() {
            if self.entries.get(path).is_some_and(|entry| {
                entry.identity == id && matches!(entry.kind, Kind::Directory(_))
            }) {
                return Ok(());
            }
            self.remove(path);
            if self.entries.len() >= MAX_ENTRIES {
                return Err(bound());
            }
            let location = CString::new(proc_fd(&file))?;
            let watch = unsafe {
                libc::inotify_add_watch(self.watcher.as_raw_fd(), location.as_ptr(), WATCH)
            };
            if watch < 0 {
                return Err(io::Error::last_os_error());
            }
            if self.watches.contains_key(&watch) {
                // A directory moved while its old path was still in the ledger.
                // Rebuild from the root with a fresh watch generation.
                return Err(io::Error::other("artifact directory watch changed paths"));
            }
            self.watches.insert(watch, path.to_owned());
            self.entries.insert(
                path.to_owned(),
                Entry {
                    identity: id,
                    kind: Kind::Directory(watch),
                },
            );
            // Register first so writes during discovery are queued for the next
            // batch. Resolve every entry again through the original root fd.
            for entry in fs::read_dir(proc_fd(&file))? {
                self.refresh(root, &path.join(entry?.file_name()), remaining)?;
            }
        } else if metadata.is_file() && metadata.nlink() == 1 {
            self.remove(path);
            if self.entries.len() >= MAX_ENTRIES {
                return Err(bound());
            }
            let bytes = metadata.len();
            self.used += u128::from(bytes);
            self.entries.insert(
                path.to_owned(),
                Entry {
                    identity: id,
                    kind: Kind::File(bytes),
                },
            );
        } else {
            self.remove(path);
        }
        Ok(())
    }

    fn update(&mut self, root: &File) -> io::Result<()> {
        let mut paths = BTreeMap::new();
        let mut buffer = [0u8; 16 * 1024];
        let mut read = 0;
        loop {
            let count = match self.watcher.read(&mut buffer) {
                Ok(count) if count > 0 => count,
                Ok(_) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            };
            read += count;
            if read > MAX_BATCH_BYTES {
                return Err(bound());
            }
            let mut events = &buffer[..count];
            while !events.is_empty() {
                let header = std::mem::size_of::<libc::inotify_event>();
                if events.len() < header {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                let event = unsafe {
                    events
                        .as_ptr()
                        .cast::<libc::inotify_event>()
                        .read_unaligned()
                };
                let length = header + event.len as usize;
                if length > events.len() || event.mask & libc::IN_Q_OVERFLOW != 0 {
                    return Err(io::Error::other("artifact event queue lost coverage"));
                }
                if let Some(parent) = self.watches.get(&event.wd) {
                    let name = events[header..length]
                        .split(|byte| *byte == 0)
                        .next()
                        .unwrap();
                    let path = if name.is_empty() {
                        parent.clone()
                    } else {
                        parent.join(std::ffi::OsStr::from_bytes(name))
                    };
                    *paths.entry(path).or_insert(0) |= event.mask;
                }
                events = &events[length..];
            }
        }
        let mut remaining = MAX_ENTRIES;
        // Retire stale directory registrations before adding move destinations.
        // A path which now resolves to the same directory keeps its watch.
        for (path, mask) in &paths {
            let moved =
                mask & libc::IN_ISDIR != 0 && mask & (libc::IN_MOVED_FROM | libc::IN_DELETE) != 0;
            if mask & libc::IN_IGNORED != 0 {
                if path.as_os_str().is_empty() {
                    return Err(io::Error::other("artifact root watch was removed"));
                }
                self.remove(path);
            } else if moved
                && let Some(entry) = self.entries.get(path)
                && matches!(entry.kind, Kind::Directory(_))
                && !beneath(root, path)
                    .and_then(|file| file.metadata())
                    .is_ok_and(|meta| identity(&meta) == entry.identity)
            {
                self.remove(path);
            }
        }
        for path in paths.keys() {
            self.refresh(root, path, &mut remaining)?;
        }
        Ok(())
    }

    fn largest(&self) -> Vec<Value> {
        let mut largest = Vec::with_capacity(4);
        for (path, entry) in &self.entries {
            if let Kind::File(bytes) = entry.kind {
                largest.push((bytes, path));
                largest.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
                largest.truncate(3);
            }
        }
        largest
            .into_iter()
            .map(|(bytes, path)| {
                let mut label: String = path
                    .to_string_lossy()
                    .chars()
                    .take(240)
                    .map(|c| if c.is_control() { '\u{fffd}' } else { c })
                    .collect();
                if path.to_string_lossy().chars().count() > 240 {
                    label.push('…');
                }
                json!({"relative_path": label, "logical_bytes": bytes})
            })
            .collect()
    }
}

fn bound() -> io::Error {
    io::Error::other("artifact tracking bound exceeded")
}
