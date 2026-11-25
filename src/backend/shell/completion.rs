use super::{DeadlineError, command_timeout};
use std::{future::Future, io, time::Instant};
use tokio::sync::watch;

#[derive(Debug, Default)]
pub(super) struct Outcome {
    pub(super) code: Option<i32>,
    pub(super) signal: Option<i32>,
    pub(super) error: Option<String>,
    pub(super) cancelled: bool,
    pub(super) timed_out: bool,
}

/// Preserve an exit that is already observable when a cancellation or deadline
/// is also ready. A still-running process remains immediately interruptible.
pub(super) async fn observe(
    exit: impl Future<Output = io::Result<(Option<i32>, Option<i32>)>>,
    mut cancel: watch::Receiver<bool>,
    deadline: Option<Instant>,
) -> Outcome {
    let observation = async {
        tokio::select! {
            biased;
            result = exit => match result {
                Ok((code, signal)) => Outcome { code, signal, ..Outcome::default() },
                Err(error) => Outcome { error: Some(error.to_string()), ..Outcome::default() },
            },
            _ = cancel.changed() => Outcome { cancelled: true, ..Outcome::default() },
        }
    };
    let Some(deadline) = deadline else {
        return observation.await;
    };
    match command_timeout(deadline, observation).await {
        Ok(outcome) => outcome,
        Err(DeadlineError::Elapsed) => Outcome {
            timed_out: true,
            ..Outcome::default()
        },
        #[cfg(unix)]
        Err(DeadlineError::Reactor(error)) => Outcome {
            error: Some(error.to_string()),
            ..Outcome::default()
        },
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::command_runtime::Driver;
    use std::{
        os::unix::process::ExitStatusExt,
        process::{Command, Stdio},
        time::Duration,
    };
    use zrun::process::Child;

    async fn wait(child: &mut Child) -> io::Result<(Option<i32>, Option<i32>)> {
        let status = child.wait(&Driver::get()?.timer).await?;
        Ok((status.code(), status.signal()))
    }

    // WNOWAIT leaves the status available to the production observer. No sleep
    // or scheduler timing assumption determines whether the process has exited.
    fn await_exit_without_reaping(pid: u32) {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::uninit();
        loop {
            // SAFETY: waitid writes to this live buffer for our own child.
            if unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid,
                    info.as_mut_ptr(),
                    libc::WEXITED | libc::WNOWAIT,
                )
            } == 0
            {
                return;
            }
            assert_eq!(
                io::Error::last_os_error().kind(),
                io::ErrorKind::Interrupted
            );
        }
    }

    #[tokio::test]
    async fn observed_exit_survives_queued_cancel_and_due_deadline() {
        for (script, code, signal) in [
            ("exit 0", Some(0), None),
            ("exit 23", Some(23), None),
            ("kill -TERM $$", None, Some(libc::SIGTERM)),
        ] {
            for due in [false, true] {
                let mut child = zrun::process::spawn(
                    Command::new("/bin/sh").args(["-c", script]),
                    &Driver::get().unwrap().io,
                )
                .unwrap();
                await_exit_without_reaping(child.id());
                let (cancel, receiver) = watch::channel(false);
                cancel.send(true).unwrap();
                let outcome = observe(wait(&mut child), receiver, due.then(Instant::now)).await;
                assert_eq!(
                    (outcome.code, outcome.signal),
                    (code, signal),
                    "{outcome:?}"
                );
                assert!(!outcome.cancelled && !outcome.timed_out && outcome.error.is_none());
            }
        }
    }

    #[tokio::test]
    async fn live_child_remains_interruptible_and_unknown_status_is_preserved() {
        let (reader, writer) = std::io::pipe().unwrap();
        let mut child = zrun::process::spawn(
            Command::new("/bin/cat").stdin(reader).stdout(Stdio::null()),
            &Driver::get().unwrap().io,
        )
        .unwrap();
        let (cancel, receiver) = watch::channel(false);
        let outcome = observe(
            wait(&mut child),
            receiver.clone(),
            Some(Instant::now() + Duration::from_millis(30)),
        )
        .await;
        assert!(outcome.timed_out && !outcome.cancelled && outcome.error.is_none());
        cancel.send(true).unwrap();
        let outcome = observe(wait(&mut child), receiver.clone(), Some(Instant::now())).await;
        assert!(outcome.cancelled && !outcome.timed_out && outcome.error.is_none());
        let outcome = observe(std::future::ready(Ok((None, None))), receiver, None).await;
        assert_eq!((outcome.code, outcome.signal), (None, None));
        assert!(!outcome.cancelled && !outcome.timed_out && outcome.error.is_none());
        drop(writer);
        assert_eq!(wait(&mut child).await.unwrap(), (Some(0), None));
    }
}
