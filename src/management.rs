//! Private portal-to-gateway administration. The Unix socket is readable only
//! by the daemon UID; public HTTP never accepts an identity assertion.
use crate::{
    broker::Broker,
    history::{AuditAction, AuditEvent, AuditLog, AuditOutcome, AuditPage},
    identity::Identity,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    io,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::Semaphore,
    task::JoinSet,
    time::timeout,
};

const MAX_REQUEST: usize = 16 * 1024;
const MAX_RESPONSE: usize = 8 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, Default)]
pub struct ManagementPolicy {
    admin_subjects: HashSet<String>,
    owner_subjects: HashMap<String, String>,
}

impl ManagementPolicy {
    pub fn new(
        admin_subjects: Vec<String>,
        owner_subjects: HashMap<String, String>,
    ) -> crate::Result<Self> {
        if admin_subjects.iter().any(|id| !valid_subject(id))
            || owner_subjects.iter().any(|(user, subject)| {
                !crate::broker::valid_username(user)
                    || user == crate::broker::IDENTITY_RESOURCE
                    || !valid_subject(subject)
                    || user.strip_prefix("gh-").is_some_and(|id| id != subject)
            })
        {
            return Err("management principals must use canonical GitHub numeric IDs and valid SSH usernames".into());
        }
        Ok(Self {
            admin_subjects: admin_subjects.into_iter().collect(),
            owner_subjects,
        })
    }

    pub fn is_admin(&self, identity: &Identity) -> bool {
        valid_identity(identity) && self.admin_subjects.contains(&identity.subject)
    }

    /// Only the peer-credential-backed SSH username establishes ownership.
    /// An MCP --allow entry grants tool access, never ownership.
    pub fn owner_subject(&self, ssh_user: &str) -> Option<String> {
        if let Some(id) = ssh_user.strip_prefix("gh-").filter(|id| valid_subject(id)) {
            return Some(id.to_owned());
        }
        self.owner_subjects.get(ssh_user).cloned()
    }
}

pub(crate) struct State {
    pub policy: ManagementPolicy,
    pub audit: Arc<AuditLog>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    #[default]
    Own,
    All,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AllowedIdentity {
    pub subject: String,
    pub login: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub id: String,
    pub name: String,
    pub hostname: String,
    pub cwd: String,
    pub mode: String,
    pub ssh_user: String,
    pub owner_subject: Option<String>,
    pub connected_at: u64,
    pub allowed_github: Vec<AllowedIdentity>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DashboardStats {
    pub active_sessions: usize,
    /// Distinct SSH accounts represented in the visible live sessions.
    pub owners: usize,
    /// Distinct hostnames reported by clients; not an attested device count.
    pub hosts: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DashboardSnapshot {
    pub is_admin: bool,
    pub scope: Scope,
    pub generated_at: u64,
    pub sessions: Vec<SessionSnapshot>,
    pub stats: DashboardStats,
    pub audit: Vec<AuditEvent>,
    pub audit_next_before: Option<u64>,
    pub retention_days: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagementError {
    Disabled,
    Forbidden,
    NotFound,
    Unavailable,
    Invalid,
}

impl std::fmt::Display for ManagementError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Disabled => "session management is not configured",
            Self::Forbidden => "this account cannot perform that operation",
            Self::NotFound => "the session is no longer available to this account",
            Self::Unavailable => "the session management service is unavailable",
            Self::Invalid => "invalid management request",
        })
    }
}
impl std::error::Error for ManagementError {}

pub(crate) fn valid_subject(subject: &str) -> bool {
    subject
        .parse::<u64>()
        .is_ok_and(|value| value > 0 && value.to_string() == subject)
}

pub(crate) fn valid_identity(identity: &Identity) -> bool {
    identity.provider == "github"
        && valid_subject(&identity.subject)
        && identity.login.len() <= 256
        && !identity.login.chars().any(char::is_control)
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Actor {
    provider: String,
    subject: String,
    login: String,
}
impl From<&Identity> for Actor {
    fn from(value: &Identity) -> Self {
        Self {
            provider: value.provider.clone(),
            subject: value.subject.clone(),
            login: value.login.clone(),
        }
    }
}
impl From<Actor> for Identity {
    fn from(value: Actor) -> Self {
        Self {
            provider: value.provider,
            subject: value.subject,
            login: value.login,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Snapshot {
        actor: Actor,
        scope: Scope,
    },
    Disconnect {
        actor: Actor,
        session_id: String,
    },
    Audit {
        actor: Actor,
        scope: Scope,
        limit: usize,
        before: Option<u64>,
    },
    Record {
        actor: Option<Actor>,
        action: AuditAction,
        outcome: AuditOutcome,
    },
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
enum Response {
    Snapshot { snapshot: DashboardSnapshot },
    Audit { page: AuditPage },
    Done,
    Error { error: ManagementError },
}

/// Uses a length-prefixed, bounded, one-request-per-connection protocol.
#[derive(Clone, Debug)]
pub struct ManagementClient {
    socket: PathBuf,
}
impl ManagementClient {
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    async fn request(&self, request: Request) -> Result<Response, ManagementError> {
        let result = timeout(IO_TIMEOUT, async {
            let metadata = std::fs::symlink_metadata(&self.socket)?;
            if !trusted_socket(&metadata) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "untrusted management socket",
                ));
            }
            let mut stream = UnixStream::connect(&self.socket).await?;
            verify_peer(&stream)?;
            write_message(&mut stream, &request, MAX_REQUEST).await?;
            read_message::<Response>(&mut stream, MAX_RESPONSE).await
        })
        .await
        .map_err(|_| ManagementError::Unavailable)?
        .map_err(|_| ManagementError::Unavailable)?;
        match result {
            Response::Error { error } => Err(error),
            response => Ok(response),
        }
    }

    pub async fn snapshot(
        &self,
        actor: &Identity,
        scope: Scope,
    ) -> Result<DashboardSnapshot, ManagementError> {
        match self
            .request(Request::Snapshot {
                actor: actor.into(),
                scope,
            })
            .await?
        {
            Response::Snapshot { snapshot } => Ok(snapshot),
            _ => Err(ManagementError::Unavailable),
        }
    }

    pub async fn disconnect(
        &self,
        actor: &Identity,
        session_id: &str,
    ) -> Result<(), ManagementError> {
        match self
            .request(Request::Disconnect {
                actor: actor.into(),
                session_id: session_id.into(),
            })
            .await?
        {
            Response::Done => Ok(()),
            _ => Err(ManagementError::Unavailable),
        }
    }

    pub async fn audit(
        &self,
        actor: &Identity,
        scope: Scope,
        limit: usize,
        before: Option<u64>,
    ) -> Result<AuditPage, ManagementError> {
        match self
            .request(Request::Audit {
                actor: actor.into(),
                scope,
                limit,
                before,
            })
            .await?
        {
            Response::Audit { page } => Ok(page),
            _ => Err(ManagementError::Unavailable),
        }
    }

    pub async fn record_audit(
        &self,
        actor: &Identity,
        action: AuditAction,
        outcome: AuditOutcome,
    ) -> Result<(), ManagementError> {
        self.record(Some(actor), action, outcome).await
    }

    pub async fn record_anonymous_audit(
        &self,
        action: AuditAction,
        outcome: AuditOutcome,
    ) -> Result<(), ManagementError> {
        self.record(None, action, outcome).await
    }

    async fn record(
        &self,
        actor: Option<&Identity>,
        action: AuditAction,
        outcome: AuditOutcome,
    ) -> Result<(), ManagementError> {
        match self
            .request(Request::Record {
                actor: actor.map(Actor::from),
                action,
                outcome,
            })
            .await?
        {
            Response::Done => Ok(()),
            _ => Err(ManagementError::Unavailable),
        }
    }
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no arguments or memory preconditions.
    unsafe { libc::geteuid() }
}

fn trusted_socket(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_socket()
        && metadata.uid() == effective_uid()
        && metadata.permissions().mode() & 0o777 == 0o600
}

fn verify_peer(stream: &UnixStream) -> io::Result<()> {
    if stream.peer_cred()?.uid() == effective_uid() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "management peer UID differs from daemon UID",
        ))
    }
}

/// Keeps ownership of the socket path, so dropping this server removes only the
/// socket it created. Restarting the public gateway is not needed for portal UI.
pub struct ManagementServer {
    listener: UnixListener,
    socket: PathBuf,
    device: u64,
    inode: u64,
}

impl ManagementServer {
    pub async fn bind(socket: &Path) -> io::Result<Self> {
        if !socket.is_absolute() || socket.file_name().is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "management socket must be an absolute path",
            ));
        }
        let parent = socket
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing socket parent"))?;
        let parent_meta = std::fs::symlink_metadata(parent)?;
        if !parent_meta.is_dir()
            || parent_meta.file_type().is_symlink()
            || parent_meta.permissions().mode() & 0o022 != 0
            || ![0, effective_uid()].contains(&parent_meta.uid())
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "management socket directory is not trusted",
            ));
        }
        match std::fs::symlink_metadata(socket) {
            Ok(metadata) => {
                if !trusted_socket(&metadata) {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "refusing to replace an untrusted management socket",
                    ));
                }
                match UnixStream::connect(socket).await {
                    Ok(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::AddrInUse,
                            "management socket is already active",
                        ));
                    }
                    Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                        std::fs::remove_file(socket)?;
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let listener = UnixListener::bind(socket)?;
        // No accept occurs before permissions are private; peer UID is checked
        // on every accepted connection as a second independent restriction.
        if let Err(error) = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
        {
            let _ = std::fs::remove_file(socket);
            return Err(error);
        }
        let metadata = std::fs::symlink_metadata(socket)?;
        Ok(Self {
            listener,
            socket: socket.to_owned(),
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    pub async fn run(self, broker: Arc<Broker>) -> io::Result<()> {
        let slots = Arc::new(Semaphore::new(32));
        let mut handlers = JoinSet::new();
        loop {
            tokio::select! {
                accepted = self.listener.accept() => {
                    let (stream, _) = accepted?;
                    if verify_peer(&stream).is_err() {
                        continue;
                    }
                    let Ok(permit) = slots.clone().try_acquire_owned() else { continue; };
                    let broker = broker.clone();
                    handlers.spawn(async move {
                        let _permit = permit;
                        let _ = timeout(IO_TIMEOUT, serve_one(stream, broker)).await;
                    });
                }
                _ = handlers.join_next(), if !handlers.is_empty() => {}
            }
        }
    }
}
impl Drop for ManagementServer {
    fn drop(&mut self) {
        if std::fs::symlink_metadata(&self.socket).is_ok_and(|metadata| {
            metadata.file_type().is_socket()
                && metadata.dev() == self.device
                && metadata.ino() == self.inode
        }) {
            let _ = std::fs::remove_file(&self.socket);
        }
    }
}

async fn serve_one(mut stream: UnixStream, broker: Arc<Broker>) -> io::Result<()> {
    verify_peer(&stream)?;
    let request: Request = read_message(&mut stream, MAX_REQUEST).await?;
    let response = match request {
        Request::Snapshot { actor, scope } => broker
            .management_snapshot(&actor.into(), scope)
            .await
            .map(|snapshot| Response::Snapshot { snapshot }),
        Request::Disconnect { actor, session_id } => broker
            .management_disconnect(&actor.into(), &session_id)
            .await
            .map(|()| Response::Done),
        Request::Audit {
            actor,
            scope,
            limit,
            before,
        } => broker
            .management_audit(&actor.into(), scope, limit, before)
            .await
            .map(|page| Response::Audit { page }),
        Request::Record {
            actor,
            action,
            outcome,
        } => {
            let identity = actor.map(Identity::from);
            broker
                .management_record(identity.as_ref(), action, outcome)
                .await
                .map(|()| Response::Done)
        }
    }
    .unwrap_or_else(|error| Response::Error { error });
    write_message(&mut stream, &response, MAX_RESPONSE).await
}

async fn write_message<T: Serialize>(
    stream: &mut UnixStream,
    value: &T,
    limit: usize,
) -> io::Result<()> {
    let data = serde_json::to_vec(value).map_err(io::Error::other)?;
    if data.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "management response exceeded limit",
        ));
    }
    stream.write_u32(data.len() as u32).await?;
    stream.write_all(&data).await?;
    stream.flush().await
}

async fn read_message<T: serde::de::DeserializeOwned>(
    stream: &mut UnixStream,
    limit: usize,
) -> io::Result<T> {
    let len = stream.read_u32().await? as usize;
    if len == 0 || len > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "management frame exceeded limit",
        ));
    }
    let mut data = vec![0; len];
    stream.read_exact(&mut data).await?;
    serde_json::from_slice(&data).map_err(io::Error::other)
}

#[cfg(test)]
mod tests;
