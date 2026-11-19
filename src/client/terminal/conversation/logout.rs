//! Logout targets the credential used by this connection, never a default account.
use super::*;
use crate::client::account;
use tokio::sync::oneshot;

// Deliberately not Debug/Serialize: the connection token stays only in memory.
#[derive(Clone)]
pub(in crate::client::terminal) struct Target {
    pub(in crate::client::terminal) server: String,
    pub(in crate::client::terminal) token: String,
}

type Completion = Result<Option<account::LogoutOutcome>, String>;

pub(in crate::client::terminal) struct Task {
    receiver: oneshot::Receiver<Completion>,
    handle: tokio::task::JoinHandle<()>,
}

impl Task {
    fn start(runtime: tokio::runtime::Handle, target: Option<Target>, login: Option<account::login::Task>) -> Self {
        let (sender, receiver) = oneshot::channel();
        let handle = runtime.spawn(async move {
            if let Some(login) = login {
                login.cancel_and_wait().await;
            }
            let result = match target {
                Some(target) => account::logout_connection(&target.server, &target.token).await
                    .map(Some).map_err(|error| panel::safe_text(&error.to_string())),
                None => Ok(None),
            };
            let _ = sender.send(result);
            crate::client::terminal::invalidate();
        });
        Self { receiver, handle }
    }

    fn poll(&mut self) -> Option<Completion> {
        match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(oneshot::error::TryRecvError::Empty) => None,
            Err(oneshot::error::TryRecvError::Closed) => Some(Err("Logout stopped before its result was confirmed".into())),
        }
    }

    pub(in crate::client::terminal) async fn finish(mut self) -> Completion {
        (&mut self.receiver).await
            .unwrap_or_else(|_| Err("Logout stopped before its result was confirmed".into()))
    }

    #[cfg(test)]
    fn pending_for_test() -> (Self, oneshot::Sender<Completion>) {
        let (sender, receiver) = oneshot::channel();
        (Self { receiver, handle: tokio::spawn(std::future::pending()) }, sender)
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl State {
    pub(super) fn chat_logout(&mut self, arguments: &str) -> Result<(), String> {
        if !arguments.is_empty() {
            return Err("Use /logout without arguments to sign out of this connection".into());
        }
        if self.preview {
            return Err("Preview cannot sign out or revoke a saved login".into());
        }
        self.chat_poll_logout();
        if self.conversation.logout_task.is_some() || self.conversation.exit_requested {
            return Err("This session is already signing out or closing".into());
        }
        if self.logout_target.is_none() && self.conversation.login_task.is_none() {
            self.chat_finish_logout(Ok(None));
            return Ok(());
        }
        let runtime = tokio::runtime::Handle::try_current().ok()
            .or_else(|| crate::client::interface::harness::local_runtime().ok().map(|(_, handle)| handle))
            .ok_or("Logout requires an active client runtime")?;
        let login = self.conversation.login_task.take();
        self.conversation.logout_task = Some(Task::start(runtime, self.logout_target.clone(), login));
        self.notice = Some("Signing out of this connection…".into());
        self.dirty = true;
        Ok(())
    }

    pub(super) fn chat_poll_logout(&mut self) {
        let Some(result) = self.conversation.logout_task.as_mut().and_then(Task::poll) else { return; };
        self.conversation.logout_task = None;
        self.chat_finish_logout(result);
    }

    pub(in crate::client::terminal) fn chat_finish_logout(&mut self, result: Completion) {
        match result {
            Ok(outcome) => {
                let message = outcome.map(|outcome| outcome.message()).unwrap_or_else(||
                    "Disconnected this SSH or temporary session. No saved account login was revoked.".into());
                self.conversation.exit_message = Some(message.clone());
                self.conversation.exit_requested = true;
                self.notice = Some(message);
            }
            Err(error) => {
                self.notice = Some(format!("Logout failed: {}", panel::safe_text(&error)));
            }
        }
        self.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        super::super::super::preview_state(24, 100, &Ui::default())
    }

    #[test]
    fn logout_preview_and_arguments_cannot_touch_saved_accounts() {
        let mut state = state();
        state.conversation.composer.set_text("Keep this draft");
        let before = state.chat_input_stamp().unwrap();
        for command in ["/logout", "/logout other", "/logout --server https://other.example"] {
            assert!(state.chat_command(command).is_err());
            assert!(!state.conversation.exit_requested);
            assert!(state.conversation.logout_task.is_none());
            assert_eq!(state.chat_input_stamp().as_ref(), Some(&before));
        }
    }

    #[test]
    fn ssh_or_temporary_logout_closes_only_that_connection() {
        let mut state = state();
        state.preview = false;
        assert!(state.logout_target.is_none());
        assert!(state.chat_command("/logout").unwrap());
        assert!(state.conversation.exit_requested);
        assert!(state.conversation.logout_task.is_none());
        assert!(state.conversation.exit_message.as_deref().unwrap().contains("No saved account login was revoked"));
    }

    #[tokio::test]
    async fn pending_logout_waits_for_the_real_result_and_keeps_errors_visible() {
        let mut state = state();
        state.conversation.composer.set_text("Keep this draft");
        state.conversation.target = Some("chosen-agent".into());
        let before = state.chat_input_stamp().unwrap();
        let (task, sender) = Task::pending_for_test();
        state.conversation.logout_task = Some(task);
        state.chat_poll_logout();
        assert!(!state.conversation.exit_requested);
        sender.send(Err("A newer saved login is present".into())).unwrap();
        state.chat_poll_logout();
        assert!(!state.conversation.exit_requested);
        assert!(state.conversation.exit_message.is_none());
        assert!(state.notice.as_deref().unwrap().contains("newer saved login"));
        assert_eq!(state.chat_input_stamp().as_ref(), Some(&before));
        let (task, sender) = Task::pending_for_test();
        state.conversation.logout_task = Some(task);
        sender.send(Ok(Some(account::LogoutOutcome {
            server: "https://selected.example".into(), status: account::LogoutStatus::LocalOnly,
        }))).unwrap();
        state.chat_poll_logout();
        assert!(state.conversation.exit_requested);
        let message = state.conversation.exit_message.as_deref().unwrap();
        assert!(message.contains("selected.example") && message.contains("could not be confirmed"));
        assert_eq!(state.chat_input_stamp().as_ref(), Some(&before));
    }

    #[tokio::test]
    async fn teardown_can_await_logout_before_watchdog_reauthentication() {
        let (task, sender) = Task::pending_for_test();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            let _ = sender.send(Ok(Some(account::LogoutOutcome {
                server: "https://selected.example".into(), status: account::LogoutStatus::Revoked,
            })));
        });
        let mut state = state();
        state.chat_finish_logout(task.finish().await);
        assert!(state.conversation.exit_requested);
        assert!(state.conversation.exit_message.unwrap().contains("were revoked"));
    }

    #[tokio::test]
    async fn logout_cancels_an_older_login_before_completing() {
        let (login, _actions, events) = account::login::Task::pending_for_test();
        let task = Task::start(tokio::runtime::Handle::current(), None, Some(login));
        assert!(task.finish().await.unwrap().is_none());
        assert!(events.send(account::login::Event::Notice("late login".into())).is_err());
    }
}
