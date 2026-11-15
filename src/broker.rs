//! Linux-credential-backed registrations and bounded request forwarding.
use crate::wire::{self, Frame, Operation};
use serde_json::Value;
use std::{
    collections::HashMap,
    ffi::CStr,
    io::{self, Read},
    sync::{
        Arc, Mutex,
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

pub const LOGIN_LIFETIME: u64 = 600;

struct LoginCode {
    value: String,
    expires: Instant,
}

pub struct Session {
    login: Mutex<Option<LoginCode>>,
    sender: mpsc::Sender<Call>,
    slots: Semaphore,
    alive: AtomicBool,
}

impl Session {
    pub(crate) fn is_alive(&self) -> bool {
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
    pub oauth: crate::oauth::OAuth,
    sessions: RwLock<HashMap<String, Arc<Session>>>,
}

impl Broker {
    pub fn new(public_url: String) -> Self {
        Self {
            oauth: crate::oauth::OAuth::new(public_url.clone()),
            public_url,
            sessions: RwLock::new(HashMap::new()),
        }
    }

    pub async fn authenticate(&self, user: &str, access_token: &str) -> Option<Arc<Session>> {
        if !valid_username(user) {
            return None;
        }
        self.oauth.authenticate(user, access_token)
    }

    pub async fn online_session(&self, user: &str) -> Option<Arc<Session>> {
        let sessions = self.sessions.read().await;
        sessions.get(user).filter(|s| s.is_alive()).cloned()
    }

    pub async fn redeem_login(&self, user: &str, login_code: &str) -> Option<Arc<Session>> {
        let login_code = login_code.trim();
        if !valid_username(user) || login_code.len() != 64 {
            return None;
        }
        let session = self.online_session(user).await?;
        {
            let mut pending = session.login.lock().unwrap();
            let code = pending.as_ref()?;
            if !session.is_alive()
                || Instant::now() >= code.expires
                || !bool::from(code.value.as_bytes().ct_eq(login_code.as_bytes()))
            {
                return None;
            }
            pending.take();
        }
        Some(session)
    }

    #[cfg(test)]
    pub(crate) async fn test_register(&self, user: &str, login_code: &str) -> Arc<Session> {
        let (sender, _rx) = mpsc::channel(1);
        let session = Arc::new(Session {
            login: Mutex::new(Some(LoginCode {
                value: login_code.into(),
                expires: Instant::now() + Duration::from_secs(LOGIN_LIFETIME),
            })),
            sender,
            slots: Semaphore::new(1),
            alive: AtomicBool::new(true),
        });
        if let Some(previous) = self
            .sessions
            .write()
            .await
            .insert(user.into(), session.clone())
        {
            previous.alive.store(false, Ordering::Release);
        }
        session
    }

    pub async fn disconnect_all(&self) {
        let mut sessions = self.sessions.write().await;
        for session in sessions.values() {
            session.alive.store(false, Ordering::Release);
        }
        sessions.clear();
    }

    pub async fn attach(self: Arc<Self>, stream: UnixStream) -> crate::Result<()> {
        let uid = stream.peer_cred()?.uid();
        let user = tokio::task::spawn_blocking(move || username(uid)).await??;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mode = match timeout(IO_TIMEOUT, wire::read_frame(&mut reader)).await?? {
            Some(Frame::Hello {
                version: wire::VERSION,
                mode,
            }) => mode,
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
        let login_code = generate_secret()?;
        let (sender, mut requests) = mpsc::channel::<Call>(wire::MAX_INFLIGHT);
        let session = Arc::new(Session {
            login: Mutex::new(Some(LoginCode {
                value: login_code.clone(),
                expires: Instant::now() + Duration::from_secs(LOGIN_LIFETIME),
            })),
            sender,
            slots: Semaphore::new(wire::MAX_INFLIGHT),
            alive: AtomicBool::new(true),
        });
        {
            let mut sessions = self.sessions.write().await;
            if sessions.contains_key(&user) {
                drop(sessions);
                wire::write_frame(
                    &mut writer,
                    &Frame::Reject {
                        message: "this user already has an active client".into(),
                    },
                )
                .await?;
                return Ok(());
            }
            sessions.insert(user.clone(), session.clone());
        }
        eprintln!("ssmcpd: connected {user} ({})", mode.name());
        let result: crate::Result<()> = async {
            wire::write_frame(&mut writer, &Frame::Ready { url: format!("{}/{user}", self.public_url), login_code, login_expires_in:LOGIN_LIFETIME }).await?;
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
        if sessions
            .get(&user)
            .is_some_and(|current| Arc::ptr_eq(current, &session))
        {
            sessions.remove(&user);
        }
        eprintln!("ssmcpd: disconnected {user}");
        result
    }
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
    async fn duplicate_identity_is_rejected_without_replacing_first_session() {
        let broker = Arc::new(Broker::new("https://example.test".into()));
        let (server, client) = UnixStream::pair().unwrap();
        let first = tokio::spawn(broker.clone().attach(server));
        let (reader, mut writer) = client.into_split();
        let mut reader = BufReader::new(reader);
        wire::write_frame(
            &mut writer,
            &Frame::Hello {
                version: wire::VERSION,
                mode: wire::Mode::Shell,
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
        let (server2, client2) = UnixStream::pair().unwrap();
        let second = tokio::spawn(broker.clone().attach(server2));
        let (reader2, mut writer2) = client2.into_split();
        let mut reader2 = BufReader::new(reader2);
        wire::write_frame(
            &mut writer2,
            &Frame::Hello {
                version: wire::VERSION,
                mode: wire::Mode::Shell,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            wire::read_frame(&mut reader2).await.unwrap(),
            Some(Frame::Reject { .. })
        ));
        second.await.unwrap().unwrap();
        assert!(
            broker
                .redeem_login(url.rsplit('/').next().unwrap(), &login_code)
                .await
                .is_some()
        );
        drop(reader);
        drop(writer);
        first.await.unwrap().unwrap();
    }
}
