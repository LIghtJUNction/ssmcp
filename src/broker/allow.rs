//! Access targets resolve registered relay handles into immutable subjects.
use super::Broker;
use crate::{
    identity::Identity,
    portal::Problem,
    wire::{AccessEntry, AccessSnapshot, MAX_ALLOWED_PRINCIPALS, canonical_allow_principal},
};
use std::collections::HashSet;

impl Broker {
    pub(super) async fn resolve_account_allow(
        &self,
        registration: &crate::device::Registration,
        principals: &[String],
    ) -> Result<Vec<Identity>, String> {
        let identities = if principals.is_empty() {
            vec![registration.identity.clone()]
        } else {
            self.resolve_initial_allow(principals).await?
        };
        if !registration.is_live(self).await {
            return Err("ssmcp authorization expired or revoked".into());
        }
        Ok(identities)
    }

    pub(super) async fn resolve_initial_allow(
        &self,
        principals: &[String],
    ) -> Result<Vec<Identity>, String> {
        if principals.is_empty() || principals.len() > MAX_ALLOWED_PRINCIPALS {
            return Err(format!(
                "allow list must contain 1..={MAX_ALLOWED_PRINCIPALS} username@server targets"
            ));
        }
        let mut handles = Vec::new();
        let mut seen = HashSet::new();
        for principal in principals {
            let canonical = canonical_allow_principal(principal)?;
            if canonical != *principal {
                return Err("allow entries must use canonical username@server targets".into());
            }
            if !seen.insert(canonical) {
                return Err("allow list must not contain duplicate targets".into());
            }
            handles.push(
                crate::server_endpoint::allow_handle_for_server(principal, &self.public_url)
                    .map_err(|error| error.to_string())?,
            );
        }
        if let Some(access) = &self.relay_access {
            return handles
                .iter()
                .map(|handle| {
                    access
                        .identity(handle)
                        .ok_or_else(|| "unknown provisioned relay principal".into())
                })
                .collect();
        }
        if !self.identities.enabled("github") {
            return Err("account OAuth provider is not configured".into());
        }
        // Registered handles are immutable bindings. No upstream login lookup,
        // caller label, or future registration can replace the stored subject.
        let identities = self
            .portal
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
                Problem::Banned => "allow target belongs to a banned account".to_owned(),
                _ => "could not read server account storage".to_owned(),
            })?
            .ok_or_else(|| {
                "unknown server username; the controller must register it on this server first"
                    .to_owned()
            })?;
        if identities
            .iter()
            .any(|identity| self.user_policies.is_banned(&identity.subject))
        {
            return Err("allow target belongs to a banned account".into());
        }
        Ok(identities)
    }

    pub(super) async fn access_snapshot(
        &self,
        allow: &[Identity],
        owner: Option<&Identity>,
        message: Option<String>,
    ) -> Result<AccessSnapshot, String> {
        let handles = if self.relay_access.is_some() {
            allow
                .iter()
                .map(|identity| identity.login.clone())
                .collect()
        } else {
            let identities = allow.to_vec();
            self.portal
                .storage(move |store| {
                    identities
                        .iter()
                        .map(|identity| {
                            store
                                .account_record(&identity.subject)
                                .map(|record| record.handle)
                        })
                        .collect::<Result<Vec<_>, _>>()
                })
                .await
                .map_err(|_| "could not read server account names")?
                .into_iter()
                .map(|handle| handle.ok_or("account has no registered relay username"))
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut entries = Vec::with_capacity(handles.len());
        for (identity, handle) in allow.iter().zip(handles) {
            let principal = crate::server_endpoint::allow_principal(&handle, &self.public_url)
                .map_err(|error| error.to_string())?;
            entries.push(AccessEntry {
                principal,
                login: handle,
                subject: identity.subject.clone(),
                owner: owner.is_some_and(|owner| {
                    owner.provider == identity.provider && owner.subject == identity.subject
                }),
            });
        }
        entries.sort_by(|left, right| {
            right
                .owner
                .cmp(&left.owner)
                .then_with(|| left.principal.cmp(&right.principal))
        });
        Ok(AccessSnapshot { entries, message })
    }

    pub(super) async fn revoke_target(&self, principal: &str) -> Result<Identity, String> {
        let handle = crate::server_endpoint::allow_handle_for_server(principal, &self.public_url)
            .map_err(|error| error.to_string())?;
        if let Some(access) = &self.relay_access {
            return access
                .identity(&handle)
                .ok_or_else(|| "unknown provisioned relay principal".into());
        }
        // Revocation must also work for a banned controller.
        let account = self
            .portal
            .storage(move |store| store.account_by_handle(&handle))
            .await
            .map_err(|_| "could not read server account storage")?
            .ok_or("unknown server username")?;
        Ok(Identity {
            provider: "github".into(),
            subject: account.subject,
            login: account.github_login,
        })
    }
}

#[cfg(test)]
mod tests {
    mod native_messages;
    mod project_continuity;
    mod session_io;
    use super::*;
    use crate::{
        broker::{Authorization, IDENTITY_RESOURCE, Session},
        identity::GitHubConfig,
        portal::Portal,
        user_policy::{
            DEFAULT_TRAFFIC_UTC_OFFSET_MINUTES, FREE_MONTHLY_TRAFFIC_BYTES, TrafficFrame,
        },
        wire::{self, AccessCommand, ClientMetadata, Frame, Mode, Operation},
    };
    use futures_util::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use std::{path::PathBuf, sync::Arc, time::Duration};
    use tokio::{net::TcpListener, task::JoinHandle, time::timeout};
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

    struct Temporary(PathBuf);
    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    type Socket = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;
    struct Fixture {
        broker: Arc<Broker>,
        base: String,
        server: JoinHandle<()>,
        _directory: Temporary,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.abort();
        }
    }
    fn identity(subject: &str, login: &str) -> Identity {
        Identity {
            provider: "github".into(),
            subject: subject.into(),
            login: login.into(),
        }
    }
    fn names(handles: &[&str]) -> Vec<String> {
        handles
            .iter()
            .map(|name| format!("{name}@ssmcp.test"))
            .collect()
    }
    fn operation() -> Operation {
        Operation::CallTool {
            name: "fixture.echo".into(),
            arguments: json!({"value":"safe"}),
        }
    }
    #[tokio::test]
    async fn headless_relay_checks_ingress_and_filters_mcp_by_provisioned_principal() {
        use sha2::{Digest, Sha256};
        let ingress = "a".repeat(64);
        let alice = "b".repeat(64);
        let bob = "c".repeat(64);
        let digest = |token: &str| crate::hex::encode(Sha256::digest(token));
        let access = serde_json::from_value(json!({
            "device_ingress_sha256": digest(&ingress),
            "principals": {"alice":digest(&alice), "bob":digest(&bob)}
        }))
        .unwrap();
        let broker = Arc::new(Broker::headless("https://ssmcp.test".into(), access).unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = crate::http::router(broker.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let fixture = Fixture {
            broker,
            base,
            server,
            _directory: Temporary(
                std::env::temp_dir().join(crate::broker::generate_secret().unwrap()),
            ),
        };
        let rejected = tokio_tungstenite::connect_async(fixture.request(None, true))
            .await
            .unwrap_err();
        assert!(
            matches!(rejected, tokio_tungstenite::tungstenite::Error::Http(response)
            if response.status().as_u16() == 401)
        );
        let mut request = fixture.request(None, true);
        request
            .headers_mut()
            .insert("x-ssmcp-device-ingress", ingress.parse().unwrap());
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        send(
            &mut socket,
            Frame::Hello {
                version: wire::VERSION,
                mode: Mode::Shell,
                metadata: ClientMetadata {
                    name: "headless fixture".into(),
                    title: None,
                    cwd: "/fixture".into(),
                    hostname: "localhost".into(),
                    allow: names(&["alice"]),
                },
            },
        )
        .await;
        let Frame::Ready { client_id, .. } = receive(&mut socket).await else {
            panic!("ready required")
        };
        let http = crate::http_client::builder().no_proxy().build().unwrap();
        for path in [
            "/",
            "/setup",
            "/api/client/whoami",
            "/.well-known/oauth-authorization-server",
        ] {
            assert_eq!(
                http.get(format!("{}{path}", fixture.base))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                404
            );
        }
        let payload = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list","arguments":{}}});
        for token in [&alice, &bob, &ingress] {
            let reply = http
                .post(format!("{}/mcp", fixture.base))
                .bearer_auth(token)
                .json(&payload)
                .send()
                .await
                .unwrap();
            if token == &ingress {
                assert_eq!(reply.status(), 401);
            } else {
                assert_eq!(reply.status(), 200);
                let value: Value = reply.json().await.unwrap();
                let clients = value["result"]["structuredContent"]["clients"]
                    .as_array()
                    .unwrap();
                assert_eq!(clients.len(), usize::from(token == &alice));
                if token == &alice {
                    assert_eq!(clients[0]["id"], client_id);
                }
            }
        }
    }
    impl Fixture {
        async fn new() -> Self {
            let directory = Temporary(std::env::temp_dir().join(format!(
                "ssmcp-anonymous-{}",
                crate::broker::generate_secret().unwrap()
            )));
            let mut broker = Broker::with_identity_config(
                "https://ssmcp.test".into(),
                crate::oauth::OAuthConfig::default(),
                Some(GitHubConfig {
                    client_id: "fixture".into(),
                    client_secret: "fixture-secret".into(),
                }),
            )
            .unwrap();
            broker.portal = Portal::open(&directory.0).unwrap();
            broker
                .portal
                .storage(|store| {
                    for (subject, login, handle) in [
                        ("42", "octocat", "alice"),
                        ("7", "alice", "colleague"),
                        ("123", "owner", "owner-home"),
                        ("999", "maintainer", "admin-home"),
                    ] {
                        store.login(&identity(subject, login))?;
                        store.claim_handle(subject, handle)?;
                    }
                    Ok(())
                })
                .await
                .unwrap();
            broker.configure_management(
                crate::management::ManagementPolicy::new(vec!["999".into()], Default::default())
                    .unwrap(),
                Arc::new(
                    crate::history::AuditLog::open(
                        &directory.0.join("audit"),
                        crate::history::AuditConfig::default(),
                    )
                    .unwrap(),
                ),
            );
            let broker = Arc::new(broker);
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let router = crate::http::router(broker.clone());
            let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            Self {
                broker,
                base,
                server,
                _directory: directory,
            }
        }
        async fn token(&self, subject: &str) -> String {
            let subject = subject.to_owned();
            self.broker
                .portal
                .client_storage(move |store| store.login(subject))
                .await
                .unwrap()
                .0
        }
        fn request(
            &self,
            token: Option<&str>,
            capabilities: bool,
        ) -> tokio_tungstenite::tungstenite::http::Request<()> {
            let mut request = format!(
                "{}/api/client/connect",
                self.base.replace("http://", "ws://")
            )
            .into_client_request()
            .unwrap();
            if let Some(token) = token {
                request
                    .headers_mut()
                    .insert("authorization", format!("Bearer {token}").parse().unwrap());
            }
            if capabilities {
                request.headers_mut().insert(
                    wire::CAPABILITIES_HEADER,
                    format!(
                        "{}, {}",
                        wire::SESSION_ACCESS_CAPABILITY,
                        wire::CONTROL_ATTRIBUTION_CAPABILITY
                    )
                    .parse()
                    .unwrap(),
                );
            }
            request
        }
        async fn socket(&self, token: Option<&str>, targets: Vec<String>) -> Socket {
            let (mut socket, _) = tokio_tungstenite::connect_async(self.request(token, true))
                .await
                .unwrap();
            send(
                &mut socket,
                Frame::Hello {
                    version: wire::VERSION,
                    mode: Mode::Shell,
                    metadata: ClientMetadata {
                        name: "isolated anonymous fixture".into(),
                        title: None,
                        cwd: "/fixture".into(),
                        hostname: "localhost".into(),
                        allow: targets,
                    },
                },
            )
            .await;
            socket
        }
        async fn ready(&self, token: Option<&str>, targets: Vec<String>) -> (Socket, Arc<Session>) {
            let mut socket = self.socket(token, targets).await;
            let Frame::Ready {
                client_id,
                login_code,
                login_expires_in,
                capabilities,
                ..
            } = receive(&mut socket).await
            else {
                panic!("ready required")
            };
            assert!(login_code.is_empty());
            assert_eq!(login_expires_in, 0);
            assert!(
                capabilities
                    .iter()
                    .any(|c| c == wire::SESSION_ACCESS_CAPABILITY)
            );
            assert!(
                capabilities
                    .iter()
                    .any(|c| c == wire::CONTROL_ATTRIBUTION_CAPABILITY)
            );
            let session = self
                .broker
                .sessions
                .read()
                .await
                .values()
                .find_map(|sessions| sessions.get(&client_id))
                .unwrap()
                .clone();
            (socket, session)
        }
        async fn ban(&self, subject: &str, banned: bool) {
            self.broker
                .management_set_user_policy(
                    &identity("999", "maintainer"),
                    subject,
                    Some(banned),
                    None,
                )
                .await
                .unwrap();
        }
        async fn fill_direction(&self, subject: &str, uploaded: bool, remaining: u64) {
            let period = self.broker.traffic_period();
            let subject = subject.to_owned();
            self.broker
                .portal
                .storage(move |store| {
                    let quota = store.traffic_quota(
                        &subject,
                        &period,
                        DEFAULT_TRAFFIC_UTC_OFFSET_MINUTES,
                        false,
                    )?;
                    let used = if uploaded {
                        quota.uploaded_bytes
                    } else {
                        quota.downloaded_bytes
                    };
                    store.reserve_traffic_frame(
                        &subject,
                        &period,
                        DEFAULT_TRAFFIC_UTC_OFFSET_MINUTES,
                        TrafficFrame {
                            uploaded,
                            bytes: FREE_MONTHLY_TRAFFIC_BYTES - remaining - used,
                        },
                        false,
                    )
                })
                .await
                .unwrap();
        }
    }
    async fn send(socket: &mut Socket, frame: Frame) {
        socket
            .send(Message::Binary(serde_json::to_vec(&frame).unwrap().into()))
            .await
            .unwrap();
    }
    async fn receive(socket: &mut Socket) -> Frame {
        loop {
            match timeout(Duration::from_secs(3), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
            {
                Message::Text(data) => return serde_json::from_slice(data.as_bytes()).unwrap(),
                Message::Binary(data) => return serde_json::from_slice(&data).unwrap(),
                Message::Ping(data) => socket.send(Message::Pong(data)).await.unwrap(),
                Message::Pong(_) => {}
                other => panic!("unexpected websocket frame: {other:?}"),
            }
        }
    }
    fn call(session: &Arc<Session>, subject: &str) -> JoinHandle<Result<Value, String>> {
        let session = session.clone();
        let authorization = Authorization::Identity(identity(subject, "renamed-display"));
        tokio::spawn(async move {
            session
                .call_with_authorization(operation(), None, &authorization)
                .await
        })
    }
    async fn reply(socket: &mut Socket, subject: &str, owner: Option<&str>) -> (usize, usize) {
        let frame = receive(socket).await;
        let uploaded = serde_json::to_vec(&frame).unwrap().len() + 1;
        let Frame::Call {
            id,
            attribution: Some(attribution),
            ..
        } = frame
        else {
            panic!("call")
        };
        assert_eq!(attribution.account.subject, subject);
        assert_eq!(
            attribution
                .owner
                .as_ref()
                .map(|owner| owner.subject.as_str()),
            owner
        );
        let frame = Frame::Reply {
            id,
            result: Ok(json!({"ok":true})),
        };
        let downloaded = serde_json::to_vec(&frame).unwrap().len() + 1;
        send(socket, frame).await;
        (uploaded, downloaded)
    }
    async fn close(mut socket: Socket) {
        let _ = socket.close(None).await;
    }

    fn legacy_self_hello(allow: Vec<String>) -> Frame {
        Frame::Hello {
            version: wire::VERSION,
            mode: Mode::Shell,
            metadata: ClientMetadata {
                name: "legacy account self fixture".into(),
                title: None,
                cwd: "/fixture".into(),
                hostname: "localhost".into(),
                allow,
            },
        }
    }

    #[tokio::test]
    async fn legacy_account_self_default_preserves_subject_and_negotiated_wire_shape() {
        let fixture = Fixture::new().await;
        let token = fixture.token("42").await;
        // No offer, the old access-only offer, and a peer with attribution support.
        for offered in 0..3 {
            let mut request = fixture.request(Some(&token), offered == 2);
            if offered == 1 {
                request.headers_mut().insert(
                    wire::CAPABILITIES_HEADER,
                    wire::SESSION_ACCESS_CAPABILITY.parse().unwrap(),
                );
            }
            let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
            send(
                &mut socket,
                legacy_self_hello(vec!["octocat@github".into()]),
            )
            .await;
            let ready = receive(&mut socket).await;
            if offered != 2 {
                assert!(
                    serde_json::to_value(&ready)
                        .unwrap()
                        .get("capabilities")
                        .is_none()
                );
            }
            let Frame::Ready {
                client_id,
                login_code,
                login_expires_in,
                capabilities,
                ..
            } = ready
            else {
                panic!("self-only authenticated account must connect")
            };
            assert!(login_code.is_empty());
            assert_eq!(login_expires_in, 0);
            assert!(
                !capabilities
                    .iter()
                    .any(|c| c == wire::SESSION_ACCESS_CAPABILITY)
            );
            assert_eq!(
                capabilities
                    .iter()
                    .any(|c| c == wire::CONTROL_ATTRIBUTION_CAPABILITY),
                offered == 2
            );
            let session = fixture
                .broker
                .sessions
                .read()
                .await
                .values()
                .find_map(|sessions| sessions.get(&client_id))
                .unwrap()
                .clone();
            assert!(session.identity_required);
            assert_eq!(session.owner_subject.as_deref(), Some("42"));
            assert_eq!(session.allowed().as_slice(), &[identity("42", "octocat")]);
            assert!(Authorization::Identity(identity("42", "renamed-display")).permits(&session));
            // The relay handle alice belongs to 42, while the GitHub login alice belongs to 7.
            assert!(!Authorization::Identity(identity("7", "octocat")).permits(&session));
            assert!(!Authorization::Identity(identity("999", "octocat")).permits(&session));

            for after_access_attempt in [false, true] {
                if after_access_attempt {
                    for command in [
                        AccessCommand::List {},
                        AccessCommand::Add {
                            principal: "alice@github".into(),
                        },
                        AccessCommand::Add {
                            principal: "colleague@ssmcp.test".into(),
                        },
                        AccessCommand::Remove {
                            principal: "octocat@github".into(),
                        },
                    ] {
                        send(&mut socket, Frame::Access { id: 90, command }).await;
                        let Frame::AccessReply {
                            id: 90,
                            result: Err(message),
                        } = receive(&mut socket).await
                        else {
                            panic!("legacy access commands must not edit the ACL")
                        };
                        assert!(message.contains("update the ssmcp client"), "{message}");
                        assert_eq!(session.allowed().as_slice(), &[identity("42", "octocat")]);
                    }
                }
                let pending = call(&session, "42");
                let frame = receive(&mut socket).await;
                assert_eq!(
                    serde_json::to_value(&frame)
                        .unwrap()
                        .get("attribution")
                        .is_some(),
                    offered == 2
                );
                let Frame::Call {
                    id, attribution, ..
                } = frame
                else {
                    panic!("native call required")
                };
                if let Some(attribution) = attribution {
                    assert_eq!(attribution.account.subject, "42");
                    assert_eq!(attribution.owner.unwrap().subject, "42");
                }
                send(
                    &mut socket,
                    Frame::Reply {
                        id,
                        result: Ok(json!({"ok": true})),
                    },
                )
                .await;
                assert_eq!(pending.await.unwrap().unwrap(), json!({"ok": true}));
                assert!(session.is_alive());
            }
            close(socket).await;
        }

        // Current empty and explicit server-scoped policies still negotiate both capabilities.
        for targets in [vec![], names(&["alice"])] {
            let (mut socket, session) = fixture.ready(Some(&token), targets).await;
            send(
                &mut socket,
                Frame::Access {
                    id: 1,
                    command: AccessCommand::List {},
                },
            )
            .await;
            let Frame::AccessReply {
                id: 1,
                result: Ok(snapshot),
            } = receive(&mut socket).await
            else {
                panic!("modern access controls must remain available")
            };
            assert_eq!(snapshot.entries.len(), 1);
            assert_eq!(snapshot.entries[0].principal, "alice@ssmcp.test");
            assert_eq!(snapshot.entries[0].subject, "42");
            assert!(snapshot.entries[0].owner);
            assert_eq!(session.allowed().as_slice(), &[identity("42", "octocat")]);
            close(socket).await;
        }
    }

    #[tokio::test]
    async fn legacy_account_self_rejects_foreign_ambiguous_noncanonical_and_anonymous_policies() {
        let fixture = Fixture::new().await;
        let token = fixture.token("42").await;
        for targets in [
            vec!["alice@github".into()],
            vec!["owner@github".into()],
            vec!["octocat@github".into(), "octocat@github".into()],
            vec!["octocat@github".into(), "alice@github".into()],
            vec!["octocat@github".into(), "alice@ssmcp.test".into()],
            vec!["OctoCat@github".into()],
            vec!["octocat@GitHub".into()],
            vec![" octocat@github".into()],
            vec!["octocat@github ".into()],
        ] {
            let mut socket = fixture.socket(Some(&token), targets.clone()).await;
            assert!(
                matches!(receive(&mut socket).await, Frame::Reject { .. }),
                "authenticated policy {targets:?}"
            );
            close(socket).await;
        }
        let mut socket = fixture.socket(None, vec!["octocat@github".into()]).await;
        assert!(matches!(receive(&mut socket).await, Frame::Reject { .. }));
        close(socket).await;
        assert!(fixture.broker.sessions.read().await.is_empty());
        // The generic resolver remains strict even for a principal recognized at the account boundary.
        assert!(
            fixture
                .broker
                .resolve_initial_allow(&["octocat@github".into()])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn legacy_account_self_still_validates_other_hello_metadata() {
        let fixture = Fixture::new().await;
        let token = fixture.token("42").await;
        for invalid_field in ["name", "cwd", "hostname", "title"] {
            let (mut socket, _) =
                tokio_tungstenite::connect_async(fixture.request(Some(&token), true))
                    .await
                    .unwrap();
            let mut hello = legacy_self_hello(vec!["octocat@github".into()]);
            let Frame::Hello {
                ref mut metadata, ..
            } = hello
            else {
                unreachable!()
            };
            match invalid_field {
                "name" => metadata.name.clear(),
                "cwd" => metadata.cwd = "relative/path".into(),
                "hostname" => metadata.hostname = "host\ninjection".into(),
                "title" => metadata.title = Some("title\ninjection".into()),
                _ => unreachable!(),
            }
            send(&mut socket, hello).await;
            let Frame::Reject { message } = receive(&mut socket).await else {
                panic!("invalid {invalid_field} must still be rejected")
            };
            assert!(message.contains(invalid_field), "{message}");
            close(socket).await;
        }
        assert!(fixture.broker.sessions.read().await.is_empty());
    }

    #[tokio::test]
    async fn legacy_account_self_cannot_bypass_logout_or_ban_between_upgrade_and_hello() {
        for banned in [false, true] {
            let fixture = Fixture::new().await;
            let token = fixture.token("42").await;
            let (mut socket, _) =
                tokio_tungstenite::connect_async(fixture.request(Some(&token), false))
                    .await
                    .unwrap();
            // Authentication succeeded at HTTP upgrade; revoke before the Hello is accepted.
            if banned {
                fixture.ban("42", true).await;
            } else {
                let response = crate::http_client::builder()
                    .no_proxy()
                    .build()
                    .unwrap()
                    .post(format!("{}/api/client/logout", fixture.base))
                    .bearer_auth(&token)
                    .send()
                    .await
                    .unwrap();
                assert!(response.status().is_success());
            }
            send(
                &mut socket,
                legacy_self_hello(vec!["octocat@github".into()]),
            )
            .await;
            let Frame::Reject { message } = receive(&mut socket).await else {
                panic!("revoked registration must not become a self-only session")
            };
            assert!(
                message.contains("authorization expired or revoked"),
                "{message}"
            );
            close(socket).await;
            assert!(fixture.broker.sessions.read().await.is_empty());

            let error = tokio_tungstenite::connect_async(fixture.request(Some(&token), false))
                .await
                .unwrap_err();
            let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
                panic!("revoked bearer must fail HTTP authentication: {error}")
            };
            assert_eq!(response.status().as_u16(), 401);
        }
    }

    #[tokio::test]
    async fn anonymous_allow_resolves_only_existing_handles_to_stable_subjects() {
        let fixture = Fixture::new().await;
        let (socket, session) = fixture.ready(None, names(&["alice", "colleague"])).await;
        assert!(session.owner_subject.is_none());
        assert!(session.owner_identity.is_none());
        assert!(session.identity_required);
        assert_eq!(
            session
                .allowed()
                .iter()
                .map(|i| i.subject.as_str())
                .collect::<Vec<_>>(),
            ["42", "7"]
        );
        for subject in ["42", "7"] {
            assert!(
                fixture
                    .broker
                    .client(
                        IDENTITY_RESOURCE,
                        &Authorization::Identity(identity(subject, "changed-login")),
                        session.client_id()
                    )
                    .await
                    .is_ok()
            );
        }
        for subject in ["123", "999"] {
            assert!(
                fixture
                    .broker
                    .client(
                        IDENTITY_RESOURCE,
                        &Authorization::Identity(identity(subject, "alice")),
                        session.client_id()
                    )
                    .await
                    .is_err()
            );
        }
        let legacy = Authorization::Legacy {
            user: "_anonymous".into(),
            anchor: Arc::downgrade(&session),
        };
        assert!(!legacy.permits(&session));
        let snapshot = session
            .access_command(&fixture.broker, None, AccessCommand::List {})
            .await
            .unwrap();
        assert!(snapshot.entries.iter().all(|entry| !entry.owner));
        assert_eq!(snapshot.entries[0].principal, "alice@ssmcp.test");
        snapshot.validate().unwrap();
        close(socket).await;
    }

    #[tokio::test]
    async fn anonymous_allow_rejects_empty_unknown_wrong_server_banned_and_partial_lists() {
        let fixture = Fixture::new().await;
        fixture.ban("7", true).await;
        for targets in [
            vec![],
            names(&["missing"]),
            names(&["alice", "missing"]),
            names(&["alice", "colleague"]),
            vec!["alice@other.test".into()],
            vec!["alice".into()],
            vec!["octocat@github".into()],
            vec!["Alice@ssmcp.test".into()],
            names(&["alice", "alice"]),
            (0..33).map(|i| format!("user-{i}@ssmcp.test")).collect(),
        ] {
            let mut socket = fixture.socket(None, targets.clone()).await;
            assert!(
                matches!(receive(&mut socket).await, Frame::Reject { .. }),
                "{targets:?}"
            );
            close(socket).await;
        }
        assert!(fixture.broker.sessions.read().await.is_empty());
        // The upstream GitHub login "alice" belongs to 7; the handle alice is permanently 42.
        assert_eq!(
            fixture
                .broker
                .resolve_initial_allow(&names(&["alice"]))
                .await
                .unwrap()[0]
                .subject,
            "42"
        );
        assert!(
            fixture
                .broker
                .resolve_initial_allow(&names(&["octocat"]))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn anonymous_allow_signed_in_defaults_to_self_but_explicit_list_excludes_registrar() {
        let fixture = Fixture::new().await;
        let token = fixture.token("123").await;
        let (socket, session) = fixture.ready(Some(&token), vec![]).await;
        assert_eq!(session.owner_subject.as_deref(), Some("123"));
        assert_eq!(
            session
                .allowed()
                .iter()
                .map(|i| i.subject.as_str())
                .collect::<Vec<_>>(),
            ["123"]
        );
        close(socket).await;
        let (socket, session) = fixture.ready(Some(&token), names(&["alice"])).await;
        assert_eq!(session.owner_subject.as_deref(), Some("123"));
        assert_eq!(
            session
                .allowed()
                .iter()
                .map(|i| i.subject.as_str())
                .collect::<Vec<_>>(),
            ["42"]
        );
        assert!(!Authorization::Identity(identity("123", "owner")).permits(&session));
        let snapshot = session
            .access_command(
                &fixture.broker,
                Some(&identity("123", "owner")),
                AccessCommand::List {},
            )
            .await
            .unwrap();
        assert!(snapshot.entries.iter().all(|entry| !entry.owner));
        close(socket).await;
    }

    #[tokio::test]
    async fn anonymous_allow_requires_capabilities_and_never_falls_back_from_invalid_token() {
        let fixture = Fixture::new().await;
        for (token, caps, status) in [(None, false, 400), (Some("invalid"), true, 401)] {
            let error = tokio_tungstenite::connect_async(fixture.request(token, caps))
                .await
                .unwrap_err();
            let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
                panic!("{error}")
            };
            assert_eq!(response.status().as_u16(), status);
        }
        let client = crate::http_client::builder().no_proxy().build().unwrap();
        for path in ["/api/client/invites", "/api/client/enroll"] {
            let response = client
                .post(format!("{}{path}", fixture.base))
                .json(&json!({"token":"never-create"}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
        }
        assert!(fixture.broker.sessions.read().await.is_empty());
    }

    #[tokio::test]
    async fn anonymous_allow_http_control_requires_login_and_acl_without_owner_billing() {
        let fixture = Fixture::new().await;
        let (mut socket, session) = fixture.ready(None, names(&["alice"])).await;
        let client = crate::http_client::builder().no_proxy().build().unwrap();
        let body = json!({"client_id":session.client_id(),"name":"fixture.echo","arguments":{}});
        let url = format!("{}/api/client/call", fixture.base);
        assert_eq!(
            client.post(&url).json(&body).send().await.unwrap().status(),
            401
        );
        let outsider = fixture.token("123").await;
        assert_eq!(
            client
                .post(&url)
                .bearer_auth(&outsider)
                .json(&body)
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
        let token = fixture.token("42").await;
        let task = tokio::spawn(async move {
            client
                .post(url)
                .bearer_auth(&token)
                .json(&body)
                .send()
                .await
                .unwrap()
        });
        let (uploaded, downloaded) = reply(&mut socket, "42", None).await;
        assert_eq!(task.await.unwrap().status(), 200);
        let quota = fixture.broker.traffic_quota("42").await.unwrap();
        assert_eq!(quota.uploaded_bytes, uploaded as u64);
        assert_eq!(quota.downloaded_bytes, downloaded as u64);
        assert_eq!(quota.used_bytes, uploaded.max(downloaded) as u64);
        assert_eq!(
            fixture
                .broker
                .traffic_quota("123")
                .await
                .unwrap()
                .used_bytes,
            0
        );
        close(socket).await;
    }

    #[tokio::test]
    async fn anonymous_allow_local_access_edits_only_its_stream_and_never_empty_legacy() {
        let fixture = Fixture::new().await;
        let (mut first, one) = fixture.ready(None, names(&["alice"])).await;
        let (second, two) = fixture.ready(None, names(&["alice"])).await;
        send(
            &mut first,
            Frame::Access {
                id: 1,
                command: AccessCommand::Add {
                    principal: "colleague@ssmcp.test".into(),
                },
            },
        )
        .await;
        let Frame::AccessReply {
            id: 1,
            result: Ok(snapshot),
        } = receive(&mut first).await
        else {
            panic!("access reply")
        };
        assert_eq!(snapshot.entries.len(), 2);
        assert!(Authorization::Identity(identity("7", "alice")).permits(&one));
        assert!(!Authorization::Identity(identity("7", "alice")).permits(&two));
        assert!(!Authorization::Identity(identity("999", "alice")).permits(&one));
        fixture.ban("7", true).await;
        one.access_command(
            &fixture.broker,
            None,
            AccessCommand::Remove {
                principal: "colleague@ssmcp.test".into(),
            },
        )
        .await
        .unwrap();
        assert!(
            one.access_command(
                &fixture.broker,
                None,
                AccessCommand::Remove {
                    principal: "alice@ssmcp.test".into()
                }
            )
            .await
            .unwrap_err()
            .contains("last allowed")
        );
        for target in ["alice@other.test", "missing@ssmcp.test", "octocat@github"] {
            assert!(
                one.access_command(
                    &fixture.broker,
                    None,
                    AccessCommand::Add {
                        principal: target.into()
                    }
                )
                .await
                .is_err()
            );
        }
        assert_eq!(one.allowed().len(), 1);
        close(first).await;
        close(second).await;
    }

    #[tokio::test]
    async fn anonymous_allow_request_quota_failure_preserves_other_controller_and_live_transport() {
        let fixture = Fixture::new().await;
        let (mut socket, session) = fixture.ready(None, names(&["alice", "colleague"])).await;
        fixture.fill_direction("42", true, 0).await;
        let error = call(&session, "42").await.unwrap().unwrap_err();
        assert!(error.contains("未执行"), "{error}");
        assert!(!error.contains("重新连接"), "{error}");
        assert!(session.is_alive());
        let good = call(&session, "7");
        let (uploaded, downloaded) = reply(&mut socket, "7", None).await;
        good.await.unwrap().unwrap();
        let bob = fixture.broker.traffic_quota("7").await.unwrap();
        assert_eq!(
            (bob.uploaded_bytes, bob.downloaded_bytes),
            (uploaded as u64, downloaded as u64)
        );
        assert_eq!(
            fixture
                .broker
                .traffic_quota("123")
                .await
                .unwrap()
                .used_bytes,
            0
        );
        close(socket).await;
    }

    #[tokio::test]
    async fn anonymous_allow_reply_quota_failure_keeps_other_account_and_original_subject() {
        let fixture = Fixture::new().await;
        let (mut socket, session) = fixture.ready(None, names(&["alice", "colleague"])).await;
        let blocked = call(&session, "42");
        let frame = receive(&mut socket).await;
        let Frame::Call {
            id,
            attribution: Some(actor),
            ..
        } = frame
        else {
            panic!("call")
        };
        assert_eq!(actor.account.subject, "42");
        fixture.fill_direction("42", false, 8).await;
        send(
            &mut socket,
            Frame::Reply {
                id,
                result: Ok(json!({"result":"executed but not delivered"})),
            },
        )
        .await;
        let error = blocked.await.unwrap().unwrap_err();
        assert!(error.contains("可能已经执行"), "{error}");
        assert!(session.is_alive());
        let good = call(&session, "7");
        reply(&mut socket, "7", None).await;
        good.await.unwrap().unwrap();
        assert_eq!(
            fixture
                .broker
                .traffic_quota("42")
                .await
                .unwrap()
                .downloaded_bytes,
            FREE_MONTHLY_TRAFFIC_BYTES - 8
        );
        assert!(
            fixture
                .broker
                .traffic_quota("7")
                .await
                .unwrap()
                .downloaded_bytes
                > 0
        );
        close(socket).await;
    }

    #[tokio::test]
    async fn anonymous_allow_revoke_pending_cannot_relabel_reply_or_disconnect_other_account() {
        let fixture = Fixture::new().await;
        let (mut socket, session) = fixture.ready(None, names(&["alice", "colleague"])).await;
        let pending = call(&session, "42");
        let Frame::Call { id, .. } = receive(&mut socket).await else {
            panic!("call")
        };
        session
            .access_command(
                &fixture.broker,
                None,
                AccessCommand::Remove {
                    principal: "alice@ssmcp.test".into(),
                },
            )
            .await
            .unwrap();
        send(
            &mut socket,
            Frame::Reply {
                id,
                result: Ok(json!({"secret":"for A only"})),
            },
        )
        .await;
        assert!(pending.await.unwrap().unwrap_err().contains("revoked"));
        assert_eq!(
            fixture
                .broker
                .traffic_quota("42")
                .await
                .unwrap()
                .downloaded_bytes,
            0
        );
        assert_eq!(
            fixture.broker.traffic_quota("7").await.unwrap().used_bytes,
            0
        );
        let good = call(&session, "7");
        reply(&mut socket, "7", None).await;
        good.await.unwrap().unwrap();
        assert!(session.is_alive());
        close(socket).await;
    }

    #[tokio::test]
    async fn anonymous_allow_registrar_ban_and_logout_do_not_disconnect_shared_controller() {
        let fixture = Fixture::new().await;
        let owner = fixture.token("123").await;
        let (mut socket, session) = fixture
            .ready(Some(&owner), names(&["owner-home", "alice"]))
            .await;
        fixture.ban("123", true).await;
        assert!(session.is_alive());
        assert!(call(&session, "123").await.unwrap().is_err());
        let good = call(&session, "42");
        reply(&mut socket, "42", Some("123")).await;
        good.await.unwrap().unwrap();
        fixture.ban("123", false).await;
        let client = crate::http_client::builder().no_proxy().build().unwrap();
        assert_eq!(
            client
                .get(format!("{}/api/client/whoami", fixture.base))
                .bearer_auth(&owner)
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        close(socket).await;

        let owner = fixture.token("123").await;
        let (mut socket, session) = fixture.ready(Some(&owner), names(&["alice"])).await;
        assert!(
            client
                .post(format!("{}/api/client/logout", fixture.base))
                .bearer_auth(&owner)
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
        assert_eq!(
            client
                .get(format!("{}/api/client/whoami", fixture.base))
                .bearer_auth(&owner)
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        let good = call(&session, "42");
        reply(&mut socket, "42", Some("123")).await;
        good.await.unwrap().unwrap();
        assert!(session.is_alive());
        close(socket).await;
    }

    #[tokio::test]
    async fn anonymous_allow_capacity_counts_only_explicit_accounts_and_add_is_atomic() {
        let fixture = Fixture::new().await;
        fixture
            .broker
            .portal
            .storage(|store| {
                for i in 0..33 {
                    let subject = (1000 + i).to_string();
                    let name = format!("user-{i}");
                    store.login(&identity(&subject, &name))?;
                    store.claim_handle(&subject, &name)?;
                }
                Ok(())
            })
            .await
            .unwrap();
        let targets: Vec<String> = (0..32).map(|i| format!("user-{i}@ssmcp.test")).collect();
        let token = fixture.token("123").await;
        let (socket, session) = fixture.ready(Some(&token), targets).await;
        assert_eq!(session.allowed().len(), 32);
        assert!(!Authorization::Identity(identity("123", "owner")).permits(&session));
        assert!(
            session
                .access_command(
                    &fixture.broker,
                    Some(&identity("123", "owner")),
                    AccessCommand::Add {
                        principal: "user-32@ssmcp.test".into()
                    }
                )
                .await
                .unwrap_err()
                .contains("32")
        );
        assert_eq!(session.allowed().len(), 32);
        let snapshot = session
            .access_command(
                &fixture.broker,
                None,
                AccessCommand::Add {
                    principal: "user-0@ssmcp.test".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(snapshot.entries.len(), 32);
        close(socket).await;
    }

    #[tokio::test]
    async fn anonymous_allow_requires_configured_provider_and_durable_account_storage() {
        let broker = Broker::new("https://ssmcp.test".into());
        assert!(
            broker
                .resolve_initial_allow(&names(&["alice"]))
                .await
                .unwrap_err()
                .contains("provider")
        );
        let broker = Broker::with_identity_config(
            "https://ssmcp.test".into(),
            crate::oauth::OAuthConfig::default(),
            Some(GitHubConfig {
                client_id: "fixture".into(),
                client_secret: "fixture-secret".into(),
            }),
        )
        .unwrap();
        assert!(
            broker
                .resolve_initial_allow(&names(&["alice"]))
                .await
                .unwrap_err()
                .contains("storage")
        );
    }

    #[tokio::test]
    async fn anonymous_allow_slow_request_does_not_block_other_controller() {
        let fixture = Fixture::new().await;
        let (mut socket, session) = fixture.ready(None, names(&["alice", "colleague"])).await;
        let existing = call(&session, "7");
        let Frame::Call {
            id: existing_id, ..
        } = receive(&mut socket).await
        else {
            panic!("existing B call");
        };
        fixture
            .broker
            .management_set_user_policy(&identity("999", "maintainer"), "42", None, Some(Some(1)))
            .await
            .unwrap();
        let slow = call(&session, "42");
        // A's complete serialized call cannot pass its one-byte/second policy.
        assert!(
            timeout(Duration::from_millis(50), socket.next())
                .await
                .is_err()
        );
        send(
            &mut socket,
            Frame::Reply {
                id: existing_id,
                result: Ok(json!({"existing":"B"})),
            },
        )
        .await;
        assert_eq!(
            timeout(Duration::from_secs(1), existing)
                .await
                .expect("A's rate wait blocked B's existing reply")
                .unwrap()
                .unwrap(),
            json!({"existing":"B"})
        );
        let next = call(&session, "7");
        reply(&mut socket, "7", None).await;
        timeout(Duration::from_secs(1), next)
            .await
            .expect("A's rate wait blocked B's new call")
            .unwrap()
            .unwrap();
        assert!(!slow.is_finished());
        slow.abort();
        assert!(slow.await.unwrap_err().is_cancelled());
        // A canceled admission must never dispatch after its policy is relaxed.
        fixture
            .broker
            .management_set_user_policy(&identity("999", "maintainer"), "42", None, Some(None))
            .await
            .unwrap();
        let fresh = call(&session, "42");
        let (uploaded, downloaded) = reply(&mut socket, "42", None).await;
        timeout(Duration::from_secs(1), fresh)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let quota = fixture.broker.traffic_quota("42").await.unwrap();
        assert_eq!(quota.uploaded_bytes, uploaded as u64);
        assert_eq!(quota.downloaded_bytes, downloaded as u64);
        assert!(session.is_alive());
        close(socket).await;
    }

    #[tokio::test]
    async fn anonymous_allow_slow_reply_does_not_block_other_controller() {
        let fixture = Fixture::new().await;
        let (mut socket, session) = fixture.ready(None, names(&["alice", "colleague"])).await;
        let slow = call(&session, "42");
        let Frame::Call { id: slow_id, .. } = receive(&mut socket).await else {
            panic!("A call");
        };
        let existing = call(&session, "7");
        let Frame::Call {
            id: existing_id, ..
        } = receive(&mut socket).await
        else {
            panic!("B call");
        };
        fixture
            .broker
            .management_set_user_policy(&identity("999", "maintainer"), "42", None, Some(Some(1)))
            .await
            .unwrap();
        send(
            &mut socket,
            Frame::Reply {
                id: slow_id,
                result: Ok(json!({"private":"A result remains rate limited"})),
            },
        )
        .await;
        send(
            &mut socket,
            Frame::Reply {
                id: existing_id,
                result: Ok(json!({"existing":"B"})),
            },
        )
        .await;
        assert_eq!(
            timeout(Duration::from_secs(1), existing)
                .await
                .expect("A's result rate wait blocked B's existing reply")
                .unwrap()
                .unwrap(),
            json!({"existing":"B"})
        );
        let next = call(&session, "7");
        reply(&mut socket, "7", None).await;
        timeout(Duration::from_secs(1), next)
            .await
            .expect("A's result rate wait blocked B's new call")
            .unwrap()
            .unwrap();
        assert!(!slow.is_finished());
        slow.abort();
        assert!(slow.await.unwrap_err().is_cancelled());
        // A canceled result stays undelivered and is never charged to B.
        fixture
            .broker
            .management_set_user_policy(&identity("999", "maintainer"), "42", None, Some(None))
            .await
            .unwrap();
        let fresh = call(&session, "42");
        let (_, downloaded) = reply(&mut socket, "42", None).await;
        timeout(Duration::from_secs(1), fresh)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let quota = fixture.broker.traffic_quota("42").await.unwrap();
        assert_eq!(quota.downloaded_bytes, downloaded as u64);
        assert!(session.is_alive());
        close(socket).await;
    }
}
