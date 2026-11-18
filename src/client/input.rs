//! Dashboard input belongs to its composer and navigation; fallback input uses the selected PTY.
//! Ctrl-C disconnects outside bracketed paste; pasted binary bytes remain intact.
use crate::{
    pty::Terminal,
    terminals::Terminals,
    wire::{MAX_WRITE, Operation},
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::task::JoinHandle;

pub(crate) mod paste;

pub(crate) struct Input {
    task: Option<JoinHandle<()>>,
    mode: Arc<platform::Mode>,
}

impl Input {
    pub(crate) fn attach(terminals: Arc<Terminals>) -> Result<Option<Self>, String> {
        platform::attach(terminals)
    }

    pub(crate) async fn shutdown(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
        self.mode.restore();
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.mode.restore();
    }
}

struct RestoreOnExit(Arc<platform::Mode>);
impl Drop for RestoreOnExit {
    fn drop(&mut self) {
        self.0.restore();
    }
}

fn chunks(text: &str) -> impl Iterator<Item = &str> {
    let mut remaining = text;
    std::iter::from_fn(move || {
        if remaining.is_empty() {
            return None;
        }
        let mut end = remaining.len().min(MAX_WRITE);
        while !remaining.is_char_boundary(end) {
            end -= 1;
        }
        let chunk = &remaining[..end];
        remaining = &remaining[end..];
        Some(chunk)
    })
}

async fn forward(terminal: &Terminal, text: &str) -> Result<(), String> {
    for chunk in chunks(text) {
        terminal
            .perform(Operation::Write {
                data: chunk.to_owned(),
            })
            .await?;
    }
    Ok(())
}

async fn forward_selected(terminals: &Terminals, text: &str) -> Result<(), String> {
    for chunk in chunks(text) {
        terminals
            .selected()
            .perform(Operation::Write {
                data: chunk.to_owned(),
            })
            .await?;
    }
    Ok(())
}

async fn forward_local(terminals: &Terminals, text: &str) {
    if let Err(error) = forward_selected(terminals, text).await {
        // Never retry or replay failed input into another shell. The input pump
        // must remain available so F7 can select a surviving terminal.
        crate::client::render::status(
            "input",
            "Warning",
            &format!("Local input stopped: {error}; use F7 to select another terminal"),
        );
    }
}

async fn forward_keys(terminals: &Terminals, events: Vec<InputEvent>) -> Result<(), String> {
    for event in events {
        match event {
            InputEvent::Text(text) => {
                let text = super::terminal::keyboard(&text);
                apply_display_changes(terminals).await?;
                forward_local(terminals, &text).await;
            }
            InputEvent::NextTerminal => {
                if super::terminal::active() {
                    let _ = super::terminal::keyboard("\x1b[18~");
                    apply_display_changes(terminals).await?;
                } else {
                    terminals.cycle()?;
                }
            }
        }
    }
    Ok(())
}

async fn forward_human(
    terminals: &Terminals,
    keys: &mut SessionKeys,
    text: &str,
) -> Result<(), String> {
    forward_keys(terminals, keys.feed(text, Instant::now(), false)).await
}

async fn forward_display(terminals: &Terminals, keys: &mut SessionKeys) -> Result<(), String> {
    forward_keys(terminals, keys.feed("", Instant::now(), true)).await?;
    let keyboard = super::terminal::take_keyboard();
    apply_display_changes(terminals).await?;
    if !keyboard.is_empty() {
        forward_local(terminals, &keyboard).await;
    }
    for (id, reply) in super::terminal::take_session_replies() {
        // A terminal may close between rendering its query and draining replies.
        let Ok(terminal) = terminals.get(&id) else {
            continue;
        };
        let text = std::str::from_utf8(&reply).map_err(|_| "invalid virtual terminal reply")?;
        if let Err(error) = forward(&terminal, text).await {
            crate::client::render::status("input", "Warning", &format!("terminal {id}: {error}"));
        }
    }
    Ok(())
}

async fn apply_display_changes(terminals: &Terminals) -> Result<(), String> {
    if super::interface::apply_pending()?.is_some() {
        super::interface::sync_terminal_sizes(terminals).await?;
    }
    Ok(())
}

async fn resize(terminals: &Terminals, rows: u16, cols: u16) -> Result<(), String> {
    // Some terminals report 0 while being created or minimized.
    if rows > 0 && cols > 0 {
        let (rows, cols) = super::terminal::physical_resize(rows, cols);
        if super::terminal::active() {
            super::interface::sync_terminal_sizes(terminals).await?;
        } else {
            terminals.resize_all(rows.min(1000), cols.min(1000)).await?;
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InputEvent {
    Text(String),
    NextTerminal,
}

/// Preserve ordering around F7 even when one stdin read contains input for two
/// terminals in fallback mode. Dashboard and paste input remain in their original order.
#[derive(Default)]
pub(crate) struct SessionKeys {
    pending: Vec<u8>,
    since: Option<Instant>,
    pasting: bool,
}

impl SessionKeys {
    pub(crate) fn feed(&mut self, text: &str, now: Instant, flush: bool) -> Vec<InputEvent> {
        const NEXT: &[u8] = b"\x1b[18~";
        const PASTE_START: &[u8] = b"\x1b[200~";
        const PASTE_END: &[u8] = b"\x1b[201~";
        fn emit(bytes: &mut Vec<u8>, events: &mut Vec<InputEvent>) {
            if !bytes.is_empty() {
                events.push(InputEvent::Text(
                    String::from_utf8_lossy(bytes).into_owned(),
                ));
                bytes.clear();
            }
        }
        let mut output = Vec::new();
        let mut events = Vec::new();
        for &byte in text.as_bytes() {
            if self.pending.is_empty() && byte != 0x1b {
                output.push(byte);
                continue;
            }
            self.pending.push(byte);
            if !self.pasting && self.pending == NEXT {
                emit(&mut output, &mut events);
                events.push(InputEvent::NextTerminal);
                self.pending.clear();
                self.since = None;
            } else if (!self.pasting && self.pending == PASTE_START)
                || (self.pasting && self.pending == PASTE_END)
            {
                self.pasting = !self.pasting;
                output.append(&mut self.pending);
                self.since = None;
            } else if (self.pasting && PASTE_END.starts_with(&self.pending))
                || (!self.pasting
                    && (NEXT.starts_with(&self.pending) || PASTE_START.starts_with(&self.pending)))
            {
                self.since.get_or_insert(now);
            } else {
                // A new Escape can begin another shortcut after an incomplete one.
                let restart = self.pending.last() == Some(&0x1b);
                let end = self.pending.len() - usize::from(restart);
                output.extend_from_slice(&self.pending[..end]);
                self.pending.drain(..end);
                self.since = restart.then_some(now);
            }
        }
        if flush
            && self.since.is_some_and(|since| {
                now.saturating_duration_since(since) >= Duration::from_millis(80)
            })
        {
            output.append(&mut self.pending);
            self.since = None;
        }
        emit(&mut output, &mut events);
        events
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;

    #[test]
    fn f7_preserves_text_order_and_split_shortcuts() {
        let now = Instant::now();
        let mut keys = SessionKeys::default();
        assert_eq!(
            keys.feed("old\x1b[18~new\x1b[18~end", now, false),
            vec![
                InputEvent::Text("old".into()),
                InputEvent::NextTerminal,
                InputEvent::Text("new".into()),
                InputEvent::NextTerminal,
                InputEvent::Text("end".into()),
            ]
        );
        for text in ["\x1b", "[", "1", "8"] {
            assert!(keys.feed(text, now, false).is_empty());
        }
        assert_eq!(
            keys.feed("~界", now, false),
            vec![InputEvent::NextTerminal, InputEvent::Text("界".into())]
        );
    }

    #[test]
    fn pasted_f7_and_audit_keys_remain_literal_even_across_read_boundaries() {
        let now = Instant::now();
        let mut keys = SessionKeys::default();
        let paste = "\x1b[200~界\x1b[18~\x1b[17~\x1b[5~\x1b[201~";
        let mut text = String::new();
        for character in paste.chars() {
            for event in keys.feed(&character.to_string(), now, false) {
                match event {
                    InputEvent::Text(chunk) => text.push_str(&chunk),
                    InputEvent::NextTerminal => panic!("pasted F7 must not select a terminal"),
                }
            }
        }
        assert_eq!(text, paste);
        assert!(!keys.pasting);
        assert_eq!(
            keys.feed("\x1b[18~", now, false),
            vec![InputEvent::NextTerminal]
        );
        assert_eq!(
            keys.feed("\x1b[17~\x1b[5~", now, false),
            vec![InputEvent::Text("\x1b[17~\x1b[5~".into())]
        );
    }

    #[test]
    fn incomplete_f7_flushes_without_losing_escape_or_reinterpreting_its_suffix() {
        let now = Instant::now();
        let mut keys = SessionKeys::default();
        assert!(keys.feed("\x1b[18", now, false).is_empty());
        assert_eq!(
            keys.feed("", now + Duration::from_millis(100), true),
            vec![InputEvent::Text("\x1b[18".into())]
        );
        assert_eq!(
            keys.feed("~", now + Duration::from_millis(101), false),
            vec![InputEvent::Text("~".into())]
        );
        assert!(keys.feed("\x1b", now, false).is_empty());
        assert_eq!(
            keys.feed("", now + Duration::from_millis(100), true),
            vec![InputEvent::Text("\x1b".into())]
        );
    }

    #[test]
    fn input_chunks_keep_unicode_intact_at_the_write_limit() {
        let text = format!("{}界🌍{}", "x".repeat(MAX_WRITE - 1), "y".repeat(MAX_WRITE));
        let pieces: Vec<_> = chunks(&text).collect();
        assert!(pieces.iter().all(|piece| piece.len() <= MAX_WRITE));
        assert_eq!(pieces.concat(), text);
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::{
        ffi::CStr,
        io,
        mem::MaybeUninit,
        os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        sync::atomic::{AtomicBool, Ordering},
    };
    use tokio::{
        io::{Interest, unix::AsyncFd},
        signal::unix::{Signal, SignalKind, signal},
    };

    pub(super) struct Mode {
        fd: AsyncFd<OwnedFd>,
        saved: libc::termios,
        active: AtomicBool,
    }

    impl Mode {
        fn enable(fd: OwnedFd, dashboard: bool) -> Result<Arc<Self>, String> {
            // SAFETY: OwnedFd keeps this nonblocking descriptor stable until deregistration.
            let fd = unsafe { AsyncFd::register_with_interest(fd, Interest::READABLE) }
                .map_err(|e| format!("register local terminal input: {e}"))?;
            let saved = attributes(fd.get_ref().as_raw_fd()).map_err(|e| e.to_string())?;
            let mut local = saved;
            local.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ECHONL | libc::IEXTEN);
            // The line discipline must not interpret an image's 0x03 as Ctrl-C
            // before the bracketed-paste decoder sees it. The dashboard handles
            // an unpasted Ctrl-C through the existing interrupt signal instead.
            if dashboard {
                local.c_lflag &= !libc::ISIG;
            } else {
                local.c_lflag |= libc::ISIG;
            }
            local.c_iflag &= !(libc::IXON
                | libc::IXOFF
                | libc::ICRNL
                | libc::INLCR
                | libc::IGNCR
                | libc::ISTRIP
                | libc::PARMRK);
            local.c_cc[libc::VMIN] = 1;
            local.c_cc[libc::VTIME] = 0;
            // Ctrl-Z / Ctrl-\\ should reach the inner shell instead of suspending or
            // killing this frontend with its outer terminal still in input mode.
            local.c_cc[libc::VSUSP] = libc::_POSIX_VDISABLE;
            local.c_cc[libc::VQUIT] = libc::_POSIX_VDISABLE;
            // SAFETY: fd is an open tty; local is a valid termios structure.
            if unsafe { libc::tcsetattr(fd.get_ref().as_raw_fd(), libc::TCSANOW, &local) } != 0 {
                return Err(format!(
                    "configure local terminal input: {}",
                    io::Error::last_os_error()
                ));
            }
            Ok(Arc::new(Self {
                fd,
                saved,
                active: AtomicBool::new(true),
            }))
        }

        pub(super) fn restore(&self) {
            if self.active.swap(false, Ordering::AcqRel) {
                // SAFETY: fd remains open while this guard restores its saved tty settings.
                let _ = unsafe {
                    libc::tcsetattr(self.fd.get_ref().as_raw_fd(), libc::TCSANOW, &self.saved)
                };
            }
        }
    }

    impl Drop for Mode {
        fn drop(&mut self) {
            self.restore();
        }
    }

    pub(super) fn attach(terminals: Arc<Terminals>) -> Result<Option<Input>, String> {
        // Do not steal input from a foreground job if this client was backgrounded.
        // SAFETY: these functions inspect process and terminal state only.
        let foreground = unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) };
        if foreground > 0 && foreground != unsafe { libc::getpgrp() } {
            return Ok(None);
        }
        attach_from(terminals, libc::STDIN_FILENO)
    }

    fn attach_from(terminals: Arc<Terminals>, source: RawFd) -> Result<Option<Input>, String> {
        let Some(fd) = reopen_tty(source).map_err(|e| format!("open local terminal input: {e}"))?
        else {
            return Ok(None);
        };
        // Register before changing the mode so every error path leaves it unchanged.
        let resized = signal(SignalKind::window_change()).map_err(|e| e.to_string())?;
        let mode = Mode::enable(fd, super::super::terminal::active())?;
        let task_mode = mode.clone();
        let task = tokio::spawn(async move {
            let _restore = RestoreOnExit(task_mode.clone());
            if let Err(error) = run(&terminals, &task_mode, resized).await {
                crate::client::render::status("input", "Warning", &error);
            }
        });
        Ok(Some(Input {
            task: Some(task),
            mode,
        }))
    }

    fn reopen_tty(source: RawFd) -> io::Result<Option<OwnedFd>> {
        // SAFETY: isatty does not take ownership of the borrowed descriptor.
        if unsafe { libc::isatty(source) } != 1 {
            return Ok(None);
        }
        let mut name = [0 as libc::c_char; 4096];
        // SAFETY: name is writable storage and source is a borrowed descriptor.
        let error = unsafe { libc::ttyname_r(source, name.as_mut_ptr(), name.len()) };
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error));
        }
        // SAFETY: successful ttyname_r writes a NUL-terminated path.
        let path = unsafe { CStr::from_ptr(name.as_ptr()) };
        // Reopen, rather than dup + F_SETFL: duplicated fds share O_NONBLOCK with
        // stdin and its parent shell. Never change that shared file description.
        // SAFETY: path is terminated and these flags do not create a file.
        let raw = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOCTTY,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the successful open transferred a fresh descriptor to us.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let before = identity(source)?;
        let after = identity(fd.as_raw_fd())?;
        if before != after {
            return Err(io::Error::other("stdin terminal changed while opening it"));
        }
        Ok(Some(fd))
    }

    fn identity(fd: RawFd) -> io::Result<(libc::dev_t, libc::ino_t, libc::dev_t)> {
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        // SAFETY: fstat writes the complete structure on success.
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the preceding fstat succeeded.
        let stat = unsafe { stat.assume_init() };
        Ok((stat.st_dev, stat.st_ino, stat.st_rdev))
    }

    fn attributes(fd: RawFd) -> io::Result<libc::termios> {
        let mut saved = MaybeUninit::<libc::termios>::uninit();
        // SAFETY: tcgetattr initializes saved on success.
        if unsafe { libc::tcgetattr(fd, saved.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: tcgetattr succeeded.
        Ok(unsafe { saved.assume_init() })
    }

    async fn forward_size(terminals: &Terminals, mode: &Mode) -> Result<(), String> {
        let mut size = MaybeUninit::<libc::winsize>::zeroed();
        // SAFETY: TIOCGWINSZ writes into a valid winsize structure.
        if unsafe {
            libc::ioctl(
                mode.fd.get_ref().as_raw_fd(),
                libc::TIOCGWINSZ,
                size.as_mut_ptr(),
            )
        } == 0
        {
            // SAFETY: the ioctl succeeded.
            let size = unsafe { size.assume_init() };
            resize(terminals, size.ws_row, size.ws_col).await?;
        }
        Ok(())
    }

    async fn run(terminals: &Terminals, mode: &Mode, mut resized: Signal) -> Result<(), String> {
        forward_size(terminals, mode).await?;
        let mut buffer = [0; 4096];
        let mut utf8 = Utf8Input::default();
        let mut keys = SessionKeys::default();
        let mut replies = tokio::time::interval(std::time::Duration::from_millis(20));
        replies.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        while mode.active.load(Ordering::Acquire) {
            tokio::select! {
                _ = replies.tick() => {
                    forward_display(terminals, &mut keys).await?;
                }
                event = resized.recv() => {
                    if event.is_none() { break; }
                    forward_size(terminals, mode).await?;
                }
                ready = mode.fd.readable() => {
                    let mut ready = ready.map_err(|e| format!("read local terminal: {e}"))?;
                    let read = ready.try_io(|fd| {
                        // SAFETY: buffer is writable and fd is an open nonblocking tty.
                        let n = unsafe { libc::read(fd.get_ref().as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
                        if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
                    });
                    match read {
                        Ok(Ok(0)) => { forward_input(terminals, &mut keys, &mut utf8, &[], true).await?; break; }
                        Ok(Ok(n)) => forward_input(terminals, &mut keys, &mut utf8, &buffer[..n], false).await?,
                        Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Ok(Err(e)) => return Err(format!("read local terminal: {e}")),
                        Err(_) => continue,
                    }
                }
            }
        }
        Ok(())
    }

    /// The dashboard owns raw paste bytes. Decode only the fallback PTY path,
    /// after the byte-oriented compositor has had a chance to consume input.
    async fn forward_input(
        terminals: &Terminals,
        keys: &mut SessionKeys,
        utf8: &mut Utf8Input,
        bytes: &[u8],
        eof: bool,
    ) -> Result<(), String> {
        if let Some(output) = super::super::terminal::keyboard_bytes(bytes) {
            apply_display_changes(terminals).await?;
            let text = utf8.push(&output, eof);
            if !text.is_empty() {
                forward_local(terminals, &text).await;
            }
            Ok(())
        } else {
            forward_human(terminals, keys, &utf8.push(bytes, eof)).await
        }
    }

    #[derive(Default)]
    struct Utf8Input {
        pending: Vec<u8>,
    }

    impl Utf8Input {
        fn push(&mut self, bytes: &[u8], eof: bool) -> String {
            self.pending.extend_from_slice(bytes);
            let mut text = String::new();
            let mut consumed = 0;
            while consumed < self.pending.len() {
                match std::str::from_utf8(&self.pending[consumed..]) {
                    Ok(valid) => {
                        text.push_str(valid);
                        consumed = self.pending.len();
                    }
                    Err(error) => {
                        let end = consumed + error.valid_up_to();
                        text.push_str(&String::from_utf8_lossy(&self.pending[consumed..end]));
                        consumed = end;
                        match error.error_len() {
                            Some(length) => {
                                text.push(char::REPLACEMENT_CHARACTER);
                                consumed += length;
                            }
                            None => break,
                        }
                    }
                }
            }
            self.pending.drain(..consumed);
            if eof && !self.pending.is_empty() {
                text.push_str(&String::from_utf8_lossy(&self.pending));
                self.pending.clear();
            }
            text
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use base64::{Engine, engine::general_purpose::STANDARD};
        use std::{path::Path, time::Duration};
        use tokio::time::timeout;

        fn pair() -> (OwnedFd, OwnedFd) {
            let (mut master, mut slave) = (-1, -1);
            // SAFETY: openpty initializes both descriptors; no optional buffers are supplied.
            assert_eq!(
                unsafe {
                    libc::openpty(
                        &mut master,
                        &mut slave,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                },
                0
            );
            // SAFETY: openpty returned two independently owned descriptors.
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
        }

        fn write(fd: &OwnedFd, data: &[u8]) {
            // SAFETY: fd is open and data is readable for its full length.
            assert_eq!(
                unsafe { libc::write(fd.as_raw_fd(), data.as_ptr().cast(), data.len()) },
                data.len() as isize
            );
        }

        fn dimensions(fd: &OwnedFd, rows: u16, cols: u16) {
            let size = libc::winsize {
                ws_row: rows,
                ws_col: cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            // SAFETY: fd is a tty and size is initialized.
            assert_eq!(
                unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ, &size) },
                0
            );
        }

        fn same_mode(a: libc::termios, b: libc::termios) {
            assert_eq!(a.c_iflag, b.c_iflag);
            assert_eq!(a.c_oflag, b.c_oflag);
            assert_eq!(a.c_cflag, b.c_cflag);
            assert_eq!(a.c_lflag, b.c_lflag);
            assert_eq!(a.c_cc, b.c_cc);
        }

        async fn read_until(terminal: &Terminal, cursor: &mut u64, needle: &str) -> String {
            timeout(Duration::from_secs(5), async {
                let mut output = Vec::new();
                loop {
                    let chunk = terminal
                        .perform(Operation::Read {
                            cursor: *cursor,
                            max_bytes: 16384,
                            wait_ms: 100,
                        })
                        .await
                        .unwrap();
                    *cursor = chunk["next_cursor"].as_u64().unwrap();
                    output.extend(
                        STANDARD
                            .decode(chunk["data_base64"].as_str().unwrap())
                            .unwrap(),
                    );
                    if output
                        .windows(needle.len())
                        .any(|bytes| bytes == needle.as_bytes())
                    {
                        return String::from_utf8(output).unwrap();
                    }
                    assert_ne!(
                        chunk["output_eof"],
                        true,
                        "terminal ended: {:?}",
                        String::from_utf8_lossy(&output)
                    );
                }
            })
            .await
            .expect("interactive shell did not produce expected output")
        }

        #[test]
        fn input_decoder_preserves_split_characters_and_controls() {
            let mut decoder = Utf8Input::default();
            assert_eq!(decoder.push(b"x\x1b[D\xe7", false), "x\x1b[D");
            assert_eq!(decoder.push(b"\x95", false), "");
            assert_eq!(decoder.push(b"\x8c\r", false), "界\r");
            assert_eq!(decoder.push(b"\xffz\xf0\x9f", false), "�z");
            assert_eq!(decoder.push(b"\x8c\x8d", false), "🌍");
            assert_eq!(decoder.push(b"\xe7", true), "�");
            assert!(decoder.pending.is_empty());
        }

        #[test]
        fn non_terminal_input_is_not_opened_or_consumed() {
            let mut fds = [-1; 2];
            // SAFETY: pipe initializes two descriptor slots.
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            // SAFETY: the descriptors were transferred by pipe.
            let (reader, writer) =
                unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
            write(&writer, b"untouched");
            assert!(reopen_tty(reader.as_raw_fd()).unwrap().is_none());
            let mut bytes = [0; 9];
            // SAFETY: reader is open and bytes is writable.
            assert_eq!(
                unsafe { libc::read(reader.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) },
                9
            );
            assert_eq!(&bytes, b"untouched");
        }

        #[tokio::test]
        async fn terminal_mode_is_restored_without_changing_source_flags() {
            let (_master, source) = pair();
            let saved = attributes(source.as_raw_fd()).unwrap();
            // SAFETY: F_GETFL only reads the open descriptor's flags.
            let flags = unsafe { libc::fcntl(source.as_raw_fd(), libc::F_GETFL) };
            let mode =
                Mode::enable(reopen_tty(source.as_raw_fd()).unwrap().unwrap(), false).unwrap();
            let attached = attributes(source.as_raw_fd()).unwrap();
            assert_eq!(attached.c_lflag & (libc::ICANON | libc::ECHO), 0);
            assert_ne!(attached.c_lflag & libc::ISIG, 0);
            assert_eq!(attached.c_cc[libc::VINTR], saved.c_cc[libc::VINTR]);
            // SAFETY: F_GETFL only reads source flags; the independent fd must not alter them.
            assert_eq!(
                unsafe { libc::fcntl(source.as_raw_fd(), libc::F_GETFL) },
                flags
            );
            drop(mode);
            same_mode(saved, attributes(source.as_raw_fd()).unwrap());
        }

        #[tokio::test]
        async fn dashboard_input_keeps_binary_control_bytes_and_restores_termios() {
            let (keyboard, source) = pair();
            let saved = attributes(source.as_raw_fd()).unwrap();
            let mode =
                Mode::enable(reopen_tty(source.as_raw_fd()).unwrap().unwrap(), true).unwrap();
            let attached = attributes(source.as_raw_fd()).unwrap();
            assert_eq!(attached.c_lflag & libc::ISIG, 0);
            let png = b"\x1b[200~\x89PNG\r\n\x1a\n\x00\x03\x11\x13\xff\x1b[201~";
            write(&keyboard, png);
            let mut received = Vec::new();
            timeout(Duration::from_secs(2), async {
                while received.len() < png.len() {
                    let mut ready = mode.fd.readable().await.unwrap();
                    let mut buffer = [0; 128];
                    let read = ready.try_io(|fd| {
                        // SAFETY: fd is open and buffer is writable.
                        let count = unsafe {
                            libc::read(
                                fd.get_ref().as_raw_fd(),
                                buffer.as_mut_ptr().cast(),
                                buffer.len(),
                            )
                        };
                        if count < 0 {
                            Err(io::Error::last_os_error())
                        } else {
                            Ok(count as usize)
                        }
                    });
                    if let Ok(Ok(count)) = read {
                        received.extend_from_slice(&buffer[..count]);
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(received, png);
            drop(mode);
            same_mode(saved, attributes(source.as_raw_fd()).unwrap());
        }

        #[tokio::test]
        async fn human_input_resize_and_idle_shutdown_use_the_interactive_pty() {
            let (keyboard, source) = pair();
            let saved = attributes(source.as_raw_fd()).unwrap();
            dimensions(&source, 47, 132);
            let terminals = Terminals::new(
                Path::new("/bin/sh"),
                std::env::current_dir().unwrap(),
                crate::output::OutputStore::new_connection().unwrap(),
            )
            .unwrap();
            let terminal = terminals.primary();
            let input = attach_from(terminals.clone(), source.as_raw_fd())
                .unwrap()
                .unwrap();
            let mut cursor = 0;
            write(&keyboard, b"stty -echo; printf '__R%s__\\n' READY; IFS= read -r value; printf '__VALUE_%s__\\n' \"$value\"; stty size\n");
            read_until(&terminal, &mut cursor, "__RREADY__").await;
            write(&keyboard, b"\xe7\x95");
            tokio::time::sleep(Duration::from_millis(10)).await;
            write(&keyboard, b"\x8c\r");
            let output = read_until(&terminal, &mut cursor, "47 132").await;
            assert!(output.contains("__VALUE_界__"), "{output}");
            dimensions(&source, 41, 99);
            // SAFETY: SIGWINCH is handled by the input task's registered Tokio stream.
            assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGWINCH) }, 0);
            timeout(Duration::from_secs(5), async {
                loop {
                    write(&keyboard, b"stty size; printf '__S%s__\\n' IZE\n");
                    if read_until(&terminal, &mut cursor, "__SIZE__")
                        .await
                        .contains("41 99")
                    {
                        break;
                    }
                }
            })
            .await
            .unwrap();
            timeout(Duration::from_secs(1), input.shutdown())
                .await
                .expect("idle input task must cancel promptly");
            same_mode(saved, attributes(source.as_raw_fd()).unwrap());
            terminals.shutdown().await;
        }

        #[tokio::test]
        async fn f7_in_one_input_chunk_routes_text_before_and_after_to_different_shells() {
            let terminals = Terminals::new(
                Path::new("/bin/sh"),
                std::env::current_dir().unwrap(),
                crate::output::OutputStore::new_connection().unwrap(),
            )
            .unwrap();
            let opened = terminals
                .call("terminal.open", serde_json::json!({"name":"worker"}))
                .await
                .unwrap();
            let worker = terminals
                .get(opened["terminal_id"].as_str().unwrap())
                .unwrap();
            let mut keys = SessionKeys::default();
            forward_human(
                &terminals,
                &mut keys,
                "printf '__M%s__\\n' AIN\n\x1b[18~printf '__W%s__\\n' ORKER\n",
            )
            .await
            .unwrap();
            let main = read_until(&terminals.primary(), &mut 0, "__MAIN__").await;
            let other = read_until(&worker, &mut 0, "__WORKER__").await;
            assert!(!main.contains("__WORKER__"));
            assert!(!other.contains("__MAIN__"));
            assert!(Arc::ptr_eq(&terminals.selected(), &worker));
            terminals.shutdown().await;
        }

        #[tokio::test]
        async fn exited_selected_pty_keeps_local_f7_usable_without_replaying_failed_input() {
            let (keyboard, source) = pair();
            let terminals = Terminals::new(
                Path::new("/bin/sh"),
                std::env::current_dir().unwrap(),
                crate::output::OutputStore::new_connection().unwrap(),
            )
            .unwrap();
            let opened = terminals
                .call(
                    "terminal.open",
                    serde_json::json!({"name":"exiting","select":true}),
                )
                .await
                .unwrap();
            let exiting = terminals
                .get(opened["terminal_id"].as_str().unwrap())
                .unwrap();
            let input = attach_from(terminals.clone(), source.as_raw_fd())
                .unwrap()
                .unwrap();
            write(&keyboard, b"exit\n");
            timeout(Duration::from_secs(5), async {
                while exiting.perform(Operation::Status {}).await.unwrap()["running"] == true {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert!(
                forward_selected(&terminals, "discarded input\n")
                    .await
                    .is_err()
            );
            write(
                &keyboard,
                b"printf '__DROPPED_%s__\\n' INPUT\n\x1b[18~printf '__R%s__\\n' ECOVERED\n",
            );
            let mut cursor = 0;
            let output = read_until(&terminals.primary(), &mut cursor, "__RECOVERED__").await;
            assert!(!output.contains("__DROPPED_INPUT__"), "{output}");
            assert!(Arc::ptr_eq(&terminals.selected(), &terminals.primary()));
            assert!(!input.task.as_ref().unwrap().is_finished());
            input.shutdown().await;
            terminals.shutdown().await;
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::{
        io::{self, IsTerminal},
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
        sync::atomic::{AtomicBool, Ordering},
        time::Duration,
    };
    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING},
        System::Console::{
            CONSOLE_SCREEN_BUFFER_INFO, ENABLE_ECHO_INPUT, ENABLE_EXTENDED_FLAGS,
            ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_QUICK_EDIT_MODE,
            ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_WINDOW_INPUT, GetConsoleMode,
            GetConsoleScreenBufferInfo, GetNumberOfConsoleInputEvents, GetStdHandle, INPUT_RECORD,
            KEY_EVENT, KEY_EVENT_RECORD, LEFT_ALT_PRESSED, LEFT_CTRL_PRESSED, RIGHT_ALT_PRESSED,
            RIGHT_CTRL_PRESSED, ReadConsoleInputW, SHIFT_PRESSED, STD_ERROR_HANDLE,
            STD_OUTPUT_HANDLE, SetConsoleMode, WINDOW_BUFFER_SIZE_EVENT,
        },
    };

    pub(super) struct Mode {
        handle: OwnedHandle,
        saved: u32,
        active: AtomicBool,
    }

    impl Mode {
        pub(super) fn restore(&self) {
            if self.active.swap(false, Ordering::AcqRel) {
                // SAFETY: the owned console input handle remains valid through restoration.
                let _ = unsafe { SetConsoleMode(self.handle.as_raw_handle(), self.saved) };
            }
        }
    }
    impl Drop for Mode {
        fn drop(&mut self) {
            self.restore();
        }
    }

    pub(super) fn attach(terminals: Arc<Terminals>) -> Result<Option<Input>, String> {
        if !io::stdin().is_terminal() {
            return Ok(None);
        }
        let name: Vec<u16> = "CONIN$\0".encode_utf16().collect();
        // SAFETY: name is terminated; this opens an independent, noninheritable console handle.
        let raw = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(format!(
                "open local console input: {}",
                io::Error::last_os_error()
            ));
        }
        // SAFETY: CreateFileW transferred ownership of a valid handle.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut saved = 0;
        // SAFETY: handle is an open console and saved is valid writable storage.
        if unsafe { GetConsoleMode(handle.as_raw_handle(), &mut saved) } == 0 {
            return Err(format!(
                "read local console mode: {}",
                io::Error::last_os_error()
            ));
        }
        let local = (saved | ENABLE_PROCESSED_INPUT | ENABLE_EXTENDED_FLAGS | ENABLE_WINDOW_INPUT)
            & !(ENABLE_ECHO_INPUT
                | ENABLE_LINE_INPUT
                | ENABLE_QUICK_EDIT_MODE
                | ENABLE_VIRTUAL_TERMINAL_INPUT);
        // ENABLE_PROCESSED_INPUT keeps Ctrl-C on the client's existing signal path.
        // SAFETY: handle is open and local contains supported console input mode flags.
        if unsafe { SetConsoleMode(handle.as_raw_handle(), local) } == 0 {
            return Err(format!(
                "configure local console input: {}",
                io::Error::last_os_error()
            ));
        }
        let mode = Arc::new(Mode {
            handle,
            saved,
            active: AtomicBool::new(true),
        });
        let task_mode = mode.clone();
        let task = tokio::spawn(async move {
            let _restore = RestoreOnExit(task_mode.clone());
            if let Err(error) = run(&terminals, &task_mode).await {
                crate::client::render::status("input", "Warning", &error);
            }
        });
        Ok(Some(Input {
            task: Some(task),
            mode,
        }))
    }

    fn size() -> Option<(u16, u16)> {
        for stream in [STD_ERROR_HANDLE, STD_OUTPUT_HANDLE] {
            let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
            // SAFETY: the standard handle is borrowed and info is valid output storage.
            if unsafe { GetConsoleScreenBufferInfo(GetStdHandle(stream), &mut info) } != 0 {
                return Some((
                    (info.srWindow.Bottom - info.srWindow.Top + 1).max(1) as u16,
                    (info.srWindow.Right - info.srWindow.Left + 1).max(1) as u16,
                ));
            }
        }
        None
    }

    async fn run(terminals: &Terminals, mode: &Mode) -> Result<(), String> {
        if let Some((rows, cols)) = size() {
            resize(terminals, rows, cols).await?;
        }
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut utf16 = Utf16Input::default();
        let mut keys = SessionKeys::default();
        while mode.active.load(Ordering::Acquire) {
            tick.tick().await;
            forward_display(terminals, &mut keys).await?;
            let mut count = 0;
            // ReadConsoleInputW is only called when records already exist. This
            // frontend is the sole console input consumer; no blocking stdin
            // thread survives cancellation or holds up runtime shutdown.
            // SAFETY: handle is valid and count points to writable storage.
            if unsafe { GetNumberOfConsoleInputEvents(mode.handle.as_raw_handle(), &mut count) }
                == 0
            {
                return Err(format!(
                    "inspect console input: {}",
                    io::Error::last_os_error()
                ));
            }
            if count == 0 {
                continue;
            }
            let mut records = [INPUT_RECORD::default(); 64];
            let mut read = 0;
            // SAFETY: records has capacity for min(count,64) initialized records.
            if unsafe {
                ReadConsoleInputW(
                    mode.handle.as_raw_handle(),
                    records.as_mut_ptr(),
                    count.min(64),
                    &mut read,
                )
            } == 0
            {
                return Err(format!(
                    "read console input: {}",
                    io::Error::last_os_error()
                ));
            }
            for event in &records[..read as usize] {
                match u32::from(event.EventType) {
                    KEY_EVENT => {
                        // SAFETY: EventType identifies the active union field.
                        let key = unsafe { event.Event.KeyEvent };
                        if key.bKeyDown != 0 {
                            let text = utf16.key(key);
                            forward_human(terminals, &mut keys, &text).await?;
                        }
                    }
                    WINDOW_BUFFER_SIZE_EVENT => {
                        if let Some((rows, cols)) = size() {
                            resize(terminals, rows, cols).await?;
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    #[derive(Default)]
    struct Utf16Input {
        high: Option<u16>,
    }
    impl Utf16Input {
        fn key(&mut self, key: KEY_EVENT_RECORD) -> String {
            self.key_with_mode(key, crate::client::terminal::application_cursor())
        }

        fn key_with_mode(&mut self, key: KEY_EVENT_RECORD, application_cursor: bool) -> String {
            // SAFETY: console input was requested in Unicode mode.
            let unit = unsafe { key.uChar.UnicodeChar };
            let ctrl = key.dwControlKeyState & (LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED) != 0;
            let alt = key.dwControlKeyState & (LEFT_ALT_PRESSED | RIGHT_ALT_PRESSED) != 0 && !ctrl;
            let shift = key.dwControlKeyState & SHIFT_PRESSED != 0;
            let mut text = String::new();
            if shift && matches!(key.wVirtualKeyCode, 0x09 | 0x0d) {
                let sequence = if key.wVirtualKeyCode == 0x09 {
                    "\x1b[Z"
                } else {
                    "\x1b[13;2u"
                };
                for _ in 0..key.wRepeatCount.max(1) {
                    text.push_str(sequence);
                }
            } else if unit != 0 {
                for _ in 0..key.wRepeatCount.max(1) {
                    if let Some(high) = self.high.take() {
                        if (0xdc00..=0xdfff).contains(&unit) {
                            let scalar =
                                0x10000 + ((u32::from(high) - 0xd800) << 10) + u32::from(unit)
                                    - 0xdc00;
                            text.push(
                                char::from_u32(scalar).unwrap_or(char::REPLACEMENT_CHARACTER),
                            );
                            continue;
                        }
                        text.push(char::REPLACEMENT_CHARACTER);
                    }
                    if (0xd800..=0xdbff).contains(&unit) {
                        self.high = Some(unit);
                    } else {
                        text.push(
                            char::from_u32(u32::from(unit)).unwrap_or(char::REPLACEMENT_CHARACTER),
                        );
                    }
                }
            } else {
                // Windows virtual-key codes for terminal navigation keys.
                let sequence = match key.wVirtualKeyCode {
                    0x23 if application_cursor => "\x1bOF",
                    0x24 if application_cursor => "\x1bOH",
                    0x25 if application_cursor => "\x1bOD",
                    0x26 if application_cursor => "\x1bOA",
                    0x27 if application_cursor => "\x1bOC",
                    0x28 if application_cursor => "\x1bOB",
                    0x70 => "\x1bOP",
                    0x71 => "\x1bOQ",
                    0x72 => "\x1bOR",
                    0x73 => "\x1bOS",
                    0x74 => "\x1b[15~",
                    0x75 => "\x1b[17~",
                    0x76 => "\x1b[18~",
                    0x77 => "\x1b[19~",
                    0x78 => "\x1b[20~",
                    0x21 => "\x1b[5~",
                    0x22 => "\x1b[6~",
                    0x23 => "\x1b[F",
                    0x24 => "\x1b[H",
                    0x25 => "\x1b[D",
                    0x26 => "\x1b[A",
                    0x27 => "\x1b[C",
                    0x28 => "\x1b[B",
                    0x2d => "\x1b[2~",
                    0x2e => "\x1b[3~",
                    0x20 if ctrl => "\0",
                    _ => "",
                };
                for _ in 0..key.wRepeatCount.max(1) {
                    text.push_str(sequence);
                }
            }
            if alt && !text.is_empty() {
                text.insert(0, '\x1b');
            }
            text
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows_sys::Win32::System::Console::KEY_EVENT_RECORD_0;

        fn key(unit: u16, code: u16, modifiers: u32) -> KEY_EVENT_RECORD {
            KEY_EVENT_RECORD {
                bKeyDown: 1,
                wRepeatCount: 1,
                wVirtualKeyCode: code,
                wVirtualScanCode: 0,
                uChar: KEY_EVENT_RECORD_0 { UnicodeChar: unit },
                dwControlKeyState: modifiers,
            }
        }

        #[test]
        fn console_keys_preserve_unicode_and_shell_navigation() {
            let mut input = Utf16Input::default();
            assert_eq!(input.key(key(0xd83c, 0, 0)), "");
            assert_eq!(input.key(key(0xdf0d, 0, 0)), "🌍");
            assert_eq!(input.key(key('界' as u16, 0, 0)), "界");
            assert_eq!(input.key(key(0, 0x25, 0)), "\x1b[D");
            assert_eq!(input.key(key(0, 0x2e, 0)), "\x1b[3~");
            assert_eq!(input.key(key(0, 0x75, 0)), "\x1b[17~");
            assert_eq!(input.key(key(0, 0x76, 0)), "\x1b[18~");
            assert_eq!(input.key_with_mode(key(0, 0x25, 0), true), "\x1bOD");
            assert_eq!(input.key_with_mode(key(0, 0x24, 0), true), "\x1bOH");
            assert_eq!(input.key_with_mode(key(0, 0x23, 0), true), "\x1bOF");
            assert_eq!(input.key(key('x' as u16, 0, LEFT_ALT_PRESSED)), "\x1bx");
            // AltGr is a text modifier, not a request to send a leading Escape.
            assert_eq!(
                input.key(key('@' as u16, 0, RIGHT_ALT_PRESSED | LEFT_CTRL_PRESSED)),
                "@"
            );
        }
    }
}
