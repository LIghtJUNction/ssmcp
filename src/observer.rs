//! Reusable external terminal views for connection-owned command execution.
//! The viewer receives bytes from the actual PTY; it never runs a second command.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{sync::Notify, task::JoinHandle};

const TAIL_BYTES: usize = 128 * 1024;
const MAX_WINDOWS: usize = 48;
const VIEW_TIMEOUT: Duration = Duration::from_secs(8);
const HEARTBEAT: Duration = Duration::from_secs(6);
const IDLE_WINDOW: Duration = Duration::from_secs(180);

struct View {
    bytes: VecDeque<u8>,
    end: u64,
    seen: Option<Instant>,
    launched: Instant,
    process: Option<Child>,
    retired: bool,
    last_used: Instant,
    rows: u16,
    cols: u16,
}
struct Slot {
    id: usize,
    view: Mutex<View>,
    changed: Notify,
}
impl Slot {
    fn append(&self, bytes: &[u8]) {
        let mut view = self.view.lock().unwrap();
        let tail = &bytes[bytes.len().saturating_sub(TAIL_BYTES)..];
        let excess = (view.bytes.len() + tail.len()).saturating_sub(TAIL_BYTES);
        view.bytes.drain(..excess);
        view.bytes.extend(tail);
        view.end += bytes.len() as u64;
        drop(view);
        self.changed.notify_waiters();
    }
    fn connected(&self) -> bool {
        self.view
            .lock()
            .unwrap()
            .seen
            .is_some_and(|seen| seen.elapsed() < HEARTBEAT)
    }
    #[cfg(unix)]
    fn dimensions(&self) -> (u16, u16) {
        let view = self.view.lock().unwrap();
        (view.rows, view.cols)
    }
}
struct State {
    slots: Vec<Arc<Slot>>,
    active: HashMap<String, usize>,
}
pub(crate) struct Pool {
    terminal: PathBuf,
    allocation: tokio::sync::Mutex<()>,
    helper: PathBuf,
    directory: PathBuf,
    state: Mutex<State>,
    closed: AtomicBool,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}
pub(crate) struct Lease {
    pool: Weak<Pool>,
    key: String,
    slot: Arc<Slot>,
    finished: bool,
}
impl Lease {
    pub(crate) fn slot_id(&self) -> usize {
        self.slot.id
    }
    pub(crate) fn append(&self, bytes: &[u8]) {
        self.slot.append(bytes);
    }
    #[cfg(unix)]
    pub(crate) fn dimensions(&self) -> (u16, u16) {
        self.slot.dimensions()
    }
    pub(crate) fn finish(mut self, status: &str) {
        self.slot
            .append(format!("\r\n\x1b[0;90m[{status}]\x1b[0m\r\n").as_bytes());
        self.release();
    }
    fn release(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        if let Some(pool) = self.pool.upgrade() {
            let mut state = pool.state.lock().unwrap();
            state.active.remove(&self.key);
        }
        self.slot.view.lock().unwrap().last_used = Instant::now();
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        if !self.finished {
            self.slot
                .append(b"\r\n\x1b[0;90m[execution ended]\x1b[0m\r\n");
            self.release();
        }
    }
}

impl Pool {
    pub(crate) async fn new(terminal: &str) -> crate::Result<Arc<Self>> {
        validate_terminal(terminal)?;
        let terminal = if Path::new(terminal).components().count() > 1 {
            let path = Path::new(terminal).canonicalize()?;
            if !crate::platform::is_executable(&path) {
                return Err("--tty requires an executable file".into());
            }
            path
        } else {
            crate::program::executable(terminal)?
        };
        let mut nonce = [0u8; 8];
        crate::platform::random_bytes(&mut nonce)?;
        let suffix: String = crate::hex::encode(nonce);
        let directory =
            std::env::temp_dir().join(format!("ssmcp-tty-{}-{suffix}", std::process::id()));
        crate::platform::private_directory(&directory)?;
        let server = match crate::session::ipc::Server::bind(&directory, "view.json").await {
            Ok(server) => server,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&directory);
                return Err(error);
            }
        };
        let pool = Arc::new(Self {
            terminal,
            allocation: tokio::sync::Mutex::new(()),
            helper: std::env::current_exe()?,
            directory,
            state: Mutex::new(State {
                slots: Vec::new(),
                active: HashMap::new(),
            }),
            closed: AtomicBool::new(false),
            tasks: Mutex::new(Vec::new()),
        });
        let weak = Arc::downgrade(&pool);
        let server = Arc::new(server);
        let task = tokio::spawn(async move {
            let capacity = Arc::new(tokio::sync::Semaphore::new(MAX_WINDOWS + 4));
            loop {
                let peer = match server.accept().await {
                    Ok(peer) => peer,
                    Err(_) => break,
                };
                let Ok(permit) = capacity.clone().try_acquire_owned() else {
                    continue;
                };
                let weak = weak.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let mut peer = peer;
                    let response = match peer.read().await {
                        Ok(request) => match weak.upgrade() {
                            Some(pool) => pool.read_view(request).await,
                            None => Ok(json!({"closed":true})),
                        },
                        Err(error) => Err(error.to_string()),
                    };
                    let _ = peer.reply(response).await;
                });
            }
        });
        pool.tasks.lock().unwrap().push(task);
        let slot = pool.add_slot()?;
        slot.append(b"\x1b[0;90mssmcp \xc2\xb7 waiting for commands\x1b[0m\r\n");
        pool.launch(&slot)?;
        pool.wait_for_view(&slot).await?;
        let weak = Arc::downgrade(&pool);
        pool.tasks.lock().unwrap().push(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let Some(pool) = weak.upgrade() else { break };
                if pool.closed.load(Ordering::Acquire) {
                    break;
                }
                pool.retire_idle();
                let slots = pool.state.lock().unwrap().slots.clone();
                for slot in slots {
                    let reopen = {
                        let mut view = slot.view.lock().unwrap();
                        if let Some(child) = view.process.as_mut() {
                            let _ = child.try_wait();
                        }
                        !view.retired
                            && view.launched.elapsed() > HEARTBEAT
                            && !view.seen.is_some_and(|seen| seen.elapsed() < HEARTBEAT)
                    };
                    if reopen && let Err(error) = pool.launch(&slot) {
                        eprintln!("ssmcp: command observer could not reopen: {error}");
                    }
                }
            }
        }));
        Ok(pool)
    }
    fn retire_idle(&self) {
        let state = self.state.lock().unwrap();
        for slot in &state.slots {
            if slot.id == 1 || state.active.values().any(|id| *id == slot.id) {
                continue;
            }
            let mut view = slot.view.lock().unwrap();
            if view.retired || view.last_used.elapsed() < IDLE_WINDOW {
                continue;
            }
            view.retired = true;
            view.seen = None;
            view.bytes.clear();
            view.bytes.shrink_to_fit();
            view.end = 0;
            if let Some(mut child) = view.process.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            drop(view);
            slot.changed.notify_waiters();
        }
    }
    fn add_slot(&self) -> Result<Arc<Slot>, String> {
        let mut state = self.state.lock().unwrap();
        if state.slots.len() >= MAX_WINDOWS {
            return Err("command observer window limit reached".into());
        }
        let slot = Arc::new(Slot {
            id: state.slots.len() + 1,
            view: Mutex::new(View {
                bytes: VecDeque::new(),
                end: 0,
                seen: None,
                launched: Instant::now(),
                process: None,
                retired: false,
                last_used: Instant::now(),
                rows: 30,
                cols: 100,
            }),
            changed: Notify::new(),
        });
        state.slots.push(slot.clone());
        Ok(slot)
    }
    fn launch(&self, slot: &Slot) -> Result<(), String> {
        let mut view = slot.view.lock().unwrap();
        if !view.retired && view.process.is_some() && view.launched.elapsed() < HEARTBEAT {
            return Ok(());
        }
        if let Some(mut child) = view.process.take() {
            // The launcher may have handed off to an existing terminal server.
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
        let args = launcher_args(&self.terminal, &self.helper, &self.directory, slot.id);
        let child = Command::new(&self.terminal)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("could not open {}: {error}", self.terminal.display()))?;
        view.process = Some(child);
        view.launched = Instant::now();
        view.seen = None;
        view.retired = false;
        view.last_used = Instant::now();
        Ok(())
    }
    async fn wait_for_view(&self, slot: &Slot) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + VIEW_TIMEOUT;
        loop {
            let notified = slot.changed.notified();
            if slot.connected() {
                return Ok(());
            }
            {
                let mut view = slot.view.lock().unwrap();
                if let Some(child) = view.process.as_mut()
                    && let Some(status) = child.try_wait().map_err(|error| error.to_string())?
                    && !status.success()
                {
                    return Err(format!("--tty launcher exited with {status}"));
                }
            }
            tokio::select! {
                _ = notified => {},
                _ = tokio::time::sleep(Duration::from_millis(50)) => {},
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "--tty {} did not open its observer within {} seconds; check DISPLAY/Wayland and the terminal's execute-command option",
                    self.terminal.display(),
                    VIEW_TIMEOUT.as_secs()
                ));
            }
        }
    }
    pub(crate) async fn begin(
        self: &Arc<Self>,
        key: String,
        command: &Value,
        cwd: &Path,
    ) -> Result<Lease, String> {
        if self.closed.load(Ordering::Acquire) {
            return Err("command observer is closed".into());
        }
        let allocation = self.allocation.lock().await;
        let available = {
            let state = self.state.lock().unwrap();
            state
                .slots
                .iter()
                .find(|slot| !state.active.values().any(|id| *id == slot.id))
                .cloned()
        };
        let slot = match available {
            Some(slot) => slot,
            None => {
                let slot = self.add_slot()?;
                self.launch(&slot)?;
                slot
            }
        };
        {
            let mut state = self.state.lock().unwrap();
            state.active.insert(key.clone(), slot.id);
        }
        drop(allocation);
        let lease = Lease {
            pool: Arc::downgrade(self),
            key,
            slot: slot.clone(),
            finished: false,
        };
        if !slot.connected() {
            let needs_launch = {
                let view = slot.view.lock().unwrap();
                view.retired || view.process.is_none() || view.launched.elapsed() > HEARTBEAT
            };
            if needs_launch {
                self.launch(&slot)?
            }
            self.wait_for_view(&slot).await?;
        }
        slot.append(
            format!(
                "\r\n\x1b[0;36m{}\x1b[0m\r\n$ {}\r\n",
                visible(&cwd.to_string_lossy()),
                display_command(command)
            )
            .as_bytes(),
        );
        Ok(lease)
    }
    async fn read_view(&self, request: Value) -> Result<Value, String> {
        let id = request["slot"].as_u64().ok_or("missing observer slot")? as usize;
        let cursor = request["cursor"].as_u64().unwrap_or(0);
        let slot = self
            .state
            .lock()
            .unwrap()
            .slots
            .iter()
            .find(|slot| slot.id == id)
            .cloned()
            .ok_or("unknown observer slot")?;
        if self.closed.load(Ordering::Acquire) {
            return Ok(json!({"closed":true}));
        }
        let empty = {
            let mut view = slot.view.lock().unwrap();
            if view.retired {
                return Ok(json!({"closed":true}));
            }
            view.seen = Some(Instant::now());
            view.rows = request["rows"].as_u64().unwrap_or(30).clamp(1, 500) as u16;
            view.cols = request["cols"].as_u64().unwrap_or(100).clamp(1, 1000) as u16;
            cursor == view.end
        };
        slot.changed.notify_waiters();
        let notified = slot.changed.notified();
        if empty {
            let _ = tokio::time::timeout(Duration::from_millis(1000), notified).await;
        }
        let view = slot.view.lock().unwrap();
        if view.retired {
            return Ok(json!({"closed":true}));
        }
        if cursor > view.end {
            return Err("observer cursor is ahead".into());
        }
        let start = view.end - view.bytes.len() as u64;
        let offset = cursor.max(start);
        let bytes: Vec<u8> = view
            .bytes
            .iter()
            .skip((offset - start) as usize)
            .take(32768)
            .copied()
            .collect();
        Ok(
            json!({"data_base64":STANDARD.encode(&bytes),"next_cursor":offset+bytes.len() as u64,
            "truncated":cursor<start,"closed":self.closed.load(Ordering::Acquire)}),
        )
    }
    pub(crate) async fn terminal_begin(
        self: &Arc<Self>,
        id: &str,
        cwd: &Path,
    ) -> Result<(), String> {
        let key = format!("terminal:{id}");
        if self.state.lock().unwrap().active.contains_key(&key) {
            return Ok(());
        }
        let mut lease = self
            .begin(key, &json!(format!("interactive terminal {id}")), cwd)
            .await?;
        // Interactive shells own their slot until the shell closes.
        lease.finished = true;
        Ok(())
    }
    pub(crate) fn terminal_output(&self, id: &str, bytes: &[u8]) {
        self.append_key(&format!("terminal:{id}"), bytes);
    }
    pub(crate) fn terminal_closed(&self, id: &str) {
        let key = format!("terminal:{id}");
        self.append_key(&key, b"\r\n[terminal closed]\r\n");
        let slot = {
            let mut state = self.state.lock().unwrap();
            let id = state.active.remove(&key);
            id.and_then(|id| state.slots.iter().find(|slot| slot.id == id).cloned())
        };
        if let Some(slot) = slot {
            slot.view.lock().unwrap().last_used = Instant::now();
        }
    }
    fn append_key(&self, key: &str, bytes: &[u8]) {
        let slot = {
            let state = self.state.lock().unwrap();
            let Some(id) = state.active.get(key).copied() else {
                return;
            };
            state.slots.iter().find(|slot| slot.id == id).cloned()
        };
        if let Some(slot) = slot {
            slot.append(bytes)
        }
    }
    pub(crate) fn shutdown(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        for slot in &self.state.lock().unwrap().slots {
            slot.changed.notify_waiters();
        }
    }
}
impl Drop for Pool {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        for task in self.tasks.get_mut().unwrap().drain(..) {
            task.abort()
        }
        for slot in &self.state.get_mut().unwrap().slots {
            if let Some(mut child) = slot.view.lock().unwrap().process.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}
pub(crate) fn validate_terminal(terminal: &str) -> Result<(), String> {
    if terminal.is_empty() || terminal.len() > 4096 || terminal.chars().any(char::is_control) {
        return Err("--tty requires a terminal executable name or path".into());
    }
    Ok(())
}
fn launcher_args(
    terminal: &Path,
    helper: &Path,
    directory: &Path,
    slot: usize,
) -> Vec<std::ffi::OsString> {
    let name = terminal
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    let mut args: Vec<std::ffi::OsString> = match name.as_str() {
        "kitty" => vec![
            "--title".into(),
            format!("ssmcp · commands {slot}").into(),
            "--".into(),
        ],
        "gnome-terminal" | "kgx" | "foot" | "footclient" => vec!["--".into()],
        "wezterm" => vec!["start".into(), "--".into()],
        "wt" => vec!["new-tab".into(), "--".into()],
        _ => vec!["-e".into()],
    };
    args.extend([
        helper.as_os_str().to_owned(),
        "__tty-view".into(),
        directory.as_os_str().to_owned(),
        slot.to_string().into(),
    ]);
    args
}
fn visible(text: &str) -> String {
    text.chars()
        .flat_map(|ch| {
            if ch == '\n' || ch == '\t' || !ch.is_control() {
                vec![ch]
            } else {
                ch.escape_default().collect()
            }
        })
        .collect()
}
fn quote(text: &str) -> String {
    if !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_alphanumeric() || "_./:=,@%+-".contains(c))
    {
        text.into()
    } else {
        format!("'{}'", visible(text).replace('\'', "'\\''"))
    }
}
fn display_command(command: &Value) -> String {
    command
        .as_array()
        .map(|args| {
            args.iter()
                .map(|arg| quote(arg.as_str().unwrap_or("")))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_else(|| visible(command.as_str().unwrap_or("command")))
}
pub(crate) async fn viewer(directory: &Path, slot: usize) -> crate::Result<()> {
    let mut cursor = 0;
    loop {
        let (rows, cols) = viewer_size();
        let response = tokio::select! {
            value=crate::session::ipc::request(directory,"view.json",json!({"slot":slot,"cursor":cursor,"rows":rows,"cols":cols}))=>value?,
            _=tokio::signal::ctrl_c()=>return Ok(()),
        };
        if response["closed"] == true {
            return Ok(());
        }
        let bytes = STANDARD.decode(
            response["data_base64"]
                .as_str()
                .ok_or("observer output missing")?,
        )?;
        let mut stdout = io::stdout().lock();
        if response["truncated"] == true {
            stdout.write_all(b"\r\n[older output retained in agent output artifacts]\r\n")?
        }
        stdout.write_all(&bytes)?;
        stdout.flush()?;
        cursor = response["next_cursor"]
            .as_u64()
            .ok_or("observer cursor missing")?;
    }
}
fn viewer_size() -> (u16, u16) {
    #[cfg(unix)]
    {
        let mut size = libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: stdout is borrowed, size is initialized writable storage.
        if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0
            && size.ws_row > 0
            && size.ws_col > 0
        {
            return (size.ws_row, size.ws_col);
        }
    }
    (30, 100)
}
#[cfg(unix)]
pub(crate) mod pty {
    use super::{Lease, Slot};
    use crate::command_runtime::{Driver, JoinHandle, open_pty, resize_pty};
    use std::{fs::File, io, process::Stdio, sync::Arc, time::Duration};
    use zrun::io::{AsyncFd, Nonblocking};

    pub(crate) struct Output {
        master: AsyncFd<File>,
        resize: JoinHandle<()>,
    }

    impl Output {
        pub(crate) fn open(lease: &Lease) -> io::Result<(Self, Stdio, Stdio)> {
            let (rows, cols) = lease.dimensions();
            let (master, slave) = open_pty(rows, cols)?;
            let stdout = Stdio::from(slave.try_clone()?);
            let resize_fd = master.try_clone()?;
            let driver = Driver::get()?;
            let master = AsyncFd::new(&driver.io, master, Nonblocking::Set)?;
            let slot = Arc::downgrade(&lease.slot);
            let resize = driver.spawn(async move {
                let mut previous = (rows, cols);
                while driver.timer.sleep(Duration::from_millis(250)).await.is_ok() {
                    let Some(slot) = slot.upgrade() else { break };
                    let dimensions = Slot::dimensions(&slot);
                    if dimensions != previous {
                        let _ = resize_pty(&resize_fd, dimensions.0, dimensions.1);
                        previous = dimensions;
                    }
                }
            })?;
            Ok((Self { master, resize }, stdout, Stdio::from(slave)))
        }

        pub(crate) async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.master.read(buf).await {
                Err(error) if error.raw_os_error() == Some(libc::EIO) => Ok(0),
                result => result,
            }
        }
    }

    impl Drop for Output {
        fn drop(&mut self) {
            self.resize.abort()
        }
    }
}

#[cfg(test)]
mod tests;
