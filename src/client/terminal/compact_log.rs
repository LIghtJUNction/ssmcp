//! Bounded, foldable activity display. The original transcript remains separate.
use super::{PanelTone, panel};
use serde_json::Value;
use std::collections::VecDeque;
use unicode_width::UnicodeWidthChar;

const MAX_BLOCKS: usize = 64;
const MAX_BYTES: usize = 128 * 1024;
const MAX_DETAIL: usize = 8 * 1024;

#[derive(Default)]
pub(super) struct CompactLog {
    blocks: VecDeque<Block>,
    next: u64,
    selected: Option<u64>,
    offset: usize,
    follow: bool,
    reveal: bool,
    hits: Vec<u64>,
    query: String,
    level: u8,
}

struct Block {
    id: u64,
    key: Option<String>,
    alias: Option<String>,
    call: Option<String>,
    title: String,
    status: String,
    text: String,
    expanded: bool,
    permanent: bool,
    command: bool,
    failed: bool,
    trimmed: bool,
}

impl CompactLog {
    pub(super) fn notice(&mut self, bytes: &[u8]) {
        self.record("client", &Value::Null, bytes, true);
    }

    pub(super) fn event(&mut self, label: &str, event: &Value, bytes: &[u8]) {
        self.record(label, event, bytes, false);
    }

    fn record(&mut self, label: &str, event: &Value, bytes: &[u8], notice: bool) {
        if event["rpc_correlation"] == true && event["command_rpc_completed"] == true {
            return;
        }
        let text = plain(bytes);
        let kind = event["type"].as_str().unwrap_or("");
        let rpc = event
            .get("request")
            .or_else(|| event.get("response"))
            .unwrap_or(event);
        let params = rpc["params"].get("arguments").unwrap_or(&rpc["params"]);
        let job = event["job_id"]
            .as_str()
            .or_else(|| event["process_id"].as_str())
            .or_else(|| {
                event["id"]
                    .as_str()
                    .filter(|_| kind.contains("exec.") || kind == "process_output")
            });
        let process = event.pointer("/params/processId").and_then(Value::as_str);
        let key = job
            .map(|id| format!("{label}\0job\0{id}"))
            .or_else(|| {
                rpc.get("id")
                    .filter(|id| !id.is_null())
                    .map(|id| format!("{label}\0rpc\0{id}"))
            })
            .or_else(|| {
                event
                    .get("call_id")
                    .map(|id| format!("{label}\0call\0{id}"))
            })
            .or_else(|| process.map(|id| format!("{label}\0process\0{id}")))
            .filter(|key| key.len() <= 1024);
        let alias = params["processId"]
            .as_str()
            .filter(|id| id.len() <= 1000)
            .map(|id| format!("{label}\0process\0{id}"));
        let call = event
            .get("harness_call_id")
            .map(|id| format!("{label}\0call\0{id}"))
            .filter(|key| key.len() <= 1024);
        let old = key.as_ref().and_then(|key| {
            self.blocks.iter().rposition(|block| {
                block.key.as_ref() == Some(key)
                    || block.alias.as_ref() == Some(key)
                    || block.call.as_ref() == Some(key)
            })
        });
        if text.trim().is_empty() && old.is_none() {
            return;
        }
        let diff = matches!(kind, "file.diff" | "ssmcp.file.diff")
            || (kind == "ssmcp.filechange" && event["diff"].as_str().is_some());
        let result = rpc["result"]
            .get("structuredContent")
            .unwrap_or(&rpc["result"]);
        let exit = event["exit_code"]
            .as_i64()
            .or_else(|| result["exit_code"].as_i64())
            .or_else(|| result["exitCode"].as_i64());
        let failed = !rpc["error"].is_null()
            || event["timed_out"] == true
            || result["timed_out"] == true
            || result["timedOut"] == true
            || result["isError"] == true
            || rpc["result"]["isError"] == true
            || result["success"] == false
            || event["success"] == false
            || exit.is_some_and(|code| code != 0)
            || event["exit_signal"].as_i64().is_some();
        let completed = kind.ends_with(".exited")
            || kind.ends_with(".finished")
            || kind.ends_with(".completed")
            || rpc.get("result").is_some()
            || rpc.get("error").is_some();
        let running = kind.ends_with(".started")
            || (rpc.get("id").is_some_and(|id| !id.is_null())
                && rpc
                    .get("method")
                    .is_some_and(|v| !v.as_str().unwrap_or("").starts_with("notifications/")));
        let status = if event["cancelled"] == true {
            "cancelled".into()
        } else if event["timed_out"] == true
            || result["timed_out"] == true
            || result["timedOut"] == true
        {
            "timeout".into()
        } else if let Some(exit) = exit {
            format!("exit={exit}")
        } else if failed {
            "failed".into()
        } else if result["running"] == true {
            "running".into()
        } else if completed {
            "done".into()
        } else if running {
            "running".into()
        } else {
            String::new()
        };
        let command = job.is_some()
            || process.is_some()
            || params.get("command").is_some()
            || params.get("cmd").is_some()
            || rpc["method"] == "command/exec";
        let old = old.filter(|&index| !running || self.blocks[index].status == "running");
        let index = if let Some(index) = old {
            index
        } else {
            self.next = self.next.wrapping_add(1);
            let title = text
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("Tool output");
            let title = title
                .trim()
                .strip_prefix('[')
                .and_then(|v| v.split_once(']'))
                .map_or(title, |(_, rest)| rest);
            self.blocks.push_back(Block {
                id: self.next,
                key,
                alias,
                call,
                title: title.trim().chars().take(240).collect(),
                status: String::new(),
                text: String::new(),
                expanded: notice || diff,
                permanent: notice || diff,
                command,
                failed: false,
                trimmed: false,
            });
            if self.follow || self.selected.is_none() {
                self.selected = Some(self.next);
                self.follow = true;
            }
            self.blocks.len() - 1
        };
        let block = &mut self.blocks[index];
        if !status.is_empty() {
            block.status = status;
        }
        block.failed |= failed;
        block.command |= command;
        block.permanent |= diff;
        if diff {
            block.expanded = true;
        }
        if text.len() >= MAX_DETAIL {
            let mut start = text.len() - MAX_DETAIL;
            while !text.is_char_boundary(start) {
                start += 1;
            }
            block.text = text[start..].to_owned();
            block.trimmed = true;
        } else {
            let mut discard = block
                .text
                .len()
                .saturating_add(text.len())
                .saturating_sub(MAX_DETAIL);
            while !block.text.is_char_boundary(discard) {
                discard += 1;
            }
            if discard > 0 {
                block.text.drain(..discard);
                block.trimmed = true;
            }
            block.text.push_str(&text);
        }
        while self.blocks.len() > MAX_BLOCKS || self.retained_bytes() > MAX_BYTES {
            self.blocks.pop_front();
        }
        if !self.blocks.iter().any(|b| Some(b.id) == self.selected) {
            self.selected = self.blocks.front().map(|b| b.id);
        }
    }

    fn retained_bytes(&self) -> usize {
        self.blocks
            .iter()
            .map(|b| {
                b.text.len()
                    + b.title.len()
                    + b.status.len()
                    + b.key.as_ref().map_or(0, String::len)
                    + b.alias.as_ref().map_or(0, String::len)
                    + b.call.as_ref().map_or(0, String::len)
            })
            .sum()
    }

    fn matches(block: &Block, query: &str, level: u8) -> bool {
        (level != 1 || block.command)
            && (level != 2 || block.failed)
            && (query.is_empty()
                || block.text.to_lowercase().contains(query)
                || block.title.to_lowercase().contains(query))
    }

    pub(super) fn filter(&mut self, query: &str, level: u8) {
        self.query = query.to_lowercase();
        self.level = level;
        if !self
            .blocks
            .iter()
            .any(|b| Some(b.id) == self.selected && Self::matches(b, &self.query, level))
        {
            self.selected = self
                .blocks
                .iter()
                .find(|b| Self::matches(b, &self.query, level))
                .map(|b| b.id);
            self.reveal = true;
        }
    }

    pub(super) fn select(&mut self, delta: isize) {
        let visible = self
            .blocks
            .iter()
            .filter(|b| Self::matches(b, &self.query, self.level))
            .map(|b| b.id)
            .collect::<Vec<_>>();
        if visible.is_empty() {
            return;
        }
        let current = visible
            .iter()
            .position(|id| Some(*id) == self.selected)
            .unwrap_or(visible.len() - 1);
        let index = current.saturating_add_signed(delta).min(visible.len() - 1);
        self.selected = Some(visible[index]);
        self.follow = delta == isize::MAX;
        self.reveal = true;
    }

    pub(super) fn scroll(&mut self, delta: isize) {
        self.offset = self.offset.saturating_add_signed(delta);
        self.follow = false;
        self.reveal = false;
    }

    pub(super) fn toggle(&mut self) {
        if let Some(block) = self.blocks.iter_mut().find(|b| Some(b.id) == self.selected)
            && !block.permanent
        {
            block.expanded = !block.expanded;
            self.reveal = true;
            self.follow = false;
        }
    }

    pub(super) fn collapse(&mut self) {
        if let Some(block) = self.blocks.iter_mut().find(|b| Some(b.id) == self.selected)
            && !block.permanent
        {
            block.expanded = false;
            self.reveal = true;
        }
    }

    pub(super) fn click(&mut self, row: usize) {
        if let Some(&id) = self.hits.get(row) {
            self.selected = Some(id);
            self.toggle();
        }
    }

    pub(super) fn rows(
        &mut self,
        width: u16,
        height: u16,
        query: &str,
        level: u8,
        focused: bool,
    ) -> Vec<(String, PanelTone)> {
        let width = usize::from(width);
        let height = usize::from(height);
        self.filter(query, level);
        let query = &self.query;
        let mut rows = Vec::new();
        let mut selected_row = None;
        for block in &self.blocks {
            if !Self::matches(block, query, level) {
                continue;
            }
            let selected = Some(block.id) == self.selected;
            if selected {
                selected_row = Some(rows.len());
            }
            let tone = if block.failed {
                PanelTone::Error
            } else if block.status == "running" {
                PanelTone::Accent
            } else if focused && selected {
                PanelTone::Heading
            } else {
                PanelTone::Normal
            };
            if !block.permanent {
                let marker = if block.expanded || !query.is_empty() {
                    "▾"
                } else {
                    "▸"
                };
                let status = if block.status.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", block.status)
                };
                rows.push((
                    block.id,
                    format!(
                        "{} {marker}{status} {}",
                        if focused && selected { "›" } else { " " },
                        block.title
                    ),
                    tone,
                ));
            }
            if block.expanded || !query.is_empty() {
                if block.trimmed {
                    rows.push((
                        block.id,
                        "  … earlier details are no longer retained in this view".into(),
                        PanelTone::Muted,
                    ));
                }
                for (line_index, line) in block.text.lines().enumerate() {
                    let tone = if block.permanent && line.trim_start().starts_with('+') {
                        PanelTone::Success
                    } else if block.permanent && line.trim_start().starts_with('-') {
                        PanelTone::Error
                    } else {
                        PanelTone::Normal
                    };
                    let line = if let Some((end, label, _)) = super::chat_heading(line) {
                        format!("{label}{}", &line[end..])
                    } else {
                        line.to_owned()
                    };
                    for line in wrap(&line, width.saturating_sub(2).max(1)) {
                        rows.push((
                            block.id,
                            format!(
                                "{} {line}",
                                if block.permanent && selected && focused && line_index == 0 {
                                    "›"
                                } else {
                                    " "
                                }
                            ),
                            tone,
                        ));
                    }
                }
            }
        }
        let max = rows.len().saturating_sub(height);
        if self.follow {
            self.offset = max;
        } else if self.reveal
            && let Some(row) = selected_row
        {
            if row < self.offset || row >= self.offset.saturating_add(height) {
                self.offset = row.min(max);
            }
        }
        self.reveal = false;
        self.offset = self.offset.min(max);
        self.hits.clear();
        let mut visible = Vec::new();
        for (id, text, tone) in rows.into_iter().skip(self.offset).take(height) {
            self.hits.push(id);
            visible.push((text, tone));
        }
        if visible.is_empty() {
            visible.push((
                "  No matching activity · /raw opens the original transcript".into(),
                PanelTone::Muted,
            ));
        }
        visible
    }
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut cells = 0;
    for c in text.chars() {
        let size = UnicodeWidthChar::width(c).unwrap_or(0);
        if cells + size > width && !row.is_empty() {
            rows.push(std::mem::take(&mut row));
            cells = 0;
        }
        row.push(c);
        cells += size;
    }
    rows.push(row);
    rows
}

/// Renderer output contains presentation CSI sequences, not terminal payloads.
/// Strip those sequences before presenting bounded text in an alternate view.
fn plain(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut chars = text.chars();
    let mut output = String::new();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    let mut escape = false;
                    for c in chars.by_ref() {
                        if c == '\u{7}' || (escape && c == '\\') {
                            break;
                        }
                        escape = c == '\u{1b}';
                    }
                }
                _ => {}
            }
        } else if c == '\n' {
            output.push('\n');
        } else if c == '\t' {
            output.push_str("    ");
        } else if !c.is_control() {
            output.push(c);
        }
    }
    // Preserve line boundaries while applying the same redaction/sanitization as
    // the rest of the human UI. No raw event payload is retained here.
    output
        .split('\n')
        .map(panel::safe_text)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn visible(log: &mut CompactLog) -> String {
        log.rows(100, 40, "", 0, true)
            .into_iter()
            .map(|r| r.0)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn commands_fold_and_finish_in_place_without_losing_details() {
        let mut log = CompactLog::default();
        log.event(
            "shell",
            &json!({"type":"ssmcp.exec.started","job_id":"one","command":["cargo","test"]}),
            b"[RUN] cargo test\n  cwd /project\n",
        );
        log.event(
            "shell",
            &json!({"type":"ssmcp.exec.stdout","job_id":"one"}),
            b"unique output\n",
        );
        log.event(
            "shell",
            &json!({"type":"ssmcp.exec.exited","job_id":"one","exit_code":7}),
            b"[ERR] completed\n",
        );
        let text = visible(&mut log);
        assert_eq!(log.blocks.len(), 1);
        assert!(
            text.contains("cargo test") && text.contains("exit=7"),
            "{text}"
        );
        assert!(!text.contains("unique output"));
        log.toggle();
        assert!(visible(&mut log).contains("unique output"));
        log.collapse();
        assert!(!visible(&mut log).contains("unique output"));
    }

    #[test]
    fn interleaved_jobs_and_provider_ids_do_not_merge() {
        let mut log = CompactLog::default();
        for source in ["a", "b"] {
            log.event(
                source,
                &json!({"type":"ssmcp.exec.started","job_id":"1"}),
                source.as_bytes(),
            );
        }
        log.event(
            "a",
            &json!({"type":"ssmcp.exec.exited","job_id":"1","exit_code":0}),
            b"done",
        );
        assert_eq!(log.blocks.len(), 2);
        assert_eq!(log.blocks[0].status, "exit=0");
        assert_eq!(log.blocks[1].status, "running");
    }

    #[test]
    fn file_diffs_remain_expanded_and_escape_sequences_are_not_replayed() {
        let mut log = CompactLog::default();
        log.event(
            "fs",
            &json!({"type":"ssmcp.file.diff"}),
            b"\x1b[31m-old\x1b[0m\n+new\n",
        );
        log.toggle();
        log.collapse();
        let text = visible(&mut log);
        assert!(text.contains("-old") && text.contains("+new"), "{text}");
        assert!(!text.contains('\u{1b}'));
    }

    #[test]
    fn output_and_entry_retention_are_bounded_without_evicting_current_title() {
        let mut log = CompactLog::default();
        for id in 0..100 {
            log.event(
                "shell",
                &json!({"type":"ssmcp.exec.started","job_id":id.to_string()}),
                b"[RUN] command\n",
            );
            log.event(
                "shell",
                &json!({"type":"ssmcp.exec.stdout","job_id":id.to_string()}),
                "中".repeat(10_000).as_bytes(),
            );
        }
        assert!(log.retained_bytes() <= MAX_BYTES && log.blocks.len() <= MAX_BLOCKS);
        assert_eq!(log.blocks.back().unwrap().title, "command");
        assert!(log.blocks.back().unwrap().trimmed);
        log.toggle();
        assert!(visible(&mut log).contains("no longer retained"));
    }

    #[test]
    fn filters_search_collapsed_details_and_keep_failure_status() {
        let mut log = CompactLog::default();
        log.event(
            "shell",
            &json!({"type":"ssmcp.exec.started","job_id":"a"}),
            b"cargo test\n",
        );
        log.event(
            "shell",
            &json!({"type":"ssmcp.exec.stderr","job_id":"a"}),
            b"needle\n",
        );
        log.event(
            "shell",
            &json!({"type":"ssmcp.exec.exited","job_id":"a","exit_code":2}),
            b"failed\n",
        );
        let text = log.rows(100, 10, "needle", 2, false);
        assert!(text.iter().any(|r| r.0.contains("exit=2")));
        assert!(!text.iter().any(|r| r.0 == "needle"));
    }
    #[test]
    fn harness_failure_finishes_the_correlated_rpc_and_output_is_not_a_new_request() {
        let mut log = CompactLog::default();
        log.event(
            "codex",
            &json!({"type":"ssmcp.request","harness_call_id":321,
            "request":{"id":"rpc-321","method":"command/exec","params":{"processId":"check-9"}}}),
            b"[RUN] fixture\n",
        );
        log.event(
            "codex",
            &json!({"method":"command/exec/outputDelta",
            "params":{"processId":"check-9"}}),
            b"working",
        );
        log.event(
            "codex",
            &json!({"type":"ssmcp.harness.tool.completed",
            "call_id":321,"success":false,"rpc_correlation":true,"command_rpc_completed":false}),
            b"[ERR] failed\n",
        );
        assert_eq!(log.blocks.len(), 1);
        assert_eq!(log.blocks[0].status, "failed");
        assert!(log.blocks[0].text.contains("working"));
    }

    #[test]
    fn wrapper_completion_preserves_exit_code_and_reused_ids_start_new_blocks() {
        let mut log = CompactLog::default();
        let request = json!({"type":"ssmcp.request","harness_call_id":1,
            "request":{"id":"rpc","method":"command/exec","params":{"processId":"process"}}});
        log.event("codex", &request, b"[RUN] fixture\n");
        log.event(
            "codex",
            &json!({"id":"rpc","result":{"exitCode":7}}),
            b"[ERR] exit=7\n",
        );
        log.event(
            "codex",
            &json!({"type":"ssmcp.harness.tool.completed",
            "call_id":1,"success":true,"rpc_correlation":true,"command_rpc_completed":true}),
            b"",
        );
        assert_eq!(log.blocks[0].status, "exit=7");
        log.event("codex", &request, b"[RUN] next invocation\n");
        assert_eq!(log.blocks.len(), 2);
        assert_eq!(log.blocks[1].status, "running");
    }

    #[test]
    fn filtered_navigation_visits_only_visible_blocks() {
        let mut log = CompactLog::default();
        for (id, title) in [("a", "visible one"), ("b", "hidden"), ("c", "visible two")] {
            log.event(
                "shell",
                &json!({"type":"ssmcp.exec.started","job_id":id}),
                title.as_bytes(),
            );
        }
        log.filter("visible", 0);
        log.select(isize::MIN);
        log.select(1);
        assert_eq!(log.selected, Some(log.blocks[2].id));
        log.toggle();
        assert!(log.blocks[2].expanded);
        assert!(!log.blocks[1].expanded);
    }
}
