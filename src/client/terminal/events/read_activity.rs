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

    pub(super) fn observe(&mut self, event: &Value, read: Option<&mut Read>, bookkeeping: bool) {
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
        } else if !bookkeeping {
            self.interrupt();
        }
    }
}

/// Only the native successful discovery notices are transparent. Their full
/// records remain in Logs; controller/operation/error notices still split runs.
pub(super) fn bookkeeping(event: &Value) -> bool {
    let native = match event["type"].as_str() {
        Some("ssmcp.skill.discovered") => {
            event["tool"] == "ssmcp.skills.list" && event["path"].is_string()
        }
        Some("ssmcp.skill.discovery.completed") => {
            event["tool"] == "ssmcp.skills.view" && event["shown"].is_u64()
        }
        _ => false,
    };
    native
        && event["invoked"] == false
        && [
            "controller",
            "account",
            "account_id",
            "principal",
            "identity",
            "client_id",
            "request_id",
            "command",
            "cmd",
            "request",
            "response",
            "diff",
        ]
        .iter()
        .all(|key| event.get(*key).is_none_or(Value::is_null))
}

#[derive(Clone, Debug)]
pub(super) struct Group {
    pub(super) start: usize,
    pub(super) members: Vec<usize>,
    pub(super) head: u64,
    pub(super) source: String,
    pub(super) expanded: bool,
}

impl Group {
    pub(super) fn contains(&self, index: usize) -> bool {
        self.members.contains(&index)
    }

    pub(super) fn title(&self) -> String {
        format!("Read {} items · {}", self.members.len(), self.source)
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
        let mut members = vec![start];
        let mut end = start + 1;
        while end < events.len() {
            let event = &events[end];
            if read(event).is_some_and(|next| {
                first.identity == next.identity && first.boundary == next.boundary
            }) {
                members.push(end);
            } else if !(event.meta.read_bookkeeping
                && event.meta.kind == Kind::Log
                && !event.meta.command
                && !event.meta.diff
                && !matches!(event.tone, "error" | "warning")
                && event.image.is_none()
                && !event.truncated)
            {
                break;
            }
            end += 1;
        }
        if members.len() >= MIN_READS {
            groups.push(Group {
                start,
                members,
                head: events[start].id,
                source: first.source.clone(),
                expanded: false,
            });
        }
        start = end;
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::super::{EventPosition, EventScope, EventView, Ui, unstyled};
    use super::*;
    use unicode_width::UnicodeWidthStr;

    fn view() -> EventView {
        let mut view = EventView::new();
        view.set_scope(EventScope::Conversation);
        view
    }

    fn ui() -> Ui {
        Ui {
            color: false,
            ..Ui::default()
        }
    }

    fn read(view: &mut EventView, path: &str) -> u64 {
        let event = json!({"type":"ssmcp.file.read","tool":"ssmcp.fs.read_text","path":path});
        view.ingest_event("workspace", &event, b"[READ] workspace clipped-path...\n");
        view.events.back().unwrap().id
    }

    fn controller(view: &mut EventView, id: u64, started: bool) {
        view.ingest_event(
            "controller",
            &json!({
                "type":if started {"ssmcp.controller.started"} else {"ssmcp.controller.finished"},
                "request_id":id
            }),
            b"",
        );
    }

    #[tokio::test]
    async fn read_activity_native_scan_and_interleaved_discovery_keep_audit_records() {
        let (reads, listed) = crate::context::read_activity_tests::native_scan().await;
        assert_eq!(reads.len(), 3);
        assert!(reads.iter().all(|event| event["type"] == "ssmcp.file.read"));
        let discoveries = listed["skills"]
            .as_array()
            .unwrap()
            .iter()
            .map(|skill| {
                // Runtime emits these bookkeeping notices after a successful list.
                json!({"type":"ssmcp.skill.discovered","tool":"ssmcp.skills.list",
                "root":skill["root"],"path":skill["path"],"name":skill["name"],
                "scope":"ssmcp context operation","invoked":false})
            })
            .collect::<Vec<_>>();
        for interleaved in [false, true] {
            let mut view = view();
            controller(&mut view, 1, true);
            let mut ids = Vec::new();
            for (index, event) in reads.iter().enumerate() {
                view.ingest_event("workspace", event, b"[READ] workspace clipped-path...\n");
                ids.push(view.events.back().unwrap().id);
                if interleaved {
                    view.ingest_event("audit", &discoveries[index], b"[SKIL] audit discovered\n");
                }
            }
            if !interleaved {
                for event in &discoveries {
                    view.ingest_event("audit", event, b"[SKIL] audit discovered\n");
                }
            }
            controller(&mut view, 1, false);
            let original = view
                .events
                .iter()
                .map(|event| (event.id, event.text.clone()))
                .collect::<Vec<_>>();
            let snapshot = view.snapshot();
            assert_eq!(snapshot["retained_events"], 6);
            assert_eq!(snapshot["visible_events"], 3);
            assert_eq!(snapshot["visible_cards"], 1);
            assert_eq!(snapshot["read_groups"][0]["count"], 3);
            assert_eq!(snapshot["read_groups"][0]["members"], json!(ids));
            let collapsed = view.render(120, 30, &ui(), true).join("\n");
            assert!(collapsed.contains("Read 3 items · workspace"));
            assert!(!collapsed.contains("discovered"));
            assert!(view.open());
            let expanded = view.render(120, 30, &ui(), true).join("\n");
            for event in &reads {
                assert!(
                    expanded.contains(event["path"].as_str().unwrap()),
                    "{expanded}"
                );
            }
            view.set_scope(EventScope::All);
            assert!(
                view.snapshot()["read_groups"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(view.snapshot()["visible_cards"], 6);
            assert!(
                view.render(120, 30, &ui(), true)
                    .join("\n")
                    .contains("discovered")
            );
            assert_eq!(
                view.events
                    .iter()
                    .map(|event| (event.id, event.text.clone()))
                    .collect::<Vec<_>>(),
                original
            );
        }
    }

    #[test]
    fn read_activity_full_safe_paths_survive_wrapping_search_and_fold() {
        let mut view = view();
        let path = format!(
            ".agents/skills/{}/中文-tail/SKILL.md",
            "deep-directory/".repeat(8)
        );
        let ids = [read(&mut view, "skills/a/SKILL.md"), read(&mut view, &path)];
        let secret = "fixture-path-secret-value";
        let raw = json!({"type":"ssmcp.file.read","path":format!("skills/Authorization: Bearer {secret}/SKILL.md")});
        let before = raw.clone();
        view.ingest_event(
            "workspace",
            &raw,
            b"[READ] workspace safe-summary\nretained-read-detail\n",
        );
        let safe = crate::audit::event(raw.clone());
        assert!(!safe["path"].as_str().unwrap().contains(secret));
        assert_eq!(raw, before);
        let texts = view
            .events
            .iter()
            .map(|event| event.text.clone())
            .collect::<Vec<_>>();
        assert_eq!(view.snapshot()["visible_cards"], 1);
        assert!(view.open());
        for width in [120, 48] {
            let rows = view.render(width, 400, &ui(), true);
            assert!(
                rows.iter()
                    .all(|row| UnicodeWidthStr::width(unstyled(row).as_str()) <= width as usize)
            );
            let compact = rows
                .iter()
                .map(|row| unstyled(row).replace(' ', ""))
                .collect::<String>();
            assert!(compact.contains("中文-tail/SKILL.md"));
            assert!(!compact.contains(secret));
        }
        let expanded = view.render(120, 400, &ui(), true).join("\n");
        assert!(expanded.contains("retained-read-detail"));
        assert_eq!(
            view.events
                .iter()
                .map(|event| event.text.clone())
                .collect::<Vec<_>>(),
            texts
        );
        view.filter("中文-tail", 0);
        assert!(
            view.snapshot()["read_groups"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(view.visible_ids(), vec![ids[1]]);
        assert!(
            view.render(120, 40, &ui(), true)
                .join("\n")
                .contains("中文-tail")
        );
        view.filter("", 0);
        view.select(-isize::MAX);
        assert!(view.back());
        assert_eq!(view.visible_ids(), vec![ids[0]]);
        assert_eq!(view.events.len(), 3);
    }

    #[test]
    fn read_activity_respects_explicit_identity_and_concurrent_controllers() {
        for identity in [
            "root",
            "root_id",
            "controller",
            "account",
            "account_id",
            "principal",
            "identity",
            "client_id",
            "request_id",
        ] {
            let mut view = view();
            for value in ["first", "second"] {
                for index in 0..3 {
                    let mut event =
                        json!({"type":"ssmcp.file.read","path":format!("{value}/{index}")});
                    event[identity] = json!(value);
                    view.ingest_event("workspace", &event, b"[READ] workspace path\n");
                }
            }
            assert_eq!(view.read_groups().len(), 2, "identity {identity}");
            assert!(
                view.read_groups()
                    .iter()
                    .all(|group| group.members.len() == 3)
            );
        }
        let mut view = view();
        for source in ["first-root", "second-root"] {
            for _ in 0..3 {
                view.ingest_event(
                    source,
                    &json!({"type":"ssmcp.file.read","path":"SKILL.md"}),
                    b"[READ] path\n",
                );
            }
        }
        assert_eq!(view.read_groups().len(), 2);
        view.clear();
        controller(&mut view, 1, true);
        for _ in 0..3 {
            read(&mut view, "first/SKILL.md");
        }
        controller(&mut view, 1, false);
        controller(&mut view, 2, true);
        for _ in 0..3 {
            read(&mut view, "second/SKILL.md");
        }
        controller(&mut view, 3, true);
        for _ in 0..3 {
            read(&mut view, "ambiguous/SKILL.md");
        }
        assert_eq!(view.read_groups().len(), 2);
        view.clear();
        for _ in 0..3 {
            read(&mut view, "still-ambiguous/SKILL.md");
        }
        assert!(
            view.read_groups().is_empty(),
            "clearing the display must not forget active callers"
        );
        controller(&mut view, 3, false);
        for _ in 0..3 {
            read(&mut view, "second-again/SKILL.md");
        }
        assert_eq!(view.read_groups().len(), 1);
    }

    #[test]
    fn read_activity_commands_writes_images_errors_and_unknown_logs_break_runs() {
        for (barrier, text) in [
            (
                json!({"type":"exec.started","job_id":"command","command":["true"]}),
                "[RUN] command\n",
            ),
            (
                json!({"type":"ssmcp.file.write","path":"changed"}),
                "[WRITE] changed\n",
            ),
            (
                json!({"type":"ssmcp.file.diff","path":"changed"}),
                "[DIFF] changed\n-old\n+new\n",
            ),
            (json!({"type":"image"}), "[IMAGE] retained image\n"),
            (
                json!({"type":"ssmcp.file.read","path":"image.png","tool":"ssmcp.fs.read_image"}),
                "[READ] image.png\n",
            ),
            (
                json!({"type":"ssmcp.file.read","path":"failed","success":false}),
                "[ERR] failed read\n",
            ),
            (
                json!({"type":"ssmcp.file.read","path":"running","running":true}),
                "[READ] running\n",
            ),
            (
                json!({"type":"ssmcp.file.read","path":"warning"}),
                "[WARN] partial read\n",
            ),
            (
                json!({"type":"unknown.discovery"}),
                "[SKIL] unknown notice\n",
            ),
            (
                json!({"type":"ssmcp.skill.discovered","tool":"ssmcp.skills.list","path":"SKILL.md","invoked":true}),
                "[SKIL] invoked\n",
            ),
            (
                json!({"type":"ssmcp.skill.discovered","tool":"ssmcp.skills.list","path":"SKILL.md","invoked":false,"controller":"another"}),
                "[SKIL] different caller\n",
            ),
            (
                json!({"type":"ssmcp.skill.discovered","tool":"ssmcp.skills.list","path":"SKILL.md","invoked":false,"success":false}),
                "[SKIL] failed\n",
            ),
            (
                json!({"type":"ssmcp.skill.discovered","tool":"ssmcp.skills.list","path":"SKILL.md","invoked":false}),
                "[WARN] discovery warning\n",
            ),
        ] {
            let mut view = view();
            for _ in 0..2 {
                read(&mut view, "before");
            }
            view.ingest_event("workspace", &barrier, text.as_bytes());
            for _ in 0..2 {
                read(&mut view, "after");
            }
            assert!(view.read_groups().is_empty(), "barrier {barrier}");
            assert_eq!(view.events.len(), 5);
            assert_eq!(view.events[2].text, text);
        }
    }

    #[test]
    fn read_activity_navigation_canvas_positions_and_mouse_keep_original_ids() {
        let mut view = view();
        let ids = (0..4)
            .map(|index| read(&mut view, &format!("skills/{index}/SKILL.md")))
            .collect::<Vec<_>>();
        view.ingest_event(
            "shell",
            &json!({"type":"exec.started","job_id":"next","command":["true"]}),
            b"[RUN] shell next true\n",
        );
        let command = view.events.back().unwrap().id;
        assert_eq!(view.visible_ids(), vec![ids[0], command]);
        view.select(-1);
        assert_eq!(view.selected, Some(ids[0]));
        assert!(view.open());
        view.render(48, 20, &ui(), true);
        let row = view
            .hit_rows
            .iter()
            .position(|id| *id == Some(ids[2]))
            .unwrap();
        assert!(view.mouse_select(row as u16));
        assert_eq!(view.selected, Some(ids[2]));
        let expanded_column = view.position();
        assert!(view.back());
        assert_eq!(view.selected, Some(ids[0]));
        assert!(
            !view.mouse_select(row as u16),
            "folding invalidates old row geometry"
        );
        let folded_column = view.position();
        view.restore_position(&expanded_column);
        assert_eq!(view.selected, Some(ids[2]));
        assert_eq!(view.scroll, expanded_column.scroll);
        assert_eq!(view.follow, expanded_column.follow);
        assert!(view.read_groups()[0].expanded);
        view.select(1);
        assert_eq!(view.selected, Some(ids[3]));
        view.restore_position(&folded_column);
        assert!(!view.read_groups()[0].expanded);
        view.select(1);
        assert_eq!(view.selected, Some(command));
        for _ in 0..super::super::MAX_EVENTS {
            view.ingest(b"[INFO] later background event\n");
        }
        view.restore_position(&expanded_column);
        assert!(view.read_expanded.is_empty());
        assert!(!view.events.iter().any(|event| ids.contains(&event.id)));
    }

    #[test]
    fn read_activity_new_group_preserves_paused_selection_and_scroll() {
        let mut view = view();
        read(&mut view, "skills/first/SKILL.md");
        let second = read(&mut view, "skills/second/SKILL.md");
        view.select(0);
        view.render(48, 3, &ui(), true);
        assert_eq!(view.selected, Some(second));
        read(&mut view, "skills/third/SKILL.md");
        assert_eq!(view.selected, Some(second));
        assert!(view.read_groups()[0].expanded);
        let mut position = view.position();
        position.scroll = 1;
        position.follow = false;
        position.reveal = false;
        view.restore_position(&EventPosition::default());
        view.restore_position(&position);
        view.render(48, 3, &ui(), true);
        assert_eq!(view.selected, Some(second));
        assert_eq!(view.scroll, 1);
        assert!(!view.follow);
    }

    #[test]
    fn read_activity_preview() {
        let mut view = view();
        view.ingest_event("agent", &json!({"type":"ssmcp.message","text":"Checking available skills before updating the portal."}), b"");
        for index in 0..24 {
            let path = format!(".agents/skills/design-review-{index:02}/SKILL.md");
            read(&mut view, &path);
            view.ingest_event("audit", &json!({"type":"ssmcp.skill.discovered","tool":"ssmcp.skills.list","path":path,"invoked":false}), b"[SKIL] audit discovered\n");
        }
        view.ingest_event("shell", &json!({"type":"exec.exited","job_id":"check","command":["python3","-c","..."],"exit_code":0}), b"[OK] shell check python3 -c [exit=0]\nChecked portal assets.\n");
        view.ingest_event(
            "workspace",
            &json!({"type":"ssmcp.file.write","path":"portal.css"}),
            b"[WRITE] workspace portal.css\n",
        );
        view.ingest_event(
            "workspace",
            &json!({"type":"ssmcp.file.diff","path":"portal.css"}),
            b"[DIFF] portal.css\n@@ -1 +1 @@\n-padding: 8px;\n+padding: 24px;\n",
        );
        view.ingest_event(
            "workspace",
            &json!({"type":"ssmcp.file.read","path":"missing.css","success":false}),
            b"[ERR] missing.css: file not found\n",
        );
        assert_eq!(view.read_groups().len(), 1);
        assert_eq!(view.read_groups()[0].members.len(), 24);
        let Some(directory) = std::env::var_os("SSMCP_READ_ACTIVITY_PREVIEW_DIR") else {
            return;
        };
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        for width in [120, 48] {
            for expanded in [false, true] {
                let group = view.read_groups()[0].clone();
                view.set_read_expanded(&group, expanded);
                view.selected = Some(group.head);
                view.follow = false;
                view.scroll = 0;
                view.reveal = false;
                let color = Ui {
                    color: true,
                    ..Ui::default()
                };
                let ansi = view.render(width, 36, &color, true).join("\n") + "\n";
                let state = if expanded { "expanded" } else { "collapsed" };
                let stem = format!("read-activity-{width}-{state}");
                std::fs::write(directory.join(format!("{stem}.ansi")), &ansi).unwrap();
                std::fs::write(directory.join(format!("{stem}.txt")), unstyled(&ansi)).unwrap();
                std::fs::write(
                    directory.join(format!("{stem}.json")),
                    serde_json::to_vec_pretty(&view.snapshot()).unwrap(),
                )
                .unwrap();
            }
        }
    }
}
