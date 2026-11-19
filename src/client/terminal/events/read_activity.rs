//! A display projection over retained reads, never a replacement event stream.
use super::{Event, correlation::Kind};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashSet, VecDeque};

const MIN_READS: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Read {
    pub(super) source: String,
    pub(super) path: String,
    identity: [u8; 32],
    boundary: Option<u64>,
}

impl Read {
    pub(super) fn from_event(label: &str, event: &Value) -> Option<Self> {
        if !matches!(
            event["type"].as_str(),
            Some(
                "file.read"
                    | "ssmcp.file.read"
                    | "ssmcp.fileread"
                    | "skill.read"
                    | "ssmcp.skill.read"
            )
        ) || event["tool"] == "ssmcp.fs.read_image"
        {
            return None;
        }
        // Renderer summaries intentionally clip paths. Keep the complete path
        // only after the same audit redaction used by the human transcript.
        let safe = crate::audit::event(event.clone());
        let path = safe["path"].as_str()?;
        let source = crate::audit::event(Value::String(label.to_owned()));
        let source = source.as_str()?;
        if path.is_empty()
            || path.len() > 4096
            || source.len() > 256
            || path.chars().chain(source.chars()).any(char::is_control)
        {
            return None;
        }
        // Identity is compared, never rendered. Distinct explicit roots,
        // accounts or controllers must not share even an anonymous summary.
        let identity = Sha256::digest(
            json!([
                label,
                event["root"],
                event["root_id"],
                event["controller"],
                event["account"],
                event["account_id"],
                event["principal"],
                event["identity"],
                event["client_id"],
                event["request_id"],
            ])
            .to_string()
            .as_bytes(),
        )
        .into();
        Some(Self {
            source: source.to_owned(),
            path: path.to_owned(),
            identity,
            boundary: None,
        })
    }

    pub(super) fn bytes(&self) -> usize {
        self.source.len() + self.path.len() + self.identity.len()
    }

    pub(super) fn details(&self, retained: &str) -> String {
        let text = retained.trim_matches(['\r', '\n']);
        let (first, rest) = text.split_once('\n').unwrap_or((text, ""));
        let plain = super::unstyled(first);
        let heading = super::heading(&plain);
        let summary = heading
            .strip_prefix('[')
            .and_then(|line| line.split_once(']'))
            .is_some_and(|(tag, _)| matches!(tag.trim(), "READ" | "SKIL"));
        let body = if summary { rest } else { text };
        let title = format!("{} {}", self.source, self.path);
        if body.is_empty() {
            title
        } else {
            format!("{title}\n{body}")
        }
    }
}

/// Filesystem events currently have no caller field. Group only within an
/// uninterrupted request interval, and never attribute concurrent callers by
/// guessing which of them emitted an anonymous filesystem event.
#[derive(Default)]
pub(super) struct Boundary {
    sequence: u64,
    active: HashSet<u64>,
}

impl Boundary {
    pub(super) fn interrupt(&mut self) {
        self.sequence = self.sequence.wrapping_add(1);
    }

    pub(super) fn observe(&mut self, event: &Value, read: Option<&mut Read>) {
        let kind = event["type"].as_str().unwrap_or("");
        if let Some(id) = event["request_id"].as_u64() {
            match kind {
                "ssmcp.controller.started" => {
                    self.active.insert(id);
                }
                "ssmcp.controller.finished" => {
                    self.active.remove(&id);
                }
                _ => {}
            }
        }
        if let Some(read) = read {
            read.boundary = (self.active.len() <= 1).then_some(self.sequence);
        } else {
            self.interrupt();
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct Group {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) head: u64,
    pub(super) source: String,
    pub(super) expanded: bool,
}

impl Group {
    pub(super) fn contains(&self, index: usize) -> bool {
        (self.start..self.end).contains(&index)
    }

    pub(super) fn title(&self) -> String {
        format!("Read {} items · {}", self.end - self.start, self.source)
    }
}

fn read(event: &Event) -> Option<&Read> {
    if event.meta.kind != Kind::Tool
        || event.meta.command
        || event.meta.diff
        || event
            .meta
            .status
            .is_some_and(|status| status.failed() || status == super::correlation::Status::Running)
        || matches!(event.tone, "error" | "warning")
        || event.image.is_some()
        || event.truncated
        || event.chat_expanded
    {
        return None;
    }
    event
        .meta
        .read
        .as_ref()
        .filter(|read| read.boundary.is_some())
}

pub(super) fn groups(events: &VecDeque<Event>) -> Vec<Group> {
    let mut groups = Vec::new();
    let mut start = 0;
    while start < events.len() {
        let Some(first) = read(&events[start]) else {
            start += 1;
            continue;
        };
        let mut end = start + 1;
        while end < events.len()
            && read(&events[end]).is_some_and(|next| {
                first.identity == next.identity && first.boundary == next.boundary
            })
        {
            end += 1;
        }
        if end - start >= MIN_READS {
            groups.push(Group {
                start,
                end,
                head: events[start].id,
                source: first.source.clone(),
                expanded: false,
            });
        }
        start = end;
    }
    groups
}
