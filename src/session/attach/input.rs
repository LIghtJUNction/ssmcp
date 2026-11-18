//! Cancellation-safe local keyboard input. It owns an independent terminal
//! descriptor, restores its mode on every exit, and never leaves stdin blocked.
use crate::Result;
use std::{collections::VecDeque, time::Duration};

pub(crate) enum Event {
    Bytes(Vec<u8>),
    Interrupt,
    Resize(u16, u16),
    Detach,
    End,
}
enum Raw {
    Bytes(Vec<u8>),
    Resize(u16, u16),
    End,
}

pub(crate) struct Keyboard {
    platform: platform::Keyboard,
    decoder: Decoder,
    events: VecDeque<Event>,
    escape_deadline: Option<tokio::time::Instant>,
}

impl Keyboard {
    pub(crate) fn open(read_only: bool) -> Result<Self> {
        let platform = platform::Keyboard::open()?;
        let mut events = VecDeque::new();
        if let Some((rows, cols)) = platform.size() {
            events.push_back(Event::Resize(rows, cols));
        }
        Ok(Self {
            platform,
            decoder: Decoder {
                detach_on_interrupt: read_only,
                ..Decoder::default()
            },
            events,
            escape_deadline: None,
        })
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.events.extend(self.decoder.feed(bytes, false));
        // Actual input progress starts a new inter-byte window. Cancelling next()
        // or receiving a resize leaves this deadline intact.
        self.escape_deadline = self
            .decoder
            .pending_escape()
            .then(|| tokio::time::Instant::now() + Duration::from_millis(35));
    }

    pub(crate) async fn next(&mut self) -> Result<Event> {
        loop {
            if let Some(event) = self.events.pop_front() {
                return Ok(event);
            }
            let raw = if self.decoder.pending_escape() {
                // The caller cancels this future when output or a tick wins select!.
                // Retain the deadline so continuous output cannot postpone Escape.
                let deadline = *self
                    .escape_deadline
                    .get_or_insert_with(|| tokio::time::Instant::now() + Duration::from_millis(35));
                match tokio::time::timeout_at(deadline, self.platform.next()).await {
                    Ok(result) => Some(result?),
                    Err(_) => None,
                }
            } else {
                self.escape_deadline = None;
                Some(self.platform.next().await?)
            };
            match raw {
                Some(Raw::Bytes(bytes)) => self.feed(&bytes),
                Some(Raw::Resize(rows, cols)) => return Ok(Event::Resize(rows, cols)),
                Some(Raw::End) => {
                    self.events.extend(self.decoder.feed(&[], true));
                    self.events.push_back(Event::End);
                }
                None => self.events.extend(self.decoder.flush_escape()),
            }
            if !self.decoder.pending_escape() {
                self.escape_deadline = None;
            }
        }
    }
}

#[derive(Default)]
struct Decoder {
    pasted: bool,
    escape: Vec<u8>,
    detached: bool,
    detach_on_interrupt: bool,
}
impl Decoder {
    fn pending_escape(&self) -> bool {
        !self.escape.is_empty()
    }
    fn feed(&mut self, bytes: &[u8], eof: bool) -> Vec<Event> {
        let mut events = Vec::new();
        let mut literal = Vec::new();
        for byte in bytes {
            if self.detached {
                break;
            }
            self.escape.push(*byte);
            loop {
                let marker = if self.pasted {
                    b"\x1b[201~"
                } else {
                    b"\x1b[200~"
                };
                if marker.starts_with(&self.escape) {
                    if self.escape.len() == marker.len() {
                        literal.append(&mut self.escape);
                        self.pasted = !self.pasted;
                    }
                    break;
                }
                let value = self.escape.remove(0);
                if !self.pasted && (value == 0x1d || value == 0x03) {
                    Self::emit(&mut literal, &mut events);
                    if value == 0x1d || self.detach_on_interrupt {
                        events.push(Event::Detach);
                        self.detached = true;
                        self.escape.clear();
                        break;
                    }
                    // A writer's interrupt belongs to its inner PTY, not the
                    // dashboard's process-level interrupt action.
                    events.push(Event::Interrupt);
                } else {
                    literal.push(value);
                }
                if self.escape.is_empty() {
                    break;
                }
            }
        }
        if eof {
            literal.append(&mut self.escape);
        }
        Self::emit(&mut literal, &mut events);
        events
    }
    fn flush_escape(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        Self::emit(&mut self.escape, &mut events);
        events
    }
    fn emit(bytes: &mut Vec<u8>, events: &mut Vec<Event>) {
        if !bytes.is_empty() {
            events.push(Event::Bytes(std::mem::take(bytes)));
        }
    }
}

/// Decode only after the compositor has consumed binary paste input. The
/// fallback attach protocol remains text based and preserves split UTF-8.
#[derive(Default)]
pub(crate) struct Utf8Input {
    pending: Vec<u8>,
}

impl Utf8Input {
    pub(crate) fn push(&mut self, bytes: &[u8], eof: bool) -> String {
        self.pending.extend_from_slice(bytes);
        let mut text = String::new();
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(value) => {
                    text.push_str(value);
                    self.pending.clear();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    // SAFETY: valid_up_to is the UTF-8 parser's verified prefix.
                    text.push_str(unsafe { std::str::from_utf8_unchecked(&self.pending[..valid]) });
                    self.pending.drain(..valid);
                    match error.error_len() {
                        Some(length) => {
                            text.push(char::REPLACEMENT_CHARACTER);
                            self.pending.drain(..length);
                        }
                        None => {
                            if eof {
                                text.push(char::REPLACEMENT_CHARACTER);
                                self.pending.clear();
                            }
                            break;
                        }
                    }
                }
            }
        }
        text
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::{
        ffi::CStr,
        io,
        mem::MaybeUninit,
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
    };
    use tokio::{
        io::{Interest, unix::AsyncFd},
        signal::unix::{Signal, SignalKind, signal},
    };

    struct Mode {
        fd: AsyncFd<OwnedFd>,
        saved: libc::termios,
    }
    impl Drop for Mode {
        fn drop(&mut self) {
            // SAFETY: the independently owned terminal remains open until after restoration.
            unsafe {
                libc::tcsetattr(self.fd.get_ref().as_raw_fd(), libc::TCSANOW, &self.saved);
            }
        }
    }
    pub(super) struct Keyboard {
        mode: Option<Mode>,
        resized: Signal,
    }
    impl Keyboard {
        #[cfg(test)]
        pub(super) fn idle() -> Result<Self> {
            Ok(Self {
                mode: None,
                resized: signal(SignalKind::window_change())?,
            })
        }

        pub(super) fn open() -> Result<Self> {
            let resized = signal(SignalKind::window_change())?;
            // SAFETY: these inspect the borrowed standard descriptor and process group.
            let foreground = unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) };
            if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1
                || foreground <= 0
                || foreground != unsafe { libc::getpgrp() }
            {
                return Ok(Self {
                    mode: None,
                    resized,
                });
            }
            let mut name = [0 as libc::c_char; 4096];
            // SAFETY: the array is writable and ttyname_r terminates it on success.
            let result =
                unsafe { libc::ttyname_r(libc::STDIN_FILENO, name.as_mut_ptr(), name.len()) };
            if result != 0 {
                return Err(io::Error::from_raw_os_error(result).into());
            }
            let path = unsafe { CStr::from_ptr(name.as_ptr()) };
            // Reopening avoids changing stdin's shared O_NONBLOCK file status.
            let raw = unsafe {
                libc::open(
                    path.as_ptr(),
                    libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOCTTY,
                )
            };
            if raw < 0 {
                return Err(io::Error::last_os_error().into());
            }
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            if identity(libc::STDIN_FILENO)? != identity(fd.as_raw_fd())? {
                return Err("stdin terminal changed while opening it".into());
            }
            let fd = unsafe { AsyncFd::register_with_interest(fd, Interest::READABLE) }?;
            let mut saved = MaybeUninit::<libc::termios>::uninit();
            // SAFETY: tcgetattr initializes saved on success.
            if unsafe { libc::tcgetattr(fd.get_ref().as_raw_fd(), saved.as_mut_ptr()) } != 0 {
                return Err(io::Error::last_os_error().into());
            }
            let saved = unsafe { saved.assume_init() };
            let mut local = saved;
            local.c_lflag &=
                !(libc::ICANON | libc::ECHO | libc::ECHONL | libc::IEXTEN | libc::ISIG);
            local.c_iflag &= !(libc::IXON
                | libc::IXOFF
                | libc::ICRNL
                | libc::INLCR
                | libc::IGNCR
                | libc::ISTRIP
                | libc::PARMRK);
            local.c_cc[libc::VMIN] = 1;
            local.c_cc[libc::VTIME] = 0;
            // Ctrl-C now reaches the inner PTY; Ctrl-] is handled before forwarding.
            if unsafe { libc::tcsetattr(fd.get_ref().as_raw_fd(), libc::TCSANOW, &local) } != 0 {
                return Err(io::Error::last_os_error().into());
            }
            Ok(Self {
                mode: Some(Mode { fd, saved }),
                resized,
            })
        }
        pub(super) fn size(&self) -> Option<(u16, u16)> {
            let fd = self.mode.as_ref()?.fd.get_ref().as_raw_fd();
            let mut size = MaybeUninit::<libc::winsize>::zeroed();
            if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, size.as_mut_ptr()) } != 0 {
                return None;
            }
            let size = unsafe { size.assume_init() };
            (size.ws_row > 0 && size.ws_col > 0).then_some((size.ws_row, size.ws_col))
        }
        pub(super) async fn next(&mut self) -> Result<Raw> {
            let Some(mode) = &self.mode else {
                return std::future::pending().await;
            };
            loop {
                tokio::select! {
                    _=self.resized.recv()=>{if let Some((rows,cols))=self.size(){return Ok(Raw::Resize(rows,cols));}}
                    ready=mode.fd.readable()=>{
                        let mut ready=ready?;
                        let mut bytes=[0;4096];
                        match ready.try_io(|fd|{
                            let n=unsafe{libc::read(fd.get_ref().as_raw_fd(),bytes.as_mut_ptr().cast(),bytes.len())};
                            if n<0 {Err(io::Error::last_os_error())}else{Ok(n as usize)}
                        }) {
                            Ok(Ok(0))=>return Ok(Raw::End),
                            Ok(Ok(n))=>return Ok(Raw::Bytes(bytes[..n].to_vec())),
                            Ok(Err(error)) if error.kind()==io::ErrorKind::Interrupted=>{},
                            Ok(Err(error))=>return Err(error.into()),
                            Err(_)=>{},
                        }
                    }
                }
            }
        }
    }
    fn identity(fd: libc::c_int) -> io::Result<(libc::dev_t, libc::ino_t, libc::dev_t)> {
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let stat = unsafe { stat.assume_init() };
        Ok((stat.st_dev, stat.st_ino, stat.st_rdev))
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::{
        io::{self, IsTerminal},
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
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
            RIGHT_CTRL_PRESSED, ReadConsoleInputW, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
            SetConsoleMode, WINDOW_BUFFER_SIZE_EVENT,
        },
    };
    struct Mode {
        handle: OwnedHandle,
        saved: u32,
    }
    impl Drop for Mode {
        fn drop(&mut self) {
            unsafe {
                SetConsoleMode(self.handle.as_raw_handle(), self.saved);
            }
        }
    }
    pub(super) struct Keyboard {
        mode: Option<Mode>,
        queue: VecDeque<Raw>,
        high: Option<u16>,
    }
    impl Keyboard {
        pub(super) fn open() -> Result<Self> {
            if !io::stdin().is_terminal() {
                return Ok(Self {
                    mode: None,
                    queue: VecDeque::new(),
                    high: None,
                });
            }
            let name: Vec<u16> = "CONIN$\0".encode_utf16().collect();
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
                return Err(io::Error::last_os_error().into());
            }
            let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
            let mut saved = 0;
            if unsafe { GetConsoleMode(handle.as_raw_handle(), &mut saved) } == 0 {
                return Err(io::Error::last_os_error().into());
            }
            let local = (saved | ENABLE_EXTENDED_FLAGS | ENABLE_WINDOW_INPUT)
                & !(ENABLE_PROCESSED_INPUT
                    | ENABLE_ECHO_INPUT
                    | ENABLE_LINE_INPUT
                    | ENABLE_QUICK_EDIT_MODE
                    | ENABLE_VIRTUAL_TERMINAL_INPUT);
            if unsafe { SetConsoleMode(handle.as_raw_handle(), local) } == 0 {
                return Err(io::Error::last_os_error().into());
            }
            Ok(Self {
                mode: Some(Mode { handle, saved }),
                queue: VecDeque::new(),
                high: None,
            })
        }
        pub(super) fn size(&self) -> Option<(u16, u16)> {
            self.mode.as_ref()?;
            for stream in [STD_ERROR_HANDLE, STD_OUTPUT_HANDLE] {
                let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
                if unsafe { GetConsoleScreenBufferInfo(GetStdHandle(stream), &mut info) } != 0 {
                    return Some((
                        (info.srWindow.Bottom - info.srWindow.Top + 1).max(1) as u16,
                        (info.srWindow.Right - info.srWindow.Left + 1).max(1) as u16,
                    ));
                }
            }
            None
        }
        pub(super) async fn next(&mut self) -> Result<Raw> {
            loop {
                if let Some(event) = self.queue.pop_front() {
                    return Ok(event);
                }
                let Some(mode) = &self.mode else {
                    return std::future::pending().await;
                };
                let mut count = 0;
                if unsafe { GetNumberOfConsoleInputEvents(mode.handle.as_raw_handle(), &mut count) }
                    == 0
                {
                    return Err(io::Error::last_os_error().into());
                }
                if count == 0 {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    continue;
                }
                let mut records = [INPUT_RECORD::default(); 64];
                let mut read = 0;
                if unsafe {
                    ReadConsoleInputW(
                        mode.handle.as_raw_handle(),
                        records.as_mut_ptr(),
                        count.min(64),
                        &mut read,
                    )
                } == 0
                {
                    return Err(io::Error::last_os_error().into());
                }
                if read == 0 {
                    return Ok(Raw::End);
                }
                for record in &records[..read as usize] {
                    match u32::from(record.EventType) {
                        KEY_EVENT => {
                            let key = unsafe { record.Event.KeyEvent };
                            if key.bKeyDown != 0 {
                                let text = self.key(key);
                                if !text.is_empty() {
                                    self.queue.push_back(Raw::Bytes(text.into_bytes()));
                                }
                            }
                        }
                        WINDOW_BUFFER_SIZE_EVENT => {
                            if let Some((rows, cols)) = self.size() {
                                self.queue.push_back(Raw::Resize(rows, cols));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        fn key(&mut self, key: KEY_EVENT_RECORD) -> String {
            let unit = unsafe { key.uChar.UnicodeChar };
            let ctrl = key.dwControlKeyState & (LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED) != 0;
            let alt = key.dwControlKeyState & (LEFT_ALT_PRESSED | RIGHT_ALT_PRESSED) != 0 && !ctrl;
            let mut text = String::new();
            if unit != 0 {
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
                let sequence = match key.wVirtualKeyCode {
                    0x25 => "\x1b[D",
                    0x26 => "\x1b[A",
                    0x27 => "\x1b[C",
                    0x28 => "\x1b[B",
                    0x24 => "\x1b[H",
                    0x23 => "\x1b[F",
                    0x21 => "\x1b[5~",
                    0x22 => "\x1b[6~",
                    0x2e => "\x1b[3~",
                    0x76 => "\x1b[18~",
                    _ => "",
                };
                text.push_str(sequence);
            }
            if alt && !text.is_empty() {
                text.insert(0, '\x1b');
            }
            text
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bytes(events: Vec<Event>) -> Vec<u8> {
        events
            .into_iter()
            .filter_map(|e| {
                if let Event::Bytes(s) = e {
                    Some(s)
                } else {
                    None
                }
            })
            .flatten()
            .collect()
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn escape_deadline_survives_cancelled_keyboard_reads() {
        use std::{future::Future, task::Poll};

        let mut keyboard = Keyboard {
            platform: platform::Keyboard::idle().unwrap(),
            decoder: Decoder::default(),
            events: VecDeque::new(),
            escape_deadline: None,
        };
        keyboard.feed(b"\x1b");
        assert!(keyboard.events.is_empty());

        // Poll and drop the actual input future, as select! does whenever output
        // arrives. A fresh relative timeout would never be ready on any poll.
        let mut received = None;
        for _ in 0..20 {
            let result = {
                let mut next = std::pin::pin!(keyboard.next());
                std::future::poll_fn(|cx| Poll::Ready(next.as_mut().poll(cx))).await
            };
            if let Poll::Ready(event) = result {
                received = Some(event.unwrap());
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(matches!(received, Some(Event::Bytes(ref value)) if value == b"\x1b"));
        assert!(keyboard.escape_deadline.is_none());

        // A later Escape receives a new waiting window, not an expired deadline.
        keyboard.feed(b"\x1b");
        assert!(keyboard.events.is_empty());
        {
            let mut next = std::pin::pin!(keyboard.next());
            assert!(
                std::future::poll_fn(|cx| Poll::Ready(next.as_mut().poll(cx)))
                    .await
                    .is_pending()
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn input_progress_and_new_prefixes_get_fresh_escape_windows() {
        use std::{future::Future, task::Poll};

        let mut keyboard = Keyboard {
            platform: platform::Keyboard::idle().unwrap(),
            decoder: Decoder::default(),
            events: VecDeque::new(),
            escape_deadline: None,
        };
        keyboard.feed(b"\x1b");
        keyboard.escape_deadline = Some(tokio::time::Instant::now() - Duration::from_secs(1));
        keyboard.feed(b"[");
        {
            let mut next = std::pin::pin!(keyboard.next());
            assert!(
                std::future::poll_fn(|cx| Poll::Ready(next.as_mut().poll(cx)))
                    .await
                    .is_pending()
            );
        }

        // Finish one marker and begin another in the same read. The new prefix
        // must not inherit the completed marker's elapsed waiting window.
        keyboard.escape_deadline = Some(tokio::time::Instant::now() - Duration::from_secs(1));
        keyboard.feed(b"200~\x1b");
        assert!(matches!(keyboard.next().await.unwrap(),
            Event::Bytes(value) if value == b"\x1b[200~"));
        {
            let mut next = std::pin::pin!(keyboard.next());
            assert!(
                std::future::poll_fn(|cx| Poll::Ready(next.as_mut().poll(cx)))
                    .await
                    .is_pending()
            );
        }
        keyboard.feed(b"[201~");
        assert!(matches!(keyboard.next().await.unwrap(),
            Event::Bytes(value) if value == b"\x1b[201~"));
        assert!(keyboard.escape_deadline.is_none());
    }

    #[test]
    fn utf8_is_preserved_at_every_byte_boundary() {
        let source = "中文 λ shell\n";
        for split in 0..=source.len() {
            let mut decoder = Decoder::default();
            let mut utf8 = Utf8Input::default();
            let first = utf8.push(
                &bytes(decoder.feed(&source.as_bytes()[..split], false)),
                false,
            );
            let second = utf8.push(
                &bytes(decoder.feed(&source.as_bytes()[split..], true)),
                true,
            );
            assert_eq!(first + &second, source);
        }
    }
    #[test]
    fn detach_never_forwards_key_or_trailing_bytes() {
        let mut decoder = Decoder::default();
        let events = decoder.feed(b"echo ok\n\x1dexit\n", false);
        assert!(events.iter().any(|e| matches!(e, Event::Detach)));
        assert_eq!(bytes(events), b"echo ok\n");
    }
    #[test]
    fn pasted_detach_is_literal_even_when_markers_are_split() {
        let source = b"\x1b[200~echo \x1d hello\x1b[201~";
        for split in 0..=source.len() {
            let mut decoder = Decoder::default();
            let mut events = decoder.feed(&source[..split], false);
            events.extend(decoder.feed(&source[split..], false));
            assert!(!events.iter().any(|e| matches!(e, Event::Detach)));
            assert_eq!(bytes(events), source);
            assert!(
                decoder
                    .feed(b"\x1d", false)
                    .iter()
                    .any(|e| matches!(e, Event::Detach))
            );
        }
    }
    #[test]
    fn binary_paste_is_preserved_before_utf8_decoding_at_every_boundary() {
        let source = b"\x1b[200~\x89PNG\r\n\x1a\n\x00\xff\x03\x1d\x11\x13\x1b[18~\x1b[201~";
        for read_only in [false, true] {
            for split in 0..=source.len() {
                let mut decoder = Decoder {
                    detach_on_interrupt: read_only,
                    ..Decoder::default()
                };
                let mut events = decoder.feed(&source[..split], false);
                events.extend(decoder.feed(&source[split..], true));
                assert!(
                    !events
                        .iter()
                        .any(|event| matches!(event, Event::Detach | Event::Interrupt))
                );
                assert_eq!(
                    bytes(events),
                    source,
                    "read-only {read_only}, split {split}"
                );
            }
        }
    }

    #[test]
    fn observer_interrupt_detaches_but_writer_interrupt_keeps_order() {
        let mut observer = Decoder {
            detach_on_interrupt: true,
            ..Decoder::default()
        };
        let events = observer.feed(b"before\x03after", false);
        assert!(events.iter().any(|event| matches!(event, Event::Detach)));
        assert_eq!(bytes(events), b"before");
        assert!(observer.feed(b"ignored", true).is_empty());

        let mut writer = Decoder::default();
        let events = writer.feed(b"before\x03after", false);
        assert!(matches!(
            events.as_slice(),
            [Event::Bytes(before), Event::Interrupt, Event::Bytes(after)]
                if before == b"before" && after == b"after"
        ));
    }

    #[test]
    fn fallback_utf8_keeps_invalid_and_incomplete_input_behavior() {
        let mut utf8 = Utf8Input::default();
        assert_eq!(utf8.push(b"a\xff\xe4", false), "a\u{fffd}");
        assert_eq!(utf8.push(b"\xb8\xad", false), "中");
        assert_eq!(utf8.push(b"\xe4", false), "");
        assert_eq!(utf8.push(&[], true), "\u{fffd}");
        assert_eq!(utf8.push(b"ok", true), "ok");
    }
}
