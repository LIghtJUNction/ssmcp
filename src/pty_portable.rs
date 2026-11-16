//! Native macOS PTYs and Windows ConPTY with the same cursor and spill protocol.
use crate::{
    output::{Capture, INLINE_BYTES, INLINE_LINES, OutputStore},
    wire::Operation,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::{Read, Write},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::Notify,
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
            // A failed spool remains explicit in every subsequent read/status;
            // the bounded terminal tail continues to work for interactive use.
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

pub struct Terminal {
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    pid: u32,
    running: AtomicBool,
    output: Mutex<Output>,
    changed: Notify,
    closed: AtomicBool,
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
        let store = OutputStore::global()?;
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| error.to_string())?;
        let mut command = CommandBuilder::new(shell);
        #[cfg(unix)]
        command.arg("-i");
        #[cfg(windows)]
        if shell.file_stem().is_some_and(|name| {
            name.eq_ignore_ascii_case("powershell") || name.eq_ignore_ascii_case("pwsh")
        }) {
            command.arg("-NoLogo");
        }
        command.env("TERM", "xterm-256color");
        command.cwd(std::env::current_dir()?);
        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| error.to_string())?;
        let pid = child.process_id().ok_or("shell has no process id")?;
        #[cfg(windows)]
        let job = {
            let job = crate::platform::WindowsJob::new()?;
            if let Err(error) = job.assign_pid(pid) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.into());
            }
            job
        };
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| error.to_string())?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| error.to_string())?;
        drop(pair.slave);
        let terminal = Arc::new(Self {
            master: Mutex::new(Some(pair.master)),
            writer: Arc::new(Mutex::new(Some(writer))),
            killer: Mutex::new(child.clone_killer()),
            pid,
            running: AtomicBool::new(true),
            output: Mutex::new(Output {
                capture: Some(store.capture("terminal_session")),
                ..Output::default()
            }),
            changed: Notify::new(),
            closed: AtomicBool::new(false),
            #[cfg(windows)]
            job,
        });
        let weak = Arc::downgrade(&terminal);
        std::thread::Builder::new()
            .name("ssmcp-pty-output".into())
            .spawn(move || {
                let mut data = [0u8; 8192];
                loop {
                    let read = reader.read(&mut data);
                    let Some(terminal) = weak.upgrade() else {
                        break;
                    };
                    match read {
                        Ok(n) if n > 0 => {
                            terminal.output.lock().unwrap().push(&data[..n]);
                            if live {
                                crate::client::render::terminal_output(&data[..n]);
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        _ => {
                            let mut output = terminal.output.lock().unwrap();
                            output.eof = true;
                            if let Some(capture) = &mut output.capture {
                                if let Err(error) = read {
                                    let closed = error.kind() == std::io::ErrorKind::BrokenPipe;
                                    #[cfg(unix)]
                                    let closed = closed || error.raw_os_error() == Some(libc::EIO);
                                    if !closed {
                                        capture.fail(format!("terminal read failed: {error}"));
                                    }
                                }
                                let _ = capture.finish();
                            }
                            drop(output);
                            terminal.changed.notify_waiters();
                            break;
                        }
                    }
                    terminal.changed.notify_waiters();
                }
            })?;
        let weak = Arc::downgrade(&terminal);
        std::thread::Builder::new()
            .name("ssmcp-pty-wait".into())
            .spawn(move || {
                let _ = child.wait();
                if let Some(terminal) = weak.upgrade() {
                    terminal.running.store(false, Ordering::Release);
                    terminal.changed.notify_waiters();
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
                if !self.running().await {
                    return Err("terminal has exited".into());
                }
                let length = data.len();
                let writer = self.writer.clone();
                let write = tokio::task::spawn_blocking(move || {
                    let mut writer = writer.lock().unwrap();
                    let writer = writer.as_mut().ok_or("terminal is closed")?;
                    writer
                        .write_all(data.as_bytes())
                        .and_then(|_| writer.flush())
                        .map_err(|e| e.to_string())
                });
                timeout(Duration::from_secs(5), write)
                    .await
                    .map_err(|_| {
                        "terminal write timed out; input may be partially written".to_owned()
                    })?
                    .map_err(|error| error.to_string())??;
                Ok(json!({"written":length}))
            }
            Operation::Resize { rows, cols } => {
                if !self.running().await {
                    return Err("terminal has exited".into());
                }
                let master = self.master.lock().unwrap();
                master
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
                let output = self.output.lock().unwrap();
                let mut value = json!({"running":self.running.load(Ordering::Acquire) && !self.closed.load(Ordering::Acquire),"pid":self.pid,
                    "start_cursor":output.start,"next_cursor":output.end,"output_eof":output.eof});
                if let Some(capture) = &output.capture {
                    if let Some(metadata) = capture.metadata() {
                        value["output_artifact"] = json!(metadata);
                    }
                    if let Some(error) = capture.error() {
                        value["output_error"] = json!(error);
                    }
                }
                Ok(value)
            }
            _ => Err("not a terminal operation".into()),
        }
    }

    async fn running(&self) -> bool {
        self.running.load(Ordering::Acquire) && !self.closed.load(Ordering::Acquire)
    }

    fn stop(&self) {
        #[cfg(windows)]
        self.job.terminate();
        #[cfg(unix)]
        if self.running.load(Ordering::Acquire) {
            crate::platform::stop_unix_descendants(self.pid as libc::pid_t);
        }
        if self.running.load(Ordering::Acquire) {
            let _ = self.killer.lock().unwrap().kill();
        }
    }

    pub async fn shutdown(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.stop();
        // A blocked input writer is released when the owned child and PTY close.
        self.master.lock().unwrap().take();
        let writer = self.writer.clone();
        let _ = tokio::task::spawn_blocking(move || {
            writer.lock().unwrap().take();
        })
        .await;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !self.running.load(Ordering::Acquire) && self.output.lock().unwrap().eof {
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
            capture.fail("terminal stopped before output EOF; capture may be incomplete");
            let _ = capture.finish();
        }
        output.eof = true;
        drop(output);
        self.changed.notify_waiters();
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
                snapshot["running"] = json!(self.running().await);
                return Ok(snapshot);
            }
            let _ = timeout_at(deadline, changed).await;
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
    #[tokio::test]
    async fn native_terminal_executes_and_resizes() {
        let terminal = Terminal::spawn(&crate::platform::default_shell()).unwrap();
        terminal
            .perform(Operation::Resize {
                rows: 35,
                cols: 100,
            })
            .await
            .unwrap();
        terminal
            .perform(Operation::Write {
                // The expected marker is absent from the typed command, so
                // terminal input echo cannot satisfy the execution assertion.
                #[cfg(windows)]
                data: "echo SSMCP_PORTABLE_^O^K\r\n".into(),
                #[cfg(unix)]
                data: "printf 'SSMCP_PORTABLE_%s\\n' 'OK'\n".into(),
            })
            .await
            .unwrap();
        let mut cursor = 0;
        let mut output = String::new();
        let executed = timeout(Duration::from_secs(10), async {
            loop {
                let page = terminal
                    .perform(Operation::Read {
                        cursor,
                        max_bytes: 32768,
                        wait_ms: 500,
                    })
                    .await
                    .unwrap();
                cursor = page["next_cursor"].as_u64().unwrap();
                output.push_str(page["text"].as_str().unwrap());
                // ConPTY places VT cursor controls around text; reads can also
                // split any marker across chunks. Neither changes its meaning.
                if output.contains("SSMCP_PORTABLE_OK") {
                    break;
                }
            }
        })
        .await;
        terminal.shutdown().await;
        assert!(executed.is_ok(), "native shell output: {output:?}");
        assert!(!terminal.running().await);
    }
}
