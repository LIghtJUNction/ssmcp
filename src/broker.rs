//! Linux-credential-backed registrations and bounded request forwarding.
use crate::wire::{self, ClientInfo, Frame, Operation};
use serde_json::Value;
use std::{
    collections::HashMap,
    ffi::CStr,
    io::{self, Read},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::{
    io::BufReader,
    net::UnixStream,
    sync::{RwLock, Semaphore, mpsc, oneshot},
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
    reply: oneshot::Sender<CallResult>,
}

pub struct Session {
    secret: String,
    info: ClientInfo,
    sender: mpsc::Sender<Call>,
    slots: Semaphore,
    alive: AtomicBool,
}

impl Session {
    pub fn client_id(&self) -> &str {
        &self.info.id
    }

    pub fn info(&self) -> ClientInfo {
        self.info.clone()
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    pub async fn call(&self, operation: Operation) -> CallResult {
        operation.validate()?;
        if !self.alive.load(Ordering::Acquire) {
            return Err("client disconnected".into());
        }
        let _permit = self
            .slots
            .try_acquire()
            .map_err(|_| "client busy; retry later")?;
        let duration = request_timeout(&operation);
        let (reply, response) = oneshot::channel();
        self.sender
            .try_send(Call { operation, reply })
            .map_err(|_| "client disconnected or busy")?;
        match timeout(duration, response).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("client disconnected".into()),
            Err(_) => Err("client request timed out; its outcome may be unknown".into()),
        }
    }
}

pub struct Broker {
    pub public_url: String,
    sessions: RwLock<HashMap<String, HashMap<String, Arc<Session>>>>,
}

impl Broker {
    pub fn new(public_url: String) -> Self {
        Self {
            public_url,
            sessions: RwLock::new(HashMap::new()),
        }
    }

    pub async fn authenticate(&self, user: &str, secret: &str) -> Option<Arc<Session>> {
        if !valid_username(user) || secret.len() != 64 {
            return None;
        }
        let sessions = self.sessions.read().await;
        let clients = sessions.get(user)?;
        clients
            .values()
            .find(|session| {
                session.is_alive() && bool::from(session.secret.as_bytes().ct_eq(secret.as_bytes()))
            })
            .cloned()
    }

    /// An issuing connection authorizes its owner's clients while that connection is live.
    pub async fn clients(
        &self,
        user: &str,
        anchor: &Arc<Session>,
    ) -> Result<Vec<Arc<Session>>, String> {
        let sessions = self.sessions.read().await;
        let clients = authorized_clients(&sessions, user, anchor)?;
        let mut result: Vec<_> = clients
            .values()
            .filter(|session| session.is_alive())
            .cloned()
            .collect();
        result.sort_unstable_by(|left, right| left.client_id().cmp(right.client_id()));
        Ok(result)
    }

    pub async fn client(
        &self,
        user: &str,
        anchor: &Arc<Session>,
        id: &str,
    ) -> Result<Arc<Session>, String> {
        let sessions = self.sessions.read().await;
        let clients = authorized_clients(&sessions, user, anchor)?;
        clients
            .get(id)
            .filter(|session| session.is_alive())
            .cloned()
            .ok_or_else(|| "client is unavailable; call list to refresh connected clients".into())
    }

    pub async fn disconnect_all(&self) {
        let mut sessions = self.sessions.write().await;
        for clients in sessions.values() {
            for session in clients.values() {
                session.alive.store(false, Ordering::Release);
            }
        }
        sessions.clear();
    }

    pub async fn attach(self: Arc<Self>, stream: UnixStream) -> crate::Result<()> {
        let uid = stream.peer_cred()?.uid();
        let user = tokio::task::spawn_blocking(move || username(uid)).await??;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let (mode, metadata) = match timeout(IO_TIMEOUT, wire::read_frame(&mut reader)).await?? {
            Some(Frame::Hello {
                version: wire::VERSION,
                mode,
                metadata,
            }) => (mode, metadata),
            _ => {
                wire::write_frame(
                    &mut writer,
                    &Frame::Reject {
                        message: "unsupported ssmcp wire protocol".into(),
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
        let secret = generate_secret()?;
        let client_id = generate_secret()?;
        let (sender, mut requests) = mpsc::channel::<Call>(wire::MAX_INFLIGHT);
        let session = Arc::new(Session {
            secret: secret.clone(),
            info: ClientInfo {
                id: client_id.clone(),
                name: metadata.name,
                mode,
                cwd: metadata.cwd,
                hostname: metadata.hostname,
            },
            sender,
            slots: Semaphore::new(wire::MAX_INFLIGHT),
            alive: AtomicBool::new(true),
        });
        {
            let mut sessions = self.sessions.write().await;
            let clients = sessions.entry(user.clone()).or_default();
            if clients.len() >= wire::MAX_CLIENTS_PER_USER {
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
            // An unexpected ID collision must never replace another live connection.
            if clients.contains_key(&client_id) {
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
            clients.insert(client_id.clone(), session.clone());
        }
        eprintln!("ssmcpd: connected {user}/{client_id} ({})", mode.name());
        let result: crate::Result<()> = async {
            wire::write_frame(&mut writer, &Frame::Ready { url: format!("{}/{user}", self.public_url), secret, client_id:client_id.clone() }).await?;
            let mut pending: HashMap<u64, (oneshot::Sender<CallResult>, Instant)> = HashMap::new();
            let mut next_id = 0u64;
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
                            if pending.len() >= wire::MAX_INFLIGHT {
                                let _ = call.reply.send(Err("client busy; retry later".into()));
                                continue;
                            }
                            next_id = next_id.checked_add(1).ok_or("request id exhausted")?;
                            pending.insert(next_id, (call.reply, Instant::now() + request_timeout(&call.operation)));
                            timeout(IO_TIMEOUT, wire::write_frame(&mut writer, &Frame::Call { id:next_id, operation:call.operation })).await??;
                        }
                        _ = expiry.tick() => {
                            if pending.values().any(|(_, deadline)| Instant::now() >= *deadline) {
                                return Err("client response timeout".into());
                            }
                        }
                    }
                };
                match received {
                    Some(Frame::Reply {id, result}) => {
                        if let Some((reply, _)) = pending.remove(&id) { let _ = reply.send(result); }
                    }
                    None => break,
                    _ => return Err("unexpected frame from client".into()),
                }
            }
            Ok(())
        }.await;
        session.alive.store(false, Ordering::Release);
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
        eprintln!("ssmcpd: disconnected {user}/{client_id}");
        result
    }
}

fn authorized_clients<'a>(
    sessions: &'a HashMap<String, HashMap<String, Arc<Session>>>,
    user: &str,
    anchor: &Arc<Session>,
) -> Result<&'a HashMap<String, Arc<Session>>, String> {
    let clients = sessions.get(user).ok_or("authorization connection ended")?;
    if !anchor.is_alive()
        || !clients
            .get(anchor.client_id())
            .is_some_and(|current| Arc::ptr_eq(current, anchor))
    {
        return Err("authorization connection ended or belongs to another user".into());
    }
    Ok(clients)
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

    fn metadata(name: &str, cwd: &str) -> wire::ClientMetadata {
        wire::ClientMetadata {
            name: name.into(),
            cwd: cwd.into(),
            hostname: "localhost".into(),
        }
    }

    fn session(id: &str, secret: &str) -> Arc<Session> {
        let (sender, _rx) = mpsc::channel(1);
        Arc::new(Session {
            secret: secret.into(),
            info: ClientInfo {
                id: id.into(),
                name: "same display name".into(),
                mode: wire::Mode::Shell,
                cwd: "/project".into(),
                hostname: "localhost".into(),
            },
            sender,
            slots: Semaphore::new(1),
            alive: AtomicBool::new(true),
        })
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
    async fn bearer_is_scoped_to_user_and_revocation() {
        let broker = Broker::new("https://example.test".into());
        let session = session("client-a", &"a".repeat(64));
        broker.sessions.write().await.insert(
            "alice".into(),
            HashMap::from([("client-a".into(), session)]),
        );
        assert!(
            broker
                .authenticate("alice", &"a".repeat(64))
                .await
                .is_some()
        );
        assert!(broker.authenticate("bob", &"a".repeat(64)).await.is_none());
        assert!(
            broker
                .authenticate("alice", &"b".repeat(64))
                .await
                .is_none()
        );
        broker.disconnect_all().await;
        assert!(
            broker
                .authenticate("alice", &"a".repeat(64))
                .await
                .is_none()
        );
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
        let Some(Frame::Ready { url, secret, .. }) = wire::read_frame(&mut reader).await.unwrap()
        else {
            panic!("missing registration");
        };
        let user = url.rsplit('/').next().unwrap();
        let session = broker.authenticate(user, &secret).await.unwrap();
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
        assert!(broker.authenticate(user, &secret).await.is_none());
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
                secret,
                client_id,
            }) = wire::read_frame(&mut stream).await.unwrap()
            else {
                panic!("missing registration");
            };
            credentials.push((url, secret, client_id));
            attachments.push((stream, task));
        }
        let user = credentials[0].0.rsplit('/').next().unwrap();
        assert_eq!(credentials[0].0, credentials[1].0);
        assert_ne!(credentials[0].2, credentials[1].2);
        let first = broker.authenticate(user, &credentials[0].1).await.unwrap();
        let second = broker.authenticate(user, &credentials[1].1).await.unwrap();
        let clients = broker.clients(user, &first).await.unwrap();
        assert_eq!(clients.len(), 2);
        assert_eq!(
            broker
                .client(user, &first, &credentials[1].2)
                .await
                .unwrap()
                .info()
                .cwd,
            "/second"
        );
        assert_eq!(
            broker
                .client(user, &second, &credentials[0].2)
                .await
                .unwrap()
                .info()
                .mode,
            wire::Mode::Shell
        );
        assert!(
            broker
                .client(user, &first, "same display name")
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
        assert!(broker.clients(user, &first).await.is_err());
        assert!(
            broker
                .client(user, &first, second.client_id())
                .await
                .is_err()
        );
        assert!(
            broker
                .client(user, &second, first.client_id())
                .await
                .is_err()
        );
        assert_eq!(broker.clients(user, &second).await.unwrap().len(), 1);
        assert!(broker.authenticate(user, &credentials[1].1).await.is_some());

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
        assert!(broker.clients("bob", &alice).await.is_err());
        assert!(broker.client("bob", &alice, bob.client_id()).await.is_err());
        assert!(broker.clients("alice", &bob).await.is_err());
        assert!(Arc::ptr_eq(
            &broker.client("alice", &alice, "same-id").await.unwrap(),
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
        assert!(broker.clients("alice", &alice).await.is_err());
        assert!(broker.client("alice", &alice, "same-id").await.is_err());
        assert!(broker.clients("alice", &replacement).await.is_ok());
        broker.disconnect_all().await;
        assert!(!bob.is_alive());
        assert!(!replacement.is_alive());
        assert!(broker.clients("alice", &replacement).await.is_err());
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
            broker.clients(&user, &anchor).await.unwrap().len(),
            wire::MAX_CLIENTS_PER_USER
        );
        assert!(Arc::ptr_eq(
            &broker.authenticate(&user, &"a".repeat(64)).await.unwrap(),
            &anchor
        ));
    }
}
