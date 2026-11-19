//! Keep account authorization independent from the current draft and session.
use super::*;
use crate::client::account::login::{Event, Mode, Task};

impl State {
    pub(super) fn chat_login(&mut self, arguments: &str) -> Result<(), String> {
        let mode = Mode::parse(arguments)?;
        if self.preview {
            return Err(self
                .ui
                .language
                .text(
                    "Preview does not start GitHub login or use desktop helpers",
                    "预览模式不启动 GitHub 登录，也不调用桌面工具",
                )
                .into());
        }
        if self.account == crate::client::account::status::Status::Local {
            return Err(
                self.ui.language.text("Local sessions do not need GitHub login. Use the MCP address and one-time login code in /logs to authorize the controller.", "本地会话无需 GitHub 登录。使用 /logs 中的 MCP 地址和一次性登录码授权控制端。")
                    .into(),
            );
        }
        if self.conversation.logout_task.is_some() || self.conversation.exit_requested {
            return Err(self
                .ui
                .language
                .text(
                    "This session is signing out or closing",
                    "此会话正在退出登录或关闭",
                )
                .into());
        }
        self.chat_poll_login();
        if let Some(task) = &self.conversation.login_task {
            task.request(mode)?;
            self.notice = Some(
                self.ui
                    .language
                    .text(
                        "Using the pending GitHub login…",
                        "正在使用待处理的 GitHub 登录…",
                    )
                    .into(),
            );
        } else {
            self.conversation.login_task = Some(Task::start(mode)?);
            self.notice = Some(
                self.ui
                    .language
                    .text("Preparing GitHub login…", "正在准备 GitHub 登录…")
                    .into(),
            );
        }
        self.dirty = true;
        Ok(())
    }

    pub(super) fn chat_poll_login(&mut self) {
        loop {
            let Some(event) = self.conversation.login_task.as_mut().and_then(Task::poll) else {
                return;
            };
            match event {
                Event::Link(link) => {
                    let mut text = localized!(
                        self.ui.language,
                        "Sign in with GitHub:\n{}",
                        "使用 GitHub 登录：\n{}",
                        link.url
                    );
                    if let Some(code) = link.confirmation {
                        text.push_str(self.ui.language.text("\nConfirmation code: ", "\n确认码："));
                        text.push_str(&code);
                    }
                    text.push_str(self.ui.language.text(
                        "\n/login copy copies this link; /login link shows it again.",
                        "\n/login copy 复制此链接；/login link 再次显示。",
                    ));
                    self.chat_login_message(&text);
                }
                Event::Notice(message) => {
                    self.notice = Some(self.ui.language.local_message(&message));
                }
                Event::Finished(result) => {
                    self.conversation.login_task = None;
                    let message = match result {
                        Ok(principal) => localized!(
                            self.ui.language,
                            "Signed in with GitHub as {principal}",
                            "已使用 GitHub 登录为 {principal}"
                        ),
                        Err(error) => localized!(
                            self.ui.language,
                            "GitHub login: {error}",
                            "GitHub 登录：{error}"
                        ),
                    };
                    self.chat_login_message(&message);
                    self.notice = Some(self.ui.language.local_message(&message));
                    self.dirty = true;
                    return;
                }
            }
            self.dirty = true;
        }
    }

    fn chat_login_message(&mut self, text: &str) {
        let raw = format!("\n[ACCOUNT] {text}\n");
        self.log_event(
            "account",
            &json!({"type":"ssmcp.message","author":"Account","text":text}),
            raw.as_bytes(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::account::login::Link;

    #[test]
    fn preview_login_and_invalid_modes_leave_drafts_and_desktop_untouched() {
        let mut state = super::super::super::preview_state(24, 100, &Ui::default());
        state.conversation.composer.set_text("保留草稿界");
        state.conversation.composer.left();
        state.conversation.target = Some("chosen-agent".into());
        let stamp = state.chat_input_stamp().unwrap();
        for command in [
            "/login",
            "/login copy",
            "/login link",
            "/login --token unsafe",
        ] {
            assert!(state.chat_command(command).is_err(), "{command}");
            assert!(state.conversation.login_task.is_none());
            assert_eq!(state.chat_input_stamp().as_ref(), Some(&stamp));
            assert!(state.actions.is_empty());
        }
    }

    #[test]
    fn local_login_explains_connection_authorization_without_starting_account_login() {
        let mut state = super::super::super::preview_state(24, 100, &Ui::default());
        state.preview = false;
        state.account = crate::client::account::status::Status::Local;
        state.conversation.composer.set_text("保留本地草稿");
        let before = state.chat_input_stamp().unwrap();
        for command in ["/login", "/login copy", "/login link"] {
            let error = state.chat_command(command).unwrap_err();
            assert!(error.contains("/logs"), "{error}");
            assert!(state.conversation.login_task.is_none());
            assert_eq!(state.chat_input_stamp().as_ref(), Some(&before));
            assert!(state.actions.is_empty());
        }
    }

    #[tokio::test]
    async fn pending_login_reuses_actions_and_results_preserve_editor_and_canvas() {
        let mut state = super::super::super::preview_state(24, 100, &Ui::default());
        state.preview = false;
        state.conversation.composer.set_text("原草稿界");
        state.conversation.composer.left();
        state.conversation.target = Some("chosen-agent".into());
        state.conversation.editing = Some("editable-request".into());
        state.chat_change_view(View::Audit);
        let before = state.chat_input_stamp().unwrap();
        let (task, mut actions, events) = Task::pending_for_test();
        state.conversation.login_task = Some(task);
        for (command, expected) in [
            ("/login", Mode::Open),
            ("/login copy", Mode::Copy),
            ("/login link", Mode::Link),
        ] {
            assert!(state.chat_command(command).unwrap());
            assert_eq!(actions.try_recv().unwrap(), expected);
            assert!(state.conversation.login_task.is_some());
            assert_eq!(state.chat_input_stamp().as_ref(), Some(&before));
        }
        events
            .send(Event::Link(Link {
                url: "https://example.com/setup?code=PUBLIC-CODE".into(),
                confirmation: Some("PUBLIC-CODE".into()),
            }))
            .unwrap();
        events
            .send(Event::Notice(
                "GitHub login link copied to the system clipboard".into(),
            ))
            .unwrap();
        state.chat_poll_login();
        assert_eq!(state.chat_input_stamp().as_ref(), Some(&before));
        assert_eq!(state.conversation.view, View::Audit);
        assert!(state.conversation.login_task.is_some());
        assert!(
            state
                .notice
                .as_deref()
                .unwrap()
                .contains("login link copied")
        );
        events
            .send(Event::Finished(Ok("alice@github".into())))
            .unwrap();
        state.chat_poll_login();
        assert!(state.conversation.login_task.is_none());
        assert_eq!(state.chat_input_stamp().as_ref(), Some(&before));
        assert_eq!(state.conversation.view, View::Audit);
        assert!(state.conversation.submitting.is_none());
        assert!(state.actions.is_empty());
        assert!(state.notice.as_deref().unwrap().contains("alice@github"));
    }

    #[test]
    fn diff_command_changes_only_main_diff_layout() {
        let mut state = super::super::super::preview_state(24, 100, &Ui::default());
        state.conversation.composer.set_text("保留草稿");
        state.conversation.target = Some("chosen-agent".into());
        let stamp = state.chat_input_stamp().unwrap();
        let raw_logs = state.raw_logs;
        assert_eq!(state.events.diff_mode(), events::DiffMode::Single);
        assert!(state.chat_command("/diff").unwrap());
        assert!(state.notice.as_deref().unwrap().contains("single"));
        state.dirty = false;
        assert!(state.chat_command("/diff split").unwrap());
        assert_eq!(state.events.diff_mode(), events::DiffMode::Split);
        assert!(state.dirty);
        assert!(state.chat_command("/diff invalid").is_err());
        assert_eq!(state.events.diff_mode(), events::DiffMode::Split);
        assert_eq!(state.raw_logs, raw_logs);
        assert_eq!(state.chat_input_stamp().as_ref(), Some(&stamp));
        assert!(state.chat_command("/diff single").unwrap());
        assert_eq!(state.events.diff_mode(), events::DiffMode::Single);
    }
}
