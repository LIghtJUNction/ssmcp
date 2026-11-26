//! Local connection form. Editing a destination never opens a relay connection.
use ssmcp::connection_form::{self, DEFAULT_RELAY};
use std::path::PathBuf;
use zgpui::native::{ButtonStyle, Canvas, Rect, Ui};

pub const BG: u32 = 0xffffffff;
const TEXT: u32 = 0x202124ff;
const MUTED: u32 = 0x5f6368ff;
const SOURCE: &str = "https://github.com/LIghtJUNction/ssmcp";

const USERNAME_INPUT: u64 = 501;
const RELAY_INPUT: u64 = 502;

// The native input does not expose its focus. Record the enabled controls in draw order
// so pointer and Tab selection follow the same widgets as Ui.
#[derive(Default)]
struct FormFocus {
    stops: Vec<(u64, Rect)>,
    selected: Option<u64>,
    active: bool,
}

impl FormFocus {
    fn event(&mut self, event: &zgpui::native::Event) {
        use zgpui::native::Event;
        match event {
            Event::Down(x, y) => {
                let hit = self
                    .stops
                    .iter()
                    .rev()
                    .find(|(_, rect)| rect.contains(*x, *y));
                self.active = hit.is_some();
                if let Some((id, _)) = hit {
                    self.selected = Some(*id);
                }
            }
            Event::Key { code: 27, .. } => self.active = false,
            Event::Key { code: 9, shift, .. } if !self.stops.is_empty() => {
                // Ui keeps the last widget as the Tab origin after Escape or a blank click.
                let len = self.stops.len();
                let current = self
                    .stops
                    .iter()
                    .position(|(id, _)| Some(*id) == self.selected);
                let next = if *shift {
                    (current.unwrap_or(0) + len - 1) % len
                } else {
                    current.map_or(0, |i| (i + 1) % len)
                };
                self.selected = Some(self.stops[next].0);
                self.active = true;
            }
            _ => {}
        }
    }

    fn field(&mut self, id: u64, rect: Rect) -> Rect {
        self.stops.push((id, rect));
        rect
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "track the same enabled button that Ui draws"
    )]
    fn button(
        &mut self,
        ui: &mut Ui,
        canvas: &mut Canvas<'_>,
        id: u64,
        rect: Rect,
        label: &str,
        style: ButtonStyle,
        enabled: bool,
    ) -> bool {
        if enabled {
            self.stops.push((id, rect));
        }
        ui.button(canvas, id, rect, label, style, enabled)
    }
}

pub fn executable() -> Result<PathBuf, String> {
    let name = if cfg!(windows) { "ssmcp.exe" } else { "ssmcp" };
    let this = std::env::current_exe().map_err(|e| e.to_string())?;
    let paths = std::env::var_os("PATH").unwrap_or_default();
    this.parent()
        .into_iter()
        .map(|p| p.join(name))
        .chain(
            std::env::split_paths(&paths)
                .filter(|p| p.is_absolute())
                .map(|p| p.join(name)),
        )
        .find(|p| p.is_file())
        .ok_or_else(|| format!("找不到 {name}。请将它与桌面程序放在同一目录，或安装到 PATH。"))
}

pub struct Launcher {
    editor: crate::composer::Composer,
    submit: bool,
    focus: FormFocus,
    user: String,
    relay: String,
    accepted: bool,
    saved: Option<String>,
    registration: Option<std::sync::mpsc::Receiver<Result<String, String>>>,
    pub error: String,
}

impl Launcher {
    pub fn new(target: Option<&str>) -> Self {
        let saved = connection_form::desktop_profile().ok().flatten();
        let (user, relay) = target
            .or(saved.as_deref())
            .and_then(|s| s.split_once('@'))
            .unwrap_or(("", DEFAULT_RELAY));
        let mut editor = crate::composer::Composer::default();
        editor.focus_end(user);
        Self {
            editor,
            submit: false,
            focus: FormFocus::default(),
            user: user.into(),
            relay: relay.into(),
            accepted: saved
                .as_deref()
                .is_some_and(|previous| connection_form::official_relay(previous)),
            saved,
            registration: None,
            error: String::new(),
        }
    }

    pub fn event(&mut self, event: &zgpui::native::Event) -> bool {
        use zgpui::native::Event;
        self.focus.event(event);
        if matches!(event, Event::Key { code: 13, .. }) {
            if self.editor.focused()
                || (self.focus.active && self.focus.selected == Some(RELAY_INPUT))
            {
                self.submit = true;
                return true;
            }
            return !self.focus.active;
        }
        if self.editor.focused()
            && matches!(event, Event::Text(value) if value.chars().any(char::is_control))
        {
            self.error = "用户名只能填写一行。".into();
            return true;
        }
        if self.editor.event(event, &mut self.user, true) {
            self.registration = None;
            self.error.clear();
            return true;
        }
        false
    }

    pub fn take_clipboard(&mut self) -> Option<String> {
        self.editor.take_clipboard()
    }

    fn destination(&self) -> Result<String, String> {
        let target = format!("{}@{}", self.user.trim(), self.relay.trim());
        connection_form::validate_destination(&target)?;
        if connection_form::official_relay(&target) && !self.accepted {
            return Err("使用默认中继前，请阅读并同意用户协议和隐私政策。".into());
        }
        Ok(target)
    }

    pub fn configured_target(&self) -> Option<String> {
        self.destination()
            .ok()
            .filter(|s| self.saved.as_ref() == Some(s))
    }

    pub fn draw(&mut self, c: &mut Canvas<'_>, ui: &mut Ui, w: f32, h: f32) -> Option<String> {
        self.focus.stops.clear();
        let width = (w - 40.).clamp(1., 660.);
        let x = (w - width) / 2.;
        let y = ((h - 190.) / 2.).max(67.);
        let narrow = width < 475.;
        let prefix_w = if narrow { 64. } else { 88. };
        let user_w = if narrow {
            width - prefix_w
        } else {
            (width - prefix_w - 38.) * 0.44
        };
        let user_x = x + prefix_w;
        let relay_x = if narrow {
            user_x
        } else {
            user_x + user_w + 38.
        };
        let relay_y = if narrow { y + 90. } else { y };
        c.text("ssmcp", x, y + 7., if narrow { 19 } else { 24 }, true, TEXT);
        self.editor.draw(
            c,
            ui,
            self.focus
                .field(USERNAME_INPUT, Rect::new(user_x, y, user_w.max(1.), 44.)),
            &self.user,
            true,
            "用户名",
        );
        c.ellipsis(
            "↑ 用户名，首次注册时自选",
            user_x,
            y + 53.,
            user_w.max(1.),
            12,
            false,
            MUTED,
        );
        c.text(
            "@",
            if narrow { x + 26. } else { relay_x - 28. },
            relay_y + 10.,
            18,
            false,
            MUTED,
        );
        let relay_w = (x + width - relay_x).max(1.);
        let changed_relay = ui.input(
            c,
            RELAY_INPUT,
            self.focus
                .field(RELAY_INPUT, Rect::new(relay_x, relay_y, relay_w, 44.)),
            &mut self.relay,
            "中继服务器",
            false,
            true,
        );
        c.ellipsis(
            "↑ 中继服务器，可填写自建地址",
            relay_x,
            relay_y + 53.,
            relay_w,
            12,
            false,
            MUTED,
        );
        if changed_relay {
            self.focus.selected = Some(RELAY_INPUT);
            self.focus.active = true;
            self.registration = None;
            self.error.clear();
        }
        let official = connection_form::official_relay(&format!("account@{}", self.relay.trim()));
        let actions_y = relay_y + 94.;
        if official {
            let label = if self.accepted {
                "☑ 同意"
            } else {
                "☐ 同意"
            };
            if self.focus.button(
                ui,
                c,
                503,
                Rect::new(x, actions_y, 76., 34.),
                label,
                ButtonStyle::Quiet,
                true,
            ) {
                self.accepted = !self.accepted;
                self.error.clear();
            }
            if self.focus.button(
                ui,
                c,
                504,
                Rect::new(x + 76., actions_y, 78., 34.),
                "用户协议",
                ButtonStyle::Quiet,
                true,
            ) {
                self.open(concat!("https://ssmcp.lmm.best", "/legal/terms"));
            }
            if self.focus.button(
                ui,
                c,
                505,
                Rect::new(x + 155., actions_y, 78., 34.),
                "隐私政策",
                ButtonStyle::Quiet,
                true,
            ) {
                self.open(concat!("https://ssmcp.lmm.best", "/legal/privacy"));
            }
        } else {
            c.text("使用自建中继", x + 4., actions_y + 8., 13, false, MUTED);
        }
        let submit_y = if width < 370. {
            actions_y + 48.
        } else {
            actions_y
        };
        if self.focus.button(
            ui,
            c,
            508,
            Rect::new(x + width - 92., submit_y, 92., 36.),
            "连接  →",
            ButtonStyle::Primary,
            !self.user.trim().is_empty()
                && !self.relay.trim().is_empty()
                && (!official || self.accepted),
        ) || std::mem::take(&mut self.submit)
        {
            match self.destination() {
                Ok(target) => return Some(target),
                Err(error) => self.error = error,
            }
        }
        if self.focus.button(
            ui,
            c,
            509,
            Rect::new(x, submit_y + 48., 100., 30.),
            if self.registration.is_some() {
                "正在打开…"
            } else {
                "注册账号 ↗"
            },
            ButtonStyle::Quiet,
            self.registration.is_none()
                && !self.user.trim().is_empty()
                && (!official || self.accepted),
        ) {
            match self.destination() {
                Ok(destination) => {
                    let (tx, rx) = std::sync::mpsc::channel();
                    self.registration = Some(rx);
                    std::thread::spawn(move || {
                        let result = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .map_err(|e| e.to_string())
                            .and_then(|rt| {
                                rt.block_on(connection_form::registration_url(&destination))
                                    .map_err(|e| e.to_string())
                            });
                        let _ = tx.send(result);
                    });
                }
                Err(error) => self.error = error,
            }
        }
        if self.focus.button(
            ui,
            c,
            506,
            Rect::new((w - 114.).max(4.), 20., 92., 34.),
            "GitHub ↗",
            ButtonStyle::Quiet,
            true,
        ) {
            self.open(SOURCE);
        }
        if !self.error.is_empty() {
            c.ellipsis(&self.error, x, submit_y + 88., width, 13, false, 0xb42318ff);
        }
        None
    }

    pub fn poll_registration(&mut self) -> bool {
        let Some(rx) = &self.registration else {
            return false;
        };
        let result = match rx.try_recv() {
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(_) => Err("注册请求中断。".to_owned()),
            Ok(answer) => answer,
        };
        self.registration = None;
        match result {
            Ok(url) => self.open(&url),
            Err(error) => self.error = error,
        }
        true
    }

    fn open(&mut self, url: &str) {
        if let Err(error) = connection_form::open_link(url) {
            self.error = format!("无法打开浏览器：{error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn username_accepts_typing_on_open_and_enter_runs_validation() {
        let mut form = Launcher::new(Some("Alice@relay.example"));
        form.event(&zgpui::native::Event::Text("2".into()));
        assert_eq!(form.user, "Alice2");
        form.event(&zgpui::native::Event::Key {
            code: 13,
            control: false,
            shift: false,
            text: String::new(),
        });
        assert!(form.submit);
        assert_eq!(form.destination().unwrap(), "Alice2@relay.example");
    }

    #[test]
    #[ignore = "requires a native display; can run under Xvfb"]
    fn relay_enter_submits_and_respects_consent() {
        use zgpui::native::{Event, Window};

        let mut window = Window::new("connection form test", 1080, 720).unwrap();
        let mut ui = Ui::default();
        let mut frame = |form: &mut Launcher, event: Option<Event>| {
            let consumed = event.as_ref().is_some_and(|e| form.event(e));
            ui.begin(if consumed { None } else { event });
            let mut canvas = window.begin(BG);
            let target = form.draw(&mut canvas, &mut ui, 1080., 720.);
            ui.end();
            drop(canvas);
            window.present();
            target
        };
        let enter = || {
            Some(Event::Key {
                code: 13,
                control: false,
                shift: false,
                text: String::new(),
            })
        };
        let mut form = Launcher::new(Some("e2eprobe@relay.example"));
        frame(&mut form, None);
        frame(
            &mut form,
            Some(Event::Key {
                code: 27,
                control: false,
                shift: false,
                text: String::new(),
            }),
        );
        assert_eq!(frame(&mut form, enter()), None);

        // A fresh prefilled form must not connect when the first click lands on the background.
        form = Launcher::new(Some("e2eprobe@relay.example"));
        frame(&mut form, None);
        frame(&mut form, Some(Event::Down(35., 180.)));
        frame(&mut form, Some(Event::Up(35., 180.)));
        assert_eq!(frame(&mut form, enter()), None);
        frame(&mut form, Some(Event::Down(700., 285.)));
        frame(&mut form, Some(Event::Up(700., 285.)));
        assert_eq!(
            frame(&mut form, enter()).as_deref(),
            Some("e2eprobe@relay.example")
        );

        // Tab away from the relay and Shift+Tab back without editing its preset text.
        let tab = |shift| {
            Some(Event::Key {
                code: 9,
                control: false,
                shift,
                text: String::new(),
            })
        };
        frame(&mut form, tab(false));
        frame(&mut form, tab(true));
        assert_eq!(
            frame(&mut form, enter()).as_deref(),
            Some("e2eprobe@relay.example")
        );

        form.relay = DEFAULT_RELAY.into();
        form.accepted = false;
        assert_eq!(frame(&mut form, enter()), None);
        assert!(form.error.contains("同意"));
        // Enter on the consent button toggles only that button.
        frame(&mut form, Some(Event::Down(245., 376.)));
        frame(&mut form, Some(Event::Up(245., 376.)));
        assert!(form.accepted);
        assert_eq!(frame(&mut form, enter()), None);
        assert!(!form.accepted);
        assert_eq!(frame(&mut form, enter()), None);
        assert!(form.accepted);

        frame(&mut form, Some(Event::Down(700., 285.)));
        frame(&mut form, Some(Event::Up(700., 285.)));
        assert_eq!(
            frame(&mut form, enter()),
            Some(format!("e2eprobe@{DEFAULT_RELAY}"))
        );
    }

    #[test]
    fn official_consent_is_required_for_equivalent_origins() {
        let mut form = Launcher {
            editor: Default::default(),
            submit: false,
            focus: FormFocus::default(),
            user: "Alice".into(),
            relay: "HTTPS://SSMCP.LMM.BEST:443/".into(),
            accepted: false,
            saved: None,
            registration: None,
            error: String::new(),
        };
        assert!(form.destination().is_err());
        form.accepted = true;
        assert!(form.destination().is_ok());
        form.accepted = false;
        form.relay = "http://127.0.0.1:8000".into();
        assert!(form.destination().is_ok());
        assert!(form.configured_target().is_none());
    }
}
