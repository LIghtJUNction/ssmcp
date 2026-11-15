//! Linux-credential-backed registrations and bounded request forwarding.
use crate::wire::{self, Frame, Operation};
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

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
type CallResult = std::result::Result<Value, String>;

struct Call {
    operation: Operation,
    reply: oneshot::Sender<CallResult>,
}

pub struct Session {
    secret: String,
    sender: mpsc::Sender<Call>,
    slots: Semaphore,
    alive: AtomicBool,
}

impl Session {
    pub async fn call(&self, operation: Operation) -> CallResult {
        operation.validate()?;
        if !self.alive.load(Ordering::Acquire) {
            return Err("terminal disconnected".into());
        }
        let _permit = self
            .slots
            .try_acquire()
            .map_err(|_| "terminal busy; retry later")?;
        let (reply, response) = oneshot::channel();
        self.sender
            .try_send(Call { operation, reply })
            .map_err(|_| "terminal disconnected or busy")?;
        match timeout(REQUEST_TIMEOUT, response).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("terminal disconnected".into()),
            Err(_) => Err("terminal request timed out; its outcome may be unknown".into()),
        }
    }
}

pub struct Broker {
    pub public_url: String,
    sessions: RwLock<HashMap<String, Arc<Session>>>,
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
        let session = sessions.get(user)?;
        if session.alive.load(Ordering::Acquire)
            && bool::from(session.secret.as_bytes().ct_eq(secret.as_bytes()))
        {
            Some(session.clone())
        } else {
            None
        }
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
        match timeout(REQUEST_TIMEOUT, wire::read_frame(&mut reader)).await?? {
            Some(Frame::Hello {
                version: wire::VERSION,
            }) => {}
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
        }
        let secret = generate_secret()?;
        let (sender, mut requests) = mpsc::channel::<Call>(wire::MAX_INFLIGHT);
        let session = Arc::new(Session {
            secret: secret.clone(),
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
                        message: "this user already has an active terminal".into(),
                    },
                )
                .await?;
                return Ok(());
            }
            sessions.insert(user.clone(), session.clone());
        }
        eprintln!("ssmcpd: connected {user}");
        let result: crate::Result<()> = async {
            wire::write_frame(&mut writer, &Frame::Ready { url: format!("{}/{user}", self.public_url), secret }).await?;
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
                                let _ = call.reply.send(Err("terminal busy; retry later".into()));
                                continue;
                            }
                            next_id = next_id.checked_add(1).ok_or("request id exhausted")?;
                            pending.insert(next_id, (call.reply, Instant::now()));
                            timeout(REQUEST_TIMEOUT, wire::write_frame(&mut writer, &Frame::Call { id:next_id, operation:call.operation })).await??;
                        }
                        _ = expiry.tick() => {
                            if pending.values().any(|(_, at)| at.elapsed() >= REQUEST_TIMEOUT) {
                                return Err("terminal response timeout".into());
                            }
                        }
                    }
                };
                match received {
                    Some(Frame::Reply {id, result}) => {
                        if let Some((reply, _)) = pending.remove(&id) { let _ = reply.send(result); }
                    }
                    None => break,
                    _ => return Err("unexpected frame from terminal".into()),
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
    async fn bearer_is_scoped_to_user_and_revocation() {
        let broker = Broker::new("https://example.test".into());
        let (sender, _rx) = mpsc::channel(1);
        let session = Arc::new(Session {
            secret: "a".repeat(64),
            sender,
            slots: Semaphore::new(1),
            alive: AtomicBool::new(true),
        });
        broker
            .sessions
            .write()
            .await
            .insert("alice".into(), session);
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
            },
        )
        .await
        .unwrap();
        let Some(Frame::Ready { url, secret }) = wire::read_frame(&mut reader).await.unwrap()
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
            },
        )
        .await
        .unwrap();
        let Some(Frame::Ready { url, secret }) = wire::read_frame(&mut reader).await.unwrap()
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
                .authenticate(url.rsplit('/').next().unwrap(), &secret)
                .await
                .is_some()
        );
        drop(reader);
        drop(writer);
        first.await.unwrap().unwrap();
    }
}
