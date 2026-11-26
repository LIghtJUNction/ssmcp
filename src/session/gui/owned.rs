//! Each paired gateway registers and connects only to its own private child processes.
use super::gateway::{CreationError, RelayRefusal};
use super::{Attached, Connection, CoreFuture, Host, native};
use crate::session::frontend::{OwnedSession, RelayAdmissionDenial};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::sync::{Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore};
use zbridge::{BridgeError, ErrorCode, Request, Response};

pub(super) type Consent = Arc<dyn Fn() -> crate::Result<Option<String>> + Send + Sync>;
#[derive(Clone, Default)]
pub(super) struct Policy(pub(super) Option<Consent>);
impl Policy {
    fn check(&self, destination: &str) -> Result<(), BridgeError> {
        if let Some(confirmed) = &self.0 {
            if let Some(saved) = confirmed().map_err(native)? {
                let (name, origin) =
                    crate::server_endpoint::parse_target(&saved).map_err(native)?;
                if crate::server_endpoint::allow_principal(&name, &origin).map_err(native)?
                    == destination
                {
                    return Ok(());
                }
            }
            return Err(BridgeError::new(
                ErrorCode::Denied,
                "the device owner has not confirmed this relay account",
            ));
        }
        Ok(())
    }
}

struct Member {
    core: Arc<AsyncMutex<OwnedSession>>,
    destination: String,
    _slot: OwnedSemaphorePermit,
}
pub(super) struct OwnedHost {
    cwd: PathBuf,
    chat: bool,
    namespace: PathBuf,
    receipts: PathBuf,
    policy: Policy,
    members: Mutex<BTreeMap<String, Member>>,
    refusals: Mutex<VecDeque<(String, String, RelayAdmissionDenial)>>,
    slots: Arc<Semaphore>,
    first: Option<AsyncMutex<Option<(String, String)>>>,
}
impl OwnedHost {
    pub(super) fn new(
        cwd: PathBuf,
        root: &Path,
        policy: Policy,
        browser_launcher: bool,
    ) -> crate::Result<Self> {
        let parent = root.join("gui").join("owners");
        crate::session::create_private_tree(&parent)?;
        crate::session::verify_directory(&parent)?;
        let namespace = parent.join(crate::session::random_suffix()?);
        crate::platform::private_directory(&namespace)?;
        crate::session::verify_directory(&namespace)?;
        let receipts = namespace.join("receipts");
        crate::platform::private_directory(&receipts)?;
        Ok(Self {
            cwd,
            chat: false,
            namespace,
            receipts,
            policy,
            members: Mutex::new(BTreeMap::new()),
            refusals: Mutex::new(VecDeque::new()),
            slots: Arc::new(Semaphore::new(16)),
            first: browser_launcher.then(|| AsyncMutex::new(None)),
        })
    }
    #[cfg(target_os = "android")]
    pub(super) fn chat(cwd: PathBuf, root: &Path) -> crate::Result<Self> {
        let mut host = Self::new(cwd, root, Policy::default(), false)?;
        host.chat = true;
        Ok(host)
    }

    fn marker(&self, target: &str) -> Result<PathBuf, BridgeError> {
        let id = target
            .strip_prefix("live:")
            .filter(|s| {
                s.len() == 32
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
            .ok_or_else(|| BridgeError::invalid("expected a native live session target"))?;
        Ok(self.receipts.join(format!("session-{id}.json")))
    }
    fn prune(&self) {
        self.members
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|target, member| {
                let Ok(mut core) = member.core.try_lock() else {
                    return true;
                };
                let Ok(Some(status)) = core.try_wait() else {
                    return true;
                };
                if let Some(code) = RelayAdmissionDenial::from_exit(&status) {
                    let mut history = self.refusals.lock().unwrap_or_else(|e| e.into_inner());
                    if history.len() >= 128 {
                        history.pop_front();
                    }
                    history.push_back((target.clone(), member.destination.clone(), code));
                }
                false
            });
    }
}
impl Host for OwnedHost {
    fn relay_refusal(&self, session: &str, settle: bool) -> CoreFuture<'_, Option<RelayRefusal>> {
        let session = session.to_owned();
        Box::pin(async move {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
            loop {
                // Native pipes may close slightly before wait reports child termination.
                self.prune();
                if let Some((_, destination, code)) = self
                    .refusals
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .find(|(target, _, _)| *target == session)
                {
                    self.policy.check(destination)?;
                    return Ok(RelayRefusal::from_destination(
                        *code,
                        destination,
                        zbridge::Effect::MayHaveOccurred,
                    ));
                }
                if !settle
                    || !self.retains_target(&session)
                    || tokio::time::Instant::now() >= deadline
                {
                    return Ok(None);
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
    }
    fn retains_target(&self, session: &str) -> bool {
        self.members
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(session)
    }
    fn receipt_directory(&self) -> Option<&Path> {
        Some(&self.receipts)
    }
    fn authorize_creation(&self, destination: &str) -> Result<(), BridgeError> {
        self.policy.check(destination)
    }
    fn create(&self, destination: String) -> CoreFuture<'_, String, CreationError> {
        Box::pin(async move {
            self.policy.check(&destination)?;
            self.prune();
            let slot = self.slots.clone().try_acquire_owned().map_err(|_| {
                BridgeError::new(
                    ErrorCode::Unavailable,
                    "this frontend already owns 16 running sessions",
                )
            })?;
            let exe = std::env::current_exe().map_err(|e| native(e.into()))?;
            let core = if self.chat {
                OwnedSession::launch_chat_in(&exe, &destination, &self.cwd, &self.namespace).await
            } else {
                OwnedSession::launch_in(&exe, &destination, &self.cwd, &self.namespace).await
            }
            .map_err(|error| {
                // The chat runner registers its endpoint only after the relay
                // has sent Ready. The desktop runner can publish it sooner.
                if let Some(reason) = error.downcast_ref::<RelayAdmissionDenial>()
                    && let Some(failure) = RelayRefusal::from_destination(
                        *reason,
                        &destination,
                        if self.chat {
                            zbridge::Effect::None
                        } else {
                            zbridge::Effect::MayHaveOccurred
                        },
                    )
                {
                    CreationError::Relay(failure)
                } else {
                    CreationError::Bridge(native(error).uncertain())
                }
            })?;
            self.policy.check(&destination).map_err(|e| e.uncertain())?;
            let target = core.target().to_owned();
            let marker = self.marker(&target)?;
            let mut members = self.members.lock().unwrap_or_else(|e| e.into_inner());
            if members.contains_key(&target) || marker.try_exists().map_err(|e| native(e.into()))? {
                return Err(BridgeError::new(
                    ErrorCode::Conflict,
                    "duplicate native session target",
                )
                .into());
            }
            crate::session::write_json(&marker, &target).map_err(|e| native(e).uncertain())?;
            members.insert(
                target.clone(),
                Member {
                    core: Arc::new(AsyncMutex::new(core)),
                    destination,
                    _slot: slot,
                },
            );
            Ok(target)
        })
    }
    fn end_owned(&self, session: String) -> CoreFuture<'_, ()> {
        Box::pin(async move {
            let marker = self.marker(&session)?;
            let core = self
                .members
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&session)
                .map(|m| m.core.clone());
            if let Some(core) = core {
                core.lock()
                    .await
                    .shutdown()
                    .await
                    .map_err(|e| native(e).uncertain())?;
                self.members
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&session);
                return Ok(());
            }
            if marker.try_exists().map_err(|e| native(e.into()))?
                && crate::session::read_json::<String>(&marker).map_err(native)? == session
            {
                self.refusals
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .retain(|(target, _, _)| target != &session);
                return Ok(());
            }
            Err(BridgeError::new(
                ErrorCode::Denied,
                "session does not belong to this frontend",
            ))
        })
    }
    fn launch(&self, destination: String) -> CoreFuture<'_, String, CreationError> {
        Box::pin(async move {
            let first = self.first.as_ref().ok_or_else(|| {
                BridgeError::new(
                    ErrorCode::Denied,
                    "use the request-correlated owned-sessions endpoint",
                )
            })?;
            let mut first = first.lock().await;
            if let Some((existing, target)) = first.as_ref() {
                return if existing == &destination {
                    Ok(target.clone())
                } else {
                    Err(BridgeError::invalid("this launcher already owns a session").into())
                };
            }
            let target = self.create(destination.clone()).await?;
            *first = Some((destination, target.clone()));
            Ok(target)
        })
    }
    fn sessions(&self) -> CoreFuture<'_, Vec<Value>> {
        Box::pin(async move {
            self.prune();
            let members = self.members.lock().unwrap_or_else(|e| e.into_inner());
            Ok(members.iter().filter(|(_,m)|self.policy.check(&m.destination).is_ok()).map(|(target,m)| {
                json!({"target":target,"name":m.destination,"kind":"foreground","cwd":self.cwd})
            }).collect())
        })
    }
    fn connect(&self, session: String, read_only: bool) -> CoreFuture<'_, Arc<dyn Connection>> {
        Box::pin(async move {
            let (core, destination) = {
                let members = self.members.lock().unwrap_or_else(|e| e.into_inner());
                let member = members.get(&session).ok_or_else(|| {
                    BridgeError::new(
                        ErrorCode::Denied,
                        "session does not belong to this frontend",
                    )
                })?;
                (member.core.clone(), member.destination.clone())
            };
            self.policy.check(&destination)?;
            let frontend = core.lock().await.connect(read_only).await.map_err(native)?;
            Ok(Arc::new(OwnedConnection {
                inner: Attached::new(frontend),
                policy: self.policy.clone(),
                destination,
            }) as Arc<dyn Connection>)
        })
    }
}
struct OwnedConnection {
    inner: Attached,
    policy: Policy,
    destination: String,
}
impl Connection for OwnedConnection {
    fn request(&self, request: Request) -> CoreFuture<'_, Response> {
        Box::pin(async move {
            self.policy.check(&self.destination)?;
            self.inner.request(request).await
        })
    }
    fn close(&self) -> CoreFuture<'_, ()> {
        self.inner.close()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn distinct_pairings_hide_foreign_targets_and_register_in_private_namespaces() {
        let root = std::env::temp_dir().join(crate::session::random_suffix().unwrap());
        let a = OwnedHost::new(root.clone(), &root, Policy::default(), false).unwrap();
        let b = OwnedHost::new(root.clone(), &root, Policy::default(), false).unwrap();
        assert_ne!(a.namespace, b.namespace);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&a.namespace)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        let target = "live:00000000000000000000000000000001";
        crate::session::write_json(&a.marker(target).unwrap(), &target).unwrap();
        assert!(a.end_owned(target.into()).await.is_ok());
        assert!(b.end_owned(target.into()).await.is_err());
        assert!(b.sessions().await.unwrap().is_empty());
        assert!(matches!(b.connect(target.into(),false).await,Err(e) if e.code==ErrorCode::Denied));
        assert!(
            matches!(a.launch("user@example.test".into()).await,Err(CreationError::Bridge(e)) if e.code==ErrorCode::Denied)
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn module_checks_current_native_consent() {
        let saved = Arc::new(Mutex::new(Some("alice@https://example.test".to_owned())));
        let read = saved.clone();
        let policy = Policy(Some(Arc::new(move || Ok(read.lock().unwrap().clone()))));
        assert!(policy.check("alice@example.test").is_ok());
        assert!(policy.check("other@example.test").is_err());
        *saved.lock().unwrap() = None;
        assert!(policy.check("alice@example.test").is_err());
    }
}
