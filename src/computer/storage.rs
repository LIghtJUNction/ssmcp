//! Private, receipt-bound storage. No PATH entry or external installation is changed.
use super::archive::{self, MAX_EXECUTABLE};
use super::release::{self, Release};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const SCHEMA: u32 = 1;
const MAX_RECEIPT: usize = 16 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Identity {
    volume: u64,
    file: u64,
}

fn identity(file: &File) -> Result<Identity, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        Ok(Identity {
            volume: metadata.dev(),
            file: metadata.ino(),
        })
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: this live File owns the handle and info is writable.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(Identity {
            volume: u64::from(info.dwVolumeSerialNumber),
            file: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        })
    }
}

fn no_links(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return false;
        }
    }
    !metadata.file_type().is_symlink()
}

#[cfg(unix)]
fn owned(metadata: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no preconditions.
    metadata.uid() == unsafe { libc::geteuid() }
        && metadata.mode() & 0o077 == 0
        && (metadata.is_dir() || metadata.nlink() == 1)
}

fn open(path: &Path, directory: bool) -> Result<File, String> {
    let before = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !no_links(&before)
        || (if directory {
            !before.is_dir()
        } else {
            !before.is_file()
        })
    {
        return Err(format!(
            "Expected a plain {}: {}",
            if directory { "directory" } else { "file" },
            path.display()
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(
            libc::O_NOFOLLOW | libc::O_NONBLOCK | if directory { libc::O_DIRECTORY } else { 0 },
        );
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        };
        options.custom_flags(
            FILE_FLAG_OPEN_REPARSE_POINT
                | if directory {
                    FILE_FLAG_BACKUP_SEMANTICS
                } else {
                    0
                },
        );
    }
    let file = options.open(path).map_err(|error| error.to_string())?;
    let after = file.metadata().map_err(|error| error.to_string())?;
    if !no_links(&after)
        || (if directory {
            !after.is_dir()
        } else {
            !after.is_file()
        })
    {
        return Err("Zuse storage path changed while opening it".into());
    }
    Ok(file)
}

fn directory(path: &Path) -> Result<Identity, String> {
    let file = open(path, true)?;
    #[cfg(unix)]
    if !owned(&file.metadata().map_err(|error| error.to_string())?) {
        return Err(format!(
            "Zuse manager directory is not private and owned: {}",
            path.display()
        ));
    }
    #[cfg(windows)]
    crate::platform::windows::verify_directory(&file).map_err(|error| error.to_string())?;
    identity(&file)
}

fn private_file(path: &Path) -> Result<File, String> {
    let file = open(path, false)?;
    #[cfg(unix)]
    if !owned(&file.metadata().map_err(|error| error.to_string())?) {
        return Err(format!(
            "Zuse manager file is not private and owned: {}",
            path.display()
        ));
    }
    #[cfg(windows)]
    crate::platform::windows::verify_file(&file).map_err(|error| error.to_string())?;
    Ok(file)
}

pub(super) fn bytes(path: &Path, limit: usize) -> Result<(Identity, Vec<u8>), String> {
    let file = open(path, false)?;
    let id = identity(&file)?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if metadata.len() > limit as u64 {
        return Err("Zuse file exceeds its byte limit".into());
    }
    let mut content = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut content)
        .map_err(|error| error.to_string())?;
    if content.len() > limit || content.len() as u64 != metadata.len() {
        return Err("Zuse file changed size while reading it".into());
    }
    if identity(&open(path, false)?)? != id {
        return Err("Zuse file identity changed while reading it".into());
    }
    Ok((id, content))
}

fn read<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
    private_file(path)?;
    let (_, content) = bytes(path, MAX_RECEIPT)?;
    serde_json::from_slice(&content)
        .map_err(|error| format!("Invalid Zuse ownership receipt: {error}"))
}

fn create(path: &Path, bytes: &[u8], executable: bool) -> Result<Identity, String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if executable { 0o700 } else { 0o600 });
    }
    #[cfg(windows)]
    let _ = executable;
    let mut file = options.open(path).map_err(|error| error.to_string())?;
    file.write_all(bytes).map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    identity(&file)
}

fn json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let data = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    create(path, &data, false)?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    open(path, true)?
        .sync_all()
        .map_err(|error| error.to_string())?;
    #[cfg(windows)]
    let _ = path; // File contents are flushed; Windows rename remains atomic.
    Ok(())
}

pub(super) fn id() -> Result<String, String> {
    let mut random = [0u8; 16];
    crate::platform::random_bytes(&mut random).map_err(|error| error.to_string())?;
    Ok(crate::hex::encode(random))
}

fn valid_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(super) fn default_root() -> Result<PathBuf, String> {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support"));
    #[cfg(all(unix, not(target_os = "macos")))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|path| Path::new(path).is_absolute())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
    let base = base
        .filter(|path| path.is_absolute())
        .ok_or("An absolute user data directory is required to manage Zuse")?;
    Ok(base.join("ssmcp-zuse-cli"))
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Owner {
    schema: u32,
    owner_id: String,
    directory: Identity,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Download {
    schema: u32,
    owner_id: String,
    pub download_id: String,
    pub release: Release,
    directory: Identity,
    archive: Identity,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Installation {
    schema: u32,
    owner_id: String,
    pub installation_id: String,
    pub download_id: String,
    pub release: Release,
    directory: Identity,
    binary: Identity,
    pub binary_size: u64,
    pub binary_sha256: String,
}

// This guard is created only for a directory made by this invocation. Failed
// initialization removes only recorded objects whose identities still match.
struct Initialization {
    root: PathBuf,
    identity: Identity,
    entries: Vec<(PathBuf, Identity, bool)>,
    committed: bool,
}

impl Initialization {
    fn file(
        &mut self,
        name: &str,
        bytes: &[u8],
        writer: impl FnOnce(&mut File, &[u8]) -> std::io::Result<()>,
    ) -> Result<(), String> {
        if directory(&self.root)? != self.identity {
            return Err("Zuse manager root changed during initialization".into());
        }
        let path = self.root.join(name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).map_err(|error| error.to_string())?;
        self.entries.push((path, identity(&file)?, false));
        writer(&mut file, bytes).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())
    }

    fn directory(&mut self, name: &str) -> Result<(), String> {
        if directory(&self.root)? != self.identity {
            return Err("Zuse manager root changed during initialization".into());
        }
        let path = self.root.join(name);
        crate::platform::private_directory(&path).map_err(|error| error.to_string())?;
        let id = directory(&path)?;
        self.entries.push((path, id, true));
        Ok(())
    }
}

impl Drop for Initialization {
    fn drop(&mut self) {
        if self.committed || directory(&self.root).ok().as_ref() != Some(&self.identity) {
            return;
        }
        for (path, expected, is_directory) in self.entries.iter().rev() {
            if *is_directory {
                if directory(path).ok().as_ref() == Some(expected) {
                    let _ = fs::remove_dir(path);
                }
            } else if private_file(path)
                .and_then(|file| identity(&file))
                .ok()
                .as_ref()
                == Some(expected)
            {
                let _ = fs::remove_file(path);
            }
        }
        // No recursive deletion: unknown, replaced or extra files survive.
        let _ = fs::remove_dir(&self.root);
    }
}

pub(super) struct Store {
    root: PathBuf,
    owner_id: String,
    root_identity: Identity,
    _lock: File,
    #[cfg(test)]
    pub(super) fixture_target: Option<&'static str>,
}

impl Store {
    pub(super) fn open(root: &Path, create_store: bool) -> Result<Option<Self>, String> {
        Self::open_with(root, create_store, |file, bytes| file.write_all(bytes))
    }

    fn open_with(
        root: &Path,
        create_store: bool,
        owner_writer: impl FnOnce(&mut File, &[u8]) -> std::io::Result<()>,
    ) -> Result<Option<Self>, String> {
        let exists = match fs::symlink_metadata(root) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.to_string()),
        };
        if !exists && !create_store {
            return Ok(None);
        }
        let created = if exists {
            false
        } else {
            let parent = root.parent().ok_or("Zuse manager root has no parent")?;
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            match crate::platform::private_directory(root) {
                Ok(()) => true,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
                Err(error) => return Err(error.to_string()),
            }
        };
        let root_identity = directory(root)?;
        let mut initialization = created.then(|| Initialization {
            root: root.to_owned(),
            identity: root_identity.clone(),
            entries: Vec::new(),
            committed: false,
        });
        let lock_path = root.join("manager.lock");
        if let Some(initialization) = initialization.as_mut() {
            initialization.file("manager.lock", b"", |file, bytes| file.write_all(bytes))?;
        }
        let lock = private_file(&lock_path)?;
        crate::platform::lock_file(&lock, true).map_err(|error| {
            format!("Zuse manager is busy or cannot acquire its ownership lock: {error}")
        })?;
        if directory(root)? != root_identity {
            return Err("Zuse manager root changed while acquiring its lock".into());
        }
        if let Some(initialization) = initialization.as_mut() {
            let owner = Owner {
                schema: SCHEMA,
                owner_id: id()?,
                directory: root_identity.clone(),
            };
            initialization.directory("downloads")?;
            initialization.directory("installations")?;
            let bytes = serde_json::to_vec_pretty(&owner).map_err(|error| error.to_string())?;
            initialization.file("owner.json", &bytes, owner_writer)?;
            sync_directory(root)?;
        }
        let owner: Owner = read(&root.join("owner.json"))?;
        if owner.schema != SCHEMA || !valid_id(&owner.owner_id) || owner.directory != root_identity
        {
            return Err("Zuse manager ownership receipt does not match this directory".into());
        }
        directory(&root.join("downloads"))?;
        directory(&root.join("installations"))?;
        if let Some(initialization) = initialization.as_mut() {
            initialization.committed = true;
        }
        Ok(Some(Self {
            root: root.to_owned(),
            owner_id: owner.owner_id,
            root_identity,
            _lock: lock,
            #[cfg(test)]
            fixture_target: None,
        }))
    }

    fn release_target(&self) -> Result<&'static str, String> {
        #[cfg(test)]
        if let Some(target) = self.fixture_target {
            return Ok(target);
        }
        release::target()
    }

    fn check(&self) -> Result<(), String> {
        if directory(&self.root)? != self.root_identity {
            return Err("Zuse manager root identity changed".into());
        }
        let owner: Owner = read(&self.root.join("owner.json"))?;
        if owner.owner_id != self.owner_id
            || owner.directory != self.root_identity
            || owner.schema != SCHEMA
        {
            return Err("Zuse manager ownership changed".into());
        }
        Ok(())
    }

    fn location(&self, kind: &str, id: &str) -> Result<PathBuf, String> {
        if !valid_id(id) {
            return Err("Zuse management ID must be the exact ID returned by this manager".into());
        }
        self.check()?;
        directory(&self.root.join(kind))?;
        Ok(self.root.join(kind).join(id))
    }

    pub(super) fn save_download(&self, release: Release, bytes: &[u8]) -> Result<Download, String> {
        release.verify(bytes, self.release_target()?)?;
        let download_id = id()?;
        let destination = self.location("downloads", &download_id)?;
        let stage = Stage::new(destination)?;
        let archive = create(&stage.path.join("archive"), bytes, false)?;
        let receipt = Download {
            schema: SCHEMA,
            owner_id: self.owner_id.clone(),
            download_id,
            release,
            directory: stage.identity.clone(),
            archive,
        };
        json(&stage.path.join("receipt.json"), &receipt)?;
        stage.publish()?;
        Ok(receipt)
    }

    pub(super) fn install(&self, download_id: &str) -> Result<Installation, String> {
        let path = self.location("downloads", download_id)?;
        exact_files(&path, &["archive", "receipt.json"])?;
        let receipt: Download = read(&path.join("receipt.json"))?;
        if receipt.schema != SCHEMA
            || receipt.owner_id != self.owner_id
            || receipt.download_id != download_id
            || directory(&path)? != receipt.directory
        {
            return Err("Zuse download ownership does not match this manager".into());
        }
        private_file(&path.join("archive"))?;
        let (archive_id, content) = bytes(&path.join("archive"), release::MAX_ARCHIVE)?;
        if archive_id != receipt.archive {
            return Err("The recorded Zuse download was replaced".into());
        }
        let binary = archive::executable(&content, &receipt.release, self.release_target()?)?;
        let installation_id = id()?;
        let destination = self.location("installations", &installation_id)?;
        let stage = Stage::new(destination)?;
        let binary_identity = create(
            &stage
                .path
                .join(release::executable_name(&receipt.release.target)),
            &binary,
            true,
        )?;
        let installed = Installation {
            schema: SCHEMA,
            owner_id: self.owner_id.clone(),
            installation_id,
            download_id: receipt.download_id,
            release: receipt.release,
            directory: stage.identity.clone(),
            binary: binary_identity,
            binary_size: binary.len() as u64,
            binary_sha256: release::digest(&binary),
        };
        json(&stage.path.join("receipt.json"), &installed)?;
        stage.publish()?;
        Ok(installed)
    }

    pub(super) fn executable(&self, installation: &Installation) -> PathBuf {
        self.root
            .join("installations")
            .join(&installation.installation_id)
            .join(release::executable_name(&installation.release.target))
    }

    fn verify_installation(&self, id: &str) -> Result<Installation, String> {
        let path = self.location("installations", id)?;
        let receipt: Installation = read(&path.join("receipt.json"))?;
        receipt.release.validate(self.release_target()?)?;
        exact_files(
            &path,
            &[
                release::executable_name(&receipt.release.target),
                "receipt.json",
            ],
        )?;
        if receipt.schema != SCHEMA
            || receipt.owner_id != self.owner_id
            || receipt.installation_id != id
            || !valid_id(&receipt.download_id)
            || directory(&path)? != receipt.directory
        {
            return Err("Zuse installation is not owned by this manager".into());
        }
        let binary = self.executable(&receipt);
        private_file(&binary)?;
        let (actual_id, content) = bytes(&binary, MAX_EXECUTABLE)?;
        if actual_id != receipt.binary
            || content.len() as u64 != receipt.binary_size
            || release::digest(&content) != receipt.binary_sha256
        {
            return Err("Zuse installation identity or bytes changed; refusing ownership".into());
        }
        Ok(receipt)
    }

    pub(super) fn latest(&self) -> Result<Option<Installation>, String> {
        self.check()?;
        let mut installed = Vec::new();
        for (index, entry) in fs::read_dir(self.root.join("installations"))
            .map_err(|error| error.to_string())?
            .enumerate()
        {
            if index >= 256 {
                return Err("Too many Zuse installation entries to inspect safely".into());
            }
            let name = entry.map_err(|error| error.to_string())?.file_name();
            let Some(id) = name.to_str().filter(|name| valid_id(name)) else {
                continue;
            };
            installed.push(self.verify_installation(id)?);
        }
        installed.sort_by_key(|entry| {
            (
                entry
                    .release
                    .version
                    .split('.')
                    .map(|part| part.parse::<u32>().unwrap_or(0))
                    .collect::<Vec<_>>(),
                entry.installation_id.clone(),
            )
        });
        Ok(installed.pop())
    }

    pub(super) fn matching(&self, executable: &Path) -> Result<Option<Installation>, String> {
        // program::executable returns a canonical path (extended-length on
        // Windows). Compare the same representation on both sides; path
        // equality alone never grants ownership.
        let executable = executable
            .canonicalize()
            .map_err(|error| error.to_string())?;
        let installations = self
            .root
            .join("installations")
            .canonicalize()
            .map_err(|error| error.to_string())?;
        let Some(parent) = executable.parent() else {
            return Ok(None);
        };
        if parent.parent() != Some(installations.as_path()) {
            return Ok(None);
        }
        let Some(id) = parent
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|id| valid_id(id))
        else {
            return Ok(None);
        };
        let receipt = self.verify_installation(id)?;
        if self
            .executable(&receipt)
            .canonicalize()
            .map_err(|error| error.to_string())?
            != executable
        {
            return Ok(None);
        }
        Ok(Some(receipt))
    }

    pub(super) fn uninstall(&self, id: &str) -> Result<Installation, String> {
        let receipt = self.verify_installation(id)?;
        let path = self.location("installations", id)?;
        let tombstone = path.with_file_name(format!(".removing-{id}-{}", self::id()?));
        // Move only the identity-checked private installation out of the
        // published namespace before removing its two known ordinary files.
        fs::rename(&path, &tombstone).map_err(|error| error.to_string())?;
        if directory(&tombstone)? != receipt.directory {
            return Err(
                "Zuse installation changed before removal; no contents were deleted".into(),
            );
        }
        let name = release::executable_name(&receipt.release.target);
        exact_files(&tombstone, &[name, "receipt.json"])?;
        let (actual_id, content) = bytes(&tombstone.join(name), MAX_EXECUTABLE)?;
        if actual_id != receipt.binary || release::digest(&content) != receipt.binary_sha256 {
            return Err("Zuse executable changed before removal; no contents were deleted".into());
        }
        fs::remove_file(tombstone.join(name)).map_err(|error| error.to_string())?;
        fs::remove_file(tombstone.join("receipt.json")).map_err(|error| error.to_string())?;
        fs::remove_dir(&tombstone).map_err(|error| error.to_string())?;
        sync_directory(&self.root.join("installations"))?;
        Ok(receipt)
    }
}

fn exact_files(path: &Path, names: &[&str]) -> Result<(), String> {
    directory(path)?;
    let entries = fs::read_dir(path)
        .map_err(|error| error.to_string())?
        .take(names.len() + 1)
        .map(|entry| {
            entry
                .map(|entry| entry.file_name())
                .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    if entries.len() != names.len()
        || entries
            .iter()
            .any(|entry| !entry.to_str().is_some_and(|entry| names.contains(&entry)))
    {
        return Err("Zuse owned directory contains unexpected or missing files".into());
    }
    Ok(())
}

struct Stage {
    path: PathBuf,
    destination: PathBuf,
    identity: Identity,
}

impl Stage {
    fn new(destination: PathBuf) -> Result<Self, String> {
        if fs::symlink_metadata(&destination).is_ok() {
            return Err("Zuse destination already exists; nothing was replaced".into());
        }
        let path = destination.with_file_name(format!(".pending-{}", id()?));
        crate::platform::private_directory(&path).map_err(|error| error.to_string())?;
        let identity = directory(&path)?;
        Ok(Self {
            path,
            destination,
            identity,
        })
    }

    fn publish(self) -> Result<(), String> {
        if directory(&self.path)? != self.identity
            || fs::symlink_metadata(&self.destination).is_ok()
        {
            return Err("Zuse staging or destination identity changed".into());
        }
        sync_directory(&self.path)?;
        fs::rename(&self.path, &self.destination).map_err(|error| error.to_string())?;
        sync_directory(
            self.destination
                .parent()
                .ok_or("Zuse destination has no parent")?,
        )?;
        Ok(())
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        if directory(&self.path).ok().as_ref() != Some(&self.identity) {
            return;
        }
        // No recursive deletion: even an unexpected extra file is preserved.
        for name in ["archive", "receipt.json", "zuse", "zuse.exe"] {
            let _ = fs::remove_file(self.path.join(name));
        }
        let _ = fs::remove_dir(&self.path);
    }
}

#[cfg(test)]
mod initialization_tests {
    use super::*;

    struct Temporary(PathBuf);

    impl Temporary {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("ssmcp-zuse-init-{}", id().unwrap()));
            crate::platform::private_directory(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn partial_owner_write_is_removed_and_initialization_can_be_retried() {
        let temporary = Temporary::new();
        let root = temporary.0.join("manager");
        let result = Store::open_with(&root, true, |file, _| {
            file.write_all(b"{")?;
            Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "injected full disk",
            ))
        });
        assert!(result.is_err());
        assert!(
            !root.exists(),
            "an incomplete owned root must not poison retries"
        );
        let store = Store::open(&root, true).unwrap().unwrap();
        store.check().unwrap();
    }

    #[test]
    fn an_existing_unreceipted_directory_is_not_initialized_or_removed() {
        let temporary = Temporary::new();
        let root = temporary.0.join("manager");
        crate::platform::private_directory(&root).unwrap();
        let sentinel = root.join("user-data");
        fs::write(&sentinel, b"preserve").unwrap();
        assert!(Store::open(&root, true).is_err());
        assert_eq!(fs::read(&sentinel).unwrap(), b"preserve");
        assert!(!root.join("owner.json").exists());
        assert!(!root.join("manager.lock").exists());
    }
}
