//! Offline, same-issuer migration of the existing hash-only OAuth stores.
//! This is not a portal/account backup and never changes a token's deadline.
use super::{
    OAuth, OAuthConfig,
    grants::Grants,
    registry::{self, Registry},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::CString,
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path, PathBuf},
};

const CLIENTS: &str = "oauth-clients.toml";
const GRANTS: &str = "oauth-clients.toml.grants.json";
const MANIFEST: &str = "migration.json";
const SCHEMA: &str = "ssmcp-oauth-migration-v1";
const MAX_STATE: u64 = 64 * 1024 * 1024;
const MAX_MANIFEST: u64 = 16 * 1024;
const HELP: &str = "Usage:\n\
  ssmcpd oauth export --public-url https://HOST --client-registry FILE --output NEW_DIR\n\
  ssmcpd oauth import --public-url https://HOST --input EXPORT_DIR --output NEW_DIR\n\
\n\
Run as the daemon's state owner during an offline maintenance window.\n\
The source writer lock must be free; a live daemon is never bypassed.\n\
All paths must be absolute. NEW_DIR must not exist. Directories use 0700\n\
and files use 0600. Import requires the same issuer and preserves expiry,\n\
refresh replay markers and revocations; it cannot restore legacy sessions.\n\
The JSON receipt reports the new client_registry path. Configure it explicitly.\n\
This migrates MCP OAuth authorization, not portal accounts, device logins,\n\
network connections, server configuration or a complete server backup.\n\
An ordinary in-place upgrade keeps the existing state and issuer; no import is needed.\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operation {
    Export,
    Import,
}

struct Options {
    operation: Operation,
    issuer: String,
    source: PathBuf,
    output: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Coverage {
    AccountGrants,
    ClientRegistrationsOnly,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Member {
    bytes: u64,
    sha256: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: String,
    issuer: String,
    created_at_ms: u64,
    coverage: Coverage,
    clients: Member,
    grants: Member,
}

struct Loaded {
    oauth: OAuth,
    coverage: Coverage,
}

/// Parse the complete command before accessing any filesystem or credentials.
pub fn run(args: &[String]) -> crate::Result<String> {
    let Some(options) = parse(args)? else {
        return Ok(HELP.into());
    };
    let loaded = match options.operation {
        Operation::Export => load(&options.source, &options.issuer)?,
        Operation::Import => read_package(&options.source, &options.issuer)?,
    };
    publish(&loaded, &options)
}

fn parse(args: &[String]) -> crate::Result<Option<Options>> {
    if args.is_empty() || (args.len() == 1 && matches!(args[0].as_str(), "--help" | "-h")) {
        return Ok(None);
    }
    if args.len() == 2
        && matches!(args[0].as_str(), "export" | "import")
        && matches!(args[1].as_str(), "--help" | "-h")
    {
        return Ok(None);
    }
    let operation = match args.first().map(String::as_str) {
        Some("export") => Operation::Export,
        Some("import") => Operation::Import,
        _ => return Err("expected oauth export or import; see ssmcpd oauth --help".into()),
    };
    let source_flag = if operation == Operation::Export {
        "--client-registry"
    } else {
        "--input"
    };
    let mut values = BTreeMap::new();
    for pair in args[1..].chunks(2) {
        if pair.len() != 2
            || !["--public-url", "--output", source_flag].contains(&pair[0].as_str())
            || pair[1].is_empty()
            || values.insert(pair[0].as_str(), pair[1].as_str()).is_some()
        {
            return Err(
                "invalid or repeated OAuth migration option; see ssmcpd oauth --help".into(),
            );
        }
    }
    let required = |key| {
        values
            .get(key)
            .copied()
            .ok_or("missing OAuth migration option; see ssmcpd oauth --help")
    };
    let issuer = issuer(required("--public-url")?)?;
    let source = PathBuf::from(required(source_flag)?);
    let output = PathBuf::from(required("--output")?);
    if !source.is_absolute() || !output.is_absolute() {
        return Err("OAuth migration paths must be absolute".into());
    }
    Ok(Some(Options {
        operation,
        issuer,
        source,
        output,
    }))
}

fn issuer(input: &str) -> crate::Result<String> {
    // Match the gateway's origin rules; URL path normalization must not hide a
    // supplied path such as /folder/.. or an empty user-info component.
    let uri: axum::http::Uri = input
        .parse()
        .map_err(|_| "OAuth migration requires an HTTPS origin")?;
    let authority = uri
        .authority()
        .ok_or("OAuth migration requires a hostname")?;
    if uri.scheme_str() != Some("https")
        || authority.as_str().contains('@')
        || input.contains('#')
        || uri.query().is_some()
        || !matches!(uri.path(), "" | "/")
        || authority.host().is_empty()
    {
        return Err(
            "OAuth migration requires an HTTPS origin without credentials, path, query or fragment"
                .into(),
        );
    }
    let suffix = &authority.as_str()[authority.host().len()..];
    if !suffix.is_empty()
        && (!suffix.starts_with(':') || authority.port_u16().is_none_or(|port| port == 0))
    {
        return Err("OAuth migration requires a valid origin port".into());
    }
    let authority = authority.as_str().to_ascii_lowercase();
    Ok(format!(
        "https://{}",
        authority.strip_suffix(":443").unwrap_or(&authority)
    ))
}

fn load(path: &Path, issuer: &str) -> crate::Result<Loaded> {
    let mut oauth = OAuth::with_config(issuer.into(), OAuthConfig::default())?;
    // Unlike normal daemon startup, a typo must not create an empty source.
    let (registry, clients) = Registry::open_existing(path, issuer)?;
    let grants = Grants::open(&registry, issuer)?;
    let (directory, filename, _lease) = registry.grant_storage()?;
    let coverage = match registry::open_at(&directory, &filename, libc::O_RDONLY | libc::O_NONBLOCK)
    {
        Ok(file) => {
            registry::private_file(&file)?;
            Coverage::AccountGrants
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Coverage::ClientRegistrationsOnly,
        Err(error) => return Err(error.into()),
    };
    {
        let mut state = oauth
            .state
            .lock()
            .map_err(|_| "OAuth migration state lock failed")?;
        state.clients = clients;
        grants.restore(&mut state)?;
        state.grants = Some(grants);
    }
    oauth.registry = Some(registry);
    Ok(Loaded { oauth, coverage })
}

fn read_package(path: &Path, issuer: &str) -> crate::Result<Loaded> {
    let directory = private_directory(path)?;
    let bytes = read_private(&directory, MANIFEST, MAX_MANIFEST)?;
    let manifest: Manifest =
        serde_json::from_slice(&bytes).map_err(|_| "invalid OAuth migration manifest")?;
    if manifest.schema != SCHEMA
        || manifest.issuer != issuer
        || manifest.created_at_ms > super::grants::wall_now_ms()?
    {
        return Err("OAuth migration schema, issuer or clock does not match".into());
    }
    verify_members(&directory, &manifest)?;
    let mut loaded = load(&path.join(CLIENTS), issuer)?;
    let (held, _, _lease) = loaded.oauth.registry.as_ref().unwrap().grant_storage()?;
    if identity(&held)? != identity(&directory)? {
        return Err("OAuth migration directory changed while acquiring its writer lock".into());
    }
    // Hold that lock until destination publication, including the second read.
    verify_members(&held, &manifest)?;
    if manifest.coverage == Coverage::ClientRegistrationsOnly {
        let state = loaded
            .oauth
            .state
            .lock()
            .map_err(|_| "OAuth migration state lock failed")?;
        if !state.families.is_empty()
            || !state.access.is_empty()
            || !state.refresh.is_empty()
            || !state.codes.is_empty()
        {
            return Err("DCR-only migration contains account grants".into());
        }
    }
    loaded.coverage = manifest.coverage;
    Ok(loaded)
}

fn publish(source: &Loaded, options: &Options) -> crate::Result<String> {
    let mut stage = Stage::new(&options.output)?;
    let path = stage.path.join(CLIENTS);
    let (destination, _) = Registry::open(&path, &options.issuer)?;
    let (held, _, lease) = destination.grant_storage()?;
    if identity(&held)? != identity(&stage.directory)? {
        return Err("OAuth migration staging directory changed".into());
    }
    {
        let state = source
            .oauth
            .state
            .lock()
            .map_err(|_| "OAuth migration state lock failed")?;
        // These existing serializers retain the original absolute deadlines,
        // used refresh/code hashes and revocation omissions. No new grant is made.
        destination.save(&state.clients)?;
        Grants::open(&destination, &options.issuer)?.save(&state)?;
    }
    drop(lease);
    drop(held);
    drop(destination);
    let checked = load(&path, &options.issuer)?;
    let (clients, families, access, refresh) = {
        let state = checked
            .oauth
            .state
            .lock()
            .map_err(|_| "OAuth migration state lock failed")?;
        (
            state.clients.len(),
            state.families.len(),
            state.access.len(),
            state.refresh.len(),
        )
    };
    let manifest = Manifest {
        schema: SCHEMA.into(),
        issuer: options.issuer.clone(),
        created_at_ms: super::grants::wall_now_ms()?,
        coverage: source.coverage,
        clients: member(&stage.directory, CLIENTS)?,
        grants: member(&stage.directory, GRANTS)?,
    };
    let bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|_| "cannot encode OAuth migration manifest")?;
    if bytes.len() as u64 > MAX_MANIFEST {
        return Err("OAuth migration manifest is too large".into());
    }
    let mut file = registry::open_at(
        &stage.directory,
        &CString::new(MANIFEST)?,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NONBLOCK,
    )?;
    registry::private_file(&file)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    stage.directory.sync_all()?;
    drop(file);
    drop(checked);
    stage.publish()?;
    serde_json::to_string_pretty(&serde_json::json!({
        "schema":"ssmcp-oauth-migration-receipt-v1",
        "status":if options.operation == Operation::Export { "exported" } else { "imported" },
        "issuer":options.issuer,
        "coverage":source.coverage,
        "directory":options.output,
        "client_registry":options.output.join(CLIENTS),
        "restored_clients":clients,"restored_account_families":families,
        "restored_access_records":access,"restored_refresh_records":refresh,
        "files":{"clients":manifest.clients,"grants":manifest.grants},
        "configuration_changed":false,"service_started":false,
        "message":"Keep the original issuer and deadlines. This is MCP OAuth state, not a complete server backup."
    })).map_err(|_| "cannot encode OAuth migration receipt".into())
}

fn verify_members(directory: &File, manifest: &Manifest) -> crate::Result<()> {
    if member(directory, CLIENTS)? != manifest.clients
        || member(directory, GRANTS)? != manifest.grants
    {
        return Err("OAuth migration state does not match its manifest".into());
    }
    Ok(())
}

fn member(directory: &File, name: &str) -> crate::Result<Member> {
    let bytes = read_private(directory, name, MAX_STATE)?;
    Ok(Member {
        bytes: bytes.len() as u64,
        sha256: crate::hex::encode(Sha256::digest(&bytes)),
    })
}

fn read_private(directory: &File, name: &str, limit: u64) -> crate::Result<Vec<u8>> {
    let file = registry::open_at(
        directory,
        &CString::new(name)?,
        libc::O_RDONLY | libc::O_NONBLOCK,
    )?;
    registry::private_file(&file)?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err("OAuth migration member exceeds its size limit".into());
    }
    Ok(bytes)
}

fn identity(file: &File) -> io::Result<(u64, u64)> {
    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

fn private_directory(path: &Path) -> crate::Result<File> {
    let (parent, name) = parent(path)?;
    let directory = registry::open_at(&parent, &name, libc::O_RDONLY | libc::O_DIRECTORY)?;
    let metadata = directory.metadata()?;
    // SAFETY: geteuid has no preconditions.
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o777 != 0o700 {
        return Err(
            "OAuth migration directory must be owned by the current daemon user with mode 0700"
                .into(),
        );
    }
    Ok(directory)
}

fn parent(path: &Path) -> crate::Result<(File, CString)> {
    if !path.is_absolute() {
        return Err("OAuth migration path must be absolute".into());
    }
    let parent = path
        .parent()
        .ok_or("OAuth migration path needs a parent directory")?;
    let name = CString::new(
        path.file_name()
            .ok_or("OAuth migration path needs a final name")?
            .as_bytes(),
    )?;
    let mut directory = File::open("/")?;
    for component in parent.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                directory = registry::open_at(
                    &directory,
                    &CString::new(name.as_bytes())?,
                    libc::O_RDONLY | libc::O_DIRECTORY,
                )?;
                let metadata = directory.metadata()?;
                // SAFETY: geteuid has no preconditions. Match the registry's trusted ancestors.
                if (metadata.uid() != 0 && metadata.uid() != unsafe { libc::geteuid() })
                    || (metadata.mode() & 0o022 != 0
                        && !(metadata.uid() == 0 && metadata.mode() & 0o1000 != 0))
                {
                    return Err(
                        "OAuth migration ancestors must be trusted and not writable by other users"
                            .into(),
                    );
                }
            }
            _ => return Err("OAuth migration path cannot contain dot or parent components".into()),
        }
    }
    Ok((directory, name))
}

struct Stage {
    parent: File,
    name: CString,
    target: CString,
    directory: File,
    path: PathBuf,
    published: bool,
}

impl Stage {
    fn new(output: &Path) -> crate::Result<Self> {
        let (parent, target) = parent(output)?;
        // Reject every existing destination, including a dangling symlink.
        if entry(&parent, &target)?.is_some() {
            return Err("OAuth migration output already exists; it will not be replaced".into());
        }
        let name = super::random(".oauth-migration-")
            .map_err(|_| "cannot create an OAuth migration staging name")?;
        let path = output.parent().unwrap().join(&name);
        let name = CString::new(name)?;
        // SAFETY: names and parent descriptor remain live; this creates only our fresh private directory.
        if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        let directory = match registry::open_at(&parent, &name, libc::O_RDONLY | libc::O_DIRECTORY)
        {
            Ok(directory) => directory,
            Err(error) => {
                // SAFETY: remove only the new, still-empty directory created above.
                unsafe {
                    libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR);
                }
                return Err(error.into());
            }
        };
        Ok(Self {
            parent,
            name,
            target,
            directory,
            path,
            published: false,
        })
    }

    fn publish(&mut self) -> crate::Result<()> {
        if entry(&self.parent, &self.name)? != Some(identity(&self.directory)?) {
            return Err("OAuth migration staging directory was replaced".into());
        }
        // SAFETY: both names are relative to the held parent. NOREPLACE preserves a racing destination.
        if unsafe {
            libc::renameat2(
                self.parent.as_raw_fd(),
                self.name.as_ptr(),
                self.parent.as_raw_fd(),
                self.target.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        } != 0
        {
            return Err(io::Error::last_os_error().into());
        }
        self.published = true;
        self.parent.sync_all().map_err(|_| "OAuth migration was published but parent sync failed; preserve the existing destination and inspect it before retrying".into())
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        if self.published
            || entry(&self.parent, &self.name).ok().flatten() != identity(&self.directory).ok()
        {
            return;
        }
        for name in [
            CLIENTS,
            GRANTS,
            MANIFEST,
            "oauth-clients.toml.lock",
            "oauth-clients.toml.grants.json.pending",
        ] {
            let name = CString::new(name).expect("static filename");
            // SAFETY: cleanup is confined to known filenames in our unpublished directory descriptor.
            unsafe {
                libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0);
            }
        }
        // Unknown files cause rmdir to fail; they are never recursively deleted.
        // SAFETY: the staging dirent still identifies our held directory, checked above.
        unsafe {
            libc::unlinkat(
                self.parent.as_raw_fd(),
                self.name.as_ptr(),
                libc::AT_REMOVEDIR,
            );
        }
    }
}

fn entry(directory: &File, name: &CString) -> io::Result<Option<(u64, u64)>> {
    // SAFETY: fstatat initializes stat on success; no symlink is followed.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } == 0
    {
        return Ok(Some((stat.st_dev, stat.st_ino)));
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::NotFound {
        Ok(None)
    } else {
        Err(error)
    }
}

#[cfg(test)]
mod tests;
