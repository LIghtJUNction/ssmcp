use super::*;
use crate::client::terminal::{Layout, Mutex, Session, State, Ui};
use std::{
    sync::atomic::{AtomicBool, AtomicUsize},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn shared() -> Arc<Shared> {
    Arc::new(Shared {
        state: Mutex::new(State::new(
            30,
            120,
            &Ui {
                layout: Layout::Split,
                ..Ui::default()
            },
        )),
        stop: AtomicBool::new(false),
        alive: AtomicBool::new(true),
    })
}

#[tokio::test]
async fn startup_update_probe_skips_pipes_disconnected_and_preview_sessions() {
    let display = shared();
    for (connected, stdout) in [(true, false), (false, true), (false, false)] {
        let polls = Arc::new(AtomicUsize::new(0));
        let observed = polls.clone();
        assert!(
            spawn(&display, connected, stdout, async move {
                observed.fetch_add(1, Ordering::SeqCst);
                None
            })
            .is_none()
        );
        tokio::task::yield_now().await;
        assert_eq!(polls.load(Ordering::SeqCst), 0);
    }
    display.state.lock().unwrap().preview = true;
    assert!(
        spawn(&display, true, true, async {
            panic!("preview must not fetch releases")
        })
        .is_none()
    );
}

#[test]
fn startup_update_notice_stays_with_its_live_session_until_presented() {
    let old = shared();
    let next = shared();
    let claimed = AtomicBool::new(false);
    let shown = |_: &Notice| claimed.load(Ordering::SeqCst);
    old.stop.store(true, Ordering::Release);
    assert!(!publish(&Arc::downgrade(&old), "old".into(), shown));
    old.stop.store(false, Ordering::Release);
    old.alive.store(false, Ordering::Release);
    assert!(!publish(&Arc::downgrade(&old), "old".into(), shown));
    old.alive.store(true, Ordering::Release);
    old.state.lock().unwrap().preview = true;
    assert!(!publish(&Arc::downgrade(&old), "old".into(), shown));
    let expired = Arc::downgrade(&old);
    drop(old);
    assert!(!publish(&expired, "old".into(), shown));
    assert!(!claimed.load(Ordering::SeqCst));
    next.state.lock().unwrap().notice = Some("Keep the recovery message".into());
    assert!(publish(&Arc::downgrade(&next), "current".into(), shown));
    assert!(
        !claimed.load(Ordering::SeqCst),
        "publishing must not consume presentation"
    );
    claimed.store(true, Ordering::SeqCst);
    assert!(!publish(&Arc::downgrade(&next), "duplicate".into(), shown));
    let state = next.state.lock().unwrap();
    assert_eq!(
        state.update_notice.as_ref().map(Notice::text).as_deref(),
        Some("current")
    );
    assert_eq!(state.notice.as_deref(), Some("Keep the recovery message"));
}

#[tokio::test]
async fn startup_update_probe_session_drop_cancels_its_in_flight_http_request() {
    struct Finished(Option<tokio::sync::oneshot::Sender<()>>);
    impl Drop for Finished {
        fn drop(&mut self) {
            if let Some(done) = self.0.take() {
                let _ = done.send(());
            }
        }
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (started, request_started) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let size = stream.read(&mut buffer).await.unwrap();
            assert_ne!(size, 0);
            request.extend_from_slice(&buffer[..size]);
            assert!(request.len() < 8192);
        }
        started.send(()).unwrap();
        let closed = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer))
            .await
            .unwrap();
        assert!(matches!(closed, Ok(0)) || closed.is_err(), "{closed:?}");
        let _ = stream.shutdown().await;
    });
    let display = shared();
    let (finished, request_dropped) = tokio::sync::oneshot::channel();
    let probe = spawn(&display, true, true, async move {
        let _finished = Finished(Some(finished));
        let client = crate::http_client::builder()
            .no_proxy()
            .pool_max_idle_per_host(0)
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap();
        let _ = client.get(url).send().await;
        None
    })
    .unwrap();
    let session = Session {
        shared: display,
        thread: None,
        guide_probe: None,
        update_probe: Some(probe),
    };
    tokio::time::timeout(Duration::from_secs(5), request_started)
        .await
        .unwrap()
        .unwrap();
    // Avoid capturing a synthetic session as the process's reload checkpoint.
    session.shared.state.lock().unwrap().preview = true;
    drop(session);
    tokio::time::timeout(Duration::from_secs(5), request_dropped)
        .await
        .unwrap()
        .unwrap();
    server.await.unwrap();
}

fn connection_fixture(published: Option<&str>) -> ConnectionVersions {
    ConnectionVersions::new(
        crate::wire::versions::Notice {
            server_version: "99.0.0-dev".into(),
            published_client: published.map(|version| crate::wire::versions::PublishedClient {
                version: version.into(),
                targets: vec!["x86_64-unknown-linux-gnu".into()],
            }),
        },
        "https://relay.example",
        "1.2.3",
        Some("x86_64-unknown-linux-gnu"),
    )
    .unwrap()
}

fn preview(width: u16) -> State {
    let mut state = super::super::preview_state(
        30,
        width,
        &Ui {
            layout: Layout::Chat,
            sidebar: false,
            ..Ui::default()
        },
    );
    state.conversation.composer.set_text("");
    state.conversation.content_focus = false;
    state.events.latest();
    state
}

fn rendered(state: &mut State) -> (super::super::Frame, String) {
    let frame = state.frame(std::time::Instant::now());
    let mut parser = vt100::Parser::new(
        super::super::nonzero(frame.geometry.rows),
        super::super::nonzero(frame.geometry.cols),
        0,
    );
    parser.process(&frame.diff(None));
    let text = parser.screen().contents();
    (frame, text)
}

#[test]
fn update_notice_history_bounds_only_presented_notices() {
    let mut seen = Seen::default();
    let old = Notice::Connection(connection_fixture(None));
    assert!(!seen.contains(&old));
    seen.record(old.clone());
    seen.record(old.clone());
    assert_eq!(seen.0.len(), 1);
    let newer = Notice::Connection(connection_fixture(Some("1.2.4")));
    assert!(!seen.contains(&newer));
    seen.record(newer);
    for patch in 5..40 {
        seen.record(Notice::Release(format!("v1.2.{patch}")));
    }
    assert_eq!(seen.0.len(), 16);
    assert!(seen.contains(&Notice::Release("v1.2.39".into())));
    assert!(!seen.contains(&old));
}

#[test]
fn published_update_wins_over_connection_notice_in_either_arrival_order() {
    for width in [120, 48] {
        for published in [None, Some("1.2.4")] {
            for release_first in [true, false] {
                let mut state = preview(width);
                let release = Notice::Release("ssmcp update --check · v1.2.5 available".into());
                let connection = Notice::Connection(connection_fixture(published));
                let (first, second) = if release_first {
                    (release.clone(), connection)
                } else {
                    (connection, release.clone())
                };
                assert!(offer(&mut state, first, |_| false));
                offer(&mut state, second, |_| false);
                assert_eq!(state.update_notice.as_ref(), Some(&release));
                let (_, screen) = rendered(&mut state);
                assert!(screen.contains("ssmcp update --check"), "{width}: {screen}");
                assert!(screen.contains("v1.2.5"), "{width}: {screen}");
                assert_eq!(state.rendered_update_notice.as_ref(), Some(&release));
                assert!(state.actions.is_empty());
            }
        }
    }
}

#[test]
fn hidden_update_survives_a_new_session_and_waits_for_complete_terminal_output() {
    use std::io::{self, Write};

    struct BlockedOutput {
        bytes: Vec<u8>,
        remaining: usize,
    }
    impl Write for BlockedOutput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let count = bytes.len().min(self.remaining);
            self.bytes.extend_from_slice(&bytes[..count]);
            self.remaining -= count;
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    for width in [120, 48] {
        let release = Notice::Release(format!("ssmcp update --check · v333.2.{width} available"));
        let mut old = preview(width);
        old.conversation.composer.set_text("keep the unsent draft");
        assert!(offer(&mut old, release.clone(), was_shown));
        let (_, screen) = rendered(&mut old);
        assert!(!screen.contains("ssmcp update --check"));
        assert!(old.rendered_update_notice.is_none());
        assert!(!was_shown(&release));
        assert_eq!(old.conversation.composer.text(), "keep the unsent draft");
        drop(old);

        let mut next = preview(width);
        assert!(offer(&mut next, release.clone(), was_shown));
        let (frame, screen) = rendered(&mut next);
        assert!(screen.contains("ssmcp update --check"), "{width}: {screen}");
        let bytes = frame.diff(None);
        let mut pending = next.rendered_update_notice.take();
        assert_eq!(pending.as_ref(), Some(&release));
        assert!(!was_shown(&release), "building a Frame is not delivery");
        let mut output = BlockedOutput {
            bytes: Vec::new(),
            remaining: 7,
        };
        let mut offset = 0;
        super::super::drain(&mut output, &bytes, &mut offset).unwrap();
        assert!(offset > 0 && offset < bytes.len());
        complete(&mut pending, offset >= bytes.len());
        assert_eq!(pending.as_ref(), Some(&release));
        assert!(
            !was_shown(&release),
            "partial terminal writes must remain retryable"
        );

        output.remaining = usize::MAX;
        super::super::drain(&mut output, &bytes, &mut offset).unwrap();
        assert_eq!(output.bytes, bytes);
        complete(&mut pending, offset >= bytes.len());
        assert!(pending.is_none());
        assert!(was_shown(&release));
        let mut reconnected = preview(width);
        assert!(!offer(&mut reconnected, release, was_shown));
        assert!(reconnected.update_notice.is_none());
        assert!(reconnected.actions.is_empty());
    }
}
