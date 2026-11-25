//! Embedded terminal for the user-initiated connection. The child owns the TUI.
use ssmcp::{
    desktop_terminal::{self, TerminalReplies},
    wire::Operation,
};
use std::{
    num::NonZeroU16,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};
use tokio::sync::{Notify, mpsc, oneshot};
use zgpui::native::{Canvas, Event, Rect};

const BG: u32 = 0x172029ff;
const FG: u32 = 0xe1e8edff;
const ADVANCE: f32 = 9.;
const LEADING: f32 = 20.;
const MARGIN: f32 = 8.;
fn grid(w: f32, h: f32) -> (u16, u16) {
    (
        ((h - 2. * MARGIN) / LEADING).clamp(1., 200.) as u16,
        ((w - 2. * MARGIN) / ADVANCE).clamp(1., 300.) as u16,
    )
}
fn nz(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).unwrap()
}

fn executable() -> Result<PathBuf, String> {
    let filename = if cfg!(windows) { "ssmcp.exe" } else { "ssmcp" };
    let this = std::env::current_exe().map_err(|e| e.to_string())?;
    let paths = std::env::var_os("PATH").unwrap_or_default();
    let siblings = this.parent().into_iter().map(|dir| dir.join(filename));
    siblings
        .chain(
            std::env::split_paths(&paths)
                .filter(|p| p.is_absolute())
                .map(|p| p.join(filename)),
        )
        .find(|p| p.is_file())
        .ok_or_else(|| format!("找不到 {filename}。请将它与桌面程序放在同一目录，或安装到 PATH。"))
}

struct Frame {
    parser: vt100::Parser<TerminalReplies>,
    ended: Option<String>,
    notice: Option<String>,
}
struct Shared {
    frame: Mutex<Frame>,
    revision: AtomicU64,
    replies: Notify,
}
impl Shared {
    fn changed(&self) {
        self.revision.fetch_add(1, Ordering::Release);
    }
    fn set_notice(&self, s: String) {
        self.frame.lock().unwrap().notice = Some(s);
        self.changed();
    }
}

pub struct Console {
    shared: Arc<Shared>,
    tx: mpsc::Sender<Operation>,
    stop: Option<oneshot::Sender<()>>,
    worker: Option<thread::JoinHandle<()>>,
    size: (u16, u16),
}
impl Console {
    pub fn start(target: String, w: f32, h: f32) -> Result<Self, String> {
        let program = executable()?;
        let size = grid(w, h);
        let shared = Arc::new(Shared {
            frame: Mutex::new(Frame {
                parser: vt100::Parser::new_with_callbacks(
                    nz(size.0),
                    nz(size.1),
                    256,
                    TerminalReplies::default(),
                ),
                ended: None,
                notice: None,
            }),
            revision: AtomicU64::new(1),
            replies: Notify::new(),
        });
        let (tx, mut rx) = mpsc::channel(128);
        let (stop, mut stopped) = oneshot::channel();
        let io = shared.clone();
        let worker=thread::Builder::new().name("ssmcp-desktop-terminal".into()).spawn(move||{
            let result=(|| -> Result<String,String> {
                let runtime=tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e|e.to_string())?;
                runtime.block_on(async {
                    let output=io.clone();
                    let term=desktop_terminal::launch(&program,&target,size,move|chunk|{
                        let mut frame=output.frame.lock().unwrap();
                        frame.parser.process(chunk);
                        drop(frame);
                        output.changed();
                        output.replies.notify_one();
                    }).map_err(|e|format!("无法启动 ssmcp：{e}"))?;
                    let exit=desktop_terminal::exit_status(&term);
                    tokio::pin!(exit);
                    let outcome=loop {
                        let operation=tokio::select! {
                            biased;
                            _ = &mut stopped => break Ok("窗口已关闭。".to_owned()),
                            exit = &mut exit => break exit.map(|(code,signal)| match (code,signal) {
                                (Some(0),_) => "会话已结束。".to_owned(),
                                (Some(n),_) => format!("ssmcp 已退出，退出码 {n}。"),
                                (_,Some(n)) => format!("ssmcp 收到信号 {n} 后退出。"),
                                _ => "ssmcp 进程已退出。".to_owned()
                            }).map_err(|e|e.to_string()),
                            _=io.replies.notified() => {
                                let bytes=io.frame.lock().unwrap().parser.callbacks_mut().take_bytes();
                                if bytes.is_empty(){continue;}
                                Operation::Write { data:String::from_utf8(bytes).expect("VT responses are ASCII") }
                            },
                            op=rx.recv()=> match op {Some(v)=>v,None=>break Ok("窗口已关闭。".into())}
                        };
                        let requested=if let Operation::Write {data}=&operation {Some(data.len() as u64)}else{None};
                        match term.perform(operation).await {
                            Err(e)=> io.set_notice(format!("终端操作失败：{e}")),
                            Ok(v) if requested.is_some() && v["written"].as_u64()!=requested => io.set_notice("部分输入未确认，请核对终端画面。".into()),
                            _=>{}
                        }
                    };
                    term.shutdown().await;
                    outcome
                })
            })();
            io.frame.lock().unwrap().ended=Some(result.unwrap_or_else(|e|e));
            io.changed();
        }).map_err(|e|e.to_string())?;
        Ok(Self {
            shared,
            tx,
            stop: Some(stop),
            worker: Some(worker),
            size,
        })
    }
    pub fn revision(&self) -> u64 {
        self.shared.revision.load(Ordering::Acquire)
    }
    pub fn ended(&self) -> bool {
        self.shared.frame.lock().unwrap().ended.is_some()
    }
    fn send(&self, op: Operation) -> bool {
        if self.tx.try_send(op).is_err() {
            self.shared
                .set_notice("终端输入队列已满或已关闭，此次输入未发送。".into());
            return false;
        }
        true
    }
    pub fn event(&mut self, e: &Event) -> bool {
        if self.ended() {
            return false;
        }
        let mut frame = self.shared.frame.lock().unwrap();
        let screen = frame.parser.screen_mut();
        if let Event::Scroll(n) = e {
            let current = screen.scrollback();
            let rows = n.unsigned_abs().min(12) as usize * 3;
            screen.set_scrollback(if *n > 0 {
                current.saturating_add(rows)
            } else {
                current.saturating_sub(rows)
            });
            self.shared.changed();
            return true;
        }
        let data = key_data(e, screen.application_cursor(), screen.bracketed_paste());
        if data.is_some() {
            screen.set_scrollback(0);
        }
        drop(frame);
        if let Some(data) = data {
            if !data.is_empty() {
                self.send(Operation::Write { data });
            }
            return true;
        }
        false
    }
    pub fn draw(&mut self, c: &mut Canvas<'_>, w: f32, h: f32) {
        c.rect(Rect::new(0., 0., w, h), 0., BG);
        let new = grid(w, h);
        if new != self.size
            && !self.ended()
            && self.send(Operation::Resize {
                rows: new.0,
                cols: new.1,
            })
        {
            self.shared
                .frame
                .lock()
                .unwrap()
                .parser
                .set_size(nz(new.0), nz(new.1));
            self.size = new;
        }
        let frame = self.shared.frame.lock().unwrap();
        let s = frame.parser.screen();
        let (rows, cols) = s.size();
        for row in 0..rows.get() {
            for col in 0..cols.get() {
                let Some(cell) = s.cell(row, col) else {
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                let x = MARGIN + f32::from(col) * ADVANCE;
                let y = MARGIN + f32::from(row) * LEADING;
                let mut fg = color(cell.fgcolor(), FG);
                let mut bg = color(cell.bgcolor(), BG);
                if cell.inverse() {
                    std::mem::swap(&mut fg, &mut bg);
                }
                let width = if cell.is_wide() {
                    ADVANCE * 2.
                } else {
                    ADVANCE
                };
                if bg != BG {
                    c.rect(Rect::new(x, y, width, LEADING), 0., bg);
                }
                let text = cell.contents();
                if !text.is_empty() && text != " " {
                    c.text(text.as_ref(), x, y, 15, cell.bold(), fg);
                }
                if cell.underline() {
                    c.line((x, y + 18.), (x + width, y + 18.), 1., fg);
                }
            }
        }
        if !s.hide_cursor() && s.scrollback() == 0 && frame.ended.is_none() {
            let (r, k) = s.cursor_position();
            let x = MARGIN + f32::from(k) * ADVANCE;
            let y = MARGIN + (f32::from(r) + 1.) * LEADING - 2.;
            c.rect(Rect::new(x, y, ADVANCE, 2.), 0., FG);
        }
        if let Some(msg) = frame.ended.as_ref().or(frame.notice.as_ref()) {
            c.rect(Rect::new(0., h - 30., w, 30.), 0., 0x26323bff);
            let caption = if frame.ended.is_some() {
                format!("{msg}  按 Enter 返回连接。")
            } else {
                msg.clone()
            };
            c.ellipsis(&caption, MARGIN, h - 24., (w - 16.).max(1.), 13, false, FG);
        }
    }
}
impl Drop for Console {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn key_data(e: &Event, application: bool, bracketed: bool) -> Option<String> {
    match e {
        Event::Text(value) => Some(if bracketed {
            format!("\x1b[200~{value}\x1b[201~")
        } else {
            value.clone()
        }),
        Event::Key {
            code,
            control,
            shift,
            text,
        } => {
            if *control && ((65..=90).contains(code) || (97..=122).contains(code)) {
                return Some(char::from((*code as u8) & 31).to_string());
            }
            let key = match *code {
                8 => "\x7f",
                9 if *shift => "\x1b[Z",
                9 => "\t",
                13 => "\r",
                27 => "\x1b",
                127 => "\x1b[3~",
                1001 if application => "\x1bOD",
                1002 if application => "\x1bOC",
                1003 if application => "\x1bOA",
                1004 if application => "\x1bOB",
                1001 => "\x1b[D",
                1002 => "\x1b[C",
                1003 => "\x1b[A",
                1004 => "\x1b[B",
                1005 => "\x1b[H",
                1006 => "\x1b[F",
                1007 => "\x1b[5~",
                1008 => "\x1b[6~",
                _ => text,
            };
            Some(key.to_owned())
        }
        _ => None,
    }
}
fn color(value: vt100::Color, default: u32) -> u32 {
    fn rgb(r: u8, g: u8, b: u8) -> u32 {
        u32::from_be_bytes([r, g, b, 255])
    }
    const BASE: [u32; 16] = [
        0x20272fff, 0xd95a62ff, 0x78ae85ff, 0xd4aa64ff, 0x6c9ad5ff, 0xb38ed2ff, 0x79b9bfff,
        0xd0d8dfff, 0x77818bff, 0xfb858bff, 0xa5d6acff, 0xf1ce8aff, 0x97bcf0ff, 0xcea6e5ff,
        0x9bd5dcff, 0xf5f8faff,
    ];
    match value {
        vt100::Color::Default => default,
        vt100::Color::Rgb(r, g, b) => rgb(r, g, b),
        vt100::Color::Idx(n) if n < 16 => BASE[n as usize],
        vt100::Color::Idx(n) if n >= 232 => {
            let v = 8 + (n - 232) * 10;
            rgb(v, v, v)
        }
        vt100::Color::Idx(n) => {
            let i = n - 16;
            let level = |x| if x == 0 { 0 } else { 55 + x * 40 };
            rgb(level(i / 36), level(i / 6 % 6), level(i % 6))
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn application_keys_and_paste_follow_live_terminal_modes() {
        let key = |code, control, shift| Event::Key {
            code,
            control,
            shift,
            text: String::new(),
        };
        assert_eq!(
            key_data(&key(1001, false, false), true, false).as_deref(),
            Some("\x1bOD")
        );
        assert_eq!(
            key_data(&key(67, true, false), false, false).as_deref(),
            Some("\x03")
        );
        assert_eq!(
            key_data(&key(9, false, true), false, false).as_deref(),
            Some("\x1b[Z")
        );
        assert_eq!(
            key_data(&Event::Text("甲\n乙".into()), false, true).as_deref(),
            Some("\x1b[200~甲\n乙\x1b[201~")
        );
    }
}
