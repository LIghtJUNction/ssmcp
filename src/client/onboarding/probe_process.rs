//! Owned SSH probes with cancel-safe terminal prompts.
//!
//! OpenSSH runs in Managed's private session/process tree. Its ASKPASS helper
//! relays bounded prompts through a private socket to the foreground client,
//! so a proxy cannot escape cleanup merely because a human may need to answer.

use crate::backend::process::Managed;
use std::io;
use tokio::process::{Child, Command};

pub(super) struct ProbeProcess {
    process: Managed,
    #[cfg(unix)]
    bridge: Option<unix::Bridge>,
}

impl ProbeProcess {
    pub(super) fn spawn(command: &mut Command, interactive: bool) -> io::Result<Self> {
        #[cfg(unix)]
        let bridge = if interactive {
            Some(unix::Bridge::start(command)?)
        } else {
            None
        };
        #[cfg(not(unix))]
        let _ = interactive;
        Ok(Self {
            process: Managed::spawn(command)?,
            #[cfg(unix)]
            bridge,
        })
    }

    pub(super) fn child_mut(&mut self) -> &mut Child {
        &mut self.process.child
    }

    pub(super) async fn shutdown(&mut self) {
        #[cfg(unix)]
        if let Some(bridge) = self.bridge.take() {
            bridge.shutdown().await;
        }
        self.process.shutdown().await;
    }
}

impl Drop for ProbeProcess {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(bridge) = &mut self.bridge {
            bridge.stop();
        }
        self.process.terminate();
    }
}

/// Called before parsing CLI arguments. Only the private helper name enters it.
pub(super) fn run_askpass_helper() -> Option<i32> {
    #[cfg(unix)]
    {
        unix::run_helper()
    }
    #[cfg(not(unix))]
    {
        None
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::{
        ffi::OsStr,
        fs::{File, OpenOptions},
        io::{Read, Write},
        os::{
            fd::{AsRawFd, RawFd},
            unix::{
                fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink},
                net::UnixStream as SyncStream,
            },
        },
        path::{Path, PathBuf},
        sync::{Arc, Mutex, MutexGuard},
        time::Duration,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt, unix::AsyncFd},
        net::{UnixListener, UnixStream},
        task::JoinHandle,
    };

    const HELPER: &str = "ssmcp-askpass";
    const SOCKET_ENV: &str = "SSMCP_ASKPASS_SOCKET";
    const TOKEN_ENV: &str = "SSMCP_ASKPASS_TOKEN";
    const MODE_ENV: &str = "SSMCP_ASKPASS_MODE";
    const PROMPT_LIMIT: usize = 4096;
    // OpenSSH read_passphrase has a 1024-byte buffer including its final NUL.
    const ANSWER_LIMIT: usize = 1023;
    const REQUEST_LIMIT: usize = 16;

    pub(super) struct Bridge {
        terminal: Arc<Terminal>,
        task: Option<JoinHandle<()>>,
        _directory: PrivateDirectory,
    }

    impl Bridge {
        pub(super) fn start(command: &mut Command) -> io::Result<Self> {
            let terminal = Arc::new(Terminal::open()?);
            let directory = PrivateDirectory::create()?;
            let socket = directory.0.join("p");
            let listener = UnixListener::bind(&socket)?;
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
            let helper = directory.0.join(HELPER);
            symlink(std::env::current_exe()?, &helper)?;
            let mut token = [0_u8; 32];
            crate::platform::random_bytes(&mut token)?;
            let encoded: String = token.iter().map(|byte| format!("{byte:02x}")).collect();
            command
                .env("SSH_ASKPASS", helper)
                .env("SSH_ASKPASS_REQUIRE", "force")
                // Only this OpenSSH invocation may classify an individual
                // prompt. An inherited "confirm" must not expose a passphrase.
                .env_remove("SSH_ASKPASS_PROMPT")
                .env(MODE_ENV, "1")
                .env(SOCKET_ENV, socket)
                .env(TOKEN_ENV, encoded);
            let owned_terminal = terminal.clone();
            let task = tokio::spawn(async move {
                serve(listener, token, owned_terminal).await;
            });
            Ok(Self {
                terminal,
                task: Some(task),
                _directory: directory,
            })
        }

        pub(super) fn stop(&mut self) {
            // Restore synchronously as well as aborting the task: dropping a
            // future must not rely on a later runtime poll to re-enable echo.
            self.terminal.stop();
            if let Some(task) = &self.task {
                task.abort();
            }
        }

        pub(super) async fn shutdown(mut self) {
            self.stop();
            if let Some(task) = self.task.take() {
                let _ = task.await;
            }
        }
    }

    impl Drop for Bridge {
        fn drop(&mut self) {
            self.stop();
        }
    }

    struct PrivateDirectory(PathBuf);

    impl PrivateDirectory {
        fn create() -> io::Result<Self> {
            let mut base = std::env::temp_dir();
            // Darwin's sockaddr_un is shorter than Linux's. Avoid long TMPDIRs,
            // and never place the socket below a potentially long worktree.
            if base.as_os_str().len() > 60 {
                base = PathBuf::from("/tmp");
            }
            for _ in 0..4 {
                let mut random = [0_u8; 12];
                crate::platform::random_bytes(&mut random)?;
                let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
                let directory = base.join(format!("sap-{suffix}"));
                match crate::platform::private_directory(&directory) {
                    Ok(()) => {
                        let owned = Self(directory);
                        let metadata = std::fs::symlink_metadata(&owned.0)?;
                        // SAFETY: geteuid has no arguments or ownership effects.
                        if !metadata.is_dir()
                            || metadata.uid() != unsafe { libc::geteuid() }
                            || metadata.mode() & 0o077 != 0
                        {
                            return Err(io::Error::new(
                                io::ErrorKind::PermissionDenied,
                                "SSH prompt directory is not private",
                            ));
                        }
                        return Ok(owned);
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error),
                }
            }
            Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Could not reserve an SSH prompt directory",
            ))
        }
    }

    impl Drop for PrivateDirectory {
        fn drop(&mut self) {
            // Remove only names created here, never recursively follow a path.
            let _ = std::fs::remove_file(self.0.join("p"));
            let _ = std::fs::remove_file(self.0.join(HELPER));
            let _ = std::fs::remove_dir(&self.0);
        }
    }

    struct Terminal {
        file: AsyncFd<File>,
        foreground: Option<libc::pid_t>,
        state: Mutex<TerminalState>,
    }

    struct TerminalState {
        stopped: bool,
        serial: u64,
        active: Option<(u64, libc::termios)>,
    }

    impl Terminal {
        fn open() -> io::Result<Self> {
            // A fresh open owns O_NONBLOCK independently of the caller's stdin.
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOCTTY | libc::O_NONBLOCK)
                .open("/dev/tty")?;
            // SAFETY: the file remains open throughout these terminal queries.
            let group = unsafe { libc::getpgrp() };
            if unsafe { libc::tcgetpgrp(file.as_raw_fd()) } != group {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "OpenSSH prompts require the foreground terminal",
                ));
            }
            Self::from_file(file, Some(group))
        }

        fn from_file(file: File, foreground: Option<libc::pid_t>) -> io::Result<Self> {
            attributes(file.as_raw_fd())?;
            Ok(Self {
                // SAFETY: File exclusively owns the descriptor for the entire
                // AsyncFd lifetime; no raw close or descriptor replacement occurs.
                file: unsafe { AsyncFd::register(file)? },
                foreground,
                state: Mutex::new(TerminalState {
                    stopped: false,
                    serial: 0,
                    active: None,
                }),
            })
        }

        fn state(&self) -> MutexGuard<'_, TerminalState> {
            // A panic must not prevent restoration of the user's terminal.
            self.state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
        }

        fn check(&self, state: &TerminalState) -> io::Result<()> {
            if state.stopped {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "SSH prompt cancelled",
                ));
            }
            if let Some(group) = self.foreground {
                // SAFETY: AsyncFd retains ownership of this terminal descriptor.
                if unsafe { libc::tcgetpgrp(self.file.as_raw_fd()) } != group {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "OpenSSH prompts require the foreground terminal",
                    ));
                }
            }
            Ok(())
        }

        fn begin(self: &Arc<Self>, echo: bool) -> io::Result<PromptGuard> {
            let mut state = self.state();
            self.check(&state)?;
            if state.active.is_some() {
                return Err(io::Error::other("Another SSH prompt is already active"));
            }
            let old = attributes(self.file.as_raw_fd())?;
            let mut current = old;
            current.c_lflag |= libc::ICANON | libc::ISIG;
            current.c_lflag &= !(libc::ECHO | libc::ECHONL);
            if echo {
                current.c_lflag |= libc::ECHO;
            }
            set_attributes(self.file.as_raw_fd(), &current)?;
            state.serial += 1;
            let serial = state.serial;
            state.active = Some((serial, old));
            drop(state);
            let guard = PromptGuard {
                terminal: self.clone(),
                serial,
            };
            flush_input(self.file.as_raw_fd())?;
            Ok(guard)
        }

        fn restore(&self, serial: Option<u64>, stop: bool) {
            let mut state = self.state();
            state.stopped |= stop;
            if let Some((active, old)) = &state.active
                && serial.is_none_or(|serial| serial == *active)
            {
                // Flush any unfinished secret instead of exposing it in a
                // subsequent shell command or a later setup question.
                let _ = flush_input(self.file.as_raw_fd());
                let _ = set_attributes(self.file.as_raw_fd(), old);
                state.active = None;
            }
        }

        fn stop(&self) {
            self.restore(None, true);
        }

        async fn write(&self, mut bytes: &[u8]) -> io::Result<()> {
            while !bytes.is_empty() {
                let mut ready = self.file.writable().await?;
                let written = {
                    let state = self.state();
                    self.check(&state)?;
                    ready.try_io(|file| {
                        // Foreground can change after check(); an I/O error is
                        // recoverable, a background-job stop would not be.
                        let _mask = TerminalSignalMask::enter()?;
                        let mut file = file.get_ref();
                        file.write(bytes)
                    })
                };
                match written {
                    Ok(Ok(0)) => return Err(io::ErrorKind::WriteZero.into()),
                    Ok(Ok(count)) => bytes = &bytes[count..],
                    Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Ok(Err(error)) => return Err(error),
                    Err(_) => continue,
                }
            }
            Ok(())
        }

        async fn answer(self: &Arc<Self>, prompt: &str, kind: u8) -> io::Result<Secret> {
            if kind == 2 {
                self.write(format!("\r\n[OpenSSH] {}\r\n", safe_prompt(prompt)).as_bytes())
                    .await?;
                return Ok(Secret::empty());
            }
            let guard = self.begin(kind == 1)?;
            self.write(format!("\r\n[OpenSSH] {}", safe_prompt(prompt)).as_bytes())
                .await?;
            let mut secret = Secret::empty();
            loop {
                let mut ready = self.file.readable().await?;
                let read = {
                    let state = self.state();
                    self.check(&state)?;
                    ready.try_io(|file| {
                        // Foreground can change after check(); an I/O error is
                        // recoverable, a background-job stop would not be.
                        let _mask = TerminalSignalMask::enter()?;
                        let mut file = file.get_ref();
                        file.read(&mut secret.bytes[secret.length..])
                    })
                };
                let count = match read {
                    Ok(Ok(0)) => return Err(io::ErrorKind::UnexpectedEof.into()),
                    Ok(Ok(count)) => count,
                    Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Ok(Err(error)) => return Err(error),
                    Err(_) => continue,
                };
                let end = secret.length + count;
                if let Some(newline) = secret.bytes[..end].iter().position(|b| *b == b'\n') {
                    if newline > ANSWER_LIMIT || secret.bytes[..newline].contains(&0) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "SSH answer is invalid or too long",
                        ));
                    }
                    secret.length = newline;
                    // Restore before sending an answer or closing any channel.
                    drop(guard);
                    if kind != 1 {
                        self.write(b"\r\n").await?;
                    }
                    return Ok(secret);
                }
                secret.length = end;
                if end > ANSWER_LIMIT {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "SSH answer is too long",
                    ));
                }
            }
        }
    }

    struct PromptGuard {
        terminal: Arc<Terminal>,
        serial: u64,
    }

    impl Drop for PromptGuard {
        fn drop(&mut self) {
            self.terminal.restore(Some(self.serial), false);
        }
    }

    fn attributes(fd: RawFd) -> io::Result<libc::termios> {
        // SAFETY: tcgetattr initializes the local structure on success.
        let mut attributes = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut attributes) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(attributes)
    }

    struct TerminalSignalMask(libc::sigset_t);

    impl TerminalSignalMask {
        fn enter() -> io::Result<Self> {
            // These calls run synchronously on one thread. Blocking SIGTTOU
            // allows restoration after the client loses terminal foreground.
            let mut previous = unsafe { std::mem::zeroed() };
            let mut blocked = unsafe { std::mem::zeroed() };
            unsafe {
                libc::sigemptyset(&mut blocked);
                libc::sigaddset(&mut blocked, libc::SIGTTOU);
                libc::sigaddset(&mut blocked, libc::SIGTTIN);
            }
            let error = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut previous) };
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error));
            }
            Ok(Self(previous))
        }
    }

    impl Drop for TerminalSignalMask {
        fn drop(&mut self) {
            // SAFETY: this restores only the calling thread's captured mask.
            unsafe {
                libc::pthread_sigmask(libc::SIG_SETMASK, &self.0, std::ptr::null_mut());
            }
        }
    }

    fn set_attributes(fd: RawFd, attributes: &libc::termios) -> io::Result<()> {
        let _mask = TerminalSignalMask::enter()?;
        loop {
            // TCSANOW never waits for output to drain. In particular, Ctrl-S
            // or a full terminal output queue must not block cancellation.
            // SAFETY: attributes is initialized and fd is owned by Terminal.
            if unsafe { libc::tcsetattr(fd, libc::TCSANOW, attributes) } == 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    fn flush_input(fd: RawFd) -> io::Result<()> {
        let _mask = TerminalSignalMask::enter()?;
        loop {
            // SAFETY: this discards only input on the owned terminal descriptor.
            if unsafe { libc::tcflush(fd, libc::TCIFLUSH) } == 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    fn safe_prompt(prompt: &str) -> String {
        let mut safe = String::with_capacity(prompt.len().min(PROMPT_LIMIT));
        for ch in prompt.chars() {
            let ch = match ch {
                '\n' => '\n',
                '\t' | '\r' => ' ',
                ch if ch.is_control()
                    || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') =>
                {
                    '\u{fffd}'
                }
                ch => ch,
            };
            if safe.len() + ch.len_utf8() > PROMPT_LIMIT {
                break;
            }
            safe.push(ch);
        }
        safe
    }

    struct Secret {
        bytes: Vec<u8>,
        length: usize,
    }

    impl Secret {
        fn empty() -> Self {
            Self {
                bytes: vec![0; ANSWER_LIMIT + 1],
                length: 0,
            }
        }

        fn expose(&self) -> &[u8] {
            &self.bytes[..self.length]
        }
    }

    impl Drop for Secret {
        fn drop(&mut self) {
            for byte in &mut self.bytes {
                // SAFETY: every byte is live, exclusively owned storage.
                unsafe { std::ptr::write_volatile(byte, 0) };
            }
            std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
        }
    }

    async fn serve(listener: UnixListener, token: [u8; 32], terminal: Arc<Terminal>) {
        for _ in 0..REQUEST_LIMIT {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            // Socket ownership plus a fresh token binds requests to this probe.
            // SAFETY: geteuid is a read-only process credential query.
            if stream.peer_cred().map(|peer| peer.uid()).ok() != Some(unsafe { libc::geteuid() }) {
                continue;
            }
            let request =
                tokio::time::timeout(Duration::from_secs(2), read_request(&mut stream, &token))
                    .await;
            let Ok(Ok((kind, prompt))) = request else {
                continue;
            };
            let mut extra = [0_u8; 1];
            let answer = tokio::select! {
                answer = terminal.answer(&prompt, kind) => answer,
                // If OpenSSH cancels its helper, abandon the terminal read now.
                _ = stream.read(&mut extra) => Err(io::ErrorKind::Interrupted.into()),
            };
            if let Ok(answer) = answer {
                let send = async {
                    stream.write_u16(answer.length as u16).await?;
                    stream.write_all(answer.expose()).await
                };
                let _ = tokio::time::timeout(Duration::from_secs(2), send).await;
            }
        }
    }

    async fn read_request(stream: &mut UnixStream, token: &[u8; 32]) -> io::Result<(u8, String)> {
        let mut supplied = [0_u8; 32];
        stream.read_exact(&mut supplied).await?;
        // Avoid a token-prefix timing oracle even to other local processes.
        if supplied
            .iter()
            .zip(token)
            .fold(0_u8, |diff, (a, b)| diff | (a ^ b))
            != 0
        {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let kind = stream.read_u8().await?;
        let size = stream.read_u16().await? as usize;
        if kind > 2 || size == 0 || size > PROMPT_LIMIT {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut prompt = vec![0_u8; size];
        stream.read_exact(&mut prompt).await?;
        String::from_utf8(prompt)
            .map(|prompt| (kind, prompt))
            .map_err(|_| io::ErrorKind::InvalidData.into())
    }

    pub(super) fn run_helper() -> Option<i32> {
        let mut arguments = std::env::args_os();
        let name = arguments.next()?;
        if Path::new(&name).file_name() != Some(OsStr::new(HELPER)) {
            return None;
        }
        // Fail closed without invoking normal CLI parsing or printing secrets.
        Some(if helper(&name, arguments).is_ok() {
            0
        } else {
            1
        })
    }

    fn helper(
        name: &OsStr,
        mut arguments: impl Iterator<Item = std::ffi::OsString>,
    ) -> io::Result<()> {
        if std::env::var_os(MODE_ENV).as_deref() != Some(OsStr::new("1")) {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let prompt = arguments.next().ok_or(io::ErrorKind::InvalidInput)?;
        let prompt = prompt.to_str().ok_or(io::ErrorKind::InvalidData)?;
        if arguments.next().is_some() || prompt.is_empty() || prompt.len() > PROMPT_LIMIT {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let socket = PathBuf::from(std::env::var_os(SOCKET_ENV).ok_or(io::ErrorKind::NotFound)?);
        if socket.file_name() != Some(OsStr::new("p"))
            || socket.parent() != Path::new(name).parent()
        {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let metadata = std::fs::symlink_metadata(&socket)?;
        // SAFETY: geteuid is a read-only credential query.
        if !metadata.file_type().is_socket()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let token = std::env::var(TOKEN_ENV).map_err(|_| io::ErrorKind::InvalidData)?;
        let token = decode_token(&token)?;
        let kind = match std::env::var("SSH_ASKPASS_PROMPT").ok().as_deref() {
            Some("confirm") => 1,
            Some("none") => 2,
            _ => 0,
        };
        let mut stream = SyncStream::connect(&socket)?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        stream.set_read_timeout(Some(Duration::from_secs(60)))?;
        stream.write_all(&token)?;
        stream.write_all(&[kind])?;
        stream.write_all(&(prompt.len() as u16).to_be_bytes())?;
        stream.write_all(prompt.as_bytes())?;
        let mut size = [0_u8; 2];
        stream.read_exact(&mut size)?;
        let length = u16::from_be_bytes(size) as usize;
        if length > ANSWER_LIMIT {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut answer = Secret::empty();
        answer.length = length;
        stream.read_exact(&mut answer.bytes[..length])?;
        if answer
            .expose()
            .iter()
            .any(|byte| matches!(*byte, 0 | b'\r' | b'\n'))
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut output = std::io::stdout().lock();
        output.write_all(answer.expose())?;
        output.write_all(b"\n")?;
        output.flush()
    }

    fn decode_token(token: &str) -> io::Result<[u8; 32]> {
        if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut decoded = [0_u8; 32];
        for (index, byte) in decoded.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&token[index * 2..index * 2 + 2], 16)
                .map_err(|_| io::ErrorKind::InvalidData)?;
        }
        Ok(decoded)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::fd::{FromRawFd, OwnedFd};

        fn test_terminal() -> (Arc<Terminal>, File) {
            let mut master = -1;
            let mut slave = -1;
            // SAFETY: openpty initializes two owned descriptors; null optional
            // arguments request default settings for this isolated test PTY.
            let result = unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(result, 0);
            let master = unsafe { OwnedFd::from_raw_fd(master) };
            let slave = unsafe { OwnedFd::from_raw_fd(slave) };
            for fd in [&master, &slave] {
                let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
                assert!(flags >= 0);
                assert_eq!(
                    unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
                    0
                );
            }
            (
                Arc::new(Terminal::from_file(File::from(slave), None).unwrap()),
                File::from(master),
            )
        }

        async fn observe_prompt(master: &mut File) -> Vec<u8> {
            tokio::time::timeout(Duration::from_secs(3), async {
                let mut transcript = Vec::new();
                loop {
                    let mut bytes = [0_u8; 1024];
                    match master.read(&mut bytes) {
                        Ok(size) if size > 0 => {
                            transcript.extend_from_slice(&bytes[..size]);
                            if transcript.ends_with(b"fixture: ") {
                                return transcript;
                            }
                        }
                        Ok(_) => panic!("PTY unexpectedly closed"),
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                        Err(error) => panic!("PTY read: {error}"),
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap()
        }

        #[tokio::test]
        async fn real_pty_secret_is_hidden_and_terminal_is_restored() {
            let (terminal, mut master) = test_terminal();
            let before = attributes(terminal.file.as_raw_fd()).unwrap();
            let child = terminal.clone();
            let task = tokio::spawn(async move { child.answer("fixture: ", 0).await });
            let mut transcript = observe_prompt(&mut master).await;
            let active = attributes(terminal.file.as_raw_fd()).unwrap();
            assert_eq!(active.c_lflag & (libc::ECHO | libc::ECHONL), 0);
            master.write_all(b"fixture-private-answer\n").unwrap();
            let answer = tokio::time::timeout(Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(answer.expose(), b"fixture-private-answer");
            let mut tail = [0_u8; 1024];
            if let Ok(size) = master.read(&mut tail) {
                transcript.extend_from_slice(&tail[..size]);
            }
            assert!(!String::from_utf8_lossy(&transcript).contains("fixture-private-answer"));
            let after = attributes(terminal.file.as_raw_fd()).unwrap();
            assert_eq!(before.c_lflag, after.c_lflag);
            assert_eq!(before.c_cc, after.c_cc);
        }

        #[tokio::test]
        async fn cancelling_a_pending_real_pty_prompt_restores_echo_synchronously() {
            let (terminal, mut master) = test_terminal();
            let before = attributes(terminal.file.as_raw_fd()).unwrap();
            let child = terminal.clone();
            let task = tokio::spawn(async move { child.answer("fixture: ", 0).await });
            observe_prompt(&mut master).await;
            master.write_all(b"unfinished-secret").unwrap();
            terminal.stop();
            // This is checked before the aborted future gets another poll.
            let after = attributes(terminal.file.as_raw_fd()).unwrap();
            assert_eq!(before.c_lflag, after.c_lflag);
            assert_eq!(before.c_cc, after.c_cc);
            task.abort();
            assert!(matches!(task.await, Err(error) if error.is_cancelled()));
            let mut bytes = [0_u8; 128];
            assert_eq!(
                terminal.file.get_ref().read(&mut bytes).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
        }

        #[tokio::test]
        async fn socket_authentication_and_size_limits_precede_prompt_allocation() {
            let (mut one, mut two) = UnixStream::pair().unwrap();
            one.write_all(&[1_u8; 32]).await.unwrap();
            assert_eq!(
                read_request(&mut two, &[2_u8; 32])
                    .await
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
            let (mut one, mut two) = UnixStream::pair().unwrap();
            one.write_all(&[2_u8; 32]).await.unwrap();
            one.write_u8(0).await.unwrap();
            one.write_u16((PROMPT_LIMIT + 1) as u16).await.unwrap();
            assert_eq!(
                read_request(&mut two, &[2_u8; 32])
                    .await
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }

        #[test]
        fn prompt_controls_cannot_rewrite_the_terminal_or_exceed_the_limit() {
            assert_eq!(safe_prompt("a\u{1b}[2J\rb\u{202e}c\n"), "a�[2J b�c\n");
            let safe = safe_prompt(&"\u{1b}".repeat(PROMPT_LIMIT));
            assert!(safe.len() <= PROMPT_LIMIT);
            assert!(!safe.contains('\u{1b}'));
            assert!(decode_token(&"f".repeat(64)).is_ok());
            assert!(decode_token(&"f".repeat(63)).is_err());
            assert!(decode_token(&"é".repeat(32)).is_err());
        }
    }
}
