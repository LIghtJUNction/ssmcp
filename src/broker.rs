//! Linux-credential-backed registrations and bounded request forwarding.
mod access;
mod management;
mod traffic;
use crate::{
    identity::{GitHubConfig, Identity, IdentityProviders},
    user_policy::{TrafficError, TrafficFailure, TrafficFailureState, TrafficOutcome},
    wire::{self, ClientInfo, Controller, Frame, Operation},
};
use serde_json::Value;
use std::{
    collections::HashMap,
    ffi::CStr,
    io::{self, Read},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::{
    io::BufReader,
    net::UnixStream,
    sync::{Notify, RwLock, Semaphore, mpsc, oneshot},
    time::{Instant, timeout},
};

const IO_TIMEOUT: Duration = Duration::from_secs(10);
fn request_timeout(operation: &Operation) -> Duration {
    match operation {
        Operation::CallTool { .. } => Duration::from_secs(910),
        Operation::ListTools { .. } => Duration::from_secs(120),
        _ => Duration::from_secs(10),
    }
}
type CallResult = std::result::Result<Value, String>;

struct Call {
    operation: Operation,
    controller: Option<Controller>,
    code_context: Option<wire::CodeContext>,
    authorization: Option<Authorization>,
    reply: oneshot::Sender<CallResult>,
}

pub const LOGIN_LIFETIME: u64 = 1800;
pub const IDENTITY_RESOURCE: &str = "mcp";

/// OAuth identity and connection ownership remain separate authorization paths.
#[derive(Clone)]
pub enum Authorization {
    Legacy { user: String, anchor: Weak<Session> },
    Identity(Identity),
}

impl Authorization {
    pub fn is_live(&self) -> bool {
        match self {
            Self::Legacy { anchor, .. } => {
                anchor.upgrade().is_some_and(|session| session.is_alive())
            }
            Self::Identity(_) => true,
        }
    }

    fn permits(&self, session: &Session) -> bool {
        match self {
            Self::Legacy { .. } => session.allowed().is_empty(),
            Self::Identity(identity) => session.allowed().iter().any(|allowed| {
                allowed.provider == identity.provider && allowed.subject == identity.subject
            }),
        }
    }
}

struct LoginCode {
    value: String,
    expires: Instant,
}

struct SessionTrafficFailure {
    undispatched: String,
    pending: String,
}

pub struct Session {
    pub(crate) collaboration: crate::collab::Hub,
    pub(crate) steering: crate::steering::Queue,
    login: Mutex<Option<LoginCode>>,
    // Membership may change, but grants always identify immutable provider subjects.
    allow: Mutex<Vec<Identity>>,
    info: ClientInfo,
    sender: mpsc::Sender<Call>,
    reports: mpsc::Sender<wire::CodeCompletion>,
    slots: Semaphore,
    alive: AtomicBool,
    ssh_user: String,
    owner_subject: Option<String>,
    connected_at: u64,
    audit: Option<Arc<crate::history::AuditLog>>,
    disconnect: Notify,
    closed: AtomicBool,
    closed_notify: Notify,
    traffic_failure: Mutex<Option<SessionTrafficFailure>>,
}

impl Session {
    fn allowed(&self) -> std::sync::MutexGuard<'_, Vec<Identity>> {
        self.allow.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn connection_error(&self, dispatched: bool, fallback: &str) -> String {
        let failure = self
            .traffic_failure
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match failure.as_ref() {
            Some(failure) if dispatched => failure.pending.clone(),
            Some(failure) => failure.undispatched.clone(),
            None => fallback.into(),
        }
    }

    pub fn client_id(&self) -> &str {
        &self.info.id
    }

    pub fn info(&self) -> ClientInfo {
        self.info.clone()
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    pub fn supports_controller(&self) -> bool {
        // Registration accepts only the current protocol, which includes attribution.
        true
    }

    /// Called only after the HTTP caller rechecks its current token and ACL.
    pub(crate) fn report_code_mode(&self, completion: wire::CodeCompletion) -> Result<(), String> {
        completion.validate()?;
        if !self.is_alive() {
            return Err("client cannot receive code audit completion".into());
        }
        self.reports
            .try_send(completion)
            .map_err(|_| "code audit completion queue unavailable".into())
    }

    fn steering_command(
        &self,
        command: wire::SteeringCommand,
    ) -> Result<wire::SteeringSnapshot, String> {
        if !self.is_alive() {
            return Err("client connection is closed".into());
        }
        self.steering
            .command(self.client_id(), self.collaboration.presence()?, command)
    }

    pub async fn call(&self, operation: Operation) -> CallResult {
        self.call_with_controller(operation, None).await
    }

    pub async fn call_with_controller(
        &self,
        operation: Operation,
        controller: Option<Controller>,
    ) -> CallResult {
        self.call_with_actor(operation, controller, None, true, None)
            .await
    }

    /// Recheck current membership even for a previously looked-up session handle.
    /// Audit attribution never uses controller metadata or the access allowlist.
    pub(crate) async fn call_with_authorization(
        &self,
        operation: Operation,
        controller: Option<Controller>,
        authorization: &Authorization,
    ) -> CallResult {
        let actor = match authorization {
            Authorization::Identity(identity) if crate::management::valid_identity(identity) => {
                Some(identity.subject.as_str())
            }
            _ => None,
        };
        self.call_with_actor(operation, controller, actor, true, Some(authorization))
            .await
    }

    /// Internal handle teardown must proceed even if audit storage is unavailable.
    pub(crate) async fn call_cleanup(&self, operation: Operation) -> CallResult {
        self.call_with_actor(operation, None, None, false, None)
            .await
    }

    async fn call_with_actor(
        &self,
        operation: Operation,
        controller: Option<Controller>,
        actor_subject: Option<&str>,
        audit_required: bool,
        authorization: Option<&Authorization>,
    ) -> CallResult {
        if authorization
            .is_some_and(|authorization| !authorization.is_live() || !authorization.permits(self))
        {
            return Err("session access was revoked; refresh the connected clients".into());
        }
        operation.validate()?;
        let audit_run = crate::code_mode::AUDIT_RUN.try_with(Clone::clone).ok();
        // A later collab.bind in the same run must not reattribute this reply.
        let reply_agent = audit_run
            .as_ref()
            .and_then(|run| run.binding(self.client_id()))
            .map(|binding| binding.agent_id);
        let controller =
            controller.or_else(|| audit_run.as_ref().and_then(|run| run.controller.clone()));
        if let Some(controller) = &controller {
            controller.validate()?;
        }
        let code_context = audit_run.as_ref().map(|run| (*run.context).clone());
        let carries_source = code_context.is_some();
        if !self.alive.load(Ordering::Acquire) {
            return Err(self.connection_error(false, "client disconnected"));
        }
        let _permit = self
            .slots
            .try_acquire()
            .map_err(|_| "client busy; retry later")?;
        let duration = request_timeout(&operation);
        let audit_tool = match &operation {
            Operation::CallTool { name, .. } => Some(name.clone()),
            _ => None,
        };
        if audit_tool.is_some()
            && self
                .audit_call(
                    audit_tool.as_deref(),
                    actor_subject,
                    crate::history::AuditOutcome::Pending,
                )
                .await
                .is_err()
            && audit_required
        {
            return Err("audit intent could not be saved; tool was not dispatched".into());
        }
        let (reply, response) = oneshot::channel();
        if self
            .sender
            .try_send(Call {
                operation,
                controller,
                code_context,
                authorization: authorization.cloned(),
                reply,
            })
            .is_err()
        {
            if audit_tool.is_some() {
                let _ = self
                    .audit_call(
                        audit_tool.as_deref(),
                        actor_subject,
                        crate::history::AuditOutcome::Error,
                    )
                    .await;
            }
            return Err(self.connection_error(
                false,
                "client disconnected or busy; tool was not dispatched",
            ));
        }
        if carries_source && let Some(run) = audit_run.as_ref() {
            run.native_dispatched(self.client_id(), audit_tool.as_deref());
        }
        let started = std::time::Instant::now();
        let result = match timeout(duration, response).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(self.connection_error(true, "client disconnected")),
            Err(_) => Err("client request timed out; its outcome may be unknown".into()),
        };
        let result = if authorization
            .is_some_and(|authorization| !authorization.is_live() || !authorization.permits(self))
        {
            Err("session access was revoked; response withheld".into())
        } else {
            result
        };
        let received_at = std::time::Instant::now();
        if audit_tool.is_some() {
            let outcome = if result
                .as_ref()
                .is_ok_and(|value| value.get("isError").and_then(Value::as_bool) != Some(true))
            {
                crate::history::AuditOutcome::Success
            } else {
                crate::history::AuditOutcome::Error
            };
            if self
                .audit_call(audit_tool.as_deref(), actor_subject, outcome)
                .await
                .is_err()
                && audit_required
            {
                // Native Frame::Reply errors are untrusted strings. Only the
                // server's transport state can supply an authoritative quota notice.
                let traffic_notice = self
                    .traffic_failure
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .map(|failure| failure.pending.clone());
                if let Some(notice) = traffic_notice {
                    return Err(format!(
                        "{notice}\nAudit completion could not be saved; inspect the operation state before retrying."
                    ));
                }
                return Err("tool may have executed; audit completion could not be saved; inspect its state before retrying".into());
            }
        }
        if let (Some(run), Ok(value)) = (audit_run.as_ref(), &result) {
            run.native_reply(
                self.client_id(),
                reply_agent.as_deref(),
                audit_tool.as_deref(),
                value,
                received_at,
            );
        }
        let timed_out = result.as_ref().is_err_and(|e| e.contains("timed out"));
        crate::observability::log(
            if result.is_err() { "warn" } else { "debug" },
            if timed_out { "timeout" } else { "proxy" },
            &[
                ("event", "native_call_complete".into()),
                ("client_id", self.client_id().into()),
                ("session_id", self.client_id().into()),
                ("duration_ms", started.elapsed().as_millis().to_string()),
                ("timeout_ms", duration.as_millis().to_string()),
                (
                    "outcome",
                    if timed_out {
                        "timeout"
                    } else if result.is_err() {
                        "error"
                    } else {
                        "ok"
                    }
                    .into(),
                ),
            ],
        );
        result
    }
}

pub struct Broker {
    pub public_url: String,
    pub bridges: Arc<crate::bridge::Bridges>,
    pub oauth: crate::oauth::OAuth,
    pub identities: IdentityProviders,
    pub portal: crate::portal::Portal,
    pub client_downloads: Option<crate::client_downloads::Publisher>,
    management: Option<crate::management::State>,
    traffic_utc_offset_minutes: std::sync::atomic::AtomicI32,
    pub(crate) user_policies: Arc<crate::user_policy::UserPolicies>,
    pub(crate) user_policy_updates: tokio::sync::Mutex<()>,
    sessions: RwLock<HashMap<String, HashMap<String, Arc<Session>>>>,
}

impl Broker {
    pub fn new(public_url: String) -> Self {
        Self::with_oauth_config(public_url, crate::oauth::OAuthConfig::default())
            .expect("valid default OAuth lifetimes")
    }

    pub fn with_oauth_config(
        public_url: String,
        config: crate::oauth::OAuthConfig,
    ) -> crate::Result<Self> {
        Self::with_identity_config(public_url, config, None)
    }

    pub fn with_identity_config(
        public_url: String,
        config: crate::oauth::OAuthConfig,
        github: Option<GitHubConfig>,
    ) -> crate::Result<Self> {
        Ok(Self {
            oauth: crate::oauth::OAuth::with_config(public_url.clone(), config)?,
            identities: IdentityProviders::new(&public_url, github)?,
            public_url,
            bridges: Arc::new(crate::bridge::Bridges::default()),
            portal: crate::portal::Portal::default(),
            client_downloads: None,
            management: None,
            user_policies: Arc::default(),
            traffic_utc_offset_minutes: std::sync::atomic::AtomicI32::new(
                crate::user_policy::DEFAULT_TRAFFIC_UTC_OFFSET_MINUTES,
            ),
            user_policy_updates: tokio::sync::Mutex::new(()),
            sessions: RwLock::new(HashMap::new()),
        })
    }

    pub async fn authenticate(&self, user: &str, access_token: &str) -> Option<Authorization> {
        if !valid_username(user) {
            return None;
        }
        let authorization = self.oauth.authenticate(user, access_token)?;
        self.authorized_account(&authorization)
            .then_some(authorization)
    }

    fn authorized_account(&self, authorization: &Authorization) -> bool {
        match authorization {
            Authorization::Identity(identity) => !self.user_policies.is_banned(&identity.subject),
            Authorization::Legacy { anchor, .. } => anchor.upgrade().is_some_and(|session| {
                session
                    .owner_subject
                    .as_deref()
                    .is_none_or(|id| !self.user_policies.is_banned(id))
            }),
        }
    }

    /// Restore durable account restrictions before accepting any connection.
    pub async fn configure_user_policies(&self, default_rate: u64) -> crate::Result<()> {
        self.user_policies.set_default(default_rate)?;
        let accounts = match self.portal.storage(|store| store.summaries()).await {
            Ok(accounts) => accounts,
            Err(crate::portal::Problem::Disabled) => return Ok(()),
            Err(_) => return Err("could not load account restrictions; refusing to start".into()),
        };
        for account in accounts {
            self.user_policies.apply_account(
                &account.subject,
                account.banned,
                account.rate_limit_bps,
                account.has_recharged,
            );
        }
        Ok(())
    }

    pub(crate) async fn record_recharge(
        &self,
        subject: &str,
    ) -> Result<(), crate::portal::Problem> {
        let subject = subject.to_owned();
        let policies = self.user_policies.clone();
        self.portal
            .storage(move |store| {
                let account = store.record_recharge(&subject)?;
                policies.apply_account(&subject, account.banned, account.rate_limit_bps, true);
                Ok(())
            })
            .await
    }

    pub async fn online_session(&self, user: &str) -> Option<Arc<Session>> {
        if user == IDENTITY_RESOURCE {
            return None;
        }
        self.sessions
            .read()
            .await
            .get(user)?
            .values()
            .find(|session| session.is_alive() && session.allowed().is_empty())
            .cloned()
    }

    pub async fn redeem_login(&self, user: &str, login_code: &str) -> Option<Arc<Session>> {
        let login_code = login_code.trim();
        if !valid_username(user) || user == IDENTITY_RESOURCE || login_code.len() != 64 {
            return None;
        }
        let sessions = self.sessions.read().await;
        for session in sessions.get(user)?.values() {
            if !session.is_alive() || !session.allowed().is_empty() {
                continue;
            }
            let mut pending = session.login.lock().unwrap();
            let Some(code) = pending.as_ref() else {
                continue;
            };
            if Instant::now() >= code.expires
                || !bool::from(code.value.as_bytes().ct_eq(login_code.as_bytes()))
            {
                continue;
            }
            pending.take();
            return Some(session.clone());
        }
        None
    }

    #[cfg(test)]
    pub(crate) async fn test_register(&self, user: &str, login_code: &str) -> Arc<Session> {
        let (sender, _rx) = mpsc::channel(1);
        let (reports, _reports_rx) = mpsc::channel(1);
        let id = generate_secret().unwrap();
        let session = Arc::new(Session {
            collaboration: crate::collab::Hub::default(),
            steering: crate::steering::Queue::default(),
            login: Mutex::new(Some(LoginCode {
                value: login_code.into(),
                expires: Instant::now() + Duration::from_secs(LOGIN_LIFETIME),
            })),
            allow: Mutex::new(Vec::new()),
            info: ClientInfo {
                id: id.clone(),
                name: "test".into(),
                title: None,
                mode: wire::Mode::Shell,
                cwd: "/test".into(),
                hostname: "localhost".into(),
            },
            sender,
            reports,
            slots: Semaphore::new(1),
            alive: AtomicBool::new(true),
            ssh_user: user.into(),
            owner_subject: self
                .management
                .as_ref()
                .and_then(|state| state.policy.owner_subject(user)),
            connected_at: crate::management::now(),
            audit: self.management.as_ref().map(|state| state.audit.clone()),
            disconnect: Notify::new(),
            closed: AtomicBool::new(false),
            closed_notify: Notify::new(),
            traffic_failure: Mutex::new(None),
        });
        self.sessions
            .write()
            .await
            .entry(user.into())
            .or_default()
            .insert(id, session.clone());
        session
    }

    /// Enumerate only sessions allowed by this identity and resource scope.
    pub async fn clients(
        &self,
        user: &str,
        authorization: &Authorization,
    ) -> Result<Vec<Arc<Session>>, String> {
        let sessions = self.sessions.read().await;
        if !self.authorized_account(authorization) {
            return Err("account is banned".into());
        }
        validate_authorization(&sessions, user, authorization)?;
        let mut result: Vec<_> = accessible_sessions(&sessions, user, authorization)
            .cloned()
            .collect();
        result.sort_unstable_by(|left, right| left.client_id().cmp(right.client_id()));
        Ok(result)
    }

    pub async fn client(
        &self,
        user: &str,
        authorization: &Authorization,
        id: &str,
    ) -> Result<Arc<Session>, String> {
        let sessions = self.sessions.read().await;
        if !self.authorized_account(authorization) {
            return Err("account is banned".into());
        }
        validate_authorization(&sessions, user, authorization)?;
        accessible_sessions(&sessions, user, authorization)
            .find(|session| session.client_id() == id)
            .cloned()
            .ok_or_else(|| "client is unavailable; call list to refresh connected clients".into())
    }

    pub async fn disconnect_all(&self) {
        self.bridges.shutdown().await;
        let active: Vec<_> = self
            .sessions
            .read()
            .await
            .values()
            .flat_map(|clients| clients.values())
            .cloned()
            .collect();
        for session in &active {
            session.alive.store(false, Ordering::Release);
            session.collaboration.close();
            session.disconnect.notify_one();
        }
        // Drain normal wire cleanup and its bounded audit writes before the
        // server aborts connection tasks. Never hold the registry lock here.
        let _ = timeout(Duration::from_secs(5), async {
            for session in &active {
                loop {
                    let notified = session.closed_notify.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    if session.closed.load(Ordering::Acquire) || session.sender.is_closed() {
                        break;
                    }
                    notified.await;
                }
            }
        })
        .await;
        self.sessions.write().await.clear();
    }

    pub async fn attach(self: Arc<Self>, stream: UnixStream) -> crate::Result<()> {
        let uid = stream.peer_cred()?.uid();
        let user = tokio::task::spawn_blocking(move || username(uid)).await??;
        self.attach_stream(stream, user, None).await
    }

    pub(crate) async fn attach_device<T>(
        self: Arc<Self>,
        stream: T,
        registration: crate::device::Registration,
        traffic_failure: TrafficFailureState,
    ) -> crate::Result<()>
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        self.attach_stream_with_traffic(
            stream,
            registration.username.clone(),
            Some(registration),
            traffic_failure,
        )
        .await
    }

    async fn attach_stream<T>(
        self: Arc<Self>,
        stream: T,
        user: String,
        registration: Option<crate::device::Registration>,
    ) -> crate::Result<()>
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        self.attach_stream_with_traffic(stream, user, registration, TrafficFailureState::default())
            .await
    }

    async fn attach_stream_with_traffic<T>(
        self: Arc<Self>,
        stream: T,
        user: String,
        registration: Option<crate::device::Registration>,
        traffic_failure: TrafficFailureState,
    ) -> crate::Result<()>
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (reader, mut writer) = tokio::io::split(stream);
        let mut reader = BufReader::new(reader);
        let (mode, metadata) = match timeout(IO_TIMEOUT, wire::read_frame(&mut reader)).await?? {
            Some(Frame::Hello {
                version,
                mode,
                metadata,
            }) if version == wire::VERSION => (mode, metadata),
            _ => {
                wire::write_frame(
                    &mut writer,
                    &Frame::Reject {
                        message: format!("unsupported ssmcp wire protocol; server requires version {}; update ssmcp and ssmcpd", wire::VERSION),
                    },
                )
                .await?;
                return Ok(());
            }
        };
        if let Err(message) = metadata.validate() {
            wire::write_frame(&mut writer, &Frame::Reject { message }).await?;
            return Ok(());
        }
        if user == IDENTITY_RESOURCE {
            wire::write_frame(
                &mut writer,
                &Frame::Reject {
                    message: "SSH username mcp is reserved for the shared identity endpoint".into(),
                },
            )
            .await?;
            return Ok(());
        }
        if let Some(registration) = &registration
            && !registration.is_live(&self).await
        {
            wire::write_frame(
                &mut writer,
                &Frame::Reject {
                    message: "ssmcp authorization expired or revoked".into(),
                },
            )
            .await?;
            return Ok(());
        }
        let owner = registration
            .as_ref()
            .map(|r| r.identity.subject.clone())
            .or_else(|| {
                self.management
                    .as_ref()
                    .and_then(|state| state.policy.owner_subject(&user))
            })
            .or_else(|| {
                user.strip_prefix("gh-")
                    .filter(|id| crate::management::valid_subject(id))
                    .map(str::to_owned)
            });
        if self.portal.has_storage() && owner.is_none() {
            wire::write_frame(
                &mut writer,
                &Frame::Reject {
                    message: crate::user_policy::TrafficError::AccountRequired.to_string(),
                },
            )
            .await?;
            return Ok(());
        }
        if owner
            .as_deref()
            .is_some_and(|id| self.user_policies.is_banned(id))
        {
            wire::write_frame(
                &mut writer,
                &Frame::Reject {
                    message: "account is banned".into(),
                },
            )
            .await?;
            return Ok(());
        }
        let restricted = registration.is_some() || !metadata.allow.is_empty();
        let allow = if let Some(registration) = &registration {
            let principal = format!(
                "{}@github",
                registration.identity.login.to_ascii_lowercase()
            );
            if metadata.allow.iter().any(|allowed| allowed != &principal) {
                wire::write_frame(
                    &mut writer,
                    &Frame::Reject {
                        message: "account devices cannot grant access to another identity".into(),
                    },
                )
                .await?;
                return Ok(());
            }
            vec![registration.identity.clone()]
        } else if restricted {
            match self.identities.resolve_allow(&metadata.allow).await {
                Ok(identities) if !identities.is_empty() => identities,
                result => {
                    let message = result
                        .err()
                        .unwrap_or_else(|| "allow list resolved to no identities".into());
                    wire::write_frame(&mut writer, &Frame::Reject { message }).await?;
                    return Ok(());
                }
            }
        } else {
            Vec::new()
        };
        let access_owner = registration
            .as_ref()
            .filter(|registration| registration.session_access)
            .map(|registration| registration.identity.clone());
        let login_code = if restricted {
            String::new()
        } else {
            generate_secret()?
        };
        let login_expires_in = if restricted { 0 } else { LOGIN_LIFETIME };
        let resource = if restricted { IDENTITY_RESOURCE } else { &user };
        let client_id = generate_secret()?;
        let (sender, mut requests) = mpsc::channel::<Call>(wire::MAX_INFLIGHT);
        let (reports, mut code_reports) = mpsc::channel::<wire::CodeCompletion>(wire::MAX_INFLIGHT);
        let session = Arc::new(Session {
            collaboration: crate::collab::Hub::default(),
            steering: crate::steering::Queue::default(),
            login: Mutex::new((!restricted).then(|| LoginCode {
                value: login_code.clone(),
                expires: Instant::now() + Duration::from_secs(LOGIN_LIFETIME),
            })),
            allow: Mutex::new(allow),
            info: ClientInfo {
                id: client_id.clone(),
                name: metadata.name,
                title: metadata.title,
                mode,
                cwd: metadata.cwd,
                hostname: metadata.hostname,
            },
            sender,
            reports,
            slots: Semaphore::new(wire::MAX_INFLIGHT),
            alive: AtomicBool::new(true),
            ssh_user: user.clone(),
            owner_subject: owner,
            connected_at: crate::management::now(),
            audit: self.management.as_ref().map(|state| state.audit.clone()),
            disconnect: Notify::new(),
            closed: AtomicBool::new(false),
            closed_notify: Notify::new(),
            traffic_failure: Mutex::new(None),
        });
        {
            let mut sessions = self.sessions.write().await;
            if sessions
                .get(&user)
                .is_some_and(|clients| clients.len() >= wire::MAX_CLIENTS_PER_USER)
            {
                drop(sessions);
                wire::write_frame(
                    &mut writer,
                    &Frame::Reject {
                        message: "this user has reached the 128-client connection limit".into(),
                    },
                )
                .await?;
                return Ok(());
            }
            // /mcp can span owners, so connection IDs must be unique across the broker.
            if sessions
                .values()
                .any(|clients| clients.contains_key(&client_id))
            {
                drop(sessions);
                wire::write_frame(
                    &mut writer,
                    &Frame::Reject {
                        message: "could not allocate a unique client ID; reconnect".into(),
                    },
                )
                .await?;
                return Ok(());
            }
            sessions
                .entry(user.clone())
                .or_default()
                .insert(client_id.clone(), session.clone());
        }
        crate::observability::log(
            "info",
            "session",
            &[
                ("event", "connected".into()),
                ("user", user.clone()),
                ("client_id", client_id.clone()),
                ("session_id", client_id.clone()),
                ("mode", mode.name().into()),
            ],
        );
        let owner_policy = session
            .owner_subject
            .as_deref()
            .map(|id| self.user_policies.get(id));
        let legacy_subject = registration
            .is_none()
            .then(|| session.owner_subject.as_deref())
            .flatten();
        // Keep reply senders alive until the trusted terminal cause is known.
        let mut pending: HashMap<u64, (oneshot::Sender<CallResult>, Instant, bool)> =
            HashMap::new();
        let mut result: crate::Result<()> = tokio::select! {
            biased;
            _ = session.disconnect.notified() => Ok(()),
            _ = async {
                match &owner_policy {
                    Some(policy) => policy.wait_banned().await,
                    None => std::future::pending::<()>().await,
                }
            } => Ok(()),
            _ = async {
                match &registration {
                    Some(registration) => registration.wait_revoked(&self).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                let _ = timeout(IO_TIMEOUT, wire::write_frame(&mut writer, &Frame::Reject {
                    message: "ssmcp authorization expired or revoked".into(),
                })).await;
                Ok(())
            },
            result = async {
            session.audit_record(session.audit_event(crate::history::AuditAction::SessionConnected, crate::history::AuditOutcome::Success)).await.map_err(|_| "connection audit could not be saved; connection was not accepted")?;
            wire::write_frame(&mut writer, &Frame::Ready { url: format!("{}/{resource}", self.public_url), login_code, login_expires_in, client_id:client_id.clone(), capabilities: access_owner.as_ref().map(|_| vec![wire::SESSION_ACCESS_CAPABILITY.into()]).unwrap_or_default() }).await?;
            let mut next_id = 0u64;
            let mut delivered_runs = std::collections::VecDeque::<String>::new();
            let mut expiry = tokio::time::interval(Duration::from_secs(1));
            loop {
                // Keep the same read future across requests/ticks: partial JSON frames
                // must not be discarded when another select branch becomes ready.
                let next_frame = wire::read_frame(&mut reader);
                tokio::pin!(next_frame);
                let received = loop {
                    tokio::select! {
                        frame = &mut next_frame => break frame?,
                        Some(call) = requests.recv() => {
                            if call.reply.is_closed() { continue; }
                            // Queue admission does not retain authority after a local revoke.
                            if call.authorization.as_ref().is_some_and(|authorization| !authorization.is_live() || !authorization.permits(&session) || !self.authorized_account(authorization)) {
                                let _ = call.reply.send(Err("session access was revoked; tool was not dispatched".into()));
                                continue;
                            }
                            if pending.len() >= wire::MAX_INFLIGHT {
                                let _ = call.reply.send(Err("client busy; retry later".into()));
                                continue;
                            }
                            next_id = next_id.checked_add(1).ok_or("request id exhausted")?;
                            let reload = crate::client::reload::is_reload(&call.operation);
                            pending.insert(next_id, (call.reply, Instant::now() + request_timeout(&call.operation), reload));
                            let run_id = call.code_context.as_ref().map(|context| context.run_id.clone());
                            let frame = Frame::Call { id:next_id, operation:call.operation, controller:call.controller, code_context:call.code_context };
                            if let Some(subject) = legacy_subject {
                                self.charge_relay(subject, true, serde_json::to_vec(&frame)?.len() + 1)
                                    .await.map_err(|error| TrafficFailure::rejected_call(error, next_id))?;
                            }
                            timeout(IO_TIMEOUT, wire::write_frame(&mut writer, &frame)).await??;
                            if let Some(run_id) = run_id && !delivered_runs.contains(&run_id) {
                                if delivered_runs.len() >= 256 { delivered_runs.pop_front(); }
                                delivered_runs.push_back(run_id);
                            }
                        }
                        Some(completion) = code_reports.recv() => {
                            // A cancelled, queued call may never have reached the
                            // client. Do not send a source-less run to that client.
                            if let Some(index) = delivered_runs.iter().position(|id| *id == completion.run_id) {
                                delivered_runs.remove(index);
                                let frame = Frame::CodeModeFinished { completion };
                                if let Some(subject) = legacy_subject {
                                    self.charge_relay(subject, true, serde_json::to_vec(&frame)?.len() + 1)
                                        .await.map_err(TrafficFailure::new)?;
                                }
                                timeout(IO_TIMEOUT, wire::write_frame(&mut writer, &frame)).await??;
                            }
                        }
                        _ = expiry.tick() => {
                            if pending.values().any(|(_, deadline, _)| Instant::now() >= *deadline) {
                                return Err("client response timeout".into());
                            }
                        }
                    }
                };
                if let (Some(subject), Some(frame)) = (legacy_subject, &received) {
                    self.charge_relay(subject, false, serde_json::to_vec(frame)?.len() + 1)
                        .await.map_err(|error| match frame {
                            Frame::Reply { id, .. } => TrafficFailure::rejected_reply(error, *id),
                            _ => TrafficFailure::new(error),
                        })?;
                }
                match received {
                    Some(Frame::Access { id, command }) => {
                        // Only this credential-bound connection can edit its own session.
                        // Capability negotiation grants no authority by itself.
                        let result = match (access_owner.as_ref(), registration.as_ref()) {
                            (Some(owner), Some(registration)) if registration.is_live(&self).await => {
                                session.access_command(&self.identities, owner, command).await
                            }
                            _ => Err("session access controls were not negotiated on an authenticated account connection".into()),
                        };
                        timeout(IO_TIMEOUT, wire::write_frame(&mut writer, &Frame::AccessReply { id, result })).await??;
                    }
                    Some(Frame::Steering { id, command }) => {
                        let result = session.steering_command(command);
                        let frame = Frame::SteeringReply { id, result };
                        if let Some(subject) = legacy_subject {
                            self.charge_relay(subject, true, serde_json::to_vec(&frame)?.len() + 1)
                                .await.map_err(TrafficFailure::new)?;
                        }
                        timeout(IO_TIMEOUT, wire::write_frame(&mut writer, &frame)).await??;
                    }
                    Some(Frame::Reply {id, result}) => {
                        if let Some((reply, _, reload)) = pending.remove(&id) {
                            if reload && result.as_ref().is_ok_and(crate::client::reload::accepted) {
                                timeout(IO_TIMEOUT, wire::write_frame(&mut writer, &Frame::ReloadCommit { id })).await??;
                            }
                            let _ = reply.send(result);
                        }
                    }
                    None => break,
                    _ => return Err("unexpected frame from client".into()),
                }
            }
            Ok(())
        } => result,
        };
        let failure = traffic_failure.get().or_else(|| {
            result.as_ref().err().and_then(|error| {
                error.downcast_ref::<TrafficFailure>().copied().or_else(|| {
                    error
                        .downcast_ref::<TrafficError>()
                        .copied()
                        .map(TrafficFailure::new)
                })
            })
        });
        if let Some(failure) = failure {
            // Publish the cause before closing admission: calls racing with this
            // teardown must not fall back to a generic disconnected/oneshot error.
            let undispatched = failure
                .error
                .notice(&self.public_url, TrafficOutcome::NotDispatched);
            *session
                .traffic_failure
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(SessionTrafficFailure {
                undispatched: undispatched.clone(),
                pending: failure
                    .error
                    .notice(&self.public_url, TrafficOutcome::Unknown),
            });
            session.alive.store(false, Ordering::Release);
            requests.close();
            for (id, (reply, _, _)) in pending.drain() {
                let _ = reply.send(Err(failure.call_notice(&self.public_url, id)));
            }
            while let Ok(call) = requests.try_recv() {
                let _ = call.reply.send(Err(undispatched.clone()));
            }
            result = Err(failure.into());
            let _ = timeout(
                IO_TIMEOUT,
                wire::write_frame(
                    &mut writer,
                    &Frame::Reject {
                        message: failure.notice(&self.public_url),
                    },
                ),
            )
            .await;
        }
        // Non-quota shutdown keeps the existing disconnected fallback.
        drop(pending);
        drop(requests);
        // Drop both wire halves before reporting the disconnect complete.
        drop(reader);
        drop(writer);
        session.alive.store(false, Ordering::Release);
        session.collaboration.close();
        let mut sessions = self.sessions.write().await;
        if let Some(clients) = sessions.get_mut(&user) {
            if clients
                .get(&client_id)
                .is_some_and(|current| Arc::ptr_eq(current, &session))
            {
                clients.remove(&client_id);
            }
            if clients.is_empty() {
                sessions.remove(&user);
            }
        }
        drop(sessions);
        crate::observability::log(
            "info",
            "session",
            &[
                ("event", "disconnected".into()),
                ("user", user.clone()),
                ("client_id", client_id.clone()),
                ("session_id", client_id.clone()),
            ],
        );
        let _ = session
            .audit_record(session.audit_event(
                crate::history::AuditAction::SessionDisconnected,
                if result.is_ok() {
                    crate::history::AuditOutcome::Success
                } else {
                    crate::history::AuditOutcome::Error
                },
            ))
            .await;
        session.closed.store(true, Ordering::Release);
        session.closed_notify.notify_waiters();
        result
    }
}

fn validate_authorization(
    sessions: &HashMap<String, HashMap<String, Arc<Session>>>,
    user: &str,
    authorization: &Authorization,
) -> Result<(), String> {
    if !valid_username(user) {
        return Err("invalid resource scope".into());
    }
    if let Authorization::Legacy {
        user: owner,
        anchor,
    } = authorization
    {
        if user == IDENTITY_RESOURCE || owner != user {
            return Err("authorization does not permit this resource".into());
        }
        let anchor = anchor.upgrade().ok_or("authorization connection ended")?;
        let clients = sessions.get(user).ok_or("authorization connection ended")?;
        if !anchor.is_alive()
            || !anchor.allowed().is_empty()
            || !clients
                .get(anchor.client_id())
                .is_some_and(|current| Arc::ptr_eq(current, &anchor))
        {
            return Err("authorization connection ended or belongs to another user".into());
        }
    }
    Ok(())
}

fn accessible_sessions<'a>(
    sessions: &'a HashMap<String, HashMap<String, Arc<Session>>>,
    user: &'a str,
    authorization: &'a Authorization,
) -> impl Iterator<Item = &'a Arc<Session>> {
    sessions
        .iter()
        .filter(move |(owner, _)| user == IDENTITY_RESOURCE || owner.as_str() == user)
        .flat_map(|(_, clients)| clients.values())
        .filter(move |session| session.is_alive() && authorization.permits(session))
}

pub fn valid_username(user: &str) -> bool {
    !user.is_empty()
        && user.len() <= 64
        && !matches!(user, "healthz" | "." | "..")
        && user.as_bytes()[0] != b'-'
        && user
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
}

fn username(uid: u32) -> io::Result<String> {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // SAFETY: entry and buffer are writable and remain live until the name is copied.
        let code = unsafe {
            libc::getpwuid_r(
                uid,
                entry.as_mut_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut result,
            )
        };
        if code == libc::ERANGE && buf.len() < 1024 * 1024 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if code != 0 {
            return Err(io::Error::from_raw_os_error(code));
        }
        if result.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "peer uid has no Linux account",
            ));
        }
        // SAFETY: getpwuid_r succeeded and the returned entry points into buf.
        let name = unsafe { CStr::from_ptr((*result).pw_name) }
            .to_str()
            .map_err(|_| io::Error::other("non-UTF8 account name"))?
            .to_owned();
        return if valid_username(&name) {
            Ok(name)
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "account name is not a supported URL segment",
            ))
        };
    }
}

fn generate_secret() -> io::Result<String> {
    use std::fmt::Write;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let mut token = String::with_capacity(64);
    for byte in bytes {
        write!(&mut token, "{byte:02x}").expect("String writing cannot fail");
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn registration_rejects_every_noncurrent_wire_version() {
        for version in [0, 4, 5, 6, wire::VERSION + 1] {
            let broker = Arc::new(Broker::new("https://example.test".into()));
            let (server, client) = UnixStream::pair().unwrap();
            let server_task = tokio::spawn(broker.attach(server));
            let (reader, mut writer) = client.into_split();
            let mut reader = BufReader::new(reader);
            wire::write_frame(
                &mut writer,
                &Frame::Hello {
                    version,
                    mode: wire::Mode::Shell,
                    metadata: metadata("unsupported protocol", "/project"),
                },
            )
            .await
            .unwrap();
            let Some(Frame::Reject { message }) =
                timeout(Duration::from_secs(2), wire::read_frame(&mut reader))
                    .await
                    .unwrap()
                    .unwrap()
            else {
                panic!("noncurrent version {version} was not rejected");
            };
            assert!(message.contains(&format!("server requires version {}", wire::VERSION)));
            timeout(Duration::from_secs(2), server_task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
    }

    #[tokio::test]
    async fn current_protocol_carries_controller_and_code_audit() {
        let broker = Arc::new(Broker::new("https://example.test".into()));
        let (server, client) = UnixStream::pair().unwrap();
        let server_task = tokio::spawn(broker.clone().attach(server));
        let (reader, mut writer) = client.into_split();
        let mut reader = BufReader::new(reader);
        wire::write_frame(
            &mut writer,
            &Frame::Hello {
                version: wire::VERSION,
                mode: wire::Mode::Shell,
                metadata: metadata("controller test", "/project"),
            },
        )
        .await
        .unwrap();
        let Some(Frame::Ready {
            url, login_code, ..
        }) = timeout(Duration::from_secs(2), wire::read_frame(&mut reader))
            .await
            .unwrap()
            .unwrap()
        else {
            panic!("registration failed");
        };
        let user = url.rsplit('/').next().unwrap();
        let subject = broker.redeem_login(user, &login_code).await.unwrap();
        assert!(subject.supports_controller());
        let reports = subject.clone();
        let code =
            "// preserve source spacing\n  return await exec.run('one', {command:['true']});\n";
        let run = crate::code_mode::AuditRun::new(code, None).unwrap();
        let request = tokio::spawn(crate::code_mode::AUDIT_RUN.scope(run.clone(), async move {
            subject
                .call_with_controller(
                    Operation::Status {},
                    Some(wire::Controller {
                        name: Some("ChatGPT".into()),
                        model: Some("test-model".into()),
                        source: wire::ControllerSource::CallerReported,
                    }),
                )
                .await
        }));
        let Some(Frame::Call {
            id,
            controller,
            code_context,
            ..
        }) = timeout(Duration::from_secs(2), wire::read_frame(&mut reader))
            .await
            .unwrap()
            .unwrap()
        else {
            panic!("missing call");
        };
        assert_eq!(controller.unwrap().model.as_deref(), Some("test-model"));
        assert_eq!(code_context.unwrap().code, code);
        assert_eq!(run.targets(), vec![reports.client_id().to_owned()]);
        wire::write_frame(
            &mut writer,
            &Frame::Reply {
                id,
                result: Ok(serde_json::json!({"ok":true})),
            },
        )
        .await
        .unwrap();
        let result = request.await.unwrap();
        assert!(result.as_ref().unwrap()["ok"] == true);
        reports.report_code_mode(run.completion(&result)).unwrap();
        let Some(Frame::CodeModeFinished { completion }) =
            timeout(Duration::from_secs(2), wire::read_frame(&mut reader))
                .await
                .unwrap()
                .unwrap()
        else {
            panic!("missing run completion");
        };
        assert_eq!(completion.run_id, run.context.run_id);
        assert!(completion.success);
        drop(reader);
        drop(writer);
        timeout(Duration::from_secs(2), server_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn code_source_is_scoped_to_queued_target_and_never_changes_native_arguments() {
        let (sender, mut receiver) = mpsc::channel(4);
        let mut target = session("target", "secret");
        Arc::get_mut(&mut target).unwrap().sender = sender;
        let (untouched_sender, mut untouched_receiver) = mpsc::channel(4);
        let mut untouched = session("untouched", "secret");
        Arc::get_mut(&mut untouched).unwrap().sender = untouched_sender;
        let source = "return await call('target', 'tool.run', {value: 3});\n";
        let run = crate::code_mode::AuditRun::new(
            source,
            Some(wire::Controller {
                name: Some("caller".into()),
                model: None,
                source: wire::ControllerSource::CallerReported,
            }),
        )
        .unwrap();
        let called = target.clone();
        let request = tokio::spawn(crate::code_mode::AUDIT_RUN.scope(run.clone(), async move {
            called
                .call(Operation::CallTool {
                    name: "tool.run".into(),
                    arguments: serde_json::json!({"value":3}),
                })
                .await
        }));
        let call = receiver.recv().await.unwrap();
        assert_eq!(call.code_context.unwrap().code, source);
        assert_eq!(call.controller.unwrap().name.as_deref(), Some("caller"));
        let Operation::CallTool { arguments, .. } = call.operation else {
            panic!("expected a tool call");
        };
        assert_eq!(arguments, serde_json::json!({"value":3}));
        call.reply.send(Ok(Value::Null)).unwrap();
        request.await.unwrap().unwrap();
        assert_eq!(run.targets(), ["target"]);
        assert!(matches!(
            untouched_receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        untouched.alive.store(false, Ordering::Release);
        assert!(
            crate::code_mode::AUDIT_RUN
                .scope(run.clone(), untouched.call(Operation::Status {}))
                .await
                .is_err()
        );
        assert_eq!(run.targets(), ["target"]);
        let request = tokio::spawn(async move { target.call(Operation::Status {}).await });
        let call = receiver.recv().await.unwrap();
        assert!(call.code_context.is_none());
        call.reply.send(Ok(Value::Null)).unwrap();
        request.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn dialogue_receipts_use_native_ack_and_binding_at_dispatch() {
        for tool in ["ssmcp.ui.message", "ssmcp.ui.present"] {
            for bind_after_dispatch in [false, true] {
                let (sender, mut receiver) = mpsc::channel(1);
                let mut target = session("dialogue", "secret");
                Arc::get_mut(&mut target).unwrap().sender = sender;
                let run = crate::code_mode::AuditRun::new("return 1;", None).unwrap();
                if !bind_after_dispatch {
                    run.bind("dialogue", "owner", "agent_a", "private_token")
                        .unwrap();
                }
                let request =
                    tokio::spawn(crate::code_mode::AUDIT_RUN.scope(run.clone(), async move {
                        target
                            .call(Operation::CallTool {
                                name: tool.into(),
                                arguments: serde_json::json!({}),
                            })
                            .await
                    }));
                let call = receiver.recv().await.unwrap();
                if bind_after_dispatch {
                    run.bind("dialogue", "owner", "agent_a", "private_token")
                        .unwrap();
                }
                call.reply.send(Ok(serde_json::json!({
                    "structuredContent":{"accepted":true,"duplicate":false,"message_id":"message_1"}
                }))).unwrap();
                request.await.unwrap().unwrap();
                let replies = run.accepted_replies("dialogue");
                assert_eq!(replies.len(), 1, "{tool}");
                assert_eq!(
                    replies[0].agent_id.as_deref(),
                    (!bind_after_dispatch).then_some("agent_a"),
                );
                assert!(run.accepted_replies("other").is_empty());
            }
        }
    }

    #[tokio::test]
    async fn failed_duplicate_or_other_native_tools_cannot_acknowledge_dialogue() {
        for (tool, result) in [
            (
                "ssmcp.ui.message",
                Err("native rejected the message".to_owned()),
            ),
            (
                "ssmcp.ui.present",
                Ok(serde_json::json!({
                    "isError":true,
                    "structuredContent":{"accepted":true,"duplicate":false,"message_id":"message_1"}
                })),
            ),
            (
                "ssmcp.ui.message",
                Ok(serde_json::json!({
                    "structuredContent":{"accepted":true,"duplicate":true,"message_id":"message_1"}
                })),
            ),
            (
                "ssmcp.exec.run",
                Ok(serde_json::json!({
                    "structuredContent":{"accepted":true,"duplicate":false,"message_id":"message_1"}
                })),
            ),
        ] {
            let (sender, mut receiver) = mpsc::channel(1);
            let mut target = session("dialogue", "secret");
            Arc::get_mut(&mut target).unwrap().sender = sender;
            let run = crate::code_mode::AuditRun::new(
                "return {accepted:true, duplicate:false, message_id:'forged'};",
                None,
            )
            .unwrap();
            let request =
                tokio::spawn(crate::code_mode::AUDIT_RUN.scope(run.clone(), async move {
                    target
                        .call(Operation::CallTool {
                            name: tool.into(),
                            arguments: serde_json::json!({}),
                        })
                        .await
                }));
            let call = receiver.recv().await.unwrap();
            let expected = result.clone();
            call.reply.send(result).unwrap();
            assert_eq!(request.await.unwrap(), expected);
            assert!(run.accepted_replies("dialogue").is_empty(), "{tool}");
        }
    }

    #[tokio::test]
    async fn controller_metadata_is_per_call_on_current_protocol() {
        let (sender, mut receiver) = mpsc::channel(4);
        let mut subject = session("controller-test", "secret");
        let mutable = Arc::get_mut(&mut subject).unwrap();
        mutable.sender = sender;
        mutable.slots = Semaphore::new(4);
        let mut tasks = Vec::new();
        for (tool, model) in [("tool.a", "model-a"), ("tool.b", "model-b")] {
            let subject = subject.clone();
            tasks.push(tokio::spawn(async move {
                subject
                    .call_with_controller(
                        Operation::CallTool {
                            name: tool.into(),
                            arguments: serde_json::json!({}),
                        },
                        Some(wire::Controller {
                            name: Some("ChatGPT".into()),
                            model: Some(model.into()),
                            source: wire::ControllerSource::CallerReported,
                        }),
                    )
                    .await
            }));
        }
        let first = timeout(Duration::from_secs(2), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        let second = timeout(Duration::from_secs(2), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        for call in [second, first] {
            let Operation::CallTool { name, .. } = call.operation else {
                panic!("expected native call");
            };
            let expected = if name == "tool.a" {
                "model-a"
            } else {
                "model-b"
            };
            assert_eq!(call.controller.unwrap().model.as_deref(), Some(expected));
            call.reply
                .send(Ok(serde_json::json!({"tool":name})))
                .unwrap();
        }
        for task in tasks {
            assert!(
                timeout(Duration::from_secs(2), task)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_ok()
            );
        }
    }

    fn metadata(name: &str, cwd: &str) -> wire::ClientMetadata {
        wire::ClientMetadata {
            name: name.into(),
            title: None,
            cwd: cwd.into(),
            hostname: "localhost".into(),
            allow: Vec::new(),
        }
    }

    fn session(id: &str, secret: &str) -> Arc<Session> {
        allowed_session(id, secret, Vec::new())
    }

    fn allowed_session(id: &str, secret: &str, allow: Vec<Identity>) -> Arc<Session> {
        let (sender, _rx) = mpsc::channel(1);
        let (reports, _reports_rx) = mpsc::channel(1);
        Arc::new(Session {
            collaboration: crate::collab::Hub::default(),
            steering: crate::steering::Queue::default(),
            login: Mutex::new(allow.is_empty().then(|| LoginCode {
                value: secret.into(),
                expires: Instant::now() + Duration::from_secs(LOGIN_LIFETIME),
            })),
            allow: Mutex::new(allow),
            info: ClientInfo {
                id: id.into(),
                name: "same display name".into(),
                title: None,
                mode: wire::Mode::Shell,
                cwd: "/project".into(),
                hostname: "localhost".into(),
            },
            sender,
            reports,
            slots: Semaphore::new(1),
            alive: AtomicBool::new(true),
            ssh_user: "test".into(),
            owner_subject: None,
            connected_at: crate::management::now(),
            audit: None,
            disconnect: Notify::new(),
            closed: AtomicBool::new(false),
            closed_notify: Notify::new(),
            traffic_failure: Mutex::new(None),
        })
    }

    fn legacy(user: &str, anchor: &Arc<Session>) -> Authorization {
        Authorization::Legacy {
            user: user.into(),
            anchor: Arc::downgrade(anchor),
        }
    }

    fn github(subject: &str, login: &str) -> Identity {
        Identity {
            provider: "github".into(),
            subject: subject.into(),
            login: login.into(),
        }
    }
    #[test]
    fn usernames_are_single_unambiguous_path_segments() {
        for good in ["alice", "_svc", "user.name", "user-1"] {
            assert!(valid_username(good));
        }
        for bad in ["", "a/b", "../x", "%61lice", "-option", "healthz", "a\nb"] {
            assert!(!valid_username(bad));
        }
    }
    #[test]
    fn secrets_are_random_256_bit_hex() {
        let a = generate_secret().unwrap();
        let b = generate_secret().unwrap();
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
    #[tokio::test]
    async fn login_codes_are_one_use_scoped_expiring_and_not_access_tokens() {
        let broker = Broker::new("https://example.test".into());
        let code = "a".repeat(64);
        let session = broker.test_register("alice", &code).await;
        assert!(broker.authenticate("alice", &code).await.is_none());
        assert!(broker.redeem_login("bob", &code).await.is_none());
        assert!(
            broker
                .redeem_login("alice", &"b".repeat(64))
                .await
                .is_none()
        );
        assert!(broker.redeem_login("alice", &code).await.is_some());
        assert!(broker.redeem_login("alice", &code).await.is_none());
        broker.disconnect_all().await;
        assert!(!session.is_alive());
        let session = broker.test_register("alice", &code).await;
        session.login.lock().unwrap().as_mut().unwrap().expires =
            Instant::now() - Duration::from_secs(1);
        assert!(broker.redeem_login("alice", &code).await.is_none());
    }

    #[tokio::test]
    async fn fragmented_replies_survive_concurrent_calls_and_disconnect_revokes_secret() {
        use tokio::io::AsyncWriteExt;
        let broker = Arc::new(Broker::new("https://example.test".into()));
        let (server, client) = UnixStream::pair().unwrap();
        let task = tokio::spawn(broker.clone().attach(server));
        let (reader, mut writer) = client.into_split();
        let mut reader = BufReader::new(reader);
        wire::write_frame(
            &mut writer,
            &Frame::Hello {
                version: wire::VERSION,
                mode: wire::Mode::Shell,
                metadata: metadata("same display name", "/project"),
            },
        )
        .await
        .unwrap();
        let Some(Frame::Ready {
            url, login_code, ..
        }) = wire::read_frame(&mut reader).await.unwrap()
        else {
            panic!("missing registration");
        };
        let user = url.rsplit('/').next().unwrap();
        let session = broker.redeem_login(user, &login_code).await.unwrap();
        let first_session = session.clone();
        let first = tokio::spawn(async move { first_session.call(Operation::Status {}).await });
        let Some(Frame::Call { id: first_id, .. }) = wire::read_frame(&mut reader).await.unwrap()
        else {
            panic!("missing call");
        };
        let mut encoded = serde_json::to_vec(&Frame::Reply {
            id: first_id,
            result: Ok(serde_json::json!({"value":1})),
        })
        .unwrap();
        encoded.push(b'\n');
        let split = encoded.len() / 2;
        writer.write_all(&encoded[..split]).await.unwrap();
        let second = tokio::spawn(async move { session.call(Operation::Status {}).await });
        let Some(Frame::Call { id: second_id, .. }) = wire::read_frame(&mut reader).await.unwrap()
        else {
            panic!("missing second call");
        };
        writer.write_all(&encoded[split..]).await.unwrap();
        wire::write_frame(
            &mut writer,
            &Frame::Reply {
                id: second_id,
                result: Ok(serde_json::json!({"value":2})),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            timeout(Duration::from_secs(2), first)
                .await
                .unwrap()
                .unwrap()
                .unwrap()["value"],
            1
        );
        assert_eq!(
            timeout(Duration::from_secs(2), second)
                .await
                .unwrap()
                .unwrap()
                .unwrap()["value"],
            2
        );
        drop(writer);
        drop(reader);
        timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(broker.redeem_login(user, &login_code).await.is_none());
    }

    #[tokio::test]
    async fn same_owner_clients_have_unique_ids_and_disconnect_independently() {
        let broker = Arc::new(Broker::new("https://example.test".into()));
        let mut attachments = Vec::new();
        let mut credentials = Vec::new();
        for (mode, cwd) in [
            (wire::Mode::Shell, "/first"),
            (wire::Mode::Codex, "/second"),
        ] {
            let (server, client) = UnixStream::pair().unwrap();
            let task = tokio::spawn(broker.clone().attach(server));
            let mut stream = BufReader::new(client);
            wire::write_frame(
                stream.get_mut(),
                &Frame::Hello {
                    version: wire::VERSION,
                    mode,
                    metadata: metadata("same display name", cwd),
                },
            )
            .await
            .unwrap();
            let Some(Frame::Ready {
                url,
                login_code,
                client_id,
                ..
            }) = wire::read_frame(&mut stream).await.unwrap()
            else {
                panic!("missing registration");
            };
            credentials.push((url, login_code, client_id));
            attachments.push((stream, task));
        }
        let user = credentials[0].0.rsplit('/').next().unwrap();
        assert_eq!(credentials[0].0, credentials[1].0);
        assert_ne!(credentials[0].2, credentials[1].2);
        let first = broker.redeem_login(user, &credentials[0].1).await.unwrap();
        let second = broker.redeem_login(user, &credentials[1].1).await.unwrap();
        let first_authorization = legacy(user, &first);
        let second_authorization = legacy(user, &second);
        let clients = broker.clients(user, &first_authorization).await.unwrap();
        assert_eq!(clients.len(), 2);
        assert_eq!(
            broker
                .client(user, &first_authorization, &credentials[1].2)
                .await
                .unwrap()
                .info()
                .cwd,
            "/second"
        );
        assert_eq!(
            broker
                .client(user, &second_authorization, &credentials[0].2)
                .await
                .unwrap()
                .info()
                .mode,
            wire::Mode::Shell
        );
        assert!(
            broker
                .client(user, &first_authorization, "same display name")
                .await
                .is_err()
        );

        let (stream, task) = attachments.remove(0);
        drop(stream);
        timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(broker.authenticate(user, &credentials[0].1).await.is_none());
        assert!(broker.clients(user, &first_authorization).await.is_err());
        assert!(
            broker
                .client(user, &first_authorization, second.client_id())
                .await
                .is_err()
        );
        assert!(
            broker
                .client(user, &second_authorization, first.client_id())
                .await
                .is_err()
        );
        assert_eq!(
            broker
                .clients(user, &second_authorization)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(second.is_alive());

        let (stream, task) = attachments.remove(0);
        drop(stream);
        timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!broker.sessions.read().await.contains_key(user));
    }

    #[tokio::test]
    async fn discovery_and_target_selection_require_an_owner_scoped_live_anchor() {
        let broker = Broker::new("https://example.test".into());
        let alice = session("same-id", &"a".repeat(64));
        let bob = session("same-id", &"b".repeat(64));
        {
            let mut sessions = broker.sessions.write().await;
            sessions.insert(
                "alice".into(),
                HashMap::from([("same-id".into(), alice.clone())]),
            );
            sessions.insert(
                "bob".into(),
                HashMap::from([("same-id".into(), bob.clone())]),
            );
        }
        let alice_authorization = legacy("alice", &alice);
        let bob_authorization = legacy("bob", &bob);
        assert!(broker.clients("bob", &alice_authorization).await.is_err());
        assert!(
            broker
                .client("bob", &alice_authorization, bob.client_id())
                .await
                .is_err()
        );
        assert!(broker.clients("alice", &bob_authorization).await.is_err());
        assert!(Arc::ptr_eq(
            &broker
                .client("alice", &alice_authorization, "same-id")
                .await
                .unwrap(),
            &alice
        ));
        // An old grant must not regain authority if an ID is ever reused.
        let replacement = session("same-id", &"c".repeat(64));
        broker
            .sessions
            .write()
            .await
            .get_mut("alice")
            .unwrap()
            .insert("same-id".into(), replacement.clone());
        let replacement_authorization = legacy("alice", &replacement);
        assert!(broker.clients("alice", &alice_authorization).await.is_err());
        assert!(
            broker
                .client("alice", &alice_authorization, "same-id")
                .await
                .is_err()
        );
        assert!(
            broker
                .clients("alice", &replacement_authorization)
                .await
                .is_ok()
        );
        broker.disconnect_all().await;
        assert!(!bob.is_alive());
        assert!(!replacement.is_alive());
        assert!(
            broker
                .clients("alice", &replacement_authorization)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn stable_identity_acl_filters_discovery_and_direct_ids_across_owners() {
        let broker = Broker::new("https://example.test".into());
        let unrestricted = session("legacy", &"a".repeat(64));
        let alice_allowed =
            allowed_session("alice-allowed", "unused", vec![github("42", "old-name")]);
        let alice_denied = allowed_session("alice-denied", "unused", vec![github("7", "other")]);
        let bob_allowed = allowed_session("bob-allowed", "unused", vec![github("42", "old-name")]);
        {
            let mut sessions = broker.sessions.write().await;
            sessions.insert(
                "alice".into(),
                HashMap::from([
                    (unrestricted.client_id().into(), unrestricted.clone()),
                    (alice_allowed.client_id().into(), alice_allowed.clone()),
                    (alice_denied.client_id().into(), alice_denied.clone()),
                ]),
            );
            sessions.insert(
                "bob".into(),
                HashMap::from([(bob_allowed.client_id().into(), bob_allowed.clone())]),
            );
        }
        // A username change preserves the numeric provider identity. Reusing the
        // display name with a different subject must never inherit access.
        let identity = Authorization::Identity(github("42", "new-name"));
        let impostor = Authorization::Identity(github("99", "old-name"));
        let other_provider = Authorization::Identity(Identity {
            provider: "other-provider".into(),
            subject: "42".into(),
            login: "new-name".into(),
        });
        let alice_visible = broker.clients("alice", &identity).await.unwrap();
        assert_eq!(alice_visible.len(), 1);
        assert_eq!(alice_visible[0].client_id(), "alice-allowed");
        let all_visible = broker.clients(IDENTITY_RESOURCE, &identity).await.unwrap();
        assert_eq!(
            all_visible
                .iter()
                .map(|s| s.client_id())
                .collect::<Vec<_>>(),
            ["alice-allowed", "bob-allowed"]
        );
        assert!(
            broker
                .clients(IDENTITY_RESOURCE, &impostor)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            broker
                .clients(IDENTITY_RESOURCE, &other_provider)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            broker
                .clients("missing-owner", &identity)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            broker
                .client("alice", &identity, "bob-allowed")
                .await
                .is_err()
        );
        assert!(Arc::ptr_eq(
            &broker
                .client(IDENTITY_RESOURCE, &identity, "bob-allowed")
                .await
                .unwrap(),
            &bob_allowed,
        ));
        let hidden = broker
            .client(IDENTITY_RESOURCE, &identity, "alice-denied")
            .await
            .err();
        let missing = broker
            .client(IDENTITY_RESOURCE, &identity, "unknown")
            .await
            .err();
        assert_eq!(hidden, missing);
        assert!(
            broker
                .client(IDENTITY_RESOURCE, &identity, "legacy")
                .await
                .is_err()
        );

        let legacy = legacy("alice", &unrestricted);
        let legacy_visible = broker.clients("alice", &legacy).await.unwrap();
        assert_eq!(legacy_visible.len(), 1);
        assert_eq!(legacy_visible[0].client_id(), "legacy");
        assert!(
            broker
                .client("alice", &legacy, "alice-allowed")
                .await
                .is_err()
        );
        assert!(broker.clients(IDENTITY_RESOURCE, &legacy).await.is_err());
        assert!(broker.client("bob", &legacy, "bob-allowed").await.is_err());
        assert!(
            broker
                .clients("alice", &self::legacy("alice", &alice_allowed))
                .await
                .is_err()
        );
        assert!(alice_allowed.login.lock().unwrap().is_none());
        // Even an unexpected leftover login code cannot bypass a restricted ACL.
        *alice_allowed.login.lock().unwrap() = Some(LoginCode {
            value: "b".repeat(64),
            expires: Instant::now() + Duration::from_secs(LOGIN_LIFETIME),
        });
        assert!(
            broker
                .redeem_login("alice", &"b".repeat(64))
                .await
                .is_none()
        );
        assert!(Arc::ptr_eq(
            &broker.online_session("alice").await.unwrap(),
            &unrestricted
        ));
        assert!(broker.online_session("bob").await.is_none());
        assert!(broker.online_session(IDENTITY_RESOURCE).await.is_none());
    }

    #[tokio::test]
    async fn identity_authority_survives_disconnect_without_granting_future_unlisted_sessions() {
        let broker = Broker::new("https://example.test".into());
        let original = allowed_session("original", "unused", vec![github("42", "owner")]);
        broker.sessions.write().await.insert(
            "alice".into(),
            HashMap::from([(original.client_id().into(), original.clone())]),
        );
        let identity = Authorization::Identity(github("42", "owner"));
        assert_eq!(
            broker
                .clients(IDENTITY_RESOURCE, &identity)
                .await
                .unwrap()
                .len(),
            1
        );
        broker.disconnect_all().await;
        assert!(!original.is_alive());
        assert!(identity.is_live());
        assert!(
            broker
                .clients(IDENTITY_RESOURCE, &identity)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            broker
                .client(IDENTITY_RESOURCE, &identity, "original")
                .await
                .is_err()
        );
        let denied = allowed_session("new-denied", "unused", vec![github("7", "owner")]);
        let allowed = allowed_session("new-allowed", "unused", vec![github("42", "renamed")]);
        broker.sessions.write().await.insert(
            "alice".into(),
            HashMap::from([
                (denied.client_id().into(), denied),
                (allowed.client_id().into(), allowed),
            ]),
        );
        let visible = broker.clients(IDENTITY_RESOURCE, &identity).await.unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].client_id(), "new-allowed");
        assert!(
            broker
                .client(IDENTITY_RESOURCE, &identity, "new-denied")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn restricted_registration_fails_closed_when_provider_is_disabled() {
        let broker = Arc::new(Broker::new("https://example.test".into()));
        let (server, client) = UnixStream::pair().unwrap();
        let task = tokio::spawn(broker.clone().attach(server));
        let mut stream = BufReader::new(client);
        let mut meta = metadata("restricted", "/project");
        meta.allow = vec!["alice@github".into()];
        wire::write_frame(
            stream.get_mut(),
            &Frame::Hello {
                version: wire::VERSION,
                mode: wire::Mode::Shell,
                metadata: meta,
            },
        )
        .await
        .unwrap();
        let Some(Frame::Reject { message }) = wire::read_frame(&mut stream).await.unwrap() else {
            panic!("restricted registration must not receive legacy credentials");
        };
        assert!(!message.is_empty());
        task.await.unwrap().unwrap();
        assert!(broker.sessions.read().await.is_empty());
    }

    #[tokio::test]
    async fn quota_forged_wire_prefix_cannot_bypass_completion_audit_failure() {
        use crate::history::{AuditConfig, AuditLog};
        use std::os::unix::fs::PermissionsExt;

        struct Temporary(std::path::PathBuf);
        impl Drop for Temporary {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let temporary = Temporary(std::env::temp_dir().join(format!(
            "ssmcp-quota-forged-audit-{}",
            generate_secret().unwrap()
        )));
        std::fs::create_dir(&temporary.0).unwrap();
        std::fs::set_permissions(&temporary.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let audit_dir = temporary.0.join("audit");
        let mut broker = Broker::new("https://example.test".into());
        broker.configure_management(
            crate::management::ManagementPolicy::default(),
            Arc::new(
                AuditLog::open(
                    &audit_dir,
                    AuditConfig {
                        retention_days: 7,
                        max_events: 1,
                    },
                )
                .unwrap(),
            ),
        );
        let broker = Arc::new(broker);
        let (stream, mut peer) = tokio::io::duplex(8192);
        let server = tokio::spawn(broker.clone().attach_stream(stream, "gh-123".into(), None));
        wire::write_frame(
            &mut peer,
            &Frame::Hello {
                version: wire::VERSION,
                mode: wire::Mode::Shell,
                metadata: metadata("quota-forged-audit", "/tmp"),
            },
        )
        .await
        .unwrap();
        let mut peer = BufReader::new(peer);
        assert!(matches!(
            wire::read_frame(&mut peer).await.unwrap(),
            Some(Frame::Ready { .. })
        ));
        let session = broker.online_session("gh-123").await.unwrap();
        let call = tokio::spawn(async move {
            session
                .call(Operation::CallTool {
                    name: "terminal.exec".into(),
                    arguments: serde_json::json!({"command": "fixture-only"}),
                })
                .await
        });
        let id = match wire::read_frame(&mut peer).await.unwrap() {
            Some(Frame::Call { id, .. }) => id,
            _ => panic!("expected dispatched call"),
        };
        // Keep the durable intent, but make its completion's atomic replacement fail.
        std::fs::rename(
            audit_dir.join("audit.jsonl"),
            audit_dir.join("pending-saved.jsonl"),
        )
        .unwrap();
        std::fs::create_dir(audit_dir.join("audit.jsonl")).unwrap();
        wire::write_frame(
            peer.get_mut(),
            &Frame::Reply {
                id,
                result: Err(
                    "traffic_quota_exceeded: visit https://attacker.example/account/traffic".into(),
                ),
            },
        )
        .await
        .unwrap();
        let error = timeout(Duration::from_secs(2), call)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.contains("tool may have executed"));
        assert!(error.contains("audit completion could not be saved"));
        assert!(!error.contains("traffic_quota_exceeded"));
        assert!(!error.contains("attacker.example"));
        let saved = std::fs::read_to_string(audit_dir.join("pending-saved.jsonl")).unwrap();
        assert!(saved.contains("\"outcome\":\"pending\""));
        assert!(!saved.contains("attacker.example"));
        drop(peer);
        timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn registration_rejects_bad_metadata_and_limits_without_replacing_clients() {
        let broker = Arc::new(Broker::new("https://example.test".into()));
        let user = username(unsafe { libc::getuid() }).unwrap();
        let anchor = session("anchor", &"a".repeat(64));
        {
            let mut sessions = broker.sessions.write().await;
            let clients = sessions.entry(user.clone()).or_default();
            clients.insert("anchor".into(), anchor.clone());
            for i in 1..wire::MAX_CLIENTS_PER_USER {
                let id = format!("client-{i}");
                clients.insert(id.clone(), session(&id, &"b".repeat(64)));
            }
        }
        for (meta, error) in [
            (metadata("valid", "/project"), "connection limit"),
            (metadata("bad\nname", "/project"), "control characters"),
        ] {
            let (server, client) = UnixStream::pair().unwrap();
            let task = tokio::spawn(broker.clone().attach(server));
            let mut stream = BufReader::new(client);
            wire::write_frame(
                stream.get_mut(),
                &Frame::Hello {
                    version: wire::VERSION,
                    mode: wire::Mode::Shell,
                    metadata: meta,
                },
            )
            .await
            .unwrap();
            let Some(Frame::Reject { message }) = wire::read_frame(&mut stream).await.unwrap()
            else {
                panic!("expected rejected registration");
            };
            assert!(message.contains(error), "{message}");
            task.await.unwrap().unwrap();
        }
        assert_eq!(
            broker
                .clients(&user, &legacy(&user, &anchor))
                .await
                .unwrap()
                .len(),
            wire::MAX_CLIENTS_PER_USER
        );
        assert!(Arc::ptr_eq(
            &broker.redeem_login(&user, &"a".repeat(64)).await.unwrap(),
            &anchor
        ));
    }
}
