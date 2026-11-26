//! Local connection form. Editing a destination never opens a relay connection.
use ssmcp::desktop_terminal::{self, DEFAULT_RELAY};
use std::path::PathBuf;
use zgpui::native::{ButtonStyle, Canvas, Rect, Ui};

pub const BG: u32 = 0xffffffff;
const TEXT: u32 = 0x202124ff;
const MUTED: u32 = 0x5f6368ff;
const SOURCE: &str = "https://github.com/LIghtJUNction/ssmcp";

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
    user: String,
    relay: String,
    accepted: bool,
    saved: Option<String>,
    registration: Option<std::sync::mpsc::Receiver<Result<String, String>>>,
    pub error: String,
}

impl Launcher {
    pub fn new(target: Option<&str>) -> Self {
        let saved = desktop_terminal::desktop_profile().ok().flatten();
        let (user, relay) = target
            .or(saved.as_deref())
            .and_then(|s| s.split_once('@'))
            .unwrap_or(("", DEFAULT_RELAY));
        Self {
            user: user.into(),
            relay: relay.into(),
            accepted: saved
                .as_deref()
                .is_some_and(|previous| desktop_terminal::official_relay(previous)),
            saved,
            registration: None,
            error: String::new(),
        }
    }

    fn destination(&self) -> Result<String, String> {
        let target = format!("{}@{}", self.user.trim(), self.relay.trim());
        desktop_terminal::validate_destination(&target)?;
        if desktop_terminal::official_relay(&target) && !self.accepted {
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
        if ui.button(
            c,
            506,
            Rect::new((w - 114.).max(4.), 20., 92., 34.),
            "GitHub ↗",
            ButtonStyle::Quiet,
            true,
        ) {
            self.open(SOURCE);
        }
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
        let changed_user = ui.input(
            c,
            501,
            Rect::new(user_x, y, user_w.max(1.), 44.),
            &mut self.user,
            "用户名",
            false,
            true,
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
            502,
            Rect::new(relay_x, relay_y, relay_w, 44.),
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
        if changed_user || changed_relay {
            self.registration = None;
            self.error.clear();
        }
        let official = desktop_terminal::official_relay(&format!("account@{}", self.relay.trim()));
        let actions_y = relay_y + 94.;
        if official {
            let label = if self.accepted {
                "☑ 同意"
            } else {
                "☐ 同意"
            };
            if ui.button(
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
            if ui.button(
                c,
                504,
                Rect::new(x + 76., actions_y, 78., 34.),
                "用户协议",
                ButtonStyle::Quiet,
                true,
            ) {
                self.open(concat!("https://ssmcp.lmm.best", "/legal/terms"));
            }
            if ui.button(
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
        if ui.button(
            c,
            508,
            Rect::new(x + width - 92., submit_y, 92., 36.),
            "连接  →",
            ButtonStyle::Primary,
            !self.user.trim().is_empty()
                && !self.relay.trim().is_empty()
                && (!official || self.accepted),
        ) {
            match self.destination() {
                Ok(target) => return Some(target),
                Err(error) => self.error = error,
            }
        }
        if ui.button(
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
                                rt.block_on(desktop_terminal::registration_url(&destination))
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
        if let Err(error) = desktop_terminal::open_link(url) {
            self.error = format!("无法打开浏览器：{error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn official_consent_is_required_for_equivalent_origins() {
        let mut form = Launcher {
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
