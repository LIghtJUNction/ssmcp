use super::*;
use crate::client::link_metrics::{Observer, Recorder};
use tokio::{
    sync::{mpsc, oneshot},
    time::{Duration, timeout},
};

fn pair() -> (Prepared, Reporter) {
    let (parent, child) = UnixDatagram::pair().unwrap();
    parent.set_nonblocking(true).unwrap();
    child.set_nonblocking(true).unwrap();
    (Prepared { socket: parent }, Reporter(child))
}

fn sample(ms: u64, up: u64, down: u64) -> Sample {
    Sample {
        elapsed_ms: ms,
        uploaded: up,
        downloaded: down,
    }
}

#[tokio::test]
async fn link_metrics_real_datagrams_report_rates_then_disconnect() {
    let (prepared, reporter) = pair();
    let (closed, receiver) = watch::channel(None);
    let (events, mut received) = mpsc::channel(8);
    let task = observe(Some(prepared), receiver, move |status| {
        let _ = events.try_send(status);
    });
    reporter.send(&sample(10, 1, 2).encode()).unwrap();
    assert_eq!(
        timeout(Duration::from_secs(2), received.recv())
            .await
            .unwrap(),
        Some(Status::Sampling)
    );
    reporter.send(&sample(1010, 1001, 2002).encode()).unwrap();
    assert_eq!(
        timeout(Duration::from_secs(2), received.recv())
            .await
            .unwrap(),
        Some(Status::Ready {
            uploaded: 1000,
            downloaded: 2000
        })
    );
    closed.send(Some("fixture disconnected".into())).unwrap();
    assert_eq!(
        timeout(Duration::from_secs(2), received.recv())
            .await
            .unwrap(),
        Some(Status::Disconnected)
    );
    timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    assert!(reporter.send(&sample(2010, 1001, 2002).encode()).is_err());
}

#[tokio::test]
async fn link_metrics_stale_and_malformed_samples_never_become_idle_zero() {
    let (prepared, reporter) = pair();
    let (closed, receiver) = watch::channel(None);
    let (events, mut received) = mpsc::channel(8);
    let task = observe(Some(prepared), receiver, move |status| {
        let _ = events.try_send(status);
    });
    reporter.send(&sample(10, 0, 0).encode()).unwrap();
    assert_eq!(received.recv().await, Some(Status::Sampling));
    reporter.0.send(b"bad sample").unwrap();
    assert_eq!(
        timeout(Duration::from_secs(6), received.recv())
            .await
            .unwrap(),
        Some(Status::Unavailable)
    );
    reporter
        .send(&sample(9000, 999_999, 999_999).encode())
        .unwrap();
    assert_eq!(
        timeout(Duration::from_secs(2), received.recv())
            .await
            .unwrap(),
        Some(Status::Sampling)
    );
    closed.send(Some("done".into())).unwrap();
    timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
}

struct Dropped(Option<oneshot::Sender<()>>);
impl Drop for Dropped {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

#[tokio::test]
async fn link_metrics_observer_drop_cancels_task_and_releases_owned_socket() {
    let (prepared, reporter) = pair();
    let (_closed, receiver) = watch::channel(None);
    let (dropped, ended) = oneshot::channel();
    let signal = Dropped(Some(dropped));
    let task = observe(Some(prepared), receiver, move |_| {
        let _ = &signal;
    });
    let observer = Observer {
        generation: u64::MAX,
        task: Some(task),
    };
    tokio::task::yield_now().await;
    drop(observer);
    timeout(Duration::from_secs(2), ended)
        .await
        .unwrap()
        .unwrap();
    assert!(reporter.send(&sample(0, 0, 0).encode()).is_err());
}

#[test]
fn link_metrics_buffer_exhaustion_is_retryable_without_hiding_other_failures() {
    let error = send_error(io::Error::from_raw_os_error(libc::ENOBUFS));
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(
        error
            .get_ref()
            .and_then(|source| source.downcast_ref::<io::Error>())
            .and_then(io::Error::raw_os_error),
        Some(libc::ENOBUFS)
    );
    for code in [libc::EAGAIN, libc::ECONNREFUSED, libc::EBADF, libc::ENOMEM] {
        let original = io::Error::from_raw_os_error(code);
        let kind = original.kind();
        let error = send_error(original);
        assert_eq!(error.raw_os_error(), Some(code));
        assert_eq!(error.kind(), kind);
    }
    let error = send_error(io::Error::other("metrics fixture error"));
    assert_eq!(error.kind(), io::ErrorKind::Other);
    assert_eq!(error.to_string(), "metrics fixture error");
}

#[test]
fn link_metrics_reporter_is_one_hertz_bounded_and_never_waits_for_a_subscriber() {
    let (prepared, reporter) = pair();
    let mut recorder = Recorder::new(Some(reporter));
    recorder.sent(7);
    recorder.received(9);
    recorder.publish();
    let mut buffer = [0; 33];
    let n = prepared.socket.recv(&mut buffer).unwrap();
    let first = Sample::decode(&buffer[..n]).unwrap();
    assert_eq!((first.uploaded, first.downloaded), (7, 9));
    recorder.publish();
    assert_eq!(
        prepared.socket.recv(&mut buffer).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    let sender = recorder.reporter.as_ref().unwrap();
    let mut full = false;
    for _ in 0..65536 {
        if let Err(error) = sender.send(&sample(1000, 8, 10).encode()) {
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
            full = true;
            break;
        }
    }
    assert!(full, "datagram kernel buffer must be bounded");
    recorder.last_report = Some(Instant::now() - Duration::from_secs(2));
    recorder.publish();
    assert!(
        recorder.enabled(),
        "a full queue drops one sample, not the transport"
    );
    let mut drained = 0;
    loop {
        match prepared.socket.recv(&mut buffer) {
            Ok(_) => drained += 1,
            Err(error) => {
                assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
                break;
            }
        }
    }
    assert!(drained > 0, "the full queue held actual samples");
    recorder.sent(5);
    recorder.received(7);
    recorder.last_report = Some(Instant::now() - Duration::from_secs(2));
    recorder.publish();
    let n = prepared.socket.recv(&mut buffer).unwrap();
    let recovered = Sample::decode(&buffer[..n]).unwrap();
    assert_eq!((recovered.uploaded, recovered.downloaded), (12, 16));
    assert!(recorder.enabled(), "draining resumes the same reporter");
    drop(prepared);
    // A parallel fork can briefly retain the peer FD before exec closes it.
    // Keep probing for the real disconnect without waiting out the sample interval.
    let deadline = Instant::now() + Duration::from_secs(1);
    while recorder.enabled() && Instant::now() < deadline {
        recorder.last_report = Some(Instant::now() - Duration::from_secs(2));
        recorder.publish();
        if recorder.enabled() {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    assert!(
        !recorder.enabled(),
        "an absent subscriber disables telemetry"
    );
}

const HELPER: &str = "client::link_metrics::transport::tests::link_metrics_descriptor_child";
const MODE: &str = "SSMCP_TEST_METRICS_DESCRIPTOR";

#[tokio::test]
async fn link_metrics_private_descriptor_has_correct_exec_ownership() {
    // Keep the parent's table occupied beyond the formerly hard-coded FD 198.
    let mut occupied = Vec::new();
    loop {
        let file = std::fs::File::open("/dev/null").unwrap();
        let high = file.as_raw_fd() >= 220;
        occupied.push(file);
        if high {
            break;
        }
        assert!(occupied.len() < 512);
    }
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", HELPER, "--nocapture"])
        .env(MODE, "child");
    let prepared = prepare(&mut command).unwrap();
    let parent_fd = prepared.socket.as_raw_fd();
    // SAFETY: this is our live, owned socket.
    let flags = unsafe { libc::fcntl(parent_fd, libc::F_GETFD) };
    assert_ne!(flags & libc::FD_CLOEXEC, 0);
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    assert_eq!(unsafe { libc::fstat(parent_fd, stat.as_mut_ptr()) }, 0);
    let inode = unsafe { stat.assume_init() }.st_ino;
    command
        .env("SSMCP_TEST_METRICS_PARENT_FD", parent_fd.to_string())
        .env("SSMCP_TEST_METRICS_PARENT_INODE", inode.to_string());
    let mut child = command.spawn().unwrap();
    drop(command);
    let socket = tokio::net::UnixDatagram::from_std(prepared.socket).unwrap();
    let mut bytes = [0; 33];
    let n = timeout(Duration::from_secs(5), socket.recv(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(Sample::decode(&bytes[..n]), Some(sample(1000, 123, 456)));
    assert!(
        timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

#[test]
fn link_metrics_descriptor_child() {
    let Ok(mode) = std::env::var(MODE) else {
        return;
    };
    let fd: RawFd = std::env::var(ENV).unwrap().parse().unwrap();
    assert!(fd >= 3);
    if mode == "grandchild" {
        // No relay socket survives an exec into a descendant.
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
        return;
    }
    if mode == "unavailable" {
        assert!(
            reporter().is_none(),
            "a missing private FD is unavailable, not a different descriptor"
        );
        return;
    }
    assert_eq!(mode, "child");
    let parent_fd: RawFd = std::env::var("SSMCP_TEST_METRICS_PARENT_FD")
        .unwrap()
        .parse()
        .unwrap();
    let parent_inode: libc::ino_t = std::env::var("SSMCP_TEST_METRICS_PARENT_INODE")
        .unwrap()
        .parse()
        .unwrap();
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(parent_fd, stat.as_mut_ptr()) } == 0 {
        assert_ne!(unsafe { stat.assume_init() }.st_ino, parent_inode);
    }
    // The only descriptor modified is the captured socket itself, already
    // occupied before Command opened its private exec-error pipe.
    assert_eq!(
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
        0
    );
    unsafe { inherit_descriptor(fd) }.unwrap();
    assert_eq!(
        unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
    let reporter = reporter().expect("private relay socket is inherited");
    assert_ne!(
        unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", HELPER, "--nocapture"])
        .env(MODE, "grandchild")
        .status()
        .unwrap();
    assert!(status.success());
    reporter.send(&sample(1000, 123, 456).encode()).unwrap();
}

#[tokio::test]
async fn link_metrics_optional_descriptor_failure_does_not_prevent_exec() {
    use std::sync::{
        Arc,
        atomic::{AtomicI32, Ordering},
    };
    let source = Arc::new(AtomicI32::new(-1));
    let captured = source.clone();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", HELPER, "--nocapture"])
        .env(MODE, "unavailable");
    // Break only our metrics descriptor in this one forked fixture child,
    // before prepare's inheritance callback. All parent FDs remain untouched.
    unsafe {
        command.pre_exec(move || {
            let fd = captured.load(Ordering::Relaxed);
            if fd >= 3 {
                libc::close(fd);
            }
            Ok(())
        });
    }
    let _prepared = prepare(&mut command).unwrap();
    let fd: RawFd = command
        .as_std()
        .get_envs()
        .find(|(name, _)| *name == ENV)
        .unwrap()
        .1
        .unwrap()
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    source.store(fd, Ordering::Relaxed);
    assert!(
        timeout(Duration::from_secs(5), command.status())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}
