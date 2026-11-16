//! Bounded inline output, streaming spill files, and retention for client-owned logs.
//!
//! Files are private to the local UID. A writer holds an advisory lock until its
//! capture is finalized; cleanup only removes recognized, unlocked artifacts.
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(windows)]
use std::os::windows::fs::{FileExt, MetadataExt, OpenOptionsExt};
#[cfg(unix)]
use std::{
    ffi::CString,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
};

pub const INLINE_BYTES: usize = 32 * 1024;
pub const INLINE_LINES: u64 = 500;
pub const RETENTION_SECS: u64 = 24 * 60 * 60;
pub const RETAINED_BYTES: u64 = 512 * 1024 * 1024;
pub const ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;
pub const READ_BYTES: usize = 32 * 1024;
const SCHEMA: &str = "ssmcp.output.v1";
const JANITOR_SECS: u64 = 60;
const MAX_MANIFEST_BYTES: u64 = 16 * 1024;

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

pub struct OutputStore {
    directory: PathBuf,
    #[cfg_attr(windows, allow(dead_code))]
    directory_fd: File,
    policy: Policy,
}

impl OutputStore {
    pub fn global() -> Result<Arc<Self>, String> {
        static STORE: OnceLock<Result<Arc<OutputStore>, String>> = OnceLock::new();
        STORE
            .get_or_init(|| {
                #[cfg(unix)]
                {
                    let base = Path::new("/tmp/ssmcp");
                    match std::fs::DirBuilder::new().mode(0o1777).create(base) {
                        Ok(()) => {
                            // Creation is affected by umask; make the shared parent sticky.
                            std::fs::set_permissions(base, std::fs::Permissions::from_mode(0o1777))
                                .map_err(error)?;
                        }
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
                            "output directory /tmp/ssmcp has unsafe ownership or permissions"
                                .into(),
                        );
                    }
                    Self::at(base.join(uid.to_string()), Policy::default())
                }
                #[cfg(windows)]
                {
                    let base = std::env::var_os("LOCALAPPDATA")
                        .map(PathBuf::from)
                        .unwrap_or_else(std::env::temp_dir)
                        .join("ssmcp");
                    std::fs::create_dir_all(&base).map_err(error)?;
                    Self::at(base.join("output"), Policy::default())
                }
            })
            .clone()
    }

    fn at(directory: PathBuf, policy: Policy) -> Result<Arc<Self>, String> {
        match crate::platform::private_directory(&directory) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(error(e)),
        }
        let directory_fd = open_directory(&directory).map_err(error)?;
        #[cfg(unix)]
        let metadata = directory_fd.metadata().map_err(error)?;
        #[cfg(unix)]
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(
                "output directory must be owned by this user with private permissions".into(),
            );
        }
        let store = Arc::new(Self {
            directory,
            directory_fd,
            policy,
        });
        store.cleanup()?;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let weak = Arc::downgrade(&store);
            runtime.spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(JANITOR_SECS)).await;
                    let Some(store) = weak.upgrade() else { break };
                    // Directory scans and filesystem I/O must not stall the async reactor.
                    let _ = tokio::task::spawn_blocking(move || store.cleanup()).await;
                }
            });
        }
        Ok(store)
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

    #[cfg(unix)]
    fn open(&self, name: &str, flags: i32) -> io::Result<File> {
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

    #[cfg(windows)]
    fn open(&self, name: &str, flags: i32) -> io::Result<File> {
        if name.contains(['/', '\\', ':']) || name == "." || name == ".." {
            return Err(io::Error::other("invalid output filename"));
        }
        let path = self.directory.join(name);
        let mut options = OpenOptions::new();
        let access = flags & (libc::O_RDWR | libc::O_WRONLY);
        options
            .read(access != libc::O_WRONLY)
            .write(access != libc::O_RDONLY);
        if flags & libc::O_EXCL != 0 {
            options.create_new(true);
        } else {
            options.create(flags & libc::O_CREAT != 0);
        }
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
        let file = options.open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.file_attributes()
                & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                != 0
        {
            return Err(io::Error::other(
                "output artifact must be a regular file without reparse points",
            ));
        }
        Ok(file)
    }

    fn directory_lock(&self) -> Result<File, String> {
        let file = self
            .open(".cleanup.lock", libc::O_RDWR | libc::O_CREAT)
            .map_err(error)?;
        lock(&file, false).map_err(error)?;
        Ok(file)
    }

    fn create(&self, scope: &str) -> Result<Artifact, String> {
        self.cleanup()?;
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
            let mut manifest = match self.open(
                &manifest_name(&id),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            ) {
                Ok(file) => file,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(error(e)),
            };
            if let Err(error) = serde_json::to_writer(&mut manifest, &initial) {
                self.unlink(&manifest_name(&id));
                return Err(error.to_string());
            }
            match self.open(&name, libc::O_RDWR | libc::O_CREAT | libc::O_EXCL) {
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
        validate_id(id)?;
        let file = self
            .open(&manifest_name(id), libc::O_RDONLY)
            .map_err(error)?;
        if file.metadata().map_err(error)?.len() > MAX_MANIFEST_BYTES {
            return Err("output manifest exceeds its size limit".into());
        }
        let metadata: ArtifactMetadata = serde_json::from_reader(file).map_err(error)?;
        if metadata.schema != SCHEMA || metadata.id != id {
            return Err("unrecognized output manifest".into());
        }
        Ok(metadata)
    }

    fn write_manifest(&self, metadata: &ArtifactMetadata) -> Result<(), String> {
        let _guard = self.directory_lock()?;
        let temporary = format!("output-{}.meta.tmp", metadata.id);
        let mut file = self
            .open(&temporary, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)
            .map_err(error)?;
        let result = (|| {
            serde_json::to_writer(&mut file, metadata).map_err(error)?;
            file.flush().map_err(error)?;
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
            #[cfg(windows)]
            {
                drop(file);
                std::fs::rename(
                    self.directory.join(&temporary),
                    self.directory.join(manifest_name(&metadata.id)),
                )
                .map_err(error)?;
            }
            Ok(())
        })();
        if result.is_err() {
            self.unlink(&temporary);
        }
        result
    }

    #[cfg(unix)]
    fn unlink(&self, name: &str) -> bool {
        let Ok(name) = CString::new(name) else {
            return false;
        };
        // SAFETY: the fd is owned; unlinkat does not follow the final component.
        unsafe { libc::unlinkat(self.directory_fd.as_raw_fd(), name.as_ptr(), 0) == 0 }
    }

    #[cfg(windows)]
    fn unlink(&self, name: &str) -> bool {
        std::fs::remove_file(self.directory.join(name)).is_ok()
    }

    /// Evict expired and oldest completed artifacts; held writer locks always win.
    pub fn cleanup(&self) -> Result<Value, String> {
        let _guard = self.directory_lock()?;
        let current = now();
        let mut files = Vec::new();
        let mut active = 0u64;
        // Read through the opened directory, not a possibly replaced pathname.
        #[cfg(target_os = "linux")]
        let directory = PathBuf::from(format!("/proc/self/fd/{}", self.directory_fd.as_raw_fd()));
        #[cfg(not(target_os = "linux"))]
        let directory = self.directory.clone();
        let entries = std::fs::read_dir(directory).map_err(error)?;
        for entry in entries {
            let entry = entry.map_err(error)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some(id) = name
                .strip_prefix("output-")
                .and_then(|v| v.strip_suffix(".meta.json"))
            {
                if validate_id(id).is_ok()
                    && self.manifest(id).is_ok()
                    && self
                        .open(&artifact_name(id), libc::O_RDONLY)
                        .is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
                    && let Ok(file) = self.open(name, libc::O_RDONLY)
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
            if validate_id(id).is_err() || self.manifest(id).is_err() {
                continue;
            }
            let Ok(file) = self.open(name, libc::O_RDWR) else {
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
                if self.open(&temporary, libc::O_RDONLY).is_ok() {
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
                    .open(&artifact_name(&args.id), libc::O_RDONLY)
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
                    .open(&artifact_name(&args.id), libc::O_RDONLY)
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

struct Artifact {
    id: String,
    file: Option<File>,
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
#[cfg(windows)]
fn open_directory(path: &Path) -> io::Result<File> {
    use windows_sys::Win32::Storage::FileSystem::*;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::other(
            "output directory cannot be a reparse point",
        ));
    }
    Ok(file)
}
fn read_at(file: &File, bytes: &mut [u8], offset: u64) -> io::Result<usize> {
    #[cfg(unix)]
    {
        file.read_at(bytes, offset)
    }
    #[cfg(windows)]
    {
        file.seek_read(bytes, offset)
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

#[cfg(all(test, unix))]
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
