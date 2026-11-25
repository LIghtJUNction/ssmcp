//! Agent-authored labels for trusted native call groups. Display IDs and caller
//! names never authorize edits; capabilities belong to the current process.
use super::{EventView, Kind, MAX_EVENTS, Meta, Status};
use crate::client::interface::history::HistoryOwner;
use crate::runtime::call_scope::Source;
use base64::{Engine as _, engine::general_purpose::STANDARD};
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
}

pub(super) struct Group {
    pub(super) display: Display,
    scope: Option<Scope>,
    // The revision and native request that reserved its reminder.
    reminded: Option<(u64, u64)>,
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
}

pub(super) struct Call {
    connection: u64,
    request_id: u64,
    pub(super) event_id: u64,
}

fn new_id() -> Option<String> {
    let mut bytes = [0u8; 16];
    crate::platform::random_bytes(&mut bytes).ok()?;
    Some(format!("tool_group_{}", crate::hex::encode(bytes)))
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
            | "ssmcp.ui.history"
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

struct ExecReadDetail {
    text: String,
    truncated: bool,
    tone: &'static str,
}

/// The compact row is generated from a validated native read result. Its marker
/// separates the projection from retained diagnostics and untrusted output text.
pub(super) fn exec_read_preview(event: &super::Event, width: usize) -> Option<Vec<String>> {
    if event.meta.command || !event.meta.keys.is_empty() || event.summary != "[RUN] ssmcp.exec.read"
    {
        return None;
    }
    let body = event
        .text
        .strip_prefix("[RUN] ssmcp.exec.read\n[RESULT] ")?;
    let summary = body.lines().next()?;
    let shown = super::clip_plain(summary, width);
    let clipped_warning = summary.contains(" · TRUNCATED ") && !shown.contains("TRUNCATED");
    let warning = match (event.truncated, clipped_warning) {
        (true, true) => Some("TRUNCATED details/output"),
        (true, false) => Some("TRUNCATED details"),
        (false, true) => Some("TRUNCATED output"),
        (false, false) => None,
    };
    let mut rows = vec![shown];
    if let Some(warning) = warning {
        rows.push(super::clip_plain(warning, width));
    }
    // Tool windows replace this final hint with their shared footer. Warnings
    // must stay in the preceding result rows, even when the summary is clipped.
    rows.push(super::clip_plain("… Click expand", width));
    Some(rows)
}

fn exec_read_process(
    read: &Value,
    running: bool,
    exit_code: Option<i64>,
) -> (String, &'static str) {
    let snapshot = &read["snapshot"];
    if running {
        return ("Running".into(), "foreground");
    }
    let reason = if snapshot["timed_out"] == true {
        Some(("Timed out", "warning"))
    } else if snapshot["cancelled"] == true {
        Some(("Cancelled", "warning"))
    } else if snapshot["error"].as_str().is_some() || snapshot["state"] == "failed" {
        Some(("Process error", "error"))
    } else {
        None
    };
    if let Some((reason, tone)) = reason {
        let suffix = exit_code.map_or_else(String::new, |code| format!(" · exit {code}"));
        return (format!("{reason}{suffix}"), tone);
    }
    if let Some(signal) = snapshot["exit_signal"].as_i64() {
        return (format!("Signal {signal}"), "error");
    }
    match exit_code {
        Some(code) => (
            format!("Exit {code}"),
            if code == 0 { "foreground" } else { "error" },
        ),
        None => ("Exit unknown".into(), "warning"),
    }
}

fn exec_read_amount(read: &Value, stream: &str) -> Option<String> {
    // The plaintext field can contain replacement characters for invalid UTF-8.
    // Count the native bytes, never a cumulative cursor or lossy text length.
    let encoded = read.get(format!("{stream}_base64"))?.as_str()?;
    let bytes = STANDARD.decode(encoded).ok()?.len();
    Some(if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes.is_multiple_of(1024) {
        format!("{} KiB", bytes / 1024)
    } else {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    })
}

fn exec_read_detail(value: &Value) -> Option<ExecReadDetail> {
    if value["isError"] == true {
        return None;
    }
    let read = value.get("structuredContent")?;
    let job = display_id(read.get("job_id")?.as_str()?);
    let running = read.get("running")?.as_bool()?;
    let exit_code = read.get("exit_code")?;
    if !exit_code.is_null() && !exit_code.is_i64() {
        return None;
    }
    let (process, mut tone) = exec_read_process(read, running, exit_code.as_i64());
    let mut metadata = Vec::new();
    let mut streams = Vec::new();
    let mut amounts = Vec::new();
    let mut truncated_streams = Vec::new();
    let mut output_eof = true;
    for stream in ["stdout", "stderr"] {
        let text = read.get(stream)?.as_str()?;
        let cursor = read.get(format!("{stream}_cursor"))?.as_u64()?;
        let eof = read.get(format!("{stream}_eof"))?.as_bool()?;
        let truncated = read.get(format!("{stream}_truncated"))?.as_bool()?;
        output_eof &= eof;
        if truncated {
            truncated_streams.push(stream);
            if tone != "error" {
                tone = "warning";
            }
        }
        // Empty streams have no default row. Their exact state remains below.
        if !text.is_empty() {
            amounts.push(exec_read_amount(read, stream).map_or_else(
                || format!("{stream} read"),
                |amount| format!("{stream} {amount}"),
            ));
        }
        let artifact = read["snapshot"][stream]["artifact"]["id"]
            .as_str()
            .map(display_id)
            .unwrap_or_else(|| "none".into());
        metadata.push(format!(
            "{stream} cursor={cursor} eof={eof} truncated={truncated} artifact={artifact}"
        ));
        // Strip terminal controls before redaction, then retain only this
        // plaintext stream. Never copy encoded bytes or arbitrary snapshot fields.
        let safe = crate::audit::event(Value::String(super::unstyled(text)));
        let text = safe.as_str().unwrap_or_default();
        streams.push(format!(
            "[{}]\n{}\n",
            stream.to_ascii_uppercase(),
            if text.is_empty() { "(empty)" } else { text }
        ));
    }
    let mut compact = vec![process];
    if !truncated_streams.is_empty() {
        // Put the warning before sizes so it survives clipping in narrow panes.
        compact.push(format!("TRUNCATED {}", truncated_streams.join("/")));
    }
    if output_eof {
        compact.push("EOF".into());
    } else if !running {
        compact.push("output pending".into());
    }
    if amounts.is_empty() {
        compact.push("No new output".into());
    } else {
        compact.extend(amounts);
    }
    let snapshot = &read["snapshot"];
    let process_detail = crate::audit::event(json!({
        "state":snapshot["state"],
        "exit_signal":snapshot["exit_signal"],
        "cancelled":snapshot["cancelled"],
        "timed_out":snapshot["timed_out"],
        "error":snapshot["error"].as_str().map(super::unstyled),
    }));
    // Show the actual output first in the bounded tool-window viewport. The
    // retained head and tail keep the compact result and trailing diagnostics.
    let text = format!(
        "[RUN] ssmcp.exec.read\n[RESULT] {}\n\n{}\nDetails\njob={job} running={running} exit_code={exit_code}\nprocess={process_detail}\n{}",
        compact.join(" · "),
        streams.join(""),
        metadata.join("\n")
    );
    let (text, truncated) = super::retained_text(text.as_bytes());
    if truncated && tone != "error" {
        tone = "warning";
    }
    Some(ExecReadDetail {
        text,
        truncated,
        tone,
    })
}

fn display_id(id: &str) -> String {
    id.chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        .take(128)
        .collect()
}

impl EventView {
    pub(in crate::client::terminal) fn summary_call_started(
        &mut self,
        source: &Source,
        name: &str,
    ) {
        self.summary_call_labeled(source, name, name);
    }

    pub(in crate::client::terminal) fn summary_call_labeled(
        &mut self,
        source: &Source,
        name: &str,
        label: &str,
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
                reminded: None,
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
        self.ingest_inner(format!("[RUN] {label}\n").as_bytes(), meta);
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
    ) -> bool {
        let Some(call) = source.call.as_ref() else {
            return false;
        };
        let Some(id) = self
            .summary_calls
            .iter()
            .find(|item| {
                item.connection == call.connection() && item.request_id == call.request_id()
            })
            .map(|item| item.event_id)
        else {
            return false;
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
                return true;
            }
        }
        false
    }

    pub(in crate::client::terminal) fn summary_exec_read_result(
        &mut self,
        source: &Source,
        value: &Value,
    ) -> bool {
        let Some(call) = source.call.as_ref() else {
            return false;
        };
        let Some(id) = self
            .summary_calls
            .iter()
            .find(|item| {
                item.connection == call.connection() && item.request_id == call.request_id()
            })
            .map(|item| item.event_id)
        else {
            return false;
        };
        let Some(event) = self.events.iter_mut().find(|event| {
            event.id == id && !event.historical && event.summary == "[RUN] ssmcp.exec.read"
        }) else {
            return false;
        };
        let Some(detail) = exec_read_detail(value) else {
            return false;
        };
        // Keep the accepted caller's existing row, group and selection. Adding
        // job keys here would merge the reader into the job creator's card.
        event.text = detail.text;
        event.lines = event.text.lines().count().max(1);
        event.truncated = detail.truncated;
        event.tone = detail.tone;
        event.cache = None;
        self.prune();
        true
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
            group.reminded = None;
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

    /// Cancel only the reservation issued to a reply that the client cannot send.
    /// A different request or a newer summary revision keeps its own dedup state.
    pub(in crate::client::terminal) fn restore_tool_summary_reminder(
        &mut self,
        source: &Source,
        reminder: &Value,
    ) -> bool {
        let Ok(scope) = Scope::capture(source) else {
            return false;
        };
        let (Some(id), Some(revision), Some(call)) = (
            reminder["group_id"].as_str(),
            reminder["revision"].as_u64(),
            source.call.as_ref(),
        ) else {
            return false;
        };
        if reminder["schema_version"] != 1 {
            return false;
        }
        let Some(group) = self.summary_groups.iter_mut().find(|group| {
            group.display.group_id == id
                && group.scope.as_ref() == Some(&scope)
                && group.display.revision == revision
                && group.reminded == Some((revision, call.request_id()))
        }) else {
            return false;
        };
        group.reminded = None;
        true
    }

    pub(in crate::client::terminal) fn take_tool_summary_reminder(
        &mut self,
        source: &Source,
    ) -> Option<Value> {
        let scope = Scope::capture(source).ok()?;
        let request_id = source.call.as_ref()?.request_id();
        let group = self.summary_groups.iter_mut().rev().find(|group| {
            group.scope.as_ref() == Some(&scope)
                && group.display.needs_summary()
                && group
                    .reminded
                    .is_none_or(|(revision, _)| revision != group.display.revision)
        })?;
        group.reminded = Some((group.display.revision, request_id));
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
