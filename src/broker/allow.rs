//! Initial access targets resolve lookup names into immutable provider subjects.
use super::Broker;
use crate::{
    identity::Identity,
    portal::Problem,
    wire::{MAX_ALLOWED_PRINCIPALS, canonical_allow_principal},
};
use std::collections::HashSet;

impl Broker {
    pub(super) async fn resolve_account_allow(
        &self,
        registration: &crate::device::Registration,
        principals: &[String],
    ) -> Result<Vec<Identity>, String> {
        let owner = &registration.identity;
        let principal = format!("{}@github", owner.login.to_ascii_lowercase());
        let additional: Vec<String> = principals
            .iter()
            .filter(|target| **target != principal)
            .cloned()
            .collect();
        if additional.is_empty() {
            return Ok(vec![owner.clone()]);
        }
        if !registration.session_access {
            return Err("this credential cannot share access; sign in with a controller account on a server supporting per-session access".into());
        }
        let mut identities = self.resolve_initial_allow(&additional).await?;
        if !registration.is_live(self).await {
            return Err("ssmcp authorization expired or revoked".into());
        }
        identities.retain(|identity| {
            identity.provider != owner.provider || identity.subject != owner.subject
        });
        identities.insert(0, owner.clone());
        if identities.len() > MAX_ALLOWED_PRINCIPALS {
            return Err(format!(
                "allow list accepts at most {MAX_ALLOWED_PRINCIPALS} accounts including the owner"
            ));
        }
        consistent_labels(&identities)?;
        Ok(identities)
    }

    pub(super) async fn resolve_initial_allow(
        &self,
        principals: &[String],
    ) -> Result<Vec<Identity>, String> {
        if principals.is_empty() || principals.len() > MAX_ALLOWED_PRINCIPALS {
            return Err(format!(
                "allow list must contain 1..={MAX_ALLOWED_PRINCIPALS} identities"
            ));
        }
        let mut handles = Vec::new();
        let mut github = Vec::new();
        let mut seen = HashSet::new();
        for principal in principals {
            let canonical = canonical_allow_principal(principal)?;
            if canonical != *principal {
                return Err(
                    "allow entries must use lowercase server usernames or username@github".into(),
                );
            }
            if !seen.insert(canonical.clone()) {
                return Err("allow list must not contain duplicate principals".into());
            }
            if canonical.contains('@') {
                github.push(canonical);
            } else {
                handles.push(canonical);
            }
        }
        if !self.identities.enabled("github") {
            return Err("GitHub identity provider is not configured".into());
        }

        // Resolve every local name before making any upstream lookup. Unknown names
        // never fall back to a same-named GitHub user or a future local registrant.
        let mut identities = if handles.is_empty() {
            Vec::new()
        } else {
            self.portal
                .storage(move |store| {
                    let mut identities = Vec::with_capacity(handles.len());
                    for handle in handles {
                        let Some(record) = store.account_by_handle(&handle)? else {
                            return Ok(None);
                        };
                        let account = store.account(&record.subject)?;
                        identities.push(Identity {
                            provider: "github".into(),
                            subject: account.subject,
                            login: account.github_login,
                        });
                    }
                    Ok(Some(identities))
                })
                .await
                .map_err(|problem| match problem {
                    Problem::Disabled => "server account storage is not enabled".to_owned(),
                    Problem::Banned => "server username belongs to a banned account".to_owned(),
                    _ => "could not read server account storage".to_owned(),
                })?
                .ok_or_else(|| {
                    "unknown server username; register it on this server before using --allow"
                        .to_owned()
                })?
        };
        if !github.is_empty() {
            identities.extend(self.identities.resolve_allow(&github).await?);
        }
        // A server handle and an explicit GitHub login can refer to the same account.
        // The stored login remains display metadata; only provider + subject grants access.
        let mut subjects = HashSet::new();
        identities.retain(|identity| {
            subjects.insert((identity.provider.clone(), identity.subject.clone()))
        });
        if identities
            .iter()
            .any(|identity| self.user_policies.is_banned(&identity.subject))
        {
            return Err("allow target belongs to a banned account".into());
        }
        consistent_labels(&identities)?;
        Ok(identities)
    }
}

fn consistent_labels(identities: &[Identity]) -> Result<(), String> {
    for (index, identity) in identities.iter().enumerate() {
        if identities[..index].iter().any(|existing| {
            existing.provider == identity.provider
                && existing.subject != identity.subject
                && existing.login.eq_ignore_ascii_case(&identity.login)
        }) {
            return Err("a GitHub login now identifies a different account; refresh the server account and review the allow targets".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        broker::{Authorization, IDENTITY_RESOURCE},
        identity::{GitHubConfig, IdentityProviders},
        portal::Portal,
        wire::{self, ClientMetadata, Frame, Mode},
    };
    use axum::{
        Json, Router,
        extract::{Path, State},
        http::StatusCode,
        routing::get,
    };
    use futures_util::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use std::{
        collections::HashMap,
        path::PathBuf,
        sync::{Arc, Mutex},
        time::Duration,
    };
    use tokio::{
        io::{BufReader, DuplexStream},
        net::TcpListener,
        task::JoinHandle,
        time::timeout,
    };

    struct Temporary(PathBuf);

    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Default)]
    struct MockState {
        users: HashMap<String, u64>,
        requests: Vec<String>,
        pause: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
    }

    struct Mock {
        state: Arc<Mutex<MockState>>,
        task: JoinHandle<()>,
    }

    impl Mock {
        async fn new() -> (Self, IdentityProviders) {
            async fn lookup(
                State(state): State<Arc<Mutex<MockState>>>,
                Path(login): Path<String>,
            ) -> (StatusCode, Json<Value>) {
                let (response, pause) = {
                    let mut state = state.lock().unwrap();
                    state.requests.push(login.clone());
                    let response = match state.users.get(&login) {
                        Some(subject) => (
                            StatusCode::OK,
                            Json(json!({"id": subject, "login": login, "type": "User"})),
                        ),
                        None => (StatusCode::NOT_FOUND, Json(json!({}))),
                    };
                    (response, state.pause.clone())
                };
                if let Some((started, resume)) = pause {
                    started.notify_one();
                    resume.notified().await;
                }
                response
            }

            let state = Arc::new(Mutex::new(MockState {
                users: HashMap::from([
                    ("octocat".into(), 42),
                    ("alice".into(), 7),
                    ("unregistered".into(), 99),
                ]),
                ..MockState::default()
            }));
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let router = Router::new()
                .route("/users/{login}", get(lookup))
                .with_state(state.clone());
            let task = tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            });
            let providers = IdentityProviders::with_github_test_endpoints(
                "https://ssmcp.test",
                GitHubConfig {
                    client_id: "fixture".into(),
                    client_secret: "fixture-secret".into(),
                },
                &base,
            )
            .unwrap();
            (Self { state, task }, providers)
        }

        fn requests(&self) -> Vec<String> {
            self.state.lock().unwrap().requests.clone()
        }
    }

    impl Drop for Mock {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    struct Fixture {
        broker: Arc<Broker>,
        mock: Mock,
        _directory: Temporary,
    }

    impl Fixture {
        async fn new() -> Self {
            let directory = Temporary(std::env::temp_dir().join(format!(
                "ssmcp-initial-allow-{}",
                crate::broker::generate_secret().unwrap()
            )));
            let (mock, providers) = Mock::new().await;
            let mut broker = Broker::new("https://ssmcp.test".into());
            broker.identities = providers;
            broker.portal = Portal::open(&directory.0).unwrap();
            broker
                .portal
                .storage(|store| {
                    for (subject, login, handle) in [
                        ("42", "octocat", "alice"),
                        ("7", "alice", "colleague"),
                        ("123", "owner", "owner-home"),
                    ] {
                        store.login(&identity(subject, login))?;
                        store.claim_handle(subject, handle)?;
                    }
                    Ok(())
                })
                .await
                .unwrap();
            Self {
                broker: Arc::new(broker),
                mock,
                _directory: directory,
            }
        }
    }

    type Socket = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;
    struct HttpFixture {
        base: String,
        server: JoinHandle<()>,
    }
    impl HttpFixture {
        async fn start(broker: Arc<Broker>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let router = crate::http::router(broker);
            let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            Self { base, server }
        }
        async fn socket(&self, token: &str, allow: Vec<String>, access: bool) -> Socket {
            use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
            let mut request = format!(
                "{}/api/client/connect",
                self.base.replace("http://", "ws://")
            )
            .into_client_request()
            .unwrap();
            request
                .headers_mut()
                .insert("authorization", format!("Bearer {token}").parse().unwrap());
            if access {
                request.headers_mut().insert(
                    wire::CAPABILITIES_HEADER,
                    wire::SESSION_ACCESS_CAPABILITY.parse().unwrap(),
                );
            }
            let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
            let hello = Frame::Hello {
                version: wire::VERSION,
                mode: Mode::Shell,
                metadata: ClientMetadata {
                    name: "HTTPS allow fixture".into(),
                    title: None,
                    cwd: "/fixture".into(),
                    hostname: "localhost".into(),
                    allow,
                },
            };
            socket
                .send(Message::Binary(serde_json::to_vec(&hello).unwrap().into()))
                .await
                .unwrap();
            socket
        }
        async fn invited_token(&self, owner: &str) -> String {
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            let invitation: Value = client
                .post(format!("{}/api/client/invites", self.base))
                .bearer_auth(owner)
                .json(&json!({"ttl_secs":600,"session_ttl_secs":3600}))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            let enrolled: Value = client
                .post(format!("{}/api/client/enroll", self.base))
                .json(&json!({"token":invitation["token"]}))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            enrolled["device_token"].as_str().unwrap().to_owned()
        }
    }
    impl Drop for HttpFixture {
        fn drop(&mut self) {
            self.server.abort();
        }
    }
    async fn receive(socket: &mut Socket) -> Frame {
        loop {
            match timeout(Duration::from_secs(3), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
            {
                tokio_tungstenite::tungstenite::Message::Text(data) => {
                    return serde_json::from_slice(data.as_bytes()).unwrap();
                }
                tokio_tungstenite::tungstenite::Message::Binary(data) => {
                    return serde_json::from_slice(&data).unwrap();
                }
                other => assert!(!other.is_close(), "expected registration response"),
            }
        }
    }
    async fn owner_token(fixture: &Fixture) -> String {
        fixture
            .broker
            .portal
            .client_storage(|store| store.login("123".into()))
            .await
            .unwrap()
            .0
    }
    async fn reject_http(
        http: &HttpFixture,
        token: &str,
        targets: &[&str],
        capability: bool,
    ) -> String {
        let mut socket = http.socket(token, principals(targets), capability).await;
        let Frame::Reject { message } = receive(&mut socket).await else {
            panic!("invalid access targets must not produce a session")
        };
        let _ = socket.close(None).await;
        message
    }

    fn identity(subject: &str, login: &str) -> Identity {
        Identity {
            provider: "github".into(),
            subject: subject.into(),
            login: login.into(),
        }
    }

    fn principals(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    async fn connect(
        broker: Arc<Broker>,
        names: &[&str],
    ) -> (
        Frame,
        BufReader<DuplexStream>,
        JoinHandle<crate::Result<()>>,
    ) {
        let (server, client) = tokio::io::duplex(64 * 1024);
        // Exercise registration using a fixture owner, without an OS-account lookup.
        let task = tokio::spawn(broker.attach_stream(server, "gh-123".into(), None));
        let mut client = BufReader::new(client);
        wire::write_frame(
            client.get_mut(),
            &Frame::Hello {
                version: wire::VERSION,
                mode: Mode::Shell,
                metadata: ClientMetadata {
                    name: "allow fixture".into(),
                    title: None,
                    cwd: "/project".into(),
                    hostname: "localhost".into(),
                    allow: principals(names),
                },
            },
        )
        .await
        .unwrap();
        let frame = timeout(Duration::from_secs(3), wire::read_frame(&mut client))
            .await
            .unwrap()
            .unwrap()
            .expect("registration response");
        (frame, client, task)
    }

    async fn rejected(broker: &Arc<Broker>, names: &[&str]) -> String {
        let (frame, client, task) = connect(broker.clone(), names).await;
        let Frame::Reject { message } = frame else {
            panic!("an invalid allow list must not receive session or legacy credentials");
        };
        drop(client);
        timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(broker.sessions.read().await.is_empty());
        message
    }

    #[tokio::test]
    async fn initial_allow_https_shares_by_subject_and_retains_owner() {
        let fixture = Fixture::new().await;
        let http = HttpFixture::start(fixture.broker.clone()).await;
        let token = owner_token(&fixture).await;
        let mut socket = http
            .socket(
                &token,
                principals(&["alice", "alice@github", "octocat@github"]),
                true,
            )
            .await;
        let Frame::Ready {
            client_id,
            login_code,
            login_expires_in,
            capabilities,
            ..
        } = receive(&mut socket).await
        else {
            panic!("controller sharing should register")
        };
        assert!(login_code.is_empty());
        assert_eq!(login_expires_in, 0);
        assert_eq!(capabilities, [wire::SESSION_ACCESS_CAPABILITY]);
        let owner = Authorization::Identity(identity("123", "renamed-owner"));
        let session = fixture
            .broker
            .client(IDENTITY_RESOURCE, &owner, &client_id)
            .await
            .unwrap();
        assert_eq!(session.owner_subject.as_deref(), Some("123"));
        let subjects: Vec<String> = session
            .allowed()
            .iter()
            .map(|identity| identity.subject.clone())
            .collect();
        assert_eq!(subjects, ["123", "42", "7"]);
        for subject in ["42", "7"] {
            assert!(
                fixture
                    .broker
                    .client(
                        IDENTITY_RESOURCE,
                        &Authorization::Identity(identity(subject, "renamed-login")),
                        &client_id
                    )
                    .await
                    .is_ok()
            );
        }
        assert!(
            fixture
                .broker
                .client(
                    IDENTITY_RESOURCE,
                    &Authorization::Identity(identity("999", "octocat")),
                    &client_id
                )
                .await
                .is_err()
        );
        let snapshot = session
            .access_command(
                &fixture.broker.identities,
                &identity("123", "owner"),
                wire::AccessCommand::List {},
            )
            .await
            .unwrap();
        assert!(snapshot.entries[0].owner);
        assert_eq!(snapshot.entries[0].subject, "123");
        assert!(
            session
                .access_command(
                    &fixture.broker.identities,
                    &identity("42", "octocat"),
                    wire::AccessCommand::List {}
                )
                .await
                .is_err()
        );
        socket.close(None).await.unwrap();
    }

    #[tokio::test]
    async fn initial_allow_https_rejects_invites_missing_capability_and_invalid_targets() {
        let fixture = Fixture::new().await;
        let http = HttpFixture::start(fixture.broker.clone()).await;
        let token = owner_token(&fixture).await;
        assert!(
            reject_http(&http, &token, &["alice"], false)
                .await
                .contains("cannot share access")
        );
        let invited = http.invited_token(&token).await;
        assert!(
            reject_http(&http, &invited, &["alice"], true)
                .await
                .contains("cannot share access")
        );
        for targets in [
            vec!["unknown"],
            vec!["alice", "unknown"],
            vec!["alice", "missing@github"],
        ] {
            assert!(!reject_http(&http, &token, &targets, true).await.is_empty());
        }
        fixture
            .broker
            .portal
            .storage(|store| store.set_policy("42", Some(true), None))
            .await
            .unwrap();
        fixture
            .broker
            .configure_user_policies(crate::user_policy::DEFAULT_RATE_LIMIT_BPS)
            .await
            .unwrap();
        for target in ["alice", "octocat@github"] {
            assert!(
                reject_http(&http, &token, &[target], true)
                    .await
                    .contains("banned")
            );
        }
        assert!(
            fixture
                .broker
                .sessions
                .read()
                .await
                .values()
                .all(HashMap::is_empty)
        );
        // Default account and invited-device connections still work without
        // sharing capability; no change to the original owner-only policy.
        for credential in [&token, &invited] {
            let mut socket = http.socket(credential, Vec::new(), false).await;
            let Frame::Ready {
                client_id,
                capabilities,
                ..
            } = receive(&mut socket).await
            else {
                panic!("owner-only connection")
            };
            assert!(capabilities.is_empty());
            let session = fixture
                .broker
                .client(
                    IDENTITY_RESOURCE,
                    &Authorization::Identity(identity("123", "owner")),
                    &client_id,
                )
                .await
                .unwrap();
            assert_eq!(
                session
                    .allowed()
                    .iter()
                    .map(|entry| entry.subject.as_str())
                    .collect::<Vec<_>>(),
                ["123"]
            );
            socket.close(None).await.unwrap();
        }
    }

    #[tokio::test]
    async fn initial_allow_https_limit_includes_owner_and_aliases_deduplicate() {
        let fixture = Fixture::new().await;
        let http = HttpFixture::start(fixture.broker.clone()).await;
        let token = owner_token(&fixture).await;
        let mut own = http
            .socket(&token, principals(&["owner-home", "owner@github"]), true)
            .await;
        let Frame::Ready { client_id, .. } = receive(&mut own).await else {
            panic!("owner aliases")
        };
        let session = fixture
            .broker
            .client(
                IDENTITY_RESOURCE,
                &Authorization::Identity(identity("123", "owner")),
                &client_id,
            )
            .await
            .unwrap();
        assert_eq!(session.allowed().len(), 1);
        own.close(None).await.unwrap();
        fixture
            .broker
            .portal
            .storage(|store| {
                for i in 0..MAX_ALLOWED_PRINCIPALS {
                    let subject = (1000 + i).to_string();
                    let login = format!("member-{i}");
                    store.login(&identity(&subject, &login))?;
                    store.claim_handle(&subject, &login)?;
                }
                Ok(())
            })
            .await
            .unwrap();
        let targets: Vec<String> = (0..MAX_ALLOWED_PRINCIPALS)
            .map(|i| format!("member-{i}"))
            .collect();
        let mut allowed = http
            .socket(&token, targets[..MAX_ALLOWED_PRINCIPALS - 1].to_vec(), true)
            .await;
        let Frame::Ready { client_id, .. } = receive(&mut allowed).await else {
            panic!("31 targets plus owner")
        };
        let session = fixture
            .broker
            .client(
                IDENTITY_RESOURCE,
                &Authorization::Identity(identity("123", "owner")),
                &client_id,
            )
            .await
            .unwrap();
        assert_eq!(session.allowed().len(), MAX_ALLOWED_PRINCIPALS);
        assert_eq!(session.allowed()[0].subject, "123");
        let mut excess = http.socket(&token, targets, true).await;
        let Frame::Reject { message } = receive(&mut excess).await else {
            panic!("owner must count towards limit")
        };
        assert!(message.contains("including the owner"));
        let _ = excess.close(None).await;
        allowed.close(None).await.unwrap();
    }

    #[tokio::test]
    async fn initial_allow_recycled_login_cannot_label_two_different_accounts() {
        let fixture = Fixture::new().await;
        fixture
            .mock
            .state
            .lock()
            .unwrap()
            .users
            .insert("octocat".into(), 99);
        let error = fixture
            .broker
            .resolve_initial_allow(&principals(&["alice", "octocat@github"]))
            .await
            .unwrap_err();
        assert!(error.contains("different account"));
        let local = fixture
            .broker
            .resolve_initial_allow(&principals(&["alice"]))
            .await
            .unwrap();
        assert_eq!(local[0].subject, "42");
    }

    #[tokio::test]
    async fn initial_allow_https_revocation_during_lookup_prevents_session_creation() {
        let fixture = Fixture::new().await;
        let http = HttpFixture::start(fixture.broker.clone()).await;
        let token = owner_token(&fixture).await;
        let started = Arc::new(tokio::sync::Notify::new());
        let resume = Arc::new(tokio::sync::Notify::new());
        fixture.mock.state.lock().unwrap().pause = Some((started.clone(), resume.clone()));
        let mut socket = http
            .socket(&token, principals(&["unregistered@github"]), true)
            .await;
        timeout(Duration::from_secs(3), started.notified())
            .await
            .unwrap();
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(format!("{}/api/client/logout", http.base))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        resume.notify_one();
        let Frame::Reject { message } = receive(&mut socket).await else {
            panic!("a credential revoked during lookup must not create a session")
        };
        assert!(message.contains("expired or revoked"), "{message}");
        assert!(
            fixture
                .broker
                .sessions
                .read()
                .await
                .values()
                .all(HashMap::is_empty)
        );
        let _ = socket.close(None).await;
    }

    #[tokio::test]
    async fn initial_allow_keeps_local_and_github_namespaces_distinct() {
        let fixture = Fixture::new().await;
        let local = fixture
            .broker
            .resolve_initial_allow(&principals(&["alice"]))
            .await
            .unwrap();
        assert_eq!(local, [identity("42", "octocat")]);
        assert!(fixture.mock.requests().is_empty());

        let explicit = fixture
            .broker
            .resolve_initial_allow(&principals(&["alice@github"]))
            .await
            .unwrap();
        assert_eq!(explicit, [identity("7", "alice")]);
        let mixed = fixture
            .broker
            .resolve_initial_allow(&principals(&["alice", "alice@github"]))
            .await
            .unwrap();
        assert_eq!(mixed, [identity("42", "octocat"), identity("7", "alice")]);
        let aliases = fixture
            .broker
            .resolve_initial_allow(&principals(&["alice", "octocat@github"]))
            .await
            .unwrap();
        assert_eq!(aliases, [identity("42", "octocat")]);
        assert_eq!(fixture.mock.requests(), ["alice", "alice", "octocat"]);
    }

    #[tokio::test]
    async fn initial_allow_registration_binds_subjects_without_transferring_ownership() {
        let fixture = Fixture::new().await;
        let (frame, client, task) = connect(fixture.broker.clone(), &["alice"]).await;
        let Frame::Ready {
            url,
            login_code,
            login_expires_in,
            client_id,
            ..
        } = frame
        else {
            panic!("registered server username must be accepted");
        };
        assert_eq!(url, "https://ssmcp.test/mcp");
        assert!(login_code.is_empty());
        assert_eq!(login_expires_in, 0);

        let authorized = Authorization::Identity(identity("42", "renamed"));
        let session = fixture
            .broker
            .client(IDENTITY_RESOURCE, &authorized, &client_id)
            .await
            .unwrap();
        assert_eq!(session.owner_subject.as_deref(), Some("123"));
        assert!(authorized.permits(&session));
        assert!(!Authorization::Identity(identity("7", "alice")).permits(&session));
        assert!(
            !Authorization::Identity(Identity {
                provider: "other-provider".into(),
                subject: "42".into(),
                login: "octocat".into(),
            })
            .permits(&session)
        );
        assert!(
            !Authorization::Legacy {
                user: "gh-123".into(),
                anchor: Arc::downgrade(&session),
            }
            .permits(&session)
        );

        fixture
            .broker
            .portal
            .storage(|store| store.login(&identity("42", "renamed")))
            .await
            .unwrap();
        fixture
            .mock
            .state
            .lock()
            .unwrap()
            .users
            .insert("octocat".into(), 99);
        let after_rename = fixture
            .broker
            .resolve_initial_allow(&principals(&["alice"]))
            .await
            .unwrap();
        assert_eq!(after_rename, [identity("42", "renamed")]);
        assert_eq!(*session.allowed(), [identity("42", "octocat")]);
        assert!(authorized.permits(&session));
        let recycled = Authorization::Identity(identity("99", "octocat"));
        assert!(!recycled.permits(&session));
        assert!(
            fixture
                .broker
                .client(IDENTITY_RESOURCE, &recycled, &client_id)
                .await
                .is_err()
        );
        assert!(fixture.mock.requests().is_empty());

        drop(client);
        timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            fixture
                .broker
                .clients(IDENTITY_RESOURCE, &authorized)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn initial_allow_rejects_unknown_banned_and_partial_local_lists_before_lookup() {
        let fixture = Fixture::new().await;
        for names in [
            vec!["unregistered"],
            vec!["octocat@github", "unregistered"],
            vec!["alice", "unregistered"],
        ] {
            let message = rejected(&fixture.broker, &names).await;
            assert!(message.contains("unknown server username"), "{message}");
        }
        assert!(fixture.mock.requests().is_empty());

        fixture
            .broker
            .portal
            .storage(|store| store.set_policy("42", Some(true), None))
            .await
            .unwrap();
        let message = rejected(&fixture.broker, &["alice", "alice@github"]).await;
        assert!(message.contains("banned"), "{message}");
        assert!(fixture.mock.requests().is_empty());
    }

    #[tokio::test]
    async fn initial_allow_rejects_partial_lists_when_github_lookup_fails() {
        let fixture = Fixture::new().await;
        let message = rejected(&fixture.broker, &["alice", "missing@github"]).await;
        assert!(!message.is_empty());
        assert_eq!(fixture.mock.requests(), ["missing"]);
    }

    #[tokio::test]
    async fn initial_allow_requires_provider_and_server_account_storage() {
        let mut fixture = Fixture::new().await;
        Arc::get_mut(&mut fixture.broker).unwrap().identities =
            IdentityProviders::new("https://ssmcp.test", None).unwrap();
        for names in [vec!["alice"], vec!["alice@github"]] {
            let message = rejected(&fixture.broker, &names).await;
            assert!(message.contains("provider is not configured"), "{message}");
        }
        assert!(fixture.mock.requests().is_empty());

        let (mock, providers) = Mock::new().await;
        let mut broker = Broker::new("https://ssmcp.test".into());
        broker.identities = providers;
        let broker = Arc::new(broker);
        let message = rejected(&broker, &["alice"]).await;
        assert!(
            message.contains("account storage is not enabled"),
            "{message}"
        );
        assert!(mock.requests().is_empty());
        assert_eq!(
            broker
                .resolve_initial_allow(&principals(&["alice@github"]))
                .await
                .unwrap(),
            [identity("7", "alice")]
        );
    }

    #[tokio::test]
    async fn initial_allow_rejects_noncanonical_and_oversized_lists_before_lookup() {
        let fixture = Fixture::new().await;
        let mut invalid = vec![
            Vec::new(),
            principals(&[""]),
            principals(&["Alice"]),
            principals(&["alice@GitHub"]),
            principals(&["alice@other"]),
            principals(&["root"]),
            principals(&["gh-42"]),
            principals(&["a/b"]),
            principals(&["alice", "alice"]),
        ];
        invalid.push(
            (0..=MAX_ALLOWED_PRINCIPALS)
                .map(|index| format!("user{index}"))
                .collect(),
        );
        for names in invalid {
            assert!(
                fixture.broker.resolve_initial_allow(&names).await.is_err(),
                "{names:?}"
            );
        }
        assert!(fixture.mock.requests().is_empty());
        assert!(fixture.broker.sessions.read().await.is_empty());
    }
}
