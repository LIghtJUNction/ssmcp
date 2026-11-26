//! Local connection form. Editing a destination never opens a relay connection.
use ssmcp::connection_form::{self, DEFAULT_RELAY};
use std::path::PathBuf;
use zgpui::native::{ButtonStyle, Canvas, Rect, Ui};

pub use zgpui::native::colors::BG;
const TEXT: u32 = 0x202124ff;

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
        .ok_or_else(|| "缺少连接组件。请重新安装完整的 SSMCP 桌面版。".to_owned())
}

pub struct Launcher {
    focus_on_open: bool,
    submit: bool,
    focus: FormFocus,
    user: String,
    relay: String,
    accepted: bool,
    saved: Option<String>,
    registration: Option<std::sync::mpsc::Receiver<Result<String, String>>>,
    pub error: String,
}

fn saved_consent(saved: Option<&str>, username: &str, server: &str) -> bool {
    let Some(target) = saved else { return false };
    let Some((old_user, old_server)) = target.split_once('@') else {
        return false;
    };
    username.trim().eq_ignore_ascii_case(old_user)
        && connection_form::official_relay(target)
        && connection_form::relay_origin(old_server).ok()
            == connection_form::relay_origin(server).ok()
}

impl Launcher {
    pub fn new(target: Option<&str>) -> Self {
        let saved = connection_form::desktop_profile().ok().flatten();
        let (user, relay) = target
            .or(saved.as_deref())
            .and_then(|s| s.split_once('@'))
            .unwrap_or(("", DEFAULT_RELAY));
        Self {
            focus_on_open: true,
            submit: false,
            focus: FormFocus::default(),
            user: user.into(),
            relay: relay.into(),
            accepted: saved_consent(saved.as_deref(), user, relay),
            saved,
            registration: None,
            error: String::new(),
        }
    }

    pub fn event(&mut self, event: &zgpui::native::Event) -> bool {
        use zgpui::native::Event;
        self.focus.event(event);
        if self.focus.active
            && matches!(self.focus.selected, Some(USERNAME_INPUT | RELAY_INPUT))
            && matches!(event, Event::Text(text) if text.chars().any(char::is_control))
        {
            self.error = "请在每个输入框中只填写一行。".into();
            return true;
        }
        if matches!(event, Event::Key { code: 13, .. }) {
            if self.focus.active
                && matches!(self.focus.selected, Some(USERNAME_INPUT | RELAY_INPUT))
            {
                self.submit = true;
                return true;
            }
            return !self.focus.active;
        }
        false
    }

    // The first rendered frame registers the native keyboard order.
    pub fn initial_focus(&mut self) -> Option<zgpui::native::Event> {
        std::mem::take(&mut self.focus_on_open).then_some(zgpui::native::Event::Key {
            code: 9,
            control: false,
            shift: false,
            text: String::new(),
        })
    }

    pub fn show_error(&mut self, error: String) {
        self.focus = FormFocus::default();
        self.focus_on_open = true;
        self.error = error;
    }

    fn destination(&self) -> Result<String, String> {
        if self.user.trim().is_empty() {
            return Err("请输入用户名。".into());
        }
        if self.relay.trim().is_empty() {
            return Err("请输入服务器地址。".into());
        }
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
        let narrow = width < 475.;
        let y = ((h - if narrow { 262. } else { 190. }) / 2.).max(40.);
        let user_w = if narrow { width } else { (width - 20.) * 0.42 };
        let relay_x = if narrow { x } else { x + user_w + 20. };
        let relay_y = if narrow { y + 86. } else { y };
        let relay_w = x + width - relay_x;

        c.text("用户名", x, y - 24., 14, false, TEXT);
        let user_changed = ui.input(
            c,
            USERNAME_INPUT,
            self.focus
                .field(USERNAME_INPUT, Rect::new(x, y, user_w, 44.)),
            &mut self.user,
            "用户名",
            false,
            true,
        );
        c.text("服务器地址", relay_x, relay_y - 24., 14, false, TEXT);
        let relay_changed = ui.input(
            c,
            RELAY_INPUT,
            self.focus
                .field(RELAY_INPUT, Rect::new(relay_x, relay_y, relay_w, 44.)),
            &mut self.relay,
            DEFAULT_RELAY,
            false,
            true,
        );
        if user_changed || relay_changed {
            self.accepted = saved_consent(self.saved.as_deref(), &self.user, &self.relay);
            self.registration = None;
            self.error.clear();
        }
        let official = connection_form::official_relay(&format!("account@{}", self.relay.trim()));
        let actions_y = relay_y + 66.;
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
            "连接",
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
        if !self.user.trim().is_empty()
            && self.focus.button(
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
                self.registration.is_none() && (!official || self.accepted),
            )
        {
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
    fn saved_consent_follows_the_selected_account_and_server() {
        let saved = Some("alice@ssmcp.lmm.best");
        assert!(saved_consent(saved, "Alice", "https://ssmcp.lmm.best:443"));
        assert!(!saved_consent(saved, "bob", DEFAULT_RELAY));
        assert!(!saved_consent(saved, "alice", "ssmcp.lmm.best:8443"));
        assert!(!saved_consent(None, "alice", DEFAULT_RELAY));
    }

    #[test]
    fn first_run_validates_the_two_fields() {
        let mut form = Launcher::new(Some("@relay.example"));
        assert_eq!(form.destination().unwrap_err(), "请输入用户名。");
        form.user = "Alice".into();
        form.relay.clear();
        assert_eq!(form.destination().unwrap_err(), "请输入服务器地址。");
        form.relay = "relay.example".into();
        assert_eq!(form.destination().unwrap(), "Alice@relay.example");
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
        let mut form = Launcher::new(Some("e2eprob@relay.example"));
        frame(&mut form, None);
        let focus = form.initial_focus();
        frame(&mut form, focus);
        frame(&mut form, Some(Event::Text("e".into())));
        assert_eq!(form.user, "e2eprobe");
        let next = Some(Event::Key {
            code: 9,
            shift: false,
            control: false,
            text: String::new(),
        });
        frame(&mut form, next);
        assert_eq!(form.focus.selected, Some(RELAY_INPUT));
        assert_eq!(
            frame(&mut form, enter()).as_deref(),
            Some("e2eprobe@relay.example")
        );
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
        frame(&mut form, Some(Event::Down(245., 347.)));
        frame(&mut form, Some(Event::Up(245., 347.)));
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
            focus_on_open: true,
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
