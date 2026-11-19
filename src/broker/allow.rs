//! Initial SSH allow lists resolve lookup names into immutable provider subjects.
use super::Broker;
use crate::{
    identity::Identity,
    portal::Problem,
    wire::{MAX_ALLOWED_PRINCIPALS, canonical_allow_principal},
};
use std::collections::HashSet;

impl Broker {
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
                        let record = store.account_by_handle(&handle)?.ok_or(Problem::NotFound)?;
                        let account = store.account(&record.subject)?;
                        identities.push(Identity {
                            provider: "github".into(),
                            subject: account.subject,
                            login: account.github_login,
                        });
                    }
                    Ok(identities)
                })
                .await
                .map_err(|problem| match problem {
                    Problem::Disabled => "server account storage is not enabled".to_owned(),
                    Problem::NotFound => {
                        "unknown server username; register it on this server before using --allow"
                            .to_owned()
                    }
                    Problem::Banned => "server username belongs to a banned account".to_owned(),
                    _ => "could not read server account storage".to_owned(),
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
        Ok(identities)
    }
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
                let mut state = state.lock().unwrap();
                state.requests.push(login.clone());
                match state.users.get(&login) {
                    Some(subject) => (
                        StatusCode::OK,
                        Json(json!({"id": subject, "login": login, "type": "User"})),
                    ),
                    None => (StatusCode::NOT_FOUND, Json(json!({}))),
                }
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
