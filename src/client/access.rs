//! Current-connection access controls. Only an acknowledged broker reply changes the list.
pub(super) use crate::wire::AccessEntry as Entry;
use crate::wire::{AccessCommand, AccessSnapshot, canonical_github_principal};
use std::{
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const UNSUPPORTED: &str = "当前连接不支持 /allow；请升级服务器，并使用 GitHub 账户的 HTTPS 连接";
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
    deadline: Instant,
}

#[derive(Default)]
struct Manager {
    view: Snapshot,
    next_id: u64,
    sender: Option<mpsc::Sender<Request>>,
    pending: Option<Pending>,
}

impl Manager {
    fn changed(&mut self) {
        self.view.revision = self.view.revision.saturating_add(1);
    }

    fn connect(&mut self, supported: bool) -> mpsc::Receiver<Request> {
        let (sender, receiver) = mpsc::channel(1);
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
        self.sender
            .as_ref()
            .ok_or("权限连接已关闭")?
            .try_send(Request { id, command })
            .map_err(|_| "权限请求尚未发送，请稍后重试")?;
        self.next_id = id;
        self.pending = Some(Pending {
            id,
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
        self.pending = None;
        self.view.pending = false;
        match result.and_then(|result| {
            result.validate()?;
            Ok(result)
        }) {
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

fn principal(user: &str) -> Result<String, String> {
    let user = user.trim();
    let user = user.strip_prefix('@').unwrap_or(user);
    if user.contains('@') {
        canonical_github_principal(user)
    } else {
        canonical_github_principal(&format!("{user}@github"))
    }
}

#[expect(dead_code, reason = "called by the terminal access UI once integrated")]
pub(super) fn snapshot() -> Snapshot {
    state().view.clone()
}

#[expect(dead_code, reason = "called by the terminal access UI once integrated")]
pub(super) fn refresh() -> Result<(), String> {
    state().request(AccessCommand::List {})
}

#[expect(dead_code, reason = "called by the terminal access UI once integrated")]
pub(super) fn add(user: &str) -> Result<(), String> {
    state().request(AccessCommand::Add {
        principal: principal(user)?,
    })
}

#[expect(dead_code, reason = "called by the terminal access UI once integrated")]
pub(super) fn remove(user: &str) -> Result<(), String> {
    state().request(AccessCommand::Remove {
        principal: principal(user)?,
    })
}

pub(super) fn connect(supported: bool) -> mpsc::Receiver<Request> {
    state().connect(supported)
}

/// Recheck after obtaining the writer, so expired requests never reach the wire.
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
                principal: "alice@github".into(),
                login: "Alice".into(),
                subject: "42".into(),
                owner: true,
            }],
            message: None,
        }
    }

    #[test]
    fn access_old_server_never_receives_a_request() {
        let mut manager = Manager::default();
        let mut receiver = manager.connect(false);
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
        let mut receiver = manager.connect(true);
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
                principal: "bob@github".into(),
            })
            .unwrap();
        let add = receiver.try_recv().unwrap();
        assert_eq!(manager.view.entries.len(), 1);
        manager.reply(add.id, Err("GitHub 用户不存在".into()));
        assert_eq!(manager.view.entries.len(), 1);
        assert!(manager.view.last_message.is_none());
        manager.disconnect();
        let mut fresh = manager.connect(true);
        let request = fresh.try_recv().unwrap();
        manager.reply(add.id, Ok(owner()));
        assert!(manager.view.entries.is_empty());
        manager.reply(request.id, Ok(owner()));
        assert_eq!(manager.view.entries.len(), 1);
    }

    #[test]
    fn access_timeout_marks_outcome_unknown_and_ignores_late_reply() {
        let mut manager = Manager::default();
        let mut receiver = manager.connect(true);
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
    fn access_names_are_canonical_and_never_accept_controller_identity() {
        for name in ["Alice", "@ALICE", "Alice@GitHub"] {
            assert_eq!(principal(name).unwrap(), "alice@github");
        }
        for name in [
            "",
            "@",
            "a b",
            "a--b",
            "a@other",
            "../../alice",
            "alice\nforged",
        ] {
            assert!(principal(name).is_err(), "{name:?}");
        }
    }
}
