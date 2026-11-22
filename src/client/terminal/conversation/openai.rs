//! Local caller inspection and explicit browser/clipboard commands.
use super::*;
use crate::client::{
    attachments::WebAction,
    interface::history::{self as saved, HistoryOwner, openai as store},
};

const DOCS: &str = "https://developers.openai.com/plugins/reference";
const NO_URL: &str = "No ChatGPT URL is set for this live session · use /chatgpt set <URL>";
const LINK_USAGE: &str = "Use /chatgpt set <URL>, /chatgpt open, /chatgpt copy, or /chatgpt clear";

pub(super) struct Panel {
    owner: Option<HistoryOwner>,
    text: String,
}

enum LinkOutcome {
    Notice(&'static str),
    Effect(WebAction),
}

fn link_command(owner: Option<&HistoryOwner>, arguments: &str) -> Result<LinkOutcome, String> {
    let (verb, rest) = arguments
        .split_once(char::is_whitespace)
        .unwrap_or((arguments, ""));
    let rest = rest.trim();
    match (verb, rest) {
        ("set", url) if !url.is_empty() => {
            let owner = owner.ok_or("Start a live conversation before saving a URL")?;
            store::change_url(owner, Some(url))?;
            Ok(LinkOutcome::Notice(
                "ChatGPT URL kept for this live session",
            ))
        }
        ("clear", "") => {
            let changed = owner
                .map(|owner| store::change_url(owner, None))
                .transpose()?;
            Ok(LinkOutcome::Notice(if changed == Some(true) {
                "ChatGPT URL cleared from this session"
            } else {
                NO_URL
            }))
        }
        ("" | "open" | "copy", "") => {
            let url = owner
                .map(store::read_selected)
                .transpose()?
                .flatten()
                .and_then(|record| record.chatgpt_url)
                .ok_or(NO_URL)?;
            let action = if verb == "copy" {
                WebAction::Copy(url)
            } else {
                WebAction::Open(
                    store::validate_chatgpt_url(&url)?,
                    "Use /chatgpt copy to open it manually",
                )
            };
            Ok(LinkOutcome::Effect(action))
        }
        _ => Err(LINK_USAGE.into()),
    }
}

fn observation(text: &mut String, key: &str, value: Option<&store::Observed>) {
    let _ = writeln!(text, "{key}");
    if let Some(value) = value {
        let _ = writeln!(text, "{}", value.value);
        let _ = writeln!(text, "Last observed · UTC Unix ms {}", value.observed_ms);
    } else {
        let _ = writeln!(text, "Not observed for this live conversation");
    }
    text.push('\n');
}

fn panel_text(command: &str, record: Option<&store::Saved>) -> String {
    let mut text = String::from(
        "OpenAI caller metadata · Esc closes\n\nSource: MCP tools/call _meta · live conversation\n\n",
    );
    if record.is_none() {
        text.push_str("Start a live conversation to see its observations.\n\n");
    }
    if command != "/openai-subject" {
        observation(
            &mut text,
            "openai/session",
            record.and_then(|record| record.session.as_ref()),
        );
    }
    if command != "/openai-session" {
        observation(
            &mut text,
            "openai/subject",
            record.and_then(|record| record.subject.as_ref()),
        );
    }
    text
}

impl State {
    pub(super) fn chat_show_local_text(&mut self, text: String) {
        let owner = (!self.preview).then(saved::current_owner).flatten();
        self.conversation.openai_panel = Some(Panel { owner, text });
        self.conversation.about = false;
        self.conversation.plugin_help = false;
        self.conversation.help_scroll = 0;
        self.help = true;
        self.conversation.content_focus = true;
        self.dirty = true;
    }

    pub(super) fn chat_openai_command(&mut self, command: &str, args: &str) -> Result<(), String> {
        let owner = (!self.preview).then(saved::current_owner).flatten();
        match command {
            "/chatgpt" => match link_command(owner.as_ref(), args)? {
                LinkOutcome::Notice(notice) => self.notice = Some(notice.into()),
                LinkOutcome::Effect(action) => {
                    self.chat_start_input(attachments::Source::Web(action), true)?
                }
            },
            "/plugin-docs" => {
                if !args.is_empty() {
                    return Err("Use /plugin-docs without arguments".into());
                }
                let url = reqwest::Url::parse(DOCS).map_err(|_| "Invalid plugin reference URL")?;
                self.chat_start_input(attachments::Source::Web(WebAction::Open(url, DOCS)), true)?;
            }
            _ => {
                if !args.is_empty() {
                    return Err(format!("Use {command} without arguments"));
                }
                let record = owner
                    .as_ref()
                    .map(store::read_selected)
                    .transpose()?
                    .flatten();
                let text = panel_text(command, record.as_ref());
                self.conversation.openai_panel = Some(Panel { owner, text });
                self.conversation.help_scroll = 0;
                self.help = true;
                self.conversation.content_focus = true;
            }
        }
        self.dirty = true;
        Ok(())
    }

    pub(super) fn chat_openai_rows(&mut self) -> Option<Vec<String>> {
        let panel = self.conversation.openai_panel.as_ref()?;
        if panel.owner != (!self.preview).then(saved::current_owner).flatten() {
            self.conversation.openai_panel = None;
            return None;
        }
        Some(markdown::literal_rows(
            &panel.text,
            self.geometry.left,
            &self.ui,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::openai::OpenAiContext;
    use std::path::PathBuf;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let mut random = [0_u8; 16];
            crate::platform::random_bytes(&mut random).unwrap();
            let suffix = random.map(|byte| format!("{byte:02x}")).join("");
            let path = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("ssmcp-openai-{suffix}"));
            crate::platform::private_directory(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn state() -> State {
        State::new(
            26,
            80,
            &Ui {
                layout: Layout::Chat,
                sidebar: false,
                ..Ui::default()
            },
        )
    }

    #[test]
    fn missing_values_are_local_and_preview_cannot_start_a_browser() {
        let mut view = crate::client::terminal::preview_state(26, 80, &Ui::default());
        assert!(view.chat_command("/openai-session").unwrap());
        let rows = view.chat_openai_rows().unwrap().join("\n");
        assert!(rows.contains("openai/session") && rows.contains("Not observed"));
        assert!(!rows.contains("openai/subject"));
        assert!(view.chat_command("/chatgpt").unwrap_err().contains(NO_URL));
        assert!(
            view.chat_command("/plugin-docs")
                .unwrap_err()
                .contains("Preview cannot open")
        );
        assert!(view.conversation.input_task.is_none());
    }

    #[tokio::test]
    async fn url_commands_and_private_display_follow_the_selected_live_context() {
        let fixture = Fixture::new();
        saved::with_test_history(fixture.0.join("history"), async {
            saved::record("user", "Live A").unwrap();
            let a = saved::current_owner().unwrap();
            let context = OpenAiContext {
                session: Some("synthetic-A-session".into()),
                subject: Some("synthetic-A-subject".into()),
            };
            assert!(store::observe(&a, &context, 123).unwrap());
            let mut view = state();
            assert!(view.chat_command("/openai").unwrap());
            let rows = view.chat_openai_rows().unwrap().join("\n");
            assert!(rows.contains("synthetic-A-session") && rows.contains("synthetic-A-subject"));
            assert!(rows.contains("Last observed"));
            assert!(
                !view
                    .conversation
                    .snapshot()
                    .to_string()
                    .contains("synthetic-A")
            );

            let url = "https://chat.openai.com/c/fictional-a?model=fixture";
            assert!(view.chat_command(&format!("/chatgpt set {url}")).unwrap());
            assert!(view.conversation.input_task.is_none());
            for command in ["", "open"] {
                assert!(matches!(
                    link_command(Some(&a), command),
                    Ok(LinkOutcome::Effect(WebAction::Open(parsed, fallback))) if parsed.as_str() == url && fallback.contains("/chatgpt copy")
                ));
            }
            assert!(matches!(
                link_command(Some(&a), "copy"),
                Ok(LinkOutcome::Effect(WebAction::Copy(text))) if text == url
            ));

            saved::release_current().unwrap();
            saved::record("user", "Live B").unwrap();
            assert!(view.chat_openai_rows().is_none());
            assert!(view.chat_command("/openai-subject").unwrap());
            assert!(
                !view
                    .chat_openai_rows()
                    .unwrap()
                    .join("\n")
                    .contains("synthetic-A")
            );
            assert!(store::read_selected(&a).is_err());
            assert!(
                view.chat_command("/chatgpt open")
                    .unwrap_err()
                    .contains(NO_URL)
            );
            assert!(view.conversation.input_task.is_none());

            saved::resume_test_history(a.id()).unwrap();
            let resumed = saved::current_owner().unwrap();
            let record = store::read_selected(&resumed).unwrap().unwrap();
            assert_eq!(record.chatgpt_url.as_deref(), Some(url));
            assert!(view.chat_command("/chatgpt clear").unwrap());
            let record = store::read_selected(&resumed).unwrap().unwrap();
            assert!(record.chatgpt_url.is_none());
            assert_eq!(record.session.unwrap().value, "synthetic-A-session");
            assert_eq!(record.subject.unwrap().value, "synthetic-A-subject");
            assert!(view.conversation.input_task.is_none());
        })
        .await;
        assert!(!fixture.0.join("history").exists());
    }
}
