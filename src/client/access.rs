//! Current-connection access controls. Only an acknowledged broker reply changes the list.
pub(super) use crate::wire::AccessEntry as Entry;
use crate::wire::{AccessCommand, AccessSnapshot, ControlAccount};
use std::{
    collections::HashSet,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const UNSUPPORTED: &str = "当前会话不支持 /allow，请连接支持权限管理的中继服务器";
static STATE: OnceLock<Mutex<Manager>> = OnceLock::new();

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Snapshot {
    pub connected: bool,
    pub supported: bool,
    pub loaded: bool,
    pub pending: bool,
    pub revision: u64,
    pub entries: Vec<Entry>,
    pub last_error: Option<String>,
    pub last_message: Option<String>,
}

pub(super) struct Request {
    pub id: u64,
    pub command: AccessCommand,
}

struct Pending {
    id: u64,
    grant_principal: Option<String>,
    resolved: Option<(String, String)>,
    receipt: Option<tokio::sync::oneshot::Sender<Result<AccessSnapshot, String>>>,
    deadline: Instant,
}

#[derive(Default)]
struct Manager {
    view: Snapshot,
    next_id: u64,
    sender: Option<mpsc::Sender<Request>>,
    pending: Option<Pending>,
    origin: Option<String>,
    client_id: String,
    grants: HashSet<(String, String)>,
}

impl Manager {
    fn changed(&mut self) {
        self.view.revision = self.view.revision.saturating_add(1);
    }

    fn connect(
        &mut self,
        supported: bool,
        origin: Option<&str>,
        client_id: &str,
    ) -> mpsc::Receiver<Request> {
        let (sender, receiver) = mpsc::channel(1);
        self.origin = origin.map(str::to_owned);
        self.client_id = client_id.to_owned();
        self.grants.clear();
        self.sender = Some(sender);
        self.pending = None;
        self.view = Snapshot {
            connected: true,
            supported,
            revision: self.view.revision.saturating_add(1),
            ..Snapshot::default()
        };
        if supported {
            // A new connection owns a fresh ACL. Never reuse another session's list.
            let _ = self.request(AccessCommand::List {});
        }
        receiver
    }

    fn request(&mut self, command: AccessCommand) -> Result<(), String> {
        if !self.view.connected {
            return Err("尚未连接服务器，无法修改会话权限".into());
        }
        if !self.view.supported {
            return Err(UNSUPPORTED.into());
        }
        self.poll();
        if self.pending.is_some() {
            return Err("权限请求正在处理中，请等待服务器回执".into());
        }
        let id = self
            .next_id
            .checked_add(1)
            .ok_or("权限请求编号已耗尽，请重新启动客户端")?;
        let grant_principal = match &command {
            AccessCommand::Add { principal } => Some(principal.clone()),
            _ => None,
        };
        self.sender
            .as_ref()
            .ok_or("权限连接已关闭")?
            .try_send(Request { id, command })
            .map_err(|_| "权限请求尚未发送，请稍后重试")?;
        self.next_id = id;
        self.pending = Some(Pending {
            id,
            grant_principal,
            resolved: None,
            receipt: None,
            deadline: Instant::now() + REQUEST_TIMEOUT,
        });
        self.view.pending = true;
        self.view.last_error = None;
        self.view.last_message = None;
        self.changed();
        Ok(())
    }

    fn reply(&mut self, id: u64, result: Result<AccessSnapshot, String>) {
        if self.pending.as_ref().is_none_or(|pending| pending.id != id) {
            return;
        }
        self.poll();
        let Some(pending) = self.pending.take() else {
            return;
        };
        self.view.pending = false;
        let result = result.and_then(|snapshot| {
            snapshot.validate()?;
            if let Some(server) = self.origin.as_deref() {
                for entry in &snapshot.entries {
                    crate::server_endpoint::allow_handle_for_server(&entry.principal, server)
                        .map_err(|error| error.to_string())?;
                }
            }
            if let Some((provider, subject)) = pending.resolved {
                if !snapshot.entries.iter().any(|entry| {
                    entry.subject == subject
                        && Some(&entry.principal) == pending.grant_principal.as_ref()
                }) {
                    return Err(
                        "confirmed access list did not contain the resolved controller".into(),
                    );
                }
                self.grants.insert((provider, subject));
            }
            self.grants.retain(|(_, subject)| {
                snapshot
                    .entries
                    .iter()
                    .any(|entry| &entry.subject == subject)
            });
            Ok(snapshot)
        });
        if let Some(receipt) = pending.receipt {
            let _ = receipt.send(result.clone());
        }
        match result {
            Ok(result) => {
                self.view.entries = result.entries;
                self.view.loaded = true;
                self.view.last_error = None;
                self.view.last_message = result.message;
            }
            Err(error) => {
                self.view.last_error = Some(error);
                self.view.last_message = None;
            }
        }
        self.changed();
    }

    fn poll(&mut self) {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| Instant::now() >= pending.deadline)
        {
            self.pending = None;
            self.view.pending = false;
            self.view.loaded = false;
            self.view.last_error =
                Some("服务器回执超时，操作结果尚未确认；请用 /allow 重新读取清单".into());
            self.view.last_message = None;
            self.changed();
        }
    }

    fn disconnect(&mut self) {
        let pending = self.pending.take().is_some();
        self.sender = None;
        self.origin = None;
        self.client_id.clear();
        self.grants.clear();
        self.view = Snapshot {
            revision: self.view.revision.saturating_add(1),
            last_error: pending
                .then(|| "连接已断开，权限操作结果未确认；重连后请重新读取清单".into()),
            ..Snapshot::default()
        };
    }
}

fn state() -> std::sync::MutexGuard<'static, Manager> {
    STATE
        .get_or_init(|| Mutex::new(Manager::default()))
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

fn principal(user: &str, server: &str) -> Result<String, String> {
    let handle = if user.contains('@') {
        crate::server_endpoint::allow_handle_for_server(user, server)
    } else {
        crate::username::normalize(user).map_err(Into::into)
    }
    .map_err(|error| error.to_string())?;
    crate::server_endpoint::allow_principal(&handle, server).map_err(|error| error.to_string())
}

pub(super) fn snapshot() -> Snapshot {
    state().view.clone()
}
fn command(arguments: &str, server: &str) -> Result<AccessCommand, String> {
    let words: Vec<_> = arguments.split_whitespace().collect();
    match words.as_slice() {
        [] | ["list"] => Ok(AccessCommand::List {}),
        ["add", user] => Ok(AccessCommand::Add {
            principal: principal(user, server)?,
        }),
        ["remove", user] => Ok(AccessCommand::Remove {
            principal: principal(user, server)?,
        }),
        _ => Err("Use /allow list, /allow add USER, or /allow remove USER".into()),
    }
}
pub(super) fn submit(arguments: &str) -> Result<(), String> {
    let mut manager = state();
    let command = command(arguments, manager.origin.as_deref().ok_or(UNSUPPORTED)?)?;
    manager.request(command)
}

pub(super) fn connect(
    supported: bool,
    origin: Option<&str>,
    client_id: &str,
) -> mpsc::Receiver<Request> {
    state().connect(supported, origin, client_id)
}

pub(super) fn resolve_grant(
    id: u64,
    server: &str,
    principal: &str,
    provider: &str,
    subject: &str,
) -> crate::Result<()> {
    crate::server_endpoint::allow_handle_for_server(principal, server)?;
    super::controller_accounts::check_session_target(server, provider, subject)?;
    let mut manager = state();
    manager.poll();
    let pending = manager
        .pending
        .as_mut()
        .filter(|p| p.id == id && p.grant_principal.as_deref() == Some(principal))
        .ok_or("access proposal is no longer pending")?;
    if pending.resolved.is_some() {
        return Err("duplicate access proposal".into());
    }
    pending.resolved = Some((provider.to_owned(), subject.to_owned()));
    Ok(())
}

pub(super) fn permits(server: &str, client_id: &str, account: &ControlAccount) -> bool {
    let manager = state();
    manager.origin.as_deref() == Some(server)
        && manager.client_id == client_id
        && manager
            .grants
            .contains(&(account.provider.clone(), account.subject.clone()))
}

pub(crate) fn frontend_available(client_id: &str) -> bool {
    let manager = state();
    manager.view.connected && manager.view.supported && manager.client_id == client_id
}

pub(crate) enum FrontendError {
    Rejected(String),
    Unknown,
}

pub(crate) async fn frontend_command(
    client_id: &str,
    arguments: &str,
) -> Result<AccessSnapshot, FrontendError> {
    let receiver = {
        let mut manager = state();
        if manager.client_id != client_id || !manager.view.connected {
            return Err(FrontendError::Rejected("session has changed".into()));
        }
        let origin = manager
            .origin
            .as_deref()
            .ok_or_else(|| FrontendError::Rejected(UNSUPPORTED.into()))?;
        let command = command(arguments, origin).map_err(FrontendError::Rejected)?;
        manager.request(command).map_err(FrontendError::Rejected)?;
        let (sender, receiver) = tokio::sync::oneshot::channel();
        manager
            .pending
            .as_mut()
            .expect("request just queued")
            .receipt = Some(sender);
        receiver
    };
    receiver
        .await
        .map_err(|_| FrontendError::Unknown)?
        .map_err(FrontendError::Rejected)
}

/// Recheck after obtaining the writer, so expired requests never reach the wire.
pub(super) fn awaiting_proposal(id: u64, principal: &str) -> bool {
    let mut state = state();
    state.poll();
    state.pending.as_ref().is_some_and(|pending| {
        pending.id == id && pending.grant_principal.as_deref() == Some(principal)
    })
}

pub(super) fn pending(id: u64) -> bool {
    let mut state = state();
    state.poll();
    state
        .pending
        .as_ref()
        .is_some_and(|pending| pending.id == id)
}

pub(super) fn reply(id: u64, result: Result<AccessSnapshot, String>) {
    state().reply(id, result);
}

pub(super) fn poll() {
    state().poll();
}

pub(super) fn disconnect() {
    state().disconnect();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner() -> AccessSnapshot {
        AccessSnapshot {
            entries: vec![Entry {
                principal: "alice@ssmcp.test".into(),
                login: "alice".into(),
                subject: "42".into(),
                owner: true,
            }],
            message: None,
        }
    }

    #[test]
    fn access_old_server_never_receives_a_request() {
        let mut manager = Manager::default();
        let mut receiver = manager.connect(false, None, "test");
        assert!(
            manager
                .request(AccessCommand::List {})
                .unwrap_err()
                .contains("不支持")
        );
        assert!(receiver.try_recv().is_err());
        assert!(!manager.view.pending);
        assert!(!manager.view.loaded);
    }

    #[test]
    fn access_only_current_server_acknowledgement_changes_entries() {
        let mut manager = Manager::default();
        let mut receiver = manager.connect(true, Some("https://ssmcp.test"), "test");
        let request = receiver.try_recv().unwrap();
        assert!(manager.view.pending);
        assert!(manager.view.entries.is_empty());
        assert!(manager.request(AccessCommand::List {}).is_err());
        manager.reply(request.id + 1, Ok(owner()));
        assert!(manager.view.entries.is_empty());
        manager.reply(request.id, Ok(owner()));
        assert!(manager.view.loaded);
        assert_eq!(manager.view.entries[0].subject, "42");
        manager
            .request(AccessCommand::Add {
                principal: "bob@ssmcp.test".into(),
            })
            .unwrap();
        let add = receiver.try_recv().unwrap();
        assert_eq!(manager.view.entries.len(), 1);
        manager.reply(add.id, Err("服务器用户名不存在".into()));
        assert_eq!(manager.view.entries.len(), 1);
        assert!(manager.view.last_message.is_none());
        manager.disconnect();
        let mut fresh = manager.connect(true, Some("https://ssmcp.test"), "test");
        let request = fresh.try_recv().unwrap();
        manager.reply(add.id, Ok(owner()));
        assert!(manager.view.entries.is_empty());
        manager.reply(request.id, Ok(owner()));
        assert_eq!(manager.view.entries.len(), 1);
    }

    #[test]
    fn access_timeout_marks_outcome_unknown_and_ignores_late_reply() {
        let mut manager = Manager::default();
        let mut receiver = manager.connect(true, Some("https://ssmcp.test"), "test");
        let request = receiver.try_recv().unwrap();
        manager.pending.as_mut().unwrap().deadline = Instant::now();
        manager.poll();
        assert!(!manager.view.pending);
        assert!(!manager.view.loaded);
        assert!(manager.view.last_error.as_ref().unwrap().contains("未确认"));
        manager.reply(request.id, Ok(owner()));
        assert!(manager.view.entries.is_empty());
        manager.disconnect();
        assert!(!manager.view.connected);
    }

    #[test]
    fn a_session_grant_requires_the_proposed_subject_and_current_server_receipt() {
        let mut manager = Manager::default();
        let mut requests = manager.connect(true, Some("https://ssmcp.test"), "one");
        manager.reply(requests.try_recv().unwrap().id, Ok(owner()));
        let guest = Entry {
            principal: "guest@ssmcp.test".into(),
            login: "guest".into(),
            subject: "7".into(),
            owner: false,
        };
        let mut shared = owner();
        shared.entries.push(guest.clone());
        manager
            .request(AccessCommand::Add {
                principal: guest.principal.clone(),
            })
            .unwrap();
        let id = requests.try_recv().unwrap().id;
        let key = ("github".to_owned(), guest.subject.clone());
        manager.pending.as_mut().unwrap().resolved = Some(key.clone());
        manager.reply(id + 1, Ok(shared.clone()));
        assert!(manager.grants.is_empty());
        manager.reply(id, Ok(shared.clone()));
        assert!(manager.grants.contains(&key));
        manager
            .request(AccessCommand::Remove {
                principal: guest.principal.clone(),
            })
            .unwrap();
        manager.reply(requests.try_recv().unwrap().id, Ok(owner()));
        assert!(manager.grants.is_empty());
        manager
            .request(AccessCommand::Add {
                principal: guest.principal.clone(),
            })
            .unwrap();
        manager.pending.as_mut().unwrap().resolved = Some(("github".into(), "99".into()));
        manager.reply(requests.try_recv().unwrap().id, Ok(shared.clone()));
        assert!(manager.view.last_error.is_some());
        assert!(manager.grants.is_empty());
        manager.grants.insert(key);
        manager.connect(true, Some("https://ssmcp.test"), "two");
        assert!(manager.grants.is_empty());
    }

    #[test]
    fn bare_usernames_and_literal_list_use_the_current_origin() {
        let origin = "https://ssmcp.test";
        assert_eq!(command("list", origin).unwrap(), AccessCommand::List {});
        assert_eq!(
            command("add list", origin).unwrap(),
            AccessCommand::Add {
                principal: "list@ssmcp.test".into()
            }
        );
        assert_eq!(
            command("remove Bob", origin).unwrap(),
            AccessCommand::Remove {
                principal: "bob@ssmcp.test".into()
            }
        );
        assert!(command("add bob@other.test", origin).is_err());
        assert!(command("bob", origin).is_err());
    }

    #[test]
    fn access_names_are_canonical_and_never_accept_controller_identity() {
        for name in [
            "Alice@ssmcp.test",
            "ALICE@SSMCP.TEST",
            "Alice@https://ssmcp.test/",
        ] {
            assert_eq!(
                principal(name, "https://ssmcp.test").unwrap(),
                "alice@ssmcp.test"
            );
        }
        for name in [
            "",
            "@",
            "a b",
            "alice@github",
            "alice@",
            "alice@other.example",
            "../../alice",
            "alice\nforged",
        ] {
            assert!(principal(name, "https://ssmcp.test").is_err(), "{name:?}");
        }
    }
}
