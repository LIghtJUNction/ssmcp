use super::*;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};
use tokio::io::{AsyncWriteExt, duplex};

fn broken_pipe() -> crate::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "connected write failed first").into()
}

fn queued_call(id: u64) -> Frame {
    Frame::Call {
        id,
        operation: crate::wire::Operation::ListTools { cursor: None },
        controller: None,
        attribution: None,
        code_context: None,
        openai_context: None,
    }
}

fn assert_original_error(error: &crate::Error) {
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::BrokenPipe
    );
    assert_eq!(error.to_string(), "connected write failed first");
}

#[tokio::test]
async fn late_rejection_preserves_the_raw_permanent_authorization_reason() {
    // LEGACY_ACCOUNT_ALLOW is deliberately not the AUTH_EXPIRED watch special
    // case: classify the raw Reject rather than its display-prefixed watch text.
    for message in [relay::AUTH_EXPIRED, relay::LEGACY_ACCOUNT_ALLOW] {
        let (mut relay, reader) = duplex(1024);
        let mut incoming = Incoming::start(BufReader::new(reader), TransportKind::Relay);
        let error = {
            let outcome = incoming.after_connection_error(broken_pipe());
            tokio::pin!(outcome);
            assert!(
                outcome
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            write_frame(
                &mut relay,
                &Frame::Reject {
                    message: message.into(),
                },
            )
            .await
            .unwrap();
            outcome.await
        };
        assert!(error.downcast_ref::<relay::PermanentFailure>().is_some());
        assert_eq!(error.to_string(), message);
        incoming.stop().await;
    }
}

#[tokio::test]
async fn full_frame_queue_cannot_hide_a_final_rejection_after_a_write_failure() {
    // The one-byte pipe makes the reader consume each frame before its writer
    // can advance. Reading frame N+1 requires N to have reached the frame queue.
    let (mut relay, reader) = duplex(1);
    let mut incoming = Incoming::start(BufReader::new(reader), TransportKind::Relay);
    for id in 0..=MAX_INFLIGHT as u64 {
        write_frame(&mut relay, &queued_call(id)).await.unwrap();
    }
    assert_eq!(incoming.frames.len(), MAX_INFLIGHT);
    assert!(incoming.closed.borrow().is_none());
    let final_frame = tokio::spawn(async move {
        write_frame(
            &mut relay,
            &Frame::Reject {
                message: relay::AUTH_EXPIRED.into(),
            },
        )
        .await
        .unwrap();
    });
    let error = incoming.after_connection_error(broken_pipe()).await;
    assert!(error.downcast_ref::<relay::PermanentFailure>().is_some());
    assert_eq!(error.to_string(), relay::AUTH_EXPIRED);
    final_frame.await.unwrap();
    incoming.stop().await;
}

#[tokio::test]
async fn eof_reader_error_and_retryable_rejection_keep_the_original_error() {
    for scenario in ["eof", "invalid_frame", "retryable_rejection"] {
        let (mut relay, reader) = duplex(1024);
        let mut incoming = Incoming::start(BufReader::new(reader), TransportKind::Relay);
        match scenario {
            "invalid_frame" => relay.write_all(b"not-json\n").await.unwrap(),
            "retryable_rejection" => {
                write_frame(
                    &mut relay,
                    &Frame::Reject {
                        message: "transient bridge failure".into(),
                    },
                )
                .await
                .unwrap();
            }
            _ => {}
        }
        drop(relay);
        assert_original_error(&incoming.after_connection_error(broken_pipe()).await);
        incoming.stop().await;
    }
}

#[tokio::test]
async fn an_existing_permanent_failure_needs_no_further_frame_or_deadline() {
    let (_relay, reader) = duplex(1024);
    let mut incoming = Incoming::start(BufReader::new(reader), TransportKind::Relay);
    let error = {
        let outcome = incoming.after_connection_error(relay::failure(relay::AUTH_EXPIRED.into()));
        tokio::pin!(outcome);
        match outcome
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(error) => error,
            Poll::Pending => panic!("a known permanent failure must not wait for another frame"),
        }
    };
    assert!(error.downcast_ref::<relay::PermanentFailure>().is_some());
    assert_eq!(error.to_string(), relay::AUTH_EXPIRED);
    incoming.stop().await;
}

#[tokio::test]
async fn one_deadline_bounds_an_unending_stream_of_nonterminal_frames() {
    let (mut relay, reader) = duplex(1024);
    let mut incoming = Incoming::start(BufReader::new(reader), TransportKind::Relay);
    let sent = Arc::new(AtomicUsize::new(0));
    let observed = sent.clone();
    let producer = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        loop {
            tick.tick().await;
            let id = observed.load(Ordering::SeqCst) as u64;
            if write_frame(&mut relay, &queued_call(id)).await.is_err() {
                break;
            }
            observed.fetch_add(1, Ordering::SeqCst);
        }
    });
    let outcome = timeout(
        Duration::from_secs(4),
        incoming.after_connection_error(broken_pipe()),
    )
    .await;
    producer.abort();
    let _ = producer.await;
    incoming.stop().await;
    assert!(sent.load(Ordering::SeqCst) > 1);
    assert_original_error(&outcome.expect("ordinary frames must not restart the total deadline"));
}

#[tokio::test]
async fn transport_diagnostics_keep_the_rejection_frame_and_original_io_detail() {
    for (kind, label) in [
        (TransportKind::Relay, "Relay transport"),
        (TransportKind::Local, "Local transport"),
    ] {
        let (mut relay, reader) = duplex(1024);
        let mut incoming = Incoming::start(BufReader::new(reader), kind);
        let reason = "upstream rejected the current registration";
        write_frame(
            &mut relay,
            &Frame::Reject {
                message: reason.into(),
            },
        )
        .await
        .unwrap();
        let message = timeout(Duration::from_secs(5), incoming.disconnected())
            .await
            .unwrap();
        assert_eq!(
            message,
            format!("{label} server rejected the connection: {reason}")
        );
        assert!(!message.contains("SSH"));
        let frame = incoming.frames.recv().await.unwrap().unwrap().unwrap();
        assert!(matches!(frame, Frame::Reject { message } if message == reason));
        incoming.stop().await;
        let reason = "underlying wait failure";
        let message = kind.ended(Err(io::Error::new(io::ErrorKind::BrokenPipe, reason)));
        assert!(message.contains(label));
        assert!(message.contains(reason));
        assert!(!message.contains("SSH"));
        let timeout = kind.result_write_timeout();
        assert_eq!(timeout.kind(), io::ErrorKind::TimedOut);
        assert!(timeout.to_string().contains(label));
        assert!(timeout.to_string().contains("operation may have executed"));
        assert!(
            timeout
                .to_string()
                .contains("before retrying a mutating call")
        );
    }
}
