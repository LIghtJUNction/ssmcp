//! Bounded inline output, streaming spill files, and retention for client-owned logs.
//!
//! Files are private to the local UID. A writer holds an advisory lock until its
//! capture is finalized; cleanup only removes recognized, unlocked artifacts.
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
#[cfg(any(unix, test))]
use std::path::Path;
use std::{
    fs::File,
    io::{self, Read, Write},
    path::PathBuf,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(windows)]
use crate::platform::windows;
#[cfg(windows)]
use std::os::windows::fs::FileExt;
#[cfg(unix)]
use std::{
    ffi::CString,
    fs::OpenOptions,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
};
#[cfg(unix)]
type Directory = File;
#[cfg(windows)]
type Directory = cap_std::fs::Dir;

#[derive(Clone, Copy)]
enum Access {
    Read,
    ReadWrite,
    CreateReadWrite,
    CreateNewWrite,
    CreateNewReadWrite,
}

pub const INLINE_BYTES: usize = 32 * 1024;
/// Screenshot blocks travel as images, independently of the text spill budget.
pub const MAX_IMAGE_BYTES: usize = 400 * 1024;
pub const MAX_INLINE_IMAGE_RESULT_BYTES: usize = 1024 * 1024;
pub const INLINE_LINES: u64 = 500;
pub const RETENTION_SECS: u64 = 24 * 60 * 60;
pub const RETAINED_BYTES: u64 = 512 * 1024 * 1024;
pub const ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;
pub const READ_BYTES: usize = 32 * 1024;
const SCHEMA: &str = "ssmcp.output.v1";
const JANITOR_SECS: u64 = 60;
const MAX_MANIFEST_BYTES: u64 = 16 * 1024;
const RELOAD_ENV: &str = "SSMCP_OUTPUT_RELOAD";
const RELOAD_SCHEMA: &str = "ssmcp.output-reload.v1";
const RELOAD_TTL: u64 = 60;
static RELOAD_CHECKED: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
struct Policy {
    inline_bytes: usize,
    inline_lines: u64,
    retention_secs: u64,
    retained_bytes: u64,
    artifact_bytes: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            inline_bytes: INLINE_BYTES,
            inline_lines: INLINE_LINES,
            retention_secs: RETENTION_SECS,
            retained_bytes: RETAINED_BYTES,
            artifact_bytes: ARTIFACT_BYTES,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactMetadata {
    pub schema: String,
    pub id: String,
    pub path: String,
    /// Identifies the source; a terminal capture is the whole PTY session.
    pub scope: String,
    /// Digest covers exactly the first `bytes` bytes in the file at this snapshot.
    pub sha256: String,
    pub sha256_scope: String,
    pub bytes: u64,
    /// Newline-delimited records, including a final unterminated record.
    pub lines: u64,
    pub observed_bytes: u64,
    pub complete: bool,
    pub expires_at_unix: u64,
    pub retention_secs: u64,
    pub max_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    #[serde(flatten)]
    metadata: ArtifactMetadata,
    // Legacy manifests remain eligible for automatic retention, never for a new client.
    #[serde(default)]
    owner: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReloadGrant {
    schema: String,
    owner: String,
    expires_at_unix: u64,
}

/// Keeps a one-use reload grant alive until the replacement consumes it.
pub struct ReloadTicket {
    store: Arc<OutputStore>,
    name: String,
}

impl Drop for ReloadTicket {
    fn drop(&mut self) {
        self.store.unlink(&self.name);
    }
}

pub struct OutputStore {
    owner: String,
    directory: PathBuf,
    directory_fd: Directory,
    policy: Policy,
}

impl OutputStore {
    /// Compatibility accessor for a single foreground connection.
    pub fn global() -> Result<Arc<Self>, String> {
        static STORE: OnceLock<Result<Arc<OutputStore>, String>> = OnceLock::new();
        STORE.get_or_init(Self::new_connection).clone()
    }

    /// Every connection gets a distinct owner. Acknowledged reload is the only
    /// path that restores ownership, using a private, expiring, one-use grant.
    pub fn new_connection() -> Result<Arc<Self>, String> {
        let reload = if !RELOAD_CHECKED.swap(true, Ordering::AcqRel) {
            std::env::var(RELOAD_ENV).ok()
        } else {
            None
        };
        Self::at_restoring(Self::directory()?, Policy::default(), reload.as_deref())
    }

    fn directory() -> Result<PathBuf, String> {
        #[cfg(unix)]
        {
            let base = Path::new("/tmp/ssmcp");
            match std::fs::DirBuilder::new().mode(0o1777).create(base) {
                Ok(()) => std::fs::set_permissions(base, std::fs::Permissions::from_mode(0o1777))
                    .map_err(error)?,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(error(e)),
            }
            let base_fd = open_directory(base).map_err(error)?;
            let metadata = base_fd.metadata().map_err(error)?;
            let uid = unsafe { libc::geteuid() };
            if (metadata.uid() != 0 && metadata.uid() != uid)
                || (metadata.mode() & 0o002 != 0 && metadata.mode() & 0o1000 == 0)
            {
                return Err(
                    "output directory /tmp/ssmcp has unsafe ownership or permissions".into(),
                );
            }
            Ok(base.join(uid.to_string()))
        }
        #[cfg(windows)]
        {
            let base = std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir)
                .join("ssmcp");
            std::fs::create_dir_all(&base).map_err(error)?;
            Ok(base.join("output"))
        }
    }

    #[cfg(test)]
    fn at(directory: PathBuf, policy: Policy) -> Result<Arc<Self>, String> {
        Self::at_restoring(directory, policy, None)
    }

    fn at_restoring(
        directory: PathBuf,
        policy: Policy,
        reload: Option<&str>,
    ) -> Result<Arc<Self>, String> {
        #[cfg(windows)]
        let directory_fd = windows::private_directory(&directory).map_err(error)?;
        #[cfg(unix)]
        let directory_fd = {
            match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(error(e)),
            }
            let directory_fd = open_directory(&directory).map_err(error)?;
            let metadata = directory_fd.metadata().map_err(error)?;
            if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
                return Err(
                    "output directory must be owned by this user with private permissions".into(),
                );
            }
            directory_fd
        };
        let mut store = Self {
            owner: random_id()?,
            directory,
            directory_fd,
            policy,
        };
        if let Some(token) = reload
            && let Some(owner) = store.consume_reload(token)?
        {
            store.owner = owner;
        }
        let store = Arc::new(store);
        store.cleanup_all()?;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let weak = Arc::downgrade(&store);
            runtime.spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(JANITOR_SECS)).await;
                    let Some(store) = weak.upgrade() else { break };
                    // Directory scans and filesystem I/O must not stall the async reactor.
                    let _ = tokio::task::spawn_blocking(move || store.cleanup_all()).await;
                }
            });
        }
        Ok(store)
    }

    /// Call only after the broker has confirmed the reload acknowledgement.
    /// The ticket is not a tool result and must never be logged.
    pub fn prepare_reload(
        self: &Arc<Self>,
        command: &mut std::process::Command,
    ) -> Result<ReloadTicket, String> {
        let _guard = self.directory_lock()?;
        for _ in 0..8 {
            let token = random_id()?;
            let name = format!("reload-{token}.json");
            let mut file = match self.open(&name, Access::CreateNewWrite) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(error(e)),
            };
            let grant = ReloadGrant {
                schema: RELOAD_SCHEMA.into(),
                owner: self.owner.clone(),
                expires_at_unix: now().saturating_add(RELOAD_TTL),
            };
            if let Err(e) = serde_json::to_writer(&mut file, &grant)
                .map_err(error)
                .and_then(|_| file.flush().map_err(error))
            {
                self.unlink(&name);
                return Err(e);
            }
            command.env(RELOAD_ENV, token);
            return Ok(ReloadTicket {
                store: self.clone(),
                name,
            });
        }
        Err("could not create an output reload grant".into())
    }

    fn consume_reload(&self, token: &str) -> Result<Option<String>, String> {
        validate_id(token).map_err(|_| "invalid output reload grant".to_owned())?;
        let _guard = self.directory_lock()?;
        let name = format!("reload-{token}.json");
        let file = match self.open(&name, Access::Read) {
            Ok(file) => file,
            // A spent token inherited by a later child grants no old output access.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(error(e)),
        };
        if file.metadata().map_err(error)?.len() > MAX_MANIFEST_BYTES {
            return Err("output reload grant is oversized".into());
        }
        let grant: ReloadGrant =
            serde_json::from_reader(file).map_err(|_| "invalid output reload grant".to_owned())?;
        if !self.unlink(&name) {
            return Err("could not consume the output reload grant".into());
        }
        if grant.schema != RELOAD_SCHEMA
            || validate_id(&grant.owner).is_err()
            || grant.expires_at_unix <= now()
        {
            return Err("output reload grant is invalid or expired".into());
        }
        Ok(Some(grant.owner))
    }

    pub fn capture(self: &Arc<Self>, scope: impl Into<String>) -> Capture {
        Capture {
            store: self.clone(),
            scope: scope.into(),
            inline: Vec::new(),
            artifact: None,
            digest: Sha256::new(),
            bytes: 0,
            newlines: 0,
            last: None,
            observed_bytes: 0,
            finished: false,
            failure: None,
            modified: now(),
        }
    }

    fn open(&self, name: &str, access: Access) -> io::Result<File> {
        #[cfg(windows)]
        {
            use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
            let mut options = cap_std::fs::OpenOptions::new();
            options.follow(FollowSymlinks::No);
            match access {
                Access::Read => {
                    options.read(true);
                }
                Access::ReadWrite => {
                    options.read(true).write(true);
                }
                Access::CreateReadWrite => {
                    options.read(true).write(true).create(true);
                }
                Access::CreateNewWrite => {
                    options.write(true).create_new(true);
                }
                Access::CreateNewReadWrite => {
                    options.read(true).write(true).create_new(true);
                }
            }
            let file = self.directory_fd.open_with(name, &options)?.into_std();
            windows::verify_file(&file)?;
            Ok(file)
        }
        #[cfg(unix)]
        {
            let flags = match access {
                Access::Read => libc::O_RDONLY,
                Access::ReadWrite => libc::O_RDWR,
                Access::CreateReadWrite => libc::O_RDWR | libc::O_CREAT,
                Access::CreateNewWrite => libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                Access::CreateNewReadWrite => libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            };
            let name = CString::new(name).map_err(io::Error::other)?;
            // SAFETY: the directory fd is live; names are single, checked components.
            let fd = unsafe {
                libc::openat(
                    self.directory_fd.as_raw_fd(),
                    name.as_ptr(),
                    flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                    0o600,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: openat returned a newly owned descriptor.
            let file = unsafe { File::from_raw_fd(fd) };
            let metadata = file.metadata()?;
            if !metadata.is_file()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o077 != 0
                || metadata.nlink() != 1
            {
                return Err(io::Error::other(
                    "output artifact is not a private, singly linked regular file",
                ));
            }
            Ok(file)
        }
    }

    fn directory_lock(&self) -> Result<File, String> {
        let file = self
            .open(".cleanup.lock", Access::CreateReadWrite)
            .map_err(error)?;
        lock(&file, false).map_err(error)?;
        Ok(file)
    }

    fn create(&self, scope: &str) -> Result<Artifact, String> {
        self.cleanup_all()?;
        let _guard = self.directory_lock()?;
        for _ in 0..8 {
            let mut random = [0u8; 16];
            crate::platform::random_bytes(&mut random).map_err(error)?;
            let id: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
            let name = artifact_name(&id);
            // Publish a recognizable manifest first. A crash between these two
            // creates can leave only a tiny manifest, never an unowned log that
            // cleanup cannot identify safely.
            let initial = ArtifactMetadata {
                schema: SCHEMA.into(),
                id: id.clone(),
                path: self.directory.join(&name).to_string_lossy().into_owned(),
                scope: scope.into(),
                sha256: format!("{:x}", Sha256::digest(b"")),
                sha256_scope: "first_bytes_snapshot".into(),
                bytes: 0,
                lines: 0,
                observed_bytes: 0,
                complete: false,
                expires_at_unix: now().saturating_add(self.policy.retention_secs),
                retention_secs: self.policy.retention_secs,
                max_bytes: self.policy.artifact_bytes,
                error: None,
            };
            let mut manifest = match self.open(&manifest_name(&id), Access::CreateNewWrite) {
                Ok(file) => file,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(error(e)),
            };
            if let Err(error) = serde_json::to_writer(
                &mut manifest,
                &Manifest {
                    metadata: initial,
                    owner: self.owner.clone(),
                },
            ) {
                self.unlink(&manifest_name(&id));
                return Err(error.to_string());
            }
            match self.open(&name, Access::CreateNewReadWrite) {
                Ok(file) => {
                    lock(&file, true).map_err(error)?;
                    return Ok(Artifact {
                        id,
                        file: Some(file),
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    self.unlink(&manifest_name(&id));
                    continue;
                }
                Err(e) => {
                    self.unlink(&manifest_name(&id));
                    return Err(error(e));
                }
            }
        }
        Err("could not allocate a unique output artifact".into())
    }

    fn manifest(&self, id: &str) -> Result<ArtifactMetadata, String> {
        let manifest = self.load_manifest(id)?;
        if manifest.owner != self.owner {
            return Err("unknown output artifact for this connection".into());
        }
        Ok(manifest.metadata)
    }

    fn load_manifest(&self, id: &str) -> Result<Manifest, String> {
        validate_id(id)?;
        let file = self.open(&manifest_name(id), Access::Read).map_err(error)?;
        if file.metadata().map_err(error)?.len() > MAX_MANIFEST_BYTES {
            return Err("output manifest exceeds its size limit".into());
        }
        let manifest: Manifest = serde_json::from_reader(file).map_err(error)?;
        if manifest.metadata.schema != SCHEMA
            || manifest.metadata.id != id
            || (!manifest.owner.is_empty() && validate_id(&manifest.owner).is_err())
        {
            return Err("unrecognized output manifest".into());
        }
        Ok(manifest)
    }

    fn write_manifest(&self, metadata: &ArtifactMetadata) -> Result<(), String> {
        let _guard = self.directory_lock()?;
        let temporary = format!("output-{}.meta.tmp", metadata.id);
        let mut file = self
            .open(&temporary, Access::CreateNewWrite)
            .map_err(error)?;
        let result = (|| {
            serde_json::to_writer(
                &mut file,
                &Manifest {
                    metadata: metadata.clone(),
                    owner: self.owner.clone(),
                },
            )
            .map_err(error)?;
            file.flush().map_err(error)?;
            #[cfg(windows)]
            self.directory_fd
                .rename(&temporary, &self.directory_fd, manifest_name(&metadata.id))
                .map_err(error)?;
            #[cfg(unix)]
            {
                let source = CString::new(temporary.as_str()).unwrap();
                let destination = CString::new(manifest_name(&metadata.id)).unwrap();
                // SAFETY: both paths are single components relative to the owned directory.
                if unsafe {
                    libc::renameat(
                        self.directory_fd.as_raw_fd(),
                        source.as_ptr(),
                        self.directory_fd.as_raw_fd(),
                        destination.as_ptr(),
                    )
                } < 0
                {
                    return Err(error(io::Error::last_os_error()));
                }
            }
            Ok(())
        })();
        if result.is_err() {
            self.unlink(&temporary);
        }
        result
    }

    fn unlink(&self, name: &str) -> bool {
        #[cfg(windows)]
        {
            self.directory_fd.remove_file(name).is_ok()
        }
        #[cfg(unix)]
        {
            let Ok(name) = CString::new(name) else {
                return false;
            };
            // SAFETY: the fd is owned; unlinkat does not follow the final component.
            unsafe { libc::unlinkat(self.directory_fd.as_raw_fd(), name.as_ptr(), 0) == 0 }
        }
    }

    /// Explicit cleanup is limited to this connection's artifacts.
    pub fn cleanup(&self) -> Result<Value, String> {
        self.cleanup_scope(false)
    }

    // Automatic retention keeps the existing UID-wide quota and also reclaims
    // expired artifacts after the connection that produced them has ended.
    fn cleanup_all(&self) -> Result<Value, String> {
        self.cleanup_scope(true)
    }

    fn cleanup_scope(&self, all: bool) -> Result<Value, String> {
        let _guard = self.directory_lock()?;
        let current = now();
        let mut files = Vec::new();
        let mut active = 0u64;
        // Read through the opened directory, not a possibly replaced pathname.
        let entries = directory_entries(&self.directory_fd).map_err(error)?;
        for name in entries {
            let Some(name) = name.to_str() else { continue };
            if let Some(token) = name
                .strip_prefix("reload-")
                .and_then(|v| v.strip_suffix(".json"))
            {
                if validate_id(token).is_ok()
                    && let Ok(file) = self.open(name, Access::Read)
                    && file.metadata().is_ok_and(|m| m.len() <= MAX_MANIFEST_BYTES)
                    && let Ok(grant) = serde_json::from_reader::<_, ReloadGrant>(file)
                    && grant.schema == RELOAD_SCHEMA
                    && (all || grant.owner == self.owner)
                    && grant.expires_at_unix <= current
                {
                    self.unlink(name);
                }
                continue;
            }
            if let Some(id) = name
                .strip_prefix("output-")
                .and_then(|v| v.strip_suffix(".meta.json"))
            {
                if validate_id(id).is_ok()
                    && self
                        .load_manifest(id)
                        .is_ok_and(|m| all || m.owner == self.owner)
                    && self
                        .open(&artifact_name(id), Access::Read)
                        .is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
                    && let Ok(file) = self.open(name, Access::Read)
                    && let Ok(metadata) = file.metadata()
                    && let Ok(modified) = metadata.modified()
                    && current.saturating_sub(timestamp(modified)) >= self.policy.retention_secs
                {
                    self.unlink(name);
                }
                continue;
            }
            let Some(id) = name
                .strip_prefix("output-")
                .and_then(|v| v.strip_suffix(".log"))
            else {
                continue;
            };
            if validate_id(id).is_err()
                || !self
                    .load_manifest(id)
                    .is_ok_and(|m| all || m.owner == self.owner)
            {
                continue;
            }
            let Ok(file) = self.open(name, Access::ReadWrite) else {
                continue;
            };
            if lock(&file, true).is_err() {
                active += 1;
                continue;
            }
            let metadata = file.metadata().map_err(error)?;
            let modified = metadata.modified().ok().map(timestamp).unwrap_or(current);
            files.push((modified, id.to_owned(), metadata.len(), file));
        }
        files.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        let mut retained = files.iter().map(|entry| entry.2).sum::<u64>();
        let mut removed_files = 0u64;
        let mut removed_bytes = 0u64;
        for (modified, id, bytes, _file) in files {
            if current.saturating_sub(modified) < self.policy.retention_secs
                && retained <= self.policy.retained_bytes
            {
                continue;
            }
            if self.unlink(&artifact_name(&id)) {
                self.unlink(&manifest_name(&id));
                let temporary = format!("output-{id}.meta.tmp");
                if self.open(&temporary, Access::Read).is_ok() {
                    self.unlink(&temporary);
                }
                retained = retained.saturating_sub(bytes);
                removed_files += 1;
                removed_bytes += bytes;
            }
        }
        Ok(json!({
            "removed_files":removed_files,"removed_bytes":removed_bytes,
            "retained_bytes":retained,"active_artifacts":active,
            "retention_secs":self.policy.retention_secs,"max_retained_bytes":self.policy.retained_bytes
        }))
    }

    pub fn tools() -> Vec<Value> {
        vec![
            json!({"name":"ssmcp.output.read","description":"Read a bounded raw chunk of an owned output artifact by ID. Use next_offset for more; binary data is preserved in data_base64. Artifacts expire automatically.","inputSchema":{"type":"object","properties":{"id":{"type":"string","pattern":"^[a-f0-9]{32}$"},"offset":{"type":"integer","minimum":0,"default":0},"max_bytes":{"type":"integer","minimum":1,"maximum":READ_BYTES,"default":16384}},"required":["id"],"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false}}),
            json!({"name":"ssmcp.output.stat","description":"Inspect an owned output artifact and compute the SHA-256 of its current byte snapshot. A running capture can grow after this snapshot.","inputSchema":{"type":"object","properties":{"id":{"type":"string","pattern":"^[a-f0-9]{32}$"}},"required":["id"],"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false}}),
            json!({"name":"ssmcp.output.cleanup","description":"Run the same automatic retention cleanup now: remove expired artifacts and oldest completed logs above quota. Active captures and unrelated files are preserved.","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true}}),
        ]
    }

    pub async fn perform(self: &Arc<Self>, name: &str, arguments: Value) -> Result<Value, String> {
        let store = self.clone();
        let name = name.to_owned();
        tokio::task::spawn_blocking(move || store.perform_sync(&name, arguments))
            .await
            .map_err(error)?
    }

    fn perform_sync(&self, name: &str, arguments: Value) -> Result<Value, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Id {
            id: String,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ReadArgs {
            id: String,
            #[serde(default)]
            offset: u64,
            #[serde(default = "default_read")]
            max_bytes: usize,
        }
        fn default_read() -> usize {
            16 * 1024
        }
        match name {
            "ssmcp.output.read" => {
                let args: ReadArgs = serde_json::from_value(arguments).map_err(error)?;
                if !(1..=READ_BYTES).contains(&args.max_bytes) {
                    return Err(format!("max_bytes must be 1..={READ_BYTES}"));
                }
                let metadata = self.manifest(&args.id)?;
                let file = self
                    .open(&artifact_name(&args.id), Access::Read)
                    .map_err(error)?;
                let file_metadata = file.metadata().map_err(error)?;
                let size = file_metadata.len();
                if args.offset > size {
                    return Err("offset is beyond the output artifact".into());
                }
                let mut data = vec![0; args.max_bytes.min((size - args.offset) as usize)];
                let mut read = 0;
                while read < data.len() {
                    match read_at(&file, &mut data[read..], args.offset + read as u64) {
                        Ok(0) => break,
                        Ok(n) => read += n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(error(e)),
                    }
                }
                data.truncate(read);
                let next = args.offset + read as u64;
                Ok(
                    json!({"id":args.id,"path":self.directory.join(artifact_name(&args.id)),"offset":args.offset,
                    "next_offset":next,"bytes":read,"size":size,"eof":next == size,
                    "complete":metadata.complete,"data_base64":STANDARD.encode(&data),
                    "text":String::from_utf8_lossy(&data),"sha256":format!("{:x}",Sha256::digest(&data)),
                    "sha256_scope":"returned_chunk"}),
                )
            }
            "ssmcp.output.stat" => {
                let args: Id = serde_json::from_value(arguments).map_err(error)?;
                let mut metadata = self.manifest(&args.id)?;
                let file = self
                    .open(&artifact_name(&args.id), Access::Read)
                    .map_err(error)?;
                let file_metadata = file.metadata().map_err(error)?;
                let size = file_metadata.len();
                if size > self.policy.artifact_bytes {
                    return Err("output artifact exceeds its configured size limit".into());
                }
                let mut reader = file.take(size);
                let mut data = [0u8; 64 * 1024];
                let mut digest = Sha256::new();
                let mut bytes = 0u64;
                let mut newlines = 0u64;
                let mut last = None;
                loop {
                    let n = reader.read(&mut data).map_err(error)?;
                    if n == 0 {
                        break;
                    }
                    digest.update(&data[..n]);
                    bytes += n as u64;
                    newlines += data[..n].iter().filter(|byte| **byte == b'\n').count() as u64;
                    last = data.get(n - 1).copied();
                }
                metadata.bytes = bytes;
                metadata.observed_bytes = metadata.observed_bytes.max(bytes);
                metadata.lines = newlines + u64::from(last.is_some_and(|byte| byte != b'\n'));
                metadata.sha256 = format!("{:x}", digest.finalize());
                metadata.path = self
                    .directory
                    .join(artifact_name(&args.id))
                    .to_string_lossy()
                    .into_owned();
                metadata.expires_at_unix = file_metadata
                    .modified()
                    .ok()
                    .map(timestamp)
                    .unwrap_or(now())
                    .saturating_add(self.policy.retention_secs);
                serde_json::to_value(metadata).map_err(error)
            }
            "ssmcp.output.cleanup" => {
                if arguments.as_object().is_none_or(|args| !args.is_empty()) {
                    return Err("cleanup accepts an empty arguments object".into());
                }
                self.cleanup()
            }
            _ => Err("unknown output operation".into()),
        }
    }

    /// Spill a large native result as JSON while preserving the MCP error flag
    /// and a small set of execution-status fields useful without opening the log.
    pub fn guard_result(self: &Arc<Self>, value: Value) -> Value {
        if inline_image_result(&value, &self.policy) {
            return value;
        }
        let mut capture = self.capture("native_tool_result_json");
        if has_long_lines(&value, self.policy.inline_lines) {
            let _ = capture.spill();
        }
        let encoded = serde_json::to_writer(CaptureWriter(&mut capture), &value);
        if encoded.is_ok() && !capture.is_spilled() {
            return value;
        }
        let _ = capture.finish();
        let mut summary = json!({"spilled":capture.is_spilled(),"format":"mcp_tool_result_json","artifact":capture.metadata()});
        if let Some(error) = capture.error() {
            summary["output_error"] = json!(error);
        }
        if let Some(source) = value.get("structuredContent").and_then(Value::as_object) {
            for key in [
                "exit_code",
                "exitCode",
                "status",
                "running",
                "job_id",
                "jobId",
                "session_id",
                "sessionId",
                "id",
                "timed_out",
                "cancelled",
                "next_cursor",
                "output_eof",
            ] {
                if let Some(field) = source.get(key)
                    && (field.is_null()
                        || field.is_boolean()
                        || field.is_number()
                        || field.as_str().is_some_and(|s| s.len() <= 256))
                {
                    summary[key] = field.clone();
                }
            }
        }
        json!({"isError":value.get("isError").and_then(Value::as_bool).unwrap_or(false) || capture.error().is_some(),
            "content":[{"type":"text","text":summary.to_string()}],"structuredContent":summary})
    }
}

/// Validate bounded standard-base64 PNG/JPEG blocks before forwarding binary content.
pub(crate) fn validate_image_content(value: &Value) -> Result<usize, String> {
    if value["type"] != "image" {
        return Err("image content must have type=image".into());
    }
    let mime = value["mimeType"]
        .as_str()
        .filter(|mime| matches!(*mime, "image/png" | "image/jpeg"))
        .ok_or("image MIME type must be image/png or image/jpeg")?;
    let data = value["data"].as_str().ok_or("image data must be base64")?;
    if data.is_empty() || data.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 {
        return Err("image data exceeds 400 KiB or is empty".into());
    }
    let decoded = STANDARD
        .decode(data)
        .map_err(|_| "image data must be canonical base64")?;
    if decoded.len() > MAX_IMAGE_BYTES || STANDARD.encode(&decoded) != data {
        return Err("image data must be canonical base64 within 400 KiB".into());
    }
    let matches = match mime {
        "image/png" => decoded.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => decoded.starts_with(&[0xff, 0xd8, 0xff]),
        _ => false,
    };
    if !matches {
        return Err("image bytes do not match the declared PNG or JPEG MIME type".into());
    }
    Ok(decoded.len())
}

fn inline_image_result(value: &Value, policy: &Policy) -> bool {
    let Some(content) = value.get("content").and_then(Value::as_array) else {
        return false;
    };
    let images: Vec<_> = content
        .iter()
        .filter(|item| item["type"] == "image")
        .collect();
    if images.is_empty()
        || images.len() > 4
        || images
            .iter()
            .any(|image| validate_image_content(image).is_err())
        || value.to_string().len() > MAX_INLINE_IMAGE_RESULT_BYTES
    {
        return false;
    }
    // Only binary image payloads get a larger allowance. Large text, duplicate
    // base64 in structuredContent and oversized metadata still use artifacts.
    let mut metadata = value.clone();
    for item in metadata["content"].as_array_mut().unwrap() {
        if item["type"] == "image" {
            item["data"] = Value::Null;
        }
    }
    metadata.to_string().len() <= policy.inline_bytes
        && !has_long_lines(&metadata, policy.inline_lines)
}

struct Artifact {
    id: String,
    file: Option<File>,
}

fn random_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    crate::platform::random_bytes(&mut bytes).map_err(error)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub struct Capture {
    store: Arc<OutputStore>,
    scope: String,
    inline: Vec<u8>,
    artifact: Option<Artifact>,
    digest: Sha256,
    bytes: u64,
    newlines: u64,
    last: Option<u8>,
    observed_bytes: u64,
    finished: bool,
    failure: Option<String>,
    modified: u64,
}

impl Capture {
    pub fn is_spilled(&self) -> bool {
        self.artifact.is_some()
    }
    pub fn inline_bytes(&self) -> &[u8] {
        &self.inline
    }
    pub fn error(&self) -> Option<&str> {
        self.failure.as_deref()
    }
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
    pub fn lines(&self) -> u64 {
        self.newlines + u64::from(self.last.is_some_and(|byte| byte != b'\n'))
    }

    pub fn metadata(&self) -> Option<ArtifactMetadata> {
        let artifact = self.artifact.as_ref()?;
        Some(ArtifactMetadata {
            schema: SCHEMA.into(),
            id: artifact.id.clone(),
            path: self
                .store
                .directory
                .join(artifact_name(&artifact.id))
                .to_string_lossy()
                .into_owned(),
            scope: self.scope.clone(),
            sha256: format!("{:x}", self.digest.clone().finalize()),
            sha256_scope: "first_bytes_snapshot".into(),
            bytes: self.bytes,
            lines: self.lines(),
            observed_bytes: self.observed_bytes,
            complete: self.finished && self.failure.is_none(),
            expires_at_unix: self
                .modified
                .saturating_add(self.store.policy.retention_secs),
            retention_secs: self.store.policy.retention_secs,
            max_bytes: self.store.policy.artifact_bytes,
            error: self.failure.clone(),
        })
    }

    pub fn fail(&mut self, reason: impl Into<String>) {
        if self.failure.is_none() {
            let mut reason = reason.into();
            if reason.len() > 1024 {
                let mut end = 1024;
                while !reason.is_char_boundary(end) {
                    end -= 1
                }
                reason.truncate(end);
            }
            self.failure = Some(reason);
        }
    }

    pub fn spill(&mut self) -> Result<(), String> {
        if self.artifact.is_some() {
            return Ok(());
        }
        if self.finished {
            return Err("output capture is already complete".into());
        }
        let artifact = match self.store.create(&self.scope) {
            Ok(artifact) => artifact,
            Err(error) => {
                self.fail(error.clone());
                return Err(error);
            }
        };
        self.artifact = Some(artifact);
        let prefix = std::mem::take(&mut self.inline);
        self.digest = Sha256::new();
        self.bytes = 0;
        self.newlines = 0;
        self.last = None;
        self.append(&prefix)
    }

    pub fn push(&mut self, data: &[u8]) -> Result<(), String> {
        if self.finished {
            return Err("output capture is already complete".into());
        }
        self.observed_bytes = self.observed_bytes.saturating_add(data.len() as u64);
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        let newlines = self.newlines + data.iter().filter(|byte| **byte == b'\n').count() as u64;
        let last = data.last().copied().or(self.last);
        let lines = newlines + u64::from(last.is_some_and(|byte| byte != b'\n'));
        if !self.is_spilled()
            && (self.bytes.saturating_add(data.len() as u64)
                > self.store.policy.inline_bytes as u64
                || lines > self.store.policy.inline_lines)
        {
            self.spill()?;
        }
        self.append(data)
    }

    fn append(&mut self, data: &[u8]) -> Result<(), String> {
        let remaining = self.store.policy.artifact_bytes.saturating_sub(self.bytes);
        let permitted = data.len().min(remaining as usize);
        let mut offset = 0;
        while offset < permitted {
            let written = if let Some(file) = self.artifact.as_mut().and_then(|a| a.file.as_mut()) {
                match file.write(&data[offset..permitted]) {
                    Ok(0) => {
                        let message = "output artifact write made no progress".to_owned();
                        self.fail(message.clone());
                        return Err(message);
                    }
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        let message = format!("could not write output artifact: {e}");
                        self.fail(message.clone());
                        return Err(message);
                    }
                }
            } else {
                self.inline.extend_from_slice(&data[offset..permitted]);
                permitted - offset
            };
            let accepted = &data[offset..offset + written];
            self.digest.update(accepted);
            self.bytes += written as u64;
            self.newlines += accepted.iter().filter(|byte| **byte == b'\n').count() as u64;
            self.last = accepted.last().copied().or(self.last);
            self.modified = now();
            offset += written;
        }
        if permitted < data.len() {
            let message = format!(
                "output artifact reached its {} byte limit; captured prefix is partial",
                self.store.policy.artifact_bytes
            );
            self.fail(message.clone());
            return Err(message);
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Result<(), String> {
        if self.finished {
            return self.failure.clone().map_or(Ok(()), Err);
        }
        if let Some(file) = self.artifact.as_mut().and_then(|a| a.file.as_mut())
            && let Err(e) = file
                .flush()
                .and_then(|_| file.set_modified(SystemTime::now()))
        {
            self.fail(format!("could not finalize output artifact: {e}"));
        }
        self.modified = now();
        self.finished = true;
        if let Some(metadata) = self.metadata()
            && let Err(error) = self.store.write_manifest(&metadata)
        {
            self.fail(error);
        }
        if let Some(artifact) = &mut self.artifact {
            // Releasing the file also releases its lock. Completed job handles
            // must not pin files forever and defeat TTL/quota cleanup.
            artifact.file.take();
        }
        self.failure.clone().map_or(Ok(()), Err)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if !self.finished && self.is_spilled() {
            self.fail("capture ended before completion");
            let _ = self.finish();
        }
    }
}

struct CaptureWriter<'a>(&'a mut Capture);
impl Write for CaptureWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.push(bytes).map_err(io::Error::other)?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn has_long_lines(value: &Value, limit: u64) -> bool {
    match value {
        Value::String(text) => {
            text.bytes()
                .filter(|byte| *byte == b'\n')
                .take(limit as usize + 1)
                .count() as u64
                + u64::from(!text.is_empty() && !text.ends_with('\n'))
                > limit
        }
        Value::Array(values) => values.iter().any(|value| has_long_lines(value, limit)),
        Value::Object(values) => values.values().any(|value| has_long_lines(value, limit)),
        _ => false,
    }
}

fn artifact_name(id: &str) -> String {
    format!("output-{id}.log")
}
fn manifest_name(id: &str) -> String {
    format!("output-{id}.meta.json")
}
fn validate_id(id: &str) -> Result<(), String> {
    if id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err("output id must be 32 lowercase hexadecimal characters".into())
    }
}
#[cfg(unix)]
fn open_directory(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(unix)]
fn directory_entries(directory: &Directory) -> io::Result<Vec<std::ffi::OsString>> {
    use std::os::unix::ffi::OsStringExt;
    // openat(".") gives this scan an independent directory cursor. Dup would
    // share the retained descriptor's offset and miss entries on later scans.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            c".".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
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
        #[cfg(target_os = "linux")]
        let errno = unsafe { libc::__errno_location() };
        #[cfg(target_os = "macos")]
        let errno = unsafe { libc::__error() };
        // SAFETY: errno is thread-local and stream is an owned live DIR pointer.
        unsafe {
            *errno = 0;
        }
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            if unsafe { *errno } != 0 {
                return Err(io::Error::last_os_error());
            }
            break;
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            names.push(std::ffi::OsString::from_vec(name.to_vec()));
        }
    }
    Ok(names)
}

#[cfg(windows)]
fn directory_entries(directory: &Directory) -> io::Result<Vec<std::ffi::OsString>> {
    directory
        .entries()?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect()
}

fn read_at(file: &File, data: &mut [u8], offset: u64) -> io::Result<usize> {
    #[cfg(unix)]
    {
        file.read_at(data, offset)
    }
    #[cfg(windows)]
    {
        file.seek_read(data, offset)
    }
}

fn lock(file: &File, nonblocking: bool) -> io::Result<()> {
    crate::platform::lock_file(file, nonblocking)
}
fn timestamp(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn now() -> u64 {
    timestamp(SystemTime::now())
}
fn error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod portable_tests {
    use super::*;

    #[tokio::test]
    async fn live_spilled_output_is_readable_before_writer_unlocks() {
        let mut random = [0u8; 16];
        crate::platform::random_bytes(&mut random).unwrap();
        let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let path = std::env::temp_dir().join(format!("ssmcp-output-portable-{suffix}"));
        let store = OutputStore::at(path.clone(), Policy::default()).unwrap();
        let mut capture = store.capture("portable-test");
        capture.push(&vec![b'x'; INLINE_BYTES + 100]).unwrap();
        let metadata = capture.metadata().unwrap();
        let read = store
            .perform(
                "ssmcp.output.read",
                json!({"id":metadata.id,"max_bytes":64}),
            )
            .await
            .unwrap();
        assert_eq!(read["bytes"], 64);
        assert_eq!(read["text"], "x".repeat(64));
        assert_eq!(store.cleanup().unwrap()["active_artifacts"], 1);
        capture.finish().unwrap();
        drop(capture);
        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture {
        path: PathBuf,
        store: Arc<OutputStore>,
    }
    impl Fixture {
        fn new(policy: Policy) -> Self {
            let path = std::env::temp_dir().join(format!(
                "ssmcp-output-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let store = OutputStore::at(path.clone(), policy).unwrap();
            Self { path, store }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn connections_cannot_read_stat_or_explicitly_clean_each_others_artifacts() {
        let fixture = Fixture::new(Policy {
            inline_bytes: 1,
            ..Policy::default()
        });
        let mut capture = fixture.store.capture("owned");
        capture.push(b"private output").unwrap();
        capture.finish().unwrap();
        let metadata = capture.metadata().unwrap();
        let other = OutputStore::at(fixture.path.clone(), Policy::default()).unwrap();
        assert_ne!(fixture.store.owner, other.owner);
        for method in ["ssmcp.output.read", "ssmcp.output.stat"] {
            assert!(
                other
                    .perform_sync(method, json!({"id":metadata.id}))
                    .is_err()
            );
            assert!(
                fixture
                    .store
                    .perform_sync(method, json!({"id":metadata.id}))
                    .is_ok()
            );
        }
        assert!(
            serde_json::to_value(&metadata)
                .unwrap()
                .get("owner")
                .is_none()
        );
        File::options()
            .write(true)
            .open(&metadata.path)
            .unwrap()
            .set_modified(UNIX_EPOCH)
            .unwrap();
        assert_eq!(other.cleanup().unwrap()["removed_files"], 0);
        assert!(Path::new(&metadata.path).exists());
        assert_eq!(fixture.store.cleanup().unwrap()["removed_files"], 1);
    }

    #[test]
    fn reload_grant_restores_only_its_owner_once_and_is_removed_on_failure() {
        let fixture = Fixture::new(Policy {
            inline_bytes: 1,
            ..Policy::default()
        });
        let mut capture = fixture.store.capture("before-reload");
        capture
            .push(b"retained across acknowledged reload")
            .unwrap();
        capture.finish().unwrap();
        let id = capture.metadata().unwrap().id;
        let mut command = std::process::Command::new("ssmcp");
        let ticket = fixture.store.prepare_reload(&mut command).unwrap();
        let token = command
            .get_envs()
            .find(|(key, _)| key.to_str() == Some(RELOAD_ENV))
            .unwrap()
            .1
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let restored =
            OutputStore::at_restoring(fixture.path.clone(), Policy::default(), Some(&token))
                .unwrap();
        assert_eq!(restored.owner, fixture.store.owner);
        assert_eq!(
            restored
                .perform_sync("ssmcp.output.read", json!({"id":id}))
                .unwrap()["text"],
            "retained across acknowledged reload"
        );
        let replay =
            OutputStore::at_restoring(fixture.path.clone(), Policy::default(), Some(&token))
                .unwrap();
        assert_ne!(replay.owner, fixture.store.owner);
        assert!(
            replay
                .perform_sync("ssmcp.output.stat", json!({"id":id}))
                .is_err()
        );
        drop(ticket);
        let pending = fixture.store.prepare_reload(&mut command).unwrap();
        let pending_path = fixture.path.join(&pending.name);
        assert!(pending_path.exists());
        drop(pending);
        assert!(!pending_path.exists());
    }

    #[test]
    fn expired_reload_grant_never_restores_artifact_access() {
        let fixture = Fixture::new(Policy::default());
        let mut command = std::process::Command::new("ssmcp");
        let ticket = fixture.store.prepare_reload(&mut command).unwrap();
        let token = command
            .get_envs()
            .find(|(key, _)| key.to_str() == Some(RELOAD_ENV))
            .unwrap()
            .1
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let mut file = fixture.store.open(&ticket.name, Access::ReadWrite).unwrap();
        file.set_len(0).unwrap();
        serde_json::to_writer(
            &mut file,
            &ReloadGrant {
                schema: RELOAD_SCHEMA.into(),
                owner: fixture.store.owner.clone(),
                expires_at_unix: now().saturating_sub(1),
            },
        )
        .unwrap();
        drop(file);
        assert!(
            OutputStore::at_restoring(fixture.path.clone(), Policy::default(), Some(&token))
                .is_err()
        );
        assert!(!fixture.path.join(&ticket.name).exists());
    }

    #[test]
    fn short_bytes_stay_inline_and_count_unterminated_binary_lines() {
        let fixture = Fixture::new(Policy::default());
        let mut capture = fixture.store.capture("test");
        capture.push(b"a\n\xff").unwrap();
        assert_eq!(capture.lines(), 2);
        capture.push(b"\n").unwrap();
        capture.finish().unwrap();
        assert_eq!(capture.lines(), 2);
        assert_eq!(capture.inline_bytes(), b"a\n\xff\n");
        assert!(capture.metadata().is_none());
    }

    #[test]
    fn byte_threshold_preserves_prefix_and_hash_across_chunk_boundaries() {
        let fixture = Fixture::new(Policy {
            inline_bytes: 7,
            ..Policy::default()
        });
        let mut capture = fixture.store.capture("stdout");
        capture.push(b"prefix").unwrap();
        capture.push(b"\n\xe4\xb8").unwrap();
        capture.push(b"\xad\nend").unwrap();
        let snapshot = capture.metadata().unwrap();
        assert!(!snapshot.complete);
        capture.finish().unwrap();
        let metadata = capture.metadata().unwrap();
        let bytes = std::fs::read(&metadata.path).unwrap();
        assert_eq!(bytes, b"prefix\n\xe4\xb8\xad\nend");
        assert_eq!(metadata.bytes, bytes.len() as u64);
        assert_eq!(metadata.lines, 3);
        assert_eq!(metadata.sha256, format!("{:x}", Sha256::digest(&bytes)));
        assert!(metadata.complete);
        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(&metadata.path).unwrap().mode() & 0o777,
            0o600
        );
        assert_eq!(capture.inline_bytes(), b"");
    }

    #[test]
    fn line_threshold_spills_even_small_output() {
        let fixture = Fixture::new(Policy {
            inline_lines: 2,
            ..Policy::default()
        });
        let mut capture = fixture.store.capture("stderr");
        capture.push(b"a\nb\n").unwrap();
        assert!(!capture.is_spilled());
        capture.push(b"c").unwrap();
        assert!(capture.is_spilled());
        capture.finish().unwrap();
        assert_eq!(capture.metadata().unwrap().lines, 3);
    }

    #[test]
    fn hard_cap_reports_partial_prefix_with_correct_hash_and_counts() {
        let fixture = Fixture::new(Policy {
            inline_bytes: 4,
            artifact_bytes: 10,
            ..Policy::default()
        });
        let mut capture = fixture.store.capture("stdout");
        capture.push(b"1234").unwrap();
        assert!(capture.push(b"567890EXCESS").is_err());
        assert!(capture.finish().is_err());
        let metadata = capture.metadata().unwrap();
        assert_eq!(std::fs::read(&metadata.path).unwrap(), b"1234567890");
        assert_eq!(metadata.bytes, 10);
        assert_eq!(metadata.observed_bytes, 16);
        assert_eq!(
            metadata.sha256,
            format!("{:x}", Sha256::digest(b"1234567890"))
        );
        assert!(!metadata.complete);
        assert!(metadata.error.unwrap().contains("limit"));
    }

    #[test]
    fn cleanup_protects_live_captures_then_removes_expired_completed_files() {
        let fixture = Fixture::new(Policy {
            inline_bytes: 1,
            retention_secs: 1,
            retained_bytes: 1,
            ..Policy::default()
        });
        let mut capture = fixture.store.capture("stdout");
        capture.push(b"hello").unwrap();
        let metadata = capture.metadata().unwrap();
        File::options()
            .write(true)
            .open(&metadata.path)
            .unwrap()
            .set_modified(UNIX_EPOCH)
            .unwrap();
        let result = fixture.store.cleanup().unwrap();
        assert_eq!(result["active_artifacts"], 1);
        assert!(Path::new(&metadata.path).exists());
        capture.finish().unwrap();
        let result = fixture.store.cleanup().unwrap();
        assert_eq!(result["removed_files"], 1);
        assert!(!Path::new(&metadata.path).exists());
        assert!(!fixture.path.join(manifest_name(&metadata.id)).exists());
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_preserves_unrelated_files_and_symlinks() {
        let fixture = Fixture::new(Policy {
            inline_bytes: 1,
            retained_bytes: 0,
            ..Policy::default()
        });
        let unrelated = fixture.path.join("keep.txt");
        std::fs::write(&unrelated, b"keep").unwrap();
        let id = "f".repeat(32);
        std::os::unix::fs::symlink(&unrelated, fixture.path.join(artifact_name(&id))).unwrap();
        assert_eq!(fixture.store.cleanup().unwrap()["removed_files"], 0);
        assert_eq!(std::fs::read(&unrelated).unwrap(), b"keep");
        assert!(fixture.path.join(artifact_name(&id)).is_symlink());
        assert!(
            fixture
                .store
                .perform_sync("ssmcp.output.read", json!({"id":"../keep.txt"}))
                .is_err()
        );
    }

    #[test]
    fn bounded_binary_read_and_stat_reconstruct_exact_saved_bytes() {
        let fixture = Fixture::new(Policy {
            inline_bytes: 1,
            ..Policy::default()
        });
        let mut capture = fixture.store.capture("stdout");
        capture.push(b"a\0\xff\nb").unwrap();
        capture.finish().unwrap();
        let id = capture.metadata().unwrap().id;
        let first = fixture
            .store
            .perform_sync("ssmcp.output.read", json!({"id":id,"max_bytes":3}))
            .unwrap();
        let last = fixture
            .store
            .perform_sync(
                "ssmcp.output.read",
                json!({"id":id,"offset":first["next_offset"],"max_bytes":3}),
            )
            .unwrap();
        assert_eq!(
            STANDARD
                .decode(first["data_base64"].as_str().unwrap())
                .unwrap(),
            b"a\0\xff"
        );
        assert_eq!(
            STANDARD
                .decode(last["data_base64"].as_str().unwrap())
                .unwrap(),
            b"\nb"
        );
        assert_eq!(last["eof"], true);
        let stat = fixture
            .store
            .perform_sync("ssmcp.output.stat", json!({"id":id}))
            .unwrap();
        assert_eq!(stat["lines"], 2);
        assert_eq!(
            stat["sha256"],
            format!("{:x}", Sha256::digest(b"a\0\xff\nb"))
        );
    }

    #[test]
    fn output_artifacts_reject_additional_hard_links() {
        let fixture = Fixture::new(Policy {
            inline_bytes: 1,
            ..Policy::default()
        });
        let mut capture = fixture.store.capture("stdout");
        capture.push(b"private output").unwrap();
        capture.finish().unwrap();
        let metadata = capture.metadata().unwrap();
        let linked = fixture.path.join("unrelated-hardlink");
        std::fs::hard_link(&metadata.path, &linked).unwrap();
        assert!(
            fixture
                .store
                .perform_sync("ssmcp.output.read", json!({"id":metadata.id}))
                .is_err()
        );
        assert_eq!(fixture.store.cleanup().unwrap()["removed_files"], 0);
        assert_eq!(std::fs::read(linked).unwrap(), b"private output");
    }

    #[test]
    fn abandoned_capture_is_marked_incomplete_and_released_for_cleanup() {
        let fixture = Fixture::new(Policy {
            inline_bytes: 1,
            retained_bytes: 1,
            ..Policy::default()
        });
        let metadata = {
            let mut capture = fixture.store.capture("stdout");
            capture.push(b"abandoned").unwrap();
            capture.metadata().unwrap()
        };
        let manifest = fixture.store.manifest(&metadata.id).unwrap();
        assert!(!manifest.complete);
        assert!(manifest.error.unwrap().contains("before completion"));
        assert_eq!(fixture.store.cleanup().unwrap()["removed_files"], 1);
    }

    #[test]
    fn screenshots_keep_binary_content_above_the_text_budget() {
        let fixture = Fixture::new(Policy {
            inline_bytes: 192,
            ..Policy::default()
        });
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/l9sAAAAASUVORK5CYII=";
        let native = json!({
            "isError":false,
            "content":[{"type":"image","mimeType":"image/png","data":png}],
            "structuredContent":{"width":1,"height":1}
        });
        assert!(native.to_string().len() > 192);
        assert_eq!(fixture.store.guard_result(native.clone()), native);
        let mut invalid = native.clone();
        invalid["content"][0]["mimeType"] = "image/jpeg".into();
        assert!(validate_image_content(&invalid["content"][0]).is_err());
        assert_eq!(
            fixture.store.guard_result(invalid)["structuredContent"]["spilled"],
            true
        );
        let mut large_text = native;
        large_text["structuredContent"]["text"] = "x".repeat(256).into();
        assert_eq!(
            fixture.store.guard_result(large_text)["structuredContent"]["spilled"],
            true
        );
    }

    #[test]
    fn image_payload_validation_bounds_decode_and_rejects_mismatched_types() {
        for image in [
            json!({"type":"image","mimeType":"image/png","data":"not base64"}),
            json!({"type":"image","mimeType":"image/jpeg","data":"YWJj"}),
            json!({"type":"image","mimeType":"image/svg+xml","data":"PHN2Zy8+"}),
            json!({"type":"image","mimeType":"image/png","data":""}),
            json!({"type":"image","mimeType":"image/png","data":"A".repeat(MAX_IMAGE_BYTES.div_ceil(3)*4+4)}),
        ] {
            assert!(validate_image_content(&image).is_err());
        }
    }

    #[test]
    fn native_result_guard_preserves_errors_and_execution_status() {
        let fixture = Fixture::new(Policy {
            inline_bytes: 100,
            ..Policy::default()
        });
        let native = json!({"isError":true,"content":[{"type":"text","text":"x".repeat(1024)}],"structuredContent":{"exit_code":17,"job_id":"job-1","stderr":"y".repeat(1024)}});
        let guarded = fixture.store.guard_result(native.clone());
        assert_eq!(guarded["isError"], true);
        assert_eq!(guarded["structuredContent"]["exit_code"], 17);
        assert_eq!(guarded["structuredContent"]["job_id"], "job-1");
        let path = guarded["structuredContent"]["artifact"]["path"]
            .as_str()
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&std::fs::read(path).unwrap()).unwrap(),
            native
        );
    }
}
