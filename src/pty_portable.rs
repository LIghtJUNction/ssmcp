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
        Self::spawn_with_logging(shell, false)
    }

    pub fn spawn_live(shell: &Path) -> crate::Result<Arc<Self>> {
        Self::spawn_with_logging(shell, true)
    }

    fn spawn_with_logging(shell: &Path, live: bool) -> crate::Result<Arc<Self>> {
        if !shell.is_absolute() {
            return Err("shell must be an absolute path".into());
        }
        let output_store = OutputStore::global()?;
        let pair = native_pty_system()
            .openpty(PtySize::default())
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
        command.cwd(std::env::current_dir()?);
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
                capture: Some(output_store.capture("terminal_session")),
                ..Output::default()
            })),
            changed: Arc::new(Notify::new()),
            closed: AtomicBool::new(false),
            pid,
            #[cfg(windows)]
            job,
        });
        // Separate OS threads keep blocking native PTY pipes off the async
        // executor. The reader must drain while ConPTY closes (Windows < 24H2).
        let output = terminal.output.clone();
        let changed = terminal.changed.clone();
        std::thread::Builder::new()
            .name("ssmcp-pty-read".into())
            .spawn(move || {
                let mut data = [0_u8; 8192];
                loop {
                    match reader.read(&mut data) {
                        Ok(0) => break,
                        Ok(n) => {
                            output.lock().unwrap().push(&data[..n]);
                            if live {
                                crate::client::render::terminal_output(&data[..n]);
                            }
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
            self.child
                .lock()
                .unwrap()
                .as_mut()
                .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
        }
        #[cfg(unix)]
        {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: the unreaped child is still owned; WNOWAIT preserves its PID
            // so shutdown cannot signal a later process with a recycled identity.
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            result == 0 && info.si_pid == 0
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
        // SAFETY: portable-pty creates the child session; the leader was never
        // reaped by running(). Kill its group before releasing the PID.
        unsafe {
            libc::kill(-(self.pid as libc::pid_t), libc::SIGKILL);
        }
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
        if let Some(completed) = self.stop() {
            let _ = timeout(Duration::from_secs(5), completed).await;
        }
        let mut output = self.output.lock().unwrap();
        if !output.eof
            && let Some(capture) = &mut output.capture
        {
            capture.fail("terminal was stopped before output EOF; capture may be incomplete");
            let _ = capture.finish();
        }
    }
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
