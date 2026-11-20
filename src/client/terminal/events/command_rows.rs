//! Compact command headings built only from the retained redacted display copy.
use super::{Event, Status, Ui, clip_plain, paint};
use std::borrow::Cow;

impl Event {
    fn command_invocation(&self) -> Cow<'_, str> {
        self.command_source
            .as_ref()
            .and_then(|source| source.text.lines().next())
            .and_then(|line| line.strip_prefix("cmd="))
            .map(Cow::Borrowed)
            .unwrap_or_else(|| Cow::Owned(self.chat_heading()))
    }

    pub(super) fn command_heading_row(&self, width: usize, ui: &Ui) -> String {
        let label = match self.meta.status {
            Some(Status::Running) => "Running",
            Some(Status::Done | Status::Exit(0)) => "Ran",
            Some(Status::Cancelled) => "Cancelled",
            Some(Status::Timeout) => "Timeout",
            Some(Status::Signal(_)) => "Interrupted",
            Some(Status::Exit(_) | Status::Failed) => "Failed",
            None => "Run",
        };
        let history = if self.historical && self.interrupted {
            " · history · connection ended"
        } else if self.historical {
            " · history"
        } else {
            ""
        };
        let status = match self.meta.status {
            Some(Status::Exit(code)) if code != 0 => format!(" · exit={code}"),
            Some(Status::Signal(signal)) => format!(" · signal={signal}"),
            _ => String::new(),
        };
        let heading = clip_plain(
            &format!("{label} {}{status}{history}", self.command_invocation()),
            width,
        );
        let (label, command) = heading.split_once(' ').unwrap_or((&heading, ""));
        let mut row = paint(label, self.chat_tone(false), ui, true);
        if !command.is_empty() {
            row.push(' ');
        }
        let mut lexer = crate::client::highlight::Lexer::for_language("sh");
        let mut program = true;
        let mut word_start = true;
        let mut flag = false;
        for token in lexer.line(command) {
            if token.text.chars().all(char::is_whitespace) {
                program = false;
                word_start = true;
                flag = false;
                row.push_str(token.text);
                continue;
            }
            if word_start {
                flag = token.text.starts_with('-');
                word_start = false;
            }
            let tone = if program {
                "blue"
            } else if flag && token.tone == "foreground" {
                "accent"
            } else {
                token.tone
            };
            row.push_str(&paint(token.text, tone, ui, token.bold));
        }
        row
    }

    pub(super) fn command_preview_hint(&self, rows: usize, visible: usize) -> String {
        let hidden = rows.saturating_sub(visible);
        let prefix = if hidden == 0 {
            "+ Show details".to_owned()
        } else {
            format!(
                "+{hidden} {} · Show details",
                if hidden == 1 { "line" } else { "lines" }
            )
        };
        let partial = if self.truncated
            || self
                .command_source
                .as_ref()
                .is_some_and(|source| source.truncated)
        {
            " · partial"
        } else {
            ""
        };
        format!("{prefix} · Enter{partial}")
    }
}

#[cfg(test)]
mod tests {
    use super::super::{EventScope, EventView, Ui, unstyled};
    use serde_json::json;
    use unicode_width::UnicodeWidthStr;

    fn event(title: &str, command: serde_json::Value) -> EventView {
        let mut view = EventView::new();
        view.set_scope(EventScope::Conversation);
        view.ingest_event(
            "shell",
            &json!({"type":"exec.started","job_id":"readable","title":title,"command":command}),
            format!("[RUN] shell readable {title} started\n").as_bytes(),
        );
        view
    }

    #[test]
    fn actual_redacted_argv_precedes_the_optional_title_at_both_widths() {
        for width in [48, 120] {
            let mut view = event(
                "Inspect repository state",
                json!(["git", "status", "--short"]),
            );
            view.ingest_event(
                "shell",
                &json!({"type":"exec.exited","job_id":"readable","exit_code":0}),
                b"[OK] shell readable exit=0\n",
            );
            let ui = Ui {
                color: false,
                ..Ui::default()
            };
            let rows = view.render(width, 20, &ui, false);
            let text = rows.join("\n");
            assert!(text.contains("Ran git status --short"), "{text}");
            assert!(!text.contains("Inspect repository state"), "{text}");
            assert!(
                text.contains("(no output)") && text.contains("Show details"),
                "{text}"
            );
            assert!(text.contains("Enter"), "{text}");
            assert!(
                rows.iter()
                    .all(|row| UnicodeWidthStr::width(row.as_str()) <= usize::from(width))
            );
            view.selected = Some(view.events[0].id);
            assert!(view.toggle());
            let expanded = view.render(width, 24, &ui, true).join("\n");
            assert!(expanded.contains("cmd=git status --short"), "{expanded}");
            assert!(expanded.contains("Inspect repository st"), "{expanded}");
        }
    }

    #[test]
    fn command_tokens_are_colored_without_changing_visible_argv() {
        let view = event("Purpose", json!(["printf", "--", "hello world", "42"]));
        let ui = Ui {
            color: true,
            ..Ui::default()
        };
        let row = view.events[0].command_heading_row(120, &ui);
        assert_eq!(unstyled(&row), "Running printf -- 'hello world' 42");
        assert!(
            row.contains(&format!("\x1b[1;{}mprintf", ui.color("blue"))),
            "{row:?}"
        );
        assert!(
            row.contains(&format!("\x1b[{}m-", ui.color("accent"))),
            "{row:?}"
        );
        assert!(
            row.contains(&format!("\x1b[{}m'hello world'", ui.color("success"))),
            "{row:?}"
        );
        let plain = view.events[0].command_heading_row(
            120,
            &Ui {
                color: false,
                ..Ui::default()
            },
        );
        assert_eq!(plain, unstyled(&row));
        assert!(!plain.contains('\x1b'));
    }

    #[test]
    fn command_heading_never_reads_unredacted_native_arguments() {
        let view = event(
            "Request",
            json!([
                "curl",
                "-H",
                "Authorization: Bearer fixture-secret-value",
                "https://example.invalid"
            ]),
        );
        let ui = Ui {
            color: false,
            ..Ui::default()
        };
        let row = view.events[0].command_heading_row(120, &ui);
        assert!(row.contains("curl"), "{row}");
        assert!(!row.contains("fixture-secret-value"), "{row}");
    }

    #[test]
    fn long_output_has_a_count_and_an_expand_action_without_hiding_the_command() {
        for width in [48, 120] {
            let mut view = event("Inspect changes", json!(["git", "log", "--oneline"]));
            let output = (0..8).map(|n| format!("commit {n}\n")).collect::<String>();
            view.ingest_event(
                "shell",
                &json!({"type":"exec.stdout","job_id":"readable"}),
                output.as_bytes(),
            );
            let ui = Ui {
                color: false,
                ..Ui::default()
            };
            let rows = view.render(width, 24, &ui, false);
            let text = rows.join("\n");
            assert!(text.contains("Running git log --oneline"), "{text}");
            assert!(text.contains("+5 lines · Show details · Enter"), "{text}");
            assert!(
                text.contains("commit 7") && !text.contains("commit 0"),
                "{text}"
            );
            assert!(
                rows.iter()
                    .all(|row| UnicodeWidthStr::width(row.as_str()) <= usize::from(width))
            );
        }
    }
}
