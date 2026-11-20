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
fn startup_update_notice_stays_with_its_live_session_and_claims_only_once() {
    let old = shared();
    let next = shared();
    let claimed = AtomicBool::new(false);
    let claim = || !claimed.swap(true, Ordering::SeqCst);
    old.stop.store(true, Ordering::Release);
    assert!(!publish(&Arc::downgrade(&old), "old".into(), claim));
    old.stop.store(false, Ordering::Release);
    old.alive.store(false, Ordering::Release);
    assert!(!publish(&Arc::downgrade(&old), "old".into(), claim));
    old.alive.store(true, Ordering::Release);
    old.state.lock().unwrap().preview = true;
    assert!(!publish(&Arc::downgrade(&old), "old".into(), claim));
    let expired = Arc::downgrade(&old);
    drop(old);
    assert!(!publish(&expired, "old".into(), claim));
    assert!(!claimed.load(Ordering::SeqCst));
    next.state.lock().unwrap().notice = Some("Keep the recovery message".into());
    assert!(publish(&Arc::downgrade(&next), "current".into(), claim));
    assert!(!publish(&Arc::downgrade(&next), "duplicate".into(), claim));
    let state = next.state.lock().unwrap();
    assert_eq!(state.update_notice.as_deref(), Some("current"));
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
        let client = reqwest::Client::builder()
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
        account_probe: None,
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
