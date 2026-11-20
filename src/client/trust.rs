//! Optional TLS public-key pins, scoped to an exact HTTPS origin.
//! Pins supplement WebPKI validation; they never replace certificate or hostname checks.
use super::account::normalize_server;
use crate::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rustls::{
    ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
    client::{
        WebPkiServerVerifier,
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    },
    pki_types::{CertificateDer, ServerName, SubjectPublicKeyInfoDer, UnixTime, pem::PemObject},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

const STORE_LIMIT: usize = 256 * 1024;
const KEY_LIMIT: usize = 16 * 1024;
const MAX_SERVERS: usize = 128;
const MAX_KEYS: usize = 16;
const HELP: &str = "Usage:
  ssmcp trust add --server URL --public-key FILE
  ssmcp trust add --server URL --pin sha256//BASE64
  ssmcp trust list
  ssmcp trust remove --server URL
  ssmcp trust path

Import the HTTPS endpoint's PEM PUBLIC KEY or its SHA-256 SPKI fingerprint.
Obtain it from the administrator over an independently verified channel.
These pins supplement normal TLS certificate and hostname verification.
The trust file is trusted-servers.toml beside the client config.toml.";

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct Store {
    servers: Vec<Server>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Server {
    server: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    public_keys: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pins: Vec<String>,
}

fn path() -> Result<PathBuf> {
    Ok(super::config::config_path()?.with_file_name("trusted-servers.toml"))
}

fn https_origin(server: &str) -> Result<String> {
    let server = normalize_server(server)?;
    if !server.starts_with("https://") {
        return Err("server public-key pins require an HTTPS origin".into());
    }
    Ok(server)
}

fn decode_pin(pin: &str) -> Result<[u8; 32]> {
    let value = pin
        .strip_prefix("sha256//")
        .ok_or("a server pin must use sha256// followed by a base64 SHA-256 digest")?;
    let bytes = STANDARD
        .decode(value)
        .map_err(|_| "invalid server pin base64")?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| "a server pin must contain exactly 32 digest bytes")?;
    if STANDARD.encode(bytes) != value {
        return Err("server pin must use canonical padded base64".into());
    }
    Ok(bytes)
}

fn encode_pin(pin: &[u8; 32]) -> String {
    format!("sha256//{}", STANDARD.encode(pin))
}

fn key_pin(pem: &str) -> Result<[u8; 32]> {
    const BEGIN: &str = "-----BEGIN PUBLIC KEY-----";
    const END: &str = "-----END PUBLIC KEY-----";
    let text = pem.trim();
    if text.len() > KEY_LIMIT
        || !text.starts_with(BEGIN)
        || !text.ends_with(END)
        || text.matches("-----BEGIN ").count() != 1
        || text.matches("-----END ").count() != 1
    {
        return Err(
            "expected one PEM PUBLIC KEY, not an SSH key, certificate or private key".into(),
        );
    }
    let key = SubjectPublicKeyInfoDer::from_pem_slice(text.as_bytes())
        .map_err(|_| "invalid PEM public key")?;
    webpki::RawPublicKeyEntity::try_from(&key)
        .map_err(|_| "invalid DER SubjectPublicKeyInfo in public key")?;
    Ok(Sha256::digest(key.as_ref()).into())
}

impl Server {
    fn digests(&self) -> Result<Vec<[u8; 32]>> {
        if self.public_keys.len().saturating_add(self.pins.len()) > MAX_KEYS {
            return Err("a server may contain at most 16 trusted public keys or pins".into());
        }
        let mut pins = Vec::new();
        for pin in self
            .pins
            .iter()
            .map(|pin| decode_pin(pin))
            .chain(self.public_keys.iter().map(|key| key_pin(key)))
        {
            let pin = pin?;
            if !pins.contains(&pin) {
                pins.push(pin);
            }
        }
        if pins.is_empty() {
            return Err("a trusted server must contain at least one public key or pin".into());
        }
        Ok(pins)
    }
}

impl Store {
    fn parse(text: &str) -> Result<Self> {
        if text.len() > STORE_LIMIT {
            return Err("trusted-servers.toml exceeds its size limit".into());
        }
        // TOML diagnostics can echo accidentally pasted private material.
        let mut store: Self =
            toml::from_str(text).map_err(|_| "invalid trusted-servers.toml configuration")?;
        if store.servers.len() > MAX_SERVERS {
            return Err("trusted-servers.toml may contain at most 128 servers".into());
        }
        let mut seen = Vec::new();
        for entry in &mut store.servers {
            entry.server = https_origin(&entry.server)?;
            if seen.contains(&entry.server) {
                return Err("trusted-servers.toml contains a duplicate server origin".into());
            }
            seen.push(entry.server.clone());
            entry.digests()?;
        }
        Ok(store)
    }

    fn pins(&self, server: &str) -> Result<Vec<[u8; 32]>> {
        let server = normalize_server(server)?;
        self.servers
            .iter()
            .find(|entry| entry.server == server)
            .map(Server::digests)
            .transpose()
            .map(|pins| pins.unwrap_or_default())
    }
}

fn read_file(path: &Path, limit: usize, trusted: bool) -> Result<Option<String>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot read {}: {error}", path.display()).into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err("server trust input must be a regular file, not a symlink".into());
    }
    #[cfg(unix)]
    if trusted {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if ![0, unsafe { libc::geteuid() }].contains(&metadata.uid())
            || metadata.permissions().mode() & 0o022 != 0
        {
            return Err(
                "trusted-servers.toml must be user/root-owned and not writable by other users"
                    .into(),
            );
        }
    }
    #[cfg(not(unix))]
    let _ = trusted;
    if metadata.len() > limit as u64 {
        return Err("server trust input exceeds its size limit".into());
    }
    let mut text = String::new();
    file.take(limit as u64 + 1).read_to_string(&mut text)?;
    if text.len() > limit {
        return Err("server trust input exceeds its size limit".into());
    }
    Ok(Some(text))
}

fn load_at(path: &Path) -> Result<Store> {
    match read_file(path, STORE_LIMIT, true)? {
        Some(text) => Store::parse(&text),
        None => Ok(Store::default()),
    }
}

fn write_at(path: &Path, store: &Store) -> Result<()> {
    let text = toml::to_string_pretty(store)?;
    Store::parse(&text)?;
    let parent = path.parent().ok_or("trust file has no parent")?;
    fs::create_dir_all(parent)?;
    let metadata = fs::symlink_metadata(parent)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("server trust directory must be a regular directory".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o022 != 0
        {
            return Err(
                "server trust directory must belong to this user and not be writable by others"
                    .into(),
            );
        }
    }
    let _ = read_file(path, STORE_LIMIT, true)?;
    let mut random = [0; 16];
    crate::platform::random_bytes(&mut random)?;
    let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let temporary = path.with_extension(format!("tmp-{suffix}"));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

/// Return canonical SPKI fingerprints for invitation transfer.

#[derive(Debug)]
struct PinnedVerifier {
    normal: Arc<WebPkiServerVerifier>,
    pins: Vec<[u8; 32]>,
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        let verified = self.normal.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        )?;
        let certificate = webpki::EndEntityCert::try_from(end_entity).map_err(|_| {
            rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding)
        })?;
        let digest: [u8; 32] =
            Sha256::digest(certificate.subject_public_key_info().as_ref()).into();
        if !self.pins.contains(&digest) {
            return Err(rustls::Error::General(
                "server public key does not match trusted-servers.toml; verify the server's key through a trusted channel".into(),
            ));
        }
        Ok(verified)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.normal.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.normal.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.normal.supported_verify_schemes()
    }
}

fn build_tls_config(pins: Vec<[u8; 32]>, roots: RootCertStore) -> Result<Arc<ClientConfig>> {
    if pins.is_empty() {
        return Err("TLS pinning requires at least one public key".into());
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let normal =
        WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone()).build()?;
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedVerifier { normal, pins }))
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// With no pin for this origin, keep the caller's normal TLS configuration.
/// A configured pin always supplements WebPKI and fails closed on mismatch.
pub(super) fn tls_config(server: &str) -> Result<Option<Arc<ClientConfig>>> {
    let pins = load_at(&path()?)?.pins(server)?;
    if pins.is_empty() {
        return Ok(None);
    }
    let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    Ok(Some(build_tls_config(pins, roots)?))
}

fn argument<'a>(args: &'a [OsString], index: &mut usize) -> Result<&'a str> {
    *index += 1;
    args.get(*index)
        .and_then(|value| value.to_str())
        .ok_or_else(|| "trust option requires a UTF-8 value".into())
}

pub(super) fn dispatch(args: &[OsString]) -> Result<bool> {
    if !args.first().is_some_and(|arg| arg == "trust") {
        return Ok(false);
    }
    if args.len() == 1 || args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("{HELP}");
        return Ok(true);
    }
    let action = args[1].to_str().ok_or("trust command must be UTF-8")?;
    let path = path()?;
    if action == "path" && args.len() == 2 {
        println!("{}", path.display());
        return Ok(true);
    }
    if action == "list" && args.len() == 2 {
        let store = load_at(&path)?;
        for entry in &store.servers {
            for pin in entry.digests()? {
                println!("{} {}", entry.server, encode_pin(&pin));
            }
        }
        if store.servers.is_empty() {
            println!(
                "No server public-key pins configured. Normal HTTPS verification remains enabled."
            );
        }
        return Ok(true);
    }
    if !matches!(action, "add" | "remove") {
        return Err("unknown trust command or options; see ssmcp trust --help".into());
    }
    let mut server = None;
    let mut context = None;
    let mut keys = Vec::new();
    let mut fingerprints = Vec::new();
    let mut index = 2;
    while index < args.len() {
        match args[index].to_str() {
            Some("--server") if server.is_none() => {
                server = Some(https_origin(argument(args, &mut index)?)?);
            }
            Some("--context") => {
                super::connection_context::set_name(&mut context, argument(args, &mut index)?)?;
            }
            Some(value) if value.starts_with("--context=") => {
                super::connection_context::set_name(&mut context, &value["--context=".len()..])?;
            }
            Some("--public-key") if action == "add" => {
                let key_path = Path::new(argument(args, &mut index)?);
                let key = read_file(key_path, KEY_LIMIT, false)?
                    .ok_or("public key file does not exist")?;
                key_pin(&key)?;
                keys.push(key.trim().to_owned());
            }
            Some("--pin") if action == "add" => {
                let pin = argument(args, &mut index)?;
                fingerprints.push(encode_pin(&decode_pin(pin)?));
            }
            _ => return Err("unknown or repeated trust option; see ssmcp trust --help".into()),
        }
        index += 1;
    }
    let server = super::connection_context::remote_server(context.as_deref(), server.as_deref())?
        .ok_or("trust add/remove requires --server URL or a selected HTTPS context")?;
    let mut store = load_at(&path)?;
    if action == "remove" {
        let original = store.servers.len();
        store.servers.retain(|entry| entry.server != server);
        if original != store.servers.len() {
            write_at(&path, &store)?;
        }
        println!("Removed public-key pins for {server}.");
        return Ok(true);
    }
    if keys.is_empty() && fingerprints.is_empty() {
        return Err("trust add requires --public-key FILE or --pin sha256//BASE64".into());
    }
    let position = if let Some(index) = store
        .servers
        .iter()
        .position(|entry| entry.server == server)
    {
        index
    } else {
        store.servers.push(Server {
            server: server.clone(),
            public_keys: Vec::new(),
            pins: Vec::new(),
        });
        store.servers.len() - 1
    };
    let entry = &mut store.servers[position];
    let mut existing = if entry.public_keys.is_empty() && entry.pins.is_empty() {
        Vec::new()
    } else {
        entry.digests()?
    };
    for key in keys {
        let digest = key_pin(&key)?;
        if !existing.contains(&digest) {
            entry.public_keys.push(key);
            existing.push(digest);
        }
    }
    for pin in fingerprints {
        let digest = decode_pin(&pin)?;
        if !existing.contains(&digest) {
            entry.pins.push(pin);
            existing.push(digest);
        }
    }
    entry.digests()?;
    write_at(&path, &store)?;
    println!(
        "Trusted HTTPS public keys for {server} in {}.",
        path.display()
    );
    for pin in existing {
        println!("{}", encode_pin(&pin));
    }
    Ok(true)
}

#[cfg(test)]
mod tests;
