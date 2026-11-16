//! Native macOS PTYs and Windows ConPTY, with bounded cursor-addressed output.
use crate::{
    output::{Capture, INLINE_BYTES, INLINE_LINES, OutputStore},
    wire::Operation,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Notify, mpsc, oneshot},
    time::{Instant, timeout, timeout_at},
};

const OUTPUT_CAPACITY: usize = 1024 * 1024;

#[derive(Default)]
struct Output {
    bytes: VecDeque<u8>,
    start: u64,
    end: u64,
    eof: bool,
    capture: Option<Capture>,
}

impl Output {
    fn push(&mut self, data: &[u8]) {
        if self.eof {
            return;
        }
        if let Some(capture) = &mut self.capture {
            let _ = capture.push(data);
        }
        let retained = &data[data.len().saturating_sub(OUTPUT_CAPACITY)..];
        let excess = (self.bytes.len() + retained.len()).saturating_sub(OUTPUT_CAPACITY);
        self.bytes.drain(..excess);
        self.bytes.extend(retained);
        self.end += data.len() as u64;
        self.start = self.end - self.bytes.len() as u64;
    }
}

struct Input {
    bytes: Vec<u8>,
    complete: oneshot::Sender<io::Result<()>>,
}

pub struct Terminal {
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    child: Mutex<Option<Box<dyn Child + Send + Sync>>>,
    input: Mutex<Option<mpsc::Sender<Input>>>,
    output: Arc<Mutex<Output>>,
    changed: Arc<Notify>,
    closed: AtomicBool,
    pid: u32,
    #[cfg(windows)]
    job: crate::platform::WindowsJob,
}

impl Terminal {
    pub fn spawn(shell: &Path) -> crate::Result<Arc<Self>> {
        Self::spawn_with_logging(shell, false, OutputStore::global()?)
    }

    pub fn spawn_live(shell: &Path) -> crate::Result<Arc<Self>> {
        Self::spawn_with_logging(shell, true, OutputStore::global()?)
    }

    pub fn spawn_live_with_store(
        shell: &Path,
        output: Arc<OutputStore>,
    ) -> crate::Result<Arc<Self>> {
        Self::spawn_with_logging(shell, true, output)
    }

    fn spawn_with_logging(
        shell: &Path,
        live: bool,
        store: Arc<OutputStore>,
    ) -> crate::Result<Arc<Self>> {
        Self::spawn_configured(shell, None, live, store, "main")
    }

    pub fn spawn_named_with_store(
        shell: &Path,
        cwd: &Path,
        output: Arc<OutputStore>,
        terminal_id: &str,
    ) -> crate::Result<Arc<Self>> {
        Self::spawn_configured(shell, Some(cwd), true, output, terminal_id)
    }

    fn spawn_configured(
        shell: &Path,
        cwd: Option<&Path>,
        live: bool,
        store: Arc<OutputStore>,
        display_id: &str,
    ) -> crate::Result<Arc<Self>> {
        if !shell.is_absolute() {
            return Err("shell must be an absolute path".into());
        }
        let initial_size = if live {
            let (rows, cols) = crate::client::terminal::pane_size(24, 80);
            PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            }
        } else {
            PtySize::default()
        };
        let pair = native_pty_system()
            .openpty(initial_size)
            .map_err(io::Error::other)?;
        let mut reader = pair.master.try_clone_reader().map_err(io::Error::other)?;
        let mut writer = pair.master.take_writer().map_err(io::Error::other)?;
        let mut command = CommandBuilder::new(shell);
        #[cfg(unix)]
        command.arg("-i");
        #[cfg(windows)]
        if shell
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|name| {
                matches!(
                    name.to_ascii_lowercase().as_str(),
                    "bash" | "zsh" | "sh" | "fish"
                )
            })
        {
            command.arg("-i");
        }
        #[cfg(windows)]
        if shell.file_stem().is_some_and(|name| {
            name.eq_ignore_ascii_case("powershell") || name.eq_ignore_ascii_case("pwsh")
        }) {
            command.arg("-NoLogo");
        }
        command.env("TERM", "xterm-256color");
        command.cwd(
            cwd.map(Path::to_path_buf)
                .unwrap_or(std::env::current_dir()?),
        );
        #[allow(unused_mut)]
        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(io::Error::other)?;
        drop(pair.slave);
        let pid = child.process_id().ok_or("shell has no process ID")?;
        #[cfg(windows)]
        let job = match child
            .as_raw_handle()
            .ok_or_else(|| io::Error::other("shell has no process handle"))
            .and_then(crate::platform::WindowsJob::attach)
        {
            Ok(job) => job,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.into());
            }
        };
        let (input, mut input_rx) = mpsc::channel::<Input>(16);
        let terminal = Arc::new(Self {
            master: Mutex::new(Some(pair.master)),
            child: Mutex::new(Some(child)),
            input: Mutex::new(Some(input)),
            output: Arc::new(Mutex::new(Output {
                capture: Some(store.capture("terminal_session")),
                ..Output::default()
            })),
            changed: Arc::new(Notify::new()),
            closed: AtomicBool::new(false),
            pid,
            #[cfg(windows)]
            job,
        });
        #[cfg(windows)]
        let (startup_sender, startup_receiver) = std::sync::mpsc::sync_channel(1);
        #[cfg(windows)]
        let startup_input = terminal.input.lock().unwrap().clone().unwrap();
        // Separate OS threads keep blocking native PTY pipes off the async
        // executor. The reader must drain while ConPTY closes (Windows < 24H2).
        let display_id = display_id.to_owned();
        let output = terminal.output.clone();
        let changed = terminal.changed.clone();
        std::thread::Builder::new()
            .name("ssmcp-pty-read".into())
            .spawn(move || {
                let mut data = [0_u8; 8192];
                #[cfg(windows)]
                let mut startup_sender = Some(startup_sender);
                #[cfg(windows)]
                let mut startup_input = Some(startup_input);
                #[cfg(windows)]
                let mut startup_bytes = Vec::new();
                loop {
                    match reader.read(&mut data) {
                        Ok(0) => break,
                        Ok(n) => {
                            #[cfg(windows)]
                            let visible = if startup_sender.is_some() {
                                startup_bytes.extend_from_slice(&data[..n]);
                                let replied = answer_initial_cursor(
                                    &mut startup_bytes,
                                    startup_input.as_ref().unwrap(),
                                );
                                match replied {
                                    Ok(false) if startup_bytes.len() < 65536 => continue,
                                    Ok(true) => {
                                        let _ = startup_sender.take().unwrap().send(Ok(()));
                                        startup_input.take();
                                        startup_bytes.as_slice()
                                    }
                                    other => {
                                        let error = other.err().unwrap_or_else(|| {
                                            "ConPTY startup cursor query exceeded its limit".into()
                                        });
                                        let _ =
                                            startup_sender.take().unwrap().send(Err(error.clone()));
                                        if let Some(capture) = &mut output.lock().unwrap().capture {
                                            capture.fail(error);
                                        }
                                        break;
                                    }
                                }
                            } else {
                                &data[..n]
                            };
                            #[cfg(unix)]
                            let visible = &data[..n];
                            output.lock().unwrap().push(visible);
                            if live {
                                crate::client::render::terminal_output_for(&display_id, visible);
                            }
                            #[cfg(windows)]
                            startup_bytes.clear();
                            changed.notify_waiters();
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => {
                            #[cfg(unix)]
                            let normal_eof = error.raw_os_error() == Some(libc::EIO);
                            #[cfg(windows)]
                            let normal_eof = matches!(
                                error.kind(),
                                io::ErrorKind::BrokenPipe | io::ErrorKind::UnexpectedEof
                            );
                            if !normal_eof
                                && let Some(capture) = &mut output.lock().unwrap().capture
                            {
                                capture.fail(format!("terminal read failed: {error}"));
                            }
                            break;
                        }
                    }
                }
                let mut output = output.lock().unwrap();
                output.eof = true;
                if let Some(capture) = &mut output.capture {
                    let _ = capture.finish();
                }
                drop(output);
                changed.notify_waiters();
            })?;
        std::thread::Builder::new()
            .name("ssmcp-pty-write".into())
            .spawn(move || {
                while let Some(input) = input_rx.blocking_recv() {
                    let result = writer.write_all(&input.bytes).and_then(|()| writer.flush());
                    let failed = result.is_err();
                    let _ = input.complete.send(result);
                    if failed {
                        break;
                    }
                }
            })?;
        #[cfg(windows)]
        startup_receiver
            .recv_timeout(Duration::from_secs(10))
            .map_err(|error| format!("ConPTY startup handshake failed: {error}"))??;
        #[cfg(unix)]
        {
            let weak = Arc::downgrade(&terminal);
            std::thread::Builder::new()
                .name("ssmcp-pty-exit".into())
                .spawn(move || {
                    if matches!(
                        crate::platform::wait_unix_child_unreaped(pid as libc::pid_t, false),
                        Ok(true)
                    ) && let Some(terminal) = weak.upgrade()
                    {
                        // stop owns the only transition that moves/reaps the
                        // child; a concurrent shutdown simply wins that race.
                        terminal.stop();
                    }
                })?;
        }
        Ok(terminal)
    }

    pub async fn perform(&self, operation: Operation) -> Result<Value, String> {
        operation.validate()?;
        match operation {
            Operation::Read {
                cursor,
                max_bytes,
                wait_ms,
            } => self.read(cursor, max_bytes, wait_ms).await,
            Operation::Write { data } => {
                if !self.running() {
                    return Err("terminal has exited".into());
                }
                let sender = self
                    .input
                    .lock()
                    .unwrap()
                    .clone()
                    .ok_or("terminal is closed")?;
                let (complete, completed) = oneshot::channel();
                let bytes = data.len();
                timeout(Duration::from_secs(5), async {
                    sender
                        .send(Input {
                            bytes: data.into_bytes(),
                            complete,
                        })
                        .await
                        .map_err(|_| "terminal writer has exited".to_owned())?;
                    completed
                        .await
                        .map_err(|_| "terminal writer has exited".to_owned())?
                        .map_err(|error| error.to_string())
                })
                .await
                .map_err(|_| {
                    "terminal write timed out; input may be partially written".to_owned()
                })??;
                Ok(json!({"written":bytes}))
            }
            Operation::Resize { rows, cols } => {
                if !self.running() {
                    return Err("terminal has exited".into());
                }
                self.master
                    .lock()
                    .unwrap()
                    .as_ref()
                    .ok_or("terminal is closed")?
                    .resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    })
                    .map_err(|error| error.to_string())?;
                Ok(json!({"rows":rows,"cols":cols}))
            }
            Operation::Status {} => {
                let running = self.running();
                let output = self.output.lock().unwrap();
                let mut result = json!({"running":running,"pid":self.pid,"start_cursor":output.start,"next_cursor":output.end,"output_eof":output.eof});
                if let Some(capture) = &output.capture {
                    if let Some(artifact) = capture.metadata() {
                        result["output_artifact"] = json!(artifact);
                    }
                    if let Some(error) = capture.error() {
                        result["output_error"] = json!(error);
                    }
                }
                Ok(result)
            }
            Operation::ListTools { .. } | Operation::CallTool { .. } => {
                Err("not a terminal operation".into())
            }
        }
    }

    fn running(&self) -> bool {
        if self.closed.load(Ordering::Acquire) {
            return false;
        }
        #[cfg(windows)]
        {
            let running = self
                .child
                .lock()
                .unwrap()
                .as_mut()
                .is_some_and(|child| matches!(child.try_wait(), Ok(None)));
            if !running {
                self.stop();
            }
            running
        }
        #[cfg(unix)]
        {
            matches!(
                crate::platform::wait_unix_child_unreaped(self.pid as libc::pid_t, true),
                Ok(false)
            )
        }
    }

    async fn read(&self, cursor: u64, max_bytes: usize, wait_ms: u64) -> Result<Value, String> {
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let snapshot = {
                let output = self.output.lock().unwrap();
                if cursor > output.end {
                    return Err("cursor is ahead of terminal output".into());
                }
                if cursor < output.end
                    || output.eof
                    || Instant::now() >= deadline
                    || self.closed.load(Ordering::Acquire)
                {
                    let from = cursor.max(output.start);
                    let pending_lines = output
                        .bytes
                        .iter()
                        .skip((from - output.start) as usize)
                        .filter(|byte| **byte == b'\n')
                        .take(INLINE_LINES as usize + 1)
                        .count() as u64
                        + u64::from(
                            output.end > from
                                && output.bytes.back().is_some_and(|byte| *byte != b'\n'),
                        );
                    let long =
                        output.end - cursor > INLINE_BYTES as u64 || pending_lines > INLINE_LINES;
                    if long {
                        let capture = output
                            .capture
                            .as_ref()
                            .ok_or("terminal output capture unavailable")?;
                        let artifact = capture.metadata().ok_or_else(|| {
                            capture
                                .error()
                                .unwrap_or("terminal output could not be spilled")
                                .to_owned()
                        })?;
                        let mut result = json!({"text":"","data_base64":"","next_cursor":output.end,
                            "truncated":capture.error().is_some(),"output_eof":output.eof,"spilled":true,
                            "range":{"start":cursor,"end":output.end,"stored_end":artifact.bytes.min(output.end)},
                            "artifact":artifact});
                        if let Some(error) = capture.error() {
                            result["output_error"] = json!(error);
                        }
                        Some(result)
                    } else {
                        let bytes: Vec<u8> = output
                            .bytes
                            .iter()
                            .skip((from - output.start) as usize)
                            .take(max_bytes)
                            .copied()
                            .collect();
                        let mut result = json!({"text":String::from_utf8_lossy(&bytes),"data_base64":STANDARD.encode(&bytes),
                            "next_cursor":from + bytes.len() as u64,"truncated":cursor < output.start,"output_eof":output.eof});
                        if let Some(error) = output.capture.as_ref().and_then(Capture::error) {
                            result["output_error"] = json!(error);
                        }
                        Some(result)
                    }
                } else {
                    None
                }
            };
            if let Some(mut snapshot) = snapshot {
                snapshot["running"] = json!(self.running());
                return Ok(snapshot);
            }
            let _ = timeout_at(deadline, changed).await;
        }
    }

    fn stop(&self) -> Option<oneshot::Receiver<()>> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return None;
        }
        #[cfg(windows)]
        self.job.terminate();
        #[cfg(unix)]
        crate::platform::stop_unix_descendants(self.pid as libc::pid_t);
        self.input.lock().unwrap().take();
        let mut child = self.child.lock().unwrap().take();
        if let Some(child) = child.as_mut() {
            let _ = child.kill();
        }
        let master = self.master.lock().unwrap().take();
        let (complete, completed) = oneshot::channel();
        // ClosePseudoConsole can wait for its clients on older Windows versions;
        // the dedicated reader remains alive to drain output during this close.
        let _ = std::thread::Builder::new()
            .name("ssmcp-pty-close".into())
            .spawn(move || {
                drop(master);
                if let Some(mut child) = child {
                    let _ = child.wait();
                }
                let _ = complete.send(());
            });
        self.changed.notify_waiters();
        Some(completed)
    }

    pub async fn shutdown(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        if let Some(completed) = self.stop() {
            let _ = timeout_at(deadline, completed).await;
        }
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.output.lock().unwrap().eof {
                break;
            }
            if timeout_at(deadline, changed).await.is_err() {
                break;
            }
        }
        let mut output = self.output.lock().unwrap();
        if !output.eof
            && let Some(capture) = &mut output.capture
        {
            capture.fail("terminal was stopped before output EOF; capture may be incomplete");
            let _ = capture.finish();
        }
        output.eof = true;
        drop(output);
        self.changed.notify_waiters();
    }
}

#[cfg(windows)]
fn answer_initial_cursor(bytes: &mut Vec<u8>, input: &mpsc::Sender<Input>) -> Result<bool, String> {
    let Some(offset) = bytes.windows(4).position(|bytes| bytes == b"\x1b[6n") else {
        return Ok(false);
    };
    // portable-pty enables PSEUDOCONSOLE_INHERIT_CURSOR. Reply before opening
    // the session to user input, otherwise ConPTY will not process that input.
    let (complete, completed) = oneshot::channel();
    input
        .blocking_send(Input {
            bytes: b"\x1b[1;1R".to_vec(),
            complete,
        })
        .map_err(|_| "terminal writer exited during startup".to_owned())?;
    completed
        .blocking_recv()
        .map_err(|_| "terminal writer exited during startup".to_owned())?
        .map_err(|error| format!("ConPTY cursor response failed: {error}"))?;
    // This request was answered locally; forwarding it would cause the user's
    // own terminal to send an unrelated second response.
    bytes.drain(offset..offset + 4);
    Ok(true)
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ConPTY is a VT terminal stream, not a line-oriented stdout pipe. Parse the
    // accumulated bytes so a read may end inside a control sequence or marker.
    fn visible_text(bytes: &[u8]) -> String {
        let mut visible = Vec::new();
        let mut state = 0;
        for &byte in bytes {
            state = match (state, byte) {
                (0, 0x1b) => 1,
                (0, byte) => {
                    if !byte.is_ascii_control() || matches!(byte, b'\r' | b'\n' | b'\t') {
                        visible.push(byte);
                    }
                    0
                }
                (1, b'[') => 2,
                (1, b']') => 3,
                (1, _) => 0,
                (2, 0x40..=0x7e) => 0,
                (2, _) => 2,
                (3, 0x07) | (4, b'\\') => 0,
                (3, 0x1b) => 4,
                (3 | 4, _) => 3,
                _ => unreachable!(),
            };
        }
        String::from_utf8_lossy(&visible).into_owned()
    }

    #[test]
    fn terminal_marker_survives_vt_sequences_and_partial_reads() {
        let chunks: &[&[u8]] = &[
            b"\x1b]0;cmd.exe",
            b"\x07\x1b[2J\x1b[HSSMCP_",
            b"PORTABLE_\x1b[",
            b"0mOK\x1b[?25h\r\n",
        ];
        let mut bytes = Vec::new();
        for chunk in &chunks[..3] {
            bytes.extend_from_slice(chunk);
            assert!(!visible_text(&bytes).contains("SSMCP_PORTABLE_OK"));
        }
        bytes.extend_from_slice(chunks[3]);
        assert!(visible_text(&bytes).contains("SSMCP_PORTABLE_OK"));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn exiting_terminal_stops_jobs_in_separate_process_groups() {
        let terminal = Terminal::spawn(Path::new("/bin/sh")).unwrap();
        terminal
            .perform(Operation::Write {
                data: "stty -echo; trap '' HUP; sleep 60 & printf SSMCP_CHILD=; jobs -p; exit\n"
                    .into(),
            })
            .await
            .unwrap();
        let mut output = String::new();
        let mut cursor = 0;
        let eof = timeout(Duration::from_secs(5), async {
            loop {
                let page = terminal
                    .perform(Operation::Read {
                        cursor,
                        max_bytes: 32768,
                        wait_ms: 250,
                    })
                    .await
                    .unwrap();
                cursor = page["next_cursor"].as_u64().unwrap();
                output.push_str(page["text"].as_str().unwrap());
                if page["output_eof"] == true {
                    break;
                }
            }
        })
        .await;
        let pid = output.split("SSMCP_CHILD=").skip(1).find_map(|tail| {
            tail.split(|c: char| !c.is_ascii_digit())
                .next()?
                .parse::<i32>()
                .ok()
                .filter(|pid| *pid > 1)
        });
        let stopped = if let Some(pid) = pid {
            timeout(Duration::from_secs(5), async {
                loop {
                    let state = tokio::process::Command::new("/bin/ps")
                        .args(["-o", "stat=", "-p", &pid.to_string()])
                        .output()
                        .await
                        .unwrap();
                    let state = String::from_utf8_lossy(&state.stdout);
                    if state.trim().is_empty() || state.trim_start().starts_with('Z') {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .is_ok()
        } else {
            false
        };
        if !stopped && let Some(pid) = pid {
            // Do not leave the fixture running if the regression is reintroduced.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        terminal.shutdown().await;
        assert!(eof.is_ok(), "PTY did not reach EOF: {output:?}");
        assert!(stopped, "background job remained alive: {output:?}");
    }

    #[tokio::test]
    async fn native_terminal_executes_and_resizes() {
        #[cfg(windows)]
        let shell = std::env::var_os("COMSPEC")
            .map(std::path::PathBuf::from)
            .expect("COMSPEC");
        #[cfg(unix)]
        let shell = std::path::PathBuf::from("/bin/sh");
        let terminal = Terminal::spawn(&shell).unwrap();
        terminal
            .perform(Operation::Resize {
                rows: 30,
                cols: 100,
            })
            .await
            .unwrap();
        #[cfg(windows)]
        // Enter is a carriage return in ConPTY's VT input protocol. The escaped
        // suffix ensures the marker cannot be satisfied by input echo alone.
        let command = "echo SSMCP_PORTABLE_^O^K\r";
        #[cfg(unix)]
        let command = "printf 'SSMCP_PORTABLE_%s\\n' OK\n";
        assert!(!command.contains("SSMCP_PORTABLE_OK"));
        terminal
            .perform(Operation::Write {
                data: command.into(),
            })
            .await
            .unwrap();
        let mut cursor = 0;
        let mut output = Vec::new();
        let observed = timeout(Duration::from_secs(10), async {
            loop {
                let result = terminal
                    .perform(Operation::Read {
                        cursor,
                        max_bytes: 32768,
                        wait_ms: 500,
                    })
                    .await?;
                cursor = result["next_cursor"].as_u64().unwrap();
                output.extend_from_slice(
                    &STANDARD
                        .decode(result["data_base64"].as_str().unwrap())
                        .unwrap(),
                );
                if visible_text(&output).contains("SSMCP_PORTABLE_OK") {
                    return Ok::<_, String>(());
                }
                if result["output_eof"] == true {
                    return Err("terminal reached EOF before executing the command".into());
                }
            }
        })
        .await;
        let status = terminal.perform(Operation::Status {}).await.unwrap();
        terminal.shutdown().await;
        assert!(
            matches!(observed, Ok(Ok(()))),
            "PTY execution failed: {observed:?}; status={status}; raw={:?}; visible={:?}",
            String::from_utf8_lossy(&output),
            visible_text(&output),
        );
        assert_eq!(
            terminal.perform(Operation::Status {}).await.unwrap()["running"],
            false
        );
    }
}
