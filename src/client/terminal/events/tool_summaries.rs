//! Agent-authored labels for trusted native call groups. Display IDs and caller
//! names never authorize edits; reload restores text without live capabilities.
use super::{EventView, HistoryOwner, Kind, MAX_EVENTS, Meta, Status};
use crate::runtime::call_scope::Source;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(super) const THRESHOLD: u64 = 10;
const MAX_SUMMARY_BYTES: usize = 512;
const MAX_SUMMARY_CHARS: usize = 120;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Scope {
    history: HistoryOwner,
    connection: u64,
    provider: String,
    subject: String,
    agent: Option<String>,
    thread: Option<(String, String)>,
}

impl Scope {
    fn capture(source: &Source) -> Result<Self, String> {
        let history = source.require_owner()?.clone();
        let call = source
            .call
            .as_ref()
            .ok_or("tool_summary_scope_required: authenticated local call required")?;
        let attribution = call
            .attribution()
            .ok_or("tool_summary_scope_required: authenticated local call required")?;
        Ok(Self {
            history,
            connection: call.connection(),
            provider: attribution.account.provider.clone(),
            subject: attribution.account.subject.clone(),
            agent: attribution.agent.as_ref().map(|agent| agent.id.clone()),
            thread: call.thread().map(|thread| {
                (
                    thread.session_id().to_owned(),
                    thread.thread_id().to_owned(),
                )
            }),
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Display {
    pub(super) group_id: String,
    pub(super) summary: Option<String>,
    pub(super) revision: u64,
    pub(super) call_count: u64,
    pub(super) covered_call_count: u64,
}

impl Display {
    pub(super) fn needs_summary(&self) -> bool {
        self.call_count.saturating_sub(self.covered_call_count) > THRESHOLD
    }

    pub(super) fn valid(&mut self) -> bool {
        if !valid_id(&self.group_id)
            || self.call_count == 0
            || self.covered_call_count > self.call_count
        {
            return false;
        }
        if let Some(summary) = &self.summary {
            let Ok(summary) = clean_summary(summary) else {
                return false;
            };
            self.summary = Some(summary);
        }
        true
    }
}

pub(super) struct Group {
    pub(super) display: Display,
    scope: Option<Scope>,
    reminded_revision: Option<u64>,
}

impl Group {
    pub(super) fn retained_bytes(&self) -> usize {
        self.display.group_id.len()
            + self.display.summary.as_ref().map_or(0, String::len)
            + self.scope.as_ref().map_or(0, |scope| {
                scope.history.id().len()
                    + scope.provider.len()
                    + scope.subject.len()
                    + scope.agent.as_ref().map_or(0, String::len)
                    + scope
                        .thread
                        .as_ref()
                        .map_or(0, |(session, thread)| session.len() + thread.len())
            })
    }

    pub(super) fn editable(&self) -> bool {
        self.scope.is_some()
    }

    pub(super) fn restored(display: Display) -> Self {
        Self {
            display,
            scope: None,
            reminded_revision: None,
        }
    }
}

pub(super) struct Call {
    connection: u64,
    request_id: u64,
    pub(super) event_id: u64,
}

fn valid_id(id: &str) -> bool {
    id.strip_prefix("tool_group_").is_some_and(|suffix| {
        suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

fn new_id() -> Option<String> {
    let mut bytes = [0u8; 16];
    crate::platform::random_bytes(&mut bytes).ok()?;
    Some(format!(
        "tool_group_{}",
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

fn clean_summary(text: &str) -> Result<String, String> {
    if text.is_empty()
        || text.len() > MAX_SUMMARY_BYTES
        || text.chars().count() > MAX_SUMMARY_CHARS
        || text
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}'))
        || text.trim().is_empty()
    {
        return Err("tool_summary_invalid: use 1–120 plain characters, at most 512 UTF-8 bytes, on one line".into());
    }
    let safe = crate::audit::event(Value::String(text.trim().to_owned()));
    let text = safe.as_str().unwrap_or_default();
    if text.is_empty() || text.len() > MAX_SUMMARY_BYTES || text.chars().count() > MAX_SUMMARY_CHARS
    {
        return Err("tool_summary_invalid: redacted summary exceeds its display limit".into());
    }
    Ok(text.to_owned())
}

// Only observational primitives may suppress their own display diagnostics.
// A non-counted UI mutation can still emit an image, question or task event.
fn observer(name: &str) -> bool {
    matches!(
        name,
        "ssmcp.ui.tool_summary"
            | "ssmcp.ui.get"
            | "ssmcp.client.capabilities"
            | "ssmcp.resources.list"
            | "ssmcp.resources.templates"
    )
}

fn counted(name: &str) -> bool {
    !name.starts_with("ssmcp.ui.")
        && !matches!(
            name,
            "ssmcp.client.capabilities"
                | "ssmcp.resources.list"
                | "ssmcp.resources.templates"
                | "ssmcp.resources.read"
        )
}

impl EventView {
    pub(in crate::client::terminal) fn summary_call_started(
        &mut self,
        source: &Source,
        name: &str,
    ) {
        if !counted(name) {
            if observer(name)
                && let Some(call) = source.call.as_ref()
            {
                let identity = (call.connection(), call.request_id());
                if !self.summary_observers.contains(&identity) {
                    self.summary_observers.push_back(identity);
                    if self.summary_observers.len() > MAX_EVENTS {
                        self.summary_observers.pop_front();
                    }
                }
            }
            return;
        }
        let Ok(scope) = Scope::capture(source) else {
            return;
        };
        let Some(call) = source.call.as_ref() else {
            return;
        };
        if self.summary_calls.iter().any(|item| {
            item.connection == call.connection() && item.request_id == call.request_id()
        }) {
            return;
        }
        let existing = self.active_summary_group.as_ref().and_then(|id| {
            self.summary_groups.iter().position(|group| {
                group.display.group_id == *id && group.scope.as_ref() == Some(&scope)
            })
        });
        let index = if let Some(index) = existing {
            index
        } else {
            let Some(id) = new_id() else { return };
            self.active_summary_group = Some(id.clone());
            self.summary_groups.push_back(Group {
                display: Display {
                    group_id: id,
                    ..Display::default()
                },
                scope: Some(scope),
                reminded_revision: None,
            });
            self.summary_groups.len() - 1
        };
        let group = &mut self.summary_groups[index];
        group.display.call_count = group.display.call_count.saturating_add(1);
        let group_id = group.display.group_id.clone();

        // Every accepted native dispatch has one logical row, including primitives
        // without their own output event. Native lifecycle updates merge into it.
        let mut meta = Meta {
            kind: Kind::Tool,
            started: true,
            status: Some(Status::Running),
            ..Meta::default()
        };
        meta.set_source(source.display());
        self.ingest_inner(format!("[RUN] {name}\n").as_bytes(), meta);
        if let Some(event) = self.events.back_mut() {
            event.tool_group_id = Some(group_id);
            self.summary_calls.push_back(Call {
                connection: call.connection(),
                request_id: call.request_id(),
                event_id: event.id,
            });
        }
        self.prune();
    }

    pub(in crate::client::terminal) fn summary_call_finished(
        &mut self,
        source: &Source,
        success: bool,
    ) {
        let Some(call) = source.call.as_ref() else {
            return;
        };
        let Some(id) = self
            .summary_calls
            .iter()
            .find(|item| {
                item.connection == call.connection() && item.request_id == call.request_id()
            })
            .map(|item| item.event_id)
        else {
            return;
        };
        if let Some(event) = self.events.iter_mut().find(|event| event.id == id) {
            // A completed dispatch can leave an independent command running.
            // Native exits, cancellation and signals remain the authoritative state.
            if event
                .meta
                .status
                .is_none_or(|state| matches!(state, Status::Running | Status::Done))
                && (!success || !event.meta.command)
            {
                event.meta.status = Some(if success {
                    Status::Done
                } else {
                    Status::Failed
                });
                event.cache = None;
            }
        }
    }

    pub(super) fn summary_observer_event(&self, meta: &Meta) -> bool {
        meta.kind != Kind::Conversation
            && meta.scope.as_ref().is_some_and(|call| {
                self.summary_observers
                    .contains(&(call.connection(), call.request_id()))
            })
    }

    pub(super) fn summary_event_for(&self, meta: &Meta) -> Option<usize> {
        if meta.kind == Kind::Conversation || meta.read_bookkeeping {
            return None;
        }
        let call = meta.scope.as_ref()?;
        let id = self
            .summary_calls
            .iter()
            .find(|item| {
                item.connection == call.connection() && item.request_id == call.request_id()
            })?
            .event_id;
        self.events
            .iter()
            .position(|event| event.id == id && event.meta.same_source(meta) && !event.historical)
    }

    pub(super) fn prune_summary_groups(&mut self) {
        self.summary_calls
            .retain(|call| self.events.iter().any(|event| event.id == call.event_id));
        while self.summary_calls.len() > MAX_EVENTS {
            self.summary_calls.pop_front();
        }
        self.summary_groups.retain(|group| {
            self.active_summary_group.as_ref() == Some(&group.display.group_id)
                || self
                    .events
                    .iter()
                    .any(|event| event.tool_group_id.as_ref() == Some(&group.display.group_id))
        });
        while self.summary_groups.len() > MAX_EVENTS {
            self.summary_groups.pop_front();
        }
    }

    pub(in crate::client::terminal) fn tool_summary_list(
        &self,
        source: &Source,
    ) -> Result<Value, String> {
        let scope = Scope::capture(source)?;
        let retained = self
            .summary_groups
            .iter()
            .filter(|group| group.scope.as_ref() == Some(&scope))
            .count();
        let mut groups = self
            .summary_groups
            .iter()
            .rev()
            .filter(|group| group.scope.as_ref() == Some(&scope))
            .take(32)
            .map(|group| self.summary_receipt(group))
            .collect::<Vec<_>>();
        groups.reverse();
        Ok(
            json!({"schema_version":1,"groups":groups,"threshold":THRESHOLD,
            "has_more":retained > 32,"truncated":retained > 32}),
        )
    }

    pub(in crate::client::terminal) fn tool_summary_set(
        &mut self,
        source: &Source,
        id: &str,
        text: &str,
        expected_revision: Option<u64>,
    ) -> Result<Value, String> {
        let scope = Scope::capture(source)?;
        let text = clean_summary(text)?;
        let index = self
            .summary_groups
            .iter()
            .position(|group| group.display.group_id == id && group.scope.as_ref() == Some(&scope))
            .ok_or("tool_summary_not_found: no writable group in this call scope")?;
        let group = &mut self.summary_groups[index];
        if expected_revision.is_some_and(|revision| revision != group.display.revision) {
            return Err("tool_summary_revision_conflict: list the group before updating it".into());
        }
        let duplicate = group.display.summary.as_deref() == Some(text.as_str())
            && group.display.covered_call_count == group.display.call_count;
        if !duplicate {
            group.display.revision = group
                .display
                .revision
                .checked_add(1)
                .ok_or("tool_summary_revision_exhausted")?;
            group.display.summary = Some(text);
            group.display.covered_call_count = group.display.call_count;
            group.reminded_revision = None;
        }
        let mut receipt = self.summary_receipt(&self.summary_groups[index]);
        receipt["accepted"] = json!(true);
        receipt["duplicate"] = json!(duplicate);
        self.prune();
        Ok(receipt)
    }

    fn summary_receipt(&self, group: &Group) -> Value {
        let mut value = serde_json::to_value(&group.display).expect("bounded summary serializes");
        value["schema_version"] = json!(1);
        value["needs_summary"] = json!(group.display.needs_summary());
        value["summary_editable"] = json!(group.scope.is_some());
        value["shown"] = json!(self.tool_groups().iter().any(|visible| {
            visible
                .info
                .as_ref()
                .is_some_and(|info| info.group_id == group.display.group_id)
        }));
        value
    }

    pub(in crate::client::terminal) fn take_tool_summary_reminder(
        &mut self,
        source: &Source,
    ) -> Option<Value> {
        let scope = Scope::capture(source).ok()?;
        let group = self.summary_groups.iter_mut().rev().find(|group| {
            group.scope.as_ref() == Some(&scope)
                && group.display.needs_summary()
                && group.reminded_revision != Some(group.display.revision)
        })?;
        group.reminded_revision = Some(group.display.revision);
        Some(json!({
            "schema_version":1,"group_id":group.display.group_id,
            "revision":group.display.revision,"call_count":group.display.call_count,
            "covered_call_count":group.display.covered_call_count,
            "threshold":THRESHOLD,"tool":"ssmcp.ui.tool_summary"
        }))
    }
}

#[cfg(all(test, unix))]
mod tests;
