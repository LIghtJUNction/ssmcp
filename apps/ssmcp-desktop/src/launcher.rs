//! One connection command on launch; further interaction belongs to the real CLI.
use crate::{composer::Composer, console::Console};
use zgpui::native::{ButtonStyle, Canvas, Event, Rect, Ui};

const PREFIX: &str = "ssmcp ";
const RELAY: &str = "ssmcp.lmm.best";
pub const BG: u32 = 0xffffffff;
const INK: u32 = 0x23303aff;
const MUTED: u32 = 0x596879ff;

pub struct Launcher {
    command: String,
    editor: Composer,
    error: Option<String>,
    submitted: bool,
    terminal: Option<Console>,
}
impl Default for Launcher {
    fn default() -> Self {
        let mut editor = Composer::default();
        editor.focus_end(PREFIX);
        Self {
            command: PREFIX.to_owned(),
            editor,
            error: None,
            submitted: false,
            terminal: None,
        }
    }
}
impl Launcher {
    pub fn revision(&self) -> u64 {
        self.terminal.as_ref().map_or(0, Console::revision)
    }
    pub fn active(&self) -> bool {
        self.terminal.as_ref().is_some_and(|t| !t.ended())
    }
    pub fn take_clipboard(&mut self) -> Option<String> {
        self.editor.take_clipboard()
    }
    fn fill_default(&mut self) {
        let value = self.command.strip_prefix(PREFIX).unwrap_or(&self.command);
        let user = value.split_once('@').map_or(value, |(u, _)| u).trim();
        let cursor = if user.is_empty() {
            Some(PREFIX.len())
        } else {
            None
        };
        self.command = format!("{PREFIX}{user}@{RELAY}");
        self.error = None;
        self.editor
            .focus_at(&self.command, cursor.unwrap_or(self.command.len()));
    }
    pub fn event(&mut self, e: &Event) -> bool {
        if let Some(term) = &mut self.terminal {
            if term.ended() && matches!(e, Event::Key { code: 13 | 27, .. }) {
                self.terminal = None;
                self.editor.focus_end(&self.command);
                return true;
            }
            return term.event(e);
        }
        if self.editor.focused() && matches!(e, Event::Key { code: 13, .. }) {
            self.submitted = true;
            return true;
        }
        if let Event::Text(s) = e
            && self.editor.focused()
            && s.chars().any(char::is_control)
        {
            self.error = Some("连接命令只能输入一行。".into());
            return true;
        }
        let handled = self.editor.event(e, &mut self.command, true);
        if handled && !matches!(e, Event::Scroll(_)) {
            self.error = None;
        }
        handled
    }
    pub fn draw(&mut self, c: &mut Canvas<'_>, ui: &mut Ui, w: f32, h: f32) {
        if let Some(term) = &mut self.terminal {
            term.draw(c, w, h);
            return;
        }
        let margin = if w < 420. { 16. } else { 32. };
        let width = (w - 2. * margin).clamp(1., 616.);
        let x = ((w - width) * 0.5).max(0.);
        let y = (h * 0.40 - 50.).max(12.);
        c.text("连接到服务器", x, y, 24, true, INK);
        let field = Rect::new(x, y + 47., width, 46.);
        self.editor.draw(c, ui, field, &self.command, true);
        if self.command == PREFIX {
            let advance = PREFIX
                .chars()
                .map(|ch| c.measure(&ch.to_string(), 15, false))
                .sum::<f32>();
            c.ellipsis(
                "用户名@服务器域名",
                x + 14. + advance,
                field.y + 11.,
                (width - 24. - advance).max(1.),
                15,
                false,
                0x81909dff,
            );
        }
        let hint_y = field.y + 57.;
        let narrow = width < 490.;
        c.ellipsis(
            "用户名 · 服务器上的账户名",
            x,
            hint_y,
            if narrow { width } else { width - 215. },
            13,
            false,
            MUTED,
        );
        let relay_y = if narrow { hint_y + 22. } else { hint_y };
        let relay_x = if narrow { x } else { x + width - 212. };
        c.text("默认中继", relay_x, relay_y, 13, false, MUTED);
        if ui.button(
            c,
            502,
            Rect::new(relay_x + 57., relay_y - 7., 138., 30.),
            RELAY,
            ButtonStyle::Quiet,
            true,
        ) {
            self.fill_default();
        }
        let button_y = relay_y + 38.;
        if ui.button(
            c,
            503,
            Rect::new(x, button_y, 96., 38.),
            "连接  ↵",
            ButtonStyle::Primary,
            true,
        ) {
            self.submitted = true;
        }
        if self.submitted {
            self.submitted = false;
            let result = self
                .command
                .strip_prefix(PREFIX)
                .ok_or_else(|| "命令应以 ssmcp 和一个空格开头。".to_owned())
                .and_then(|dest| {
                    ssmcp::desktop_terminal::validate_destination(dest)?;
                    Console::start(dest.to_owned(), w, h)
                });
            match result {
                Ok(term) => {
                    self.error = None;
                    self.terminal = Some(term);
                }
                Err(message) => {
                    self.error = Some(message);
                    self.editor.focus_end(&self.command);
                }
            }
        }
        if let Some(msg) = &self.error {
            // Wrap at measured glyph widths so error details survive narrow windows.
            let mut line = String::new();
            let mut used = 0.;
            let mut cy = button_y + 50.;
            for ch in msg.chars() {
                let cw = c.measure(&ch.to_string(), 13, false);
                if used + cw > width && !line.is_empty() {
                    c.text(&line, x, cy, 13, false, 0xa83232ff);
                    line.clear();
                    used = 0.;
                    cy += 19.;
                }
                line.push(ch);
                used += cw;
            }
            c.text(&line, x, cy, 13, false, 0xa83232ff);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_relay_fills_only_the_server_and_leaves_an_empty_username_editable() {
        let mut form = Launcher::default();
        form.fill_default();
        form.event(&Event::Text("Alice".into()));
        assert_eq!(form.command, "ssmcp Alice@ssmcp.lmm.best");
        form.command = "ssmcp Alice@custom.example".into();
        form.fill_default();
        assert_eq!(form.command, "ssmcp Alice@ssmcp.lmm.best");
        assert!(!form.submitted);
    }
}
