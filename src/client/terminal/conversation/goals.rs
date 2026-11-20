//! Local goal controls. Only the selected conversation's value is projected.
use super::*;
use crate::client::interface::history::{self as history, HistoryOwner, goals as store};
use std::sync::mpsc::{self, Receiver, TryRecvError};

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Show,
    Set(String),
    Clear,
}

impl Command {
    fn parse(arguments: &str) -> Result<Self, String> {
        let arguments = arguments.trim();
        let (verb, rest) = arguments
            .split_once(char::is_whitespace)
            .unwrap_or((arguments, ""));
        match (verb, rest.trim()) {
            ("" | "show", "") => Ok(Self::Show),
            ("clear", "") => Ok(Self::Clear),
            ("show" | "clear", _) => Err("Use /goal, /goal set <text>, or /goal clear".into()),
            ("set", text) => store::normalize(text).map(Self::Set),
            _ => store::normalize(arguments).map(Self::Set),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Load,
    Show,
    Set,
    Clear,
}

impl Action {
    fn writes(self) -> bool {
        matches!(self, Self::Set | Self::Clear)
    }
}

struct Task {
    owner: HistoryOwner,
    action: Action,
    receiver: Receiver<Result<Option<String>, String>>,
}

impl Task {
    fn start(owner: HistoryOwner, action: Action, text: Option<String>) -> Result<Self, String> {
        let (sender, receiver) = mpsc::sync_channel(1);
        let captured = owner.clone();
        std::thread::Builder::new()
            .name("ssmcp-goal".into())
            .spawn(move || {
                let result = if action.writes() {
                    store::change(&captured, text.as_deref())
                } else {
                    store::read(&captured)
                };
                let _ = sender.send(result);
                crate::client::terminal::invalidate();
            })
            .map_err(|_| "Could not start goal storage; retry /goal")?;
        Ok(Self {
            owner,
            action,
            receiver,
        })
    }

    fn poll(&self) -> Option<Result<Option<String>, String>> {
        match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                Some(Err("Goal storage stopped; retry /goal".into()))
            }
        }
    }
}

#[derive(Default)]
pub(super) struct Goal {
    owner: Option<HistoryOwner>,
    text: Option<String>,
    loaded: bool,
    failed: bool,
    task: Option<Task>,
}

impl Goal {
    fn observe(&mut self, owner: Option<HistoryOwner>) -> bool {
        if self.owner == owner {
            return false;
        }
        self.owner = owner;
        self.text = None;
        self.loaded = false;
        self.failed = false;
        true
    }

    pub(super) fn saving(&self) -> bool {
        self.task.as_ref().is_some_and(|task| task.action.writes())
    }

    pub(super) fn current_text(&self) -> Option<&str> {
        (self.loaded && self.owner == history::current_owner())
            .then_some(self.text.as_deref())
            .flatten()
    }

    pub(super) fn snapshot(&self) -> Value {
        let Some(owner) = history::current_owner() else {
            return Value::Null;
        };
        let current = self.owner.as_ref() == Some(&owner);
        let status = if !current || !self.loaded {
            "loading"
        } else if self.failed {
            "unavailable"
        } else if self.text.is_some() {
            "set"
        } else {
            "unset"
        };
        json!({
            "conversation_id":owner.id(), "source":"user",
            "text":if current && self.loaded { self.text.as_deref() } else { None },
            "status":status,
            "saving":self.task.as_ref().is_some_and(|task| task.owner == owner && task.action.writes())
        })
    }
}

impl State {
    pub(super) fn chat_goal(&mut self, arguments: &str) -> Result<(), String> {
        let command = Command::parse(arguments)?;
        if self.preview {
            return Err("Preview does not read or change saved conversation goals".into());
        }
        self.chat_poll_goal();
        if self.conversation.goal.saving() {
            return Err("Goal is being saved; wait for confirmation before changing it".into());
        }
        let owner = history::capture(None)?;
        self.conversation.goal.observe(Some(owner.clone()));
        if command == Command::Show
            && self.conversation.goal.loaded
            && !self.conversation.goal.failed
        {
            self.chat_show_goal(Action::Show);
            return Ok(());
        }
        if command == Command::Show
            && let Some(task) = self.conversation.goal.task.as_mut()
            && task.owner == owner
            && !task.action.writes()
        {
            task.action = Action::Show;
            self.notice = Some("Reading conversation goal…".into());
            self.dirty = true;
            return Ok(());
        }
        let (action, text) = match command {
            Command::Show => (Action::Show, None),
            Command::Set(text) => (Action::Set, Some(text)),
            Command::Clear => (Action::Clear, None),
        };
        // Replacing an in-flight read is harmless. A write is never replaced,
        // retried, serialized into reload state, or sent to another conversation.
        self.conversation.goal.task = Some(Task::start(owner, action, text)?);
        self.notice = Some(
            if action.writes() {
                "Saving conversation goal…"
            } else {
                "Reading conversation goal…"
            }
            .into(),
        );
        self.dirty = true;
        Ok(())
    }

    pub(in crate::client::terminal) fn chat_poll_goal(&mut self) {
        if self.preview {
            return;
        }
        let owner = history::current_owner();
        if self.conversation.goal.observe(owner.clone()) {
            self.dirty = true;
        }
        if let Some(result) = self.conversation.goal.task.as_ref().and_then(Task::poll) {
            let task = self
                .conversation
                .goal
                .task
                .take()
                .expect("polled goal task");
            // Recheck the actual capability, not just the displayed ID. The
            // same file can be resumed with a different lease after release.
            if Some(&task.owner) == owner.as_ref() {
                self.conversation.goal.loaded = true;
                match result {
                    Ok(text) => {
                        self.conversation.goal.text = text;
                        self.conversation.goal.failed = false;
                        if task.action != Action::Load {
                            self.chat_show_goal(task.action);
                        }
                    }
                    Err(error) => {
                        self.conversation.goal.failed = true;
                        self.conversation.goal.text = None;
                        self.notice = Some(format!(
                            "Could not update goal: {}",
                            panel::safe_text(&error)
                        ));
                    }
                }
                self.dirty = true;
            }
        }
        if !self.conversation.goal.loaded
            && self.conversation.goal.task.is_none()
            && let Some(owner) = owner
        {
            match Task::start(owner, Action::Load, None) {
                Ok(task) => self.conversation.goal.task = Some(task),
                Err(error) => {
                    self.conversation.goal.loaded = true;
                    self.conversation.goal.failed = true;
                    self.notice = Some(error);
                    self.dirty = true;
                }
            }
        }
    }

    fn chat_show_goal(&mut self, action: Action) {
        let Some(owner) = self.conversation.goal.owner.clone() else {
            return;
        };
        if history::current_owner().as_ref() != Some(&owner) {
            return;
        }
        let text = match self.conversation.goal.current_text() {
            Some(goal) => format!("Current conversation goal\n\n{goal}"),
            None => "No goal is set for this conversation.\n\n/goal set <text> sets a goal.".into(),
        };
        self.log_scoped_event(
            "goal",
            &json!({"type":"ssmcp.message", "author":"Goal", "text":text}),
            format!("\n[GOAL] {text}\n").as_bytes(),
            crate::runtime::call_scope::DisplaySource {
                call: None,
                history: crate::runtime::call_scope::DisplayHistory::Local(owner.display_id()),
            },
        );
        if history::current_owner().as_ref() != Some(&owner) {
            return;
        }
        self.notice = Some(
            match action {
                Action::Set => "Goal saved · /goal to view · /goal clear to remove",
                Action::Clear => "Goal cleared · the conversation continues",
                _ => "Current goal shown · /goal set <text> to change it",
            }
            .into(),
        );
        self.dirty = true;
    }
}

#[cfg(test)]
mod tests;
