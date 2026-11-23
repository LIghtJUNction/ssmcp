use super::*;
use serde_json::{Value, json};
use std::{cell::Cell, future::pending};
use zbridge::{Effect, Event};

fn id(value: &str) -> Id {
    Id::new(value).unwrap()
}
fn observer(cursor: Option<u64>) -> EventObserver {
    EventObserver::new(id("fixture-runtime-a"), cursor.map(Counter))
}
fn empty(cursor: u64) -> EventPage {
    EventPage {
        events: vec![],
        start_cursor: Counter(cursor),
        next_cursor: Counter(cursor),
        live_cursor: Counter(cursor),
        retention_gap: false,
        has_more: false,
    }
}
fn event(cursor: u64) -> EventPage {
    EventPage {
        events: vec![Event {
            sequence: Counter(cursor),
            topic: id("worker.progress"),
            value: json!({"task_id":"fixture-task","progress":"running"}),
        }],
        start_cursor: Counter(cursor),
        next_cursor: Counter(cursor + 1),
        live_cursor: Counter(cursor + 1),
        retention_gap: false,
        has_more: false,
    }
}
fn response(request: &Request, page: EventPage) -> Response {
    Response {
        version: zbridge::VERSION,
        id: request.id.clone(),
        generation: request.operation.generation().cloned(),
        body: ResponseBody::Events(page),
    }
}
fn request_cursor(request: &Request) -> Option<Counter> {
    let Operation::Events {
        cursor, wait_ms, ..
    } = &request.operation
    else {
        panic!("observer sent a non-read operation")
    };
    assert!(*wait_ms <= zbridge::MAX_WAIT_MS);
    *cursor
}

#[tokio::test]
async fn segmented_wait_captures_one_live_boundary_and_uses_distinct_request_ids() {
    let mut observer = observer(None);
    let calls = Cell::new(0);
    let page = observer
        .wait_with(id("observation"), 16, 1200, |request| {
            let index = calls.get();
            calls.set(index + 1);
            assert_eq!(request.id.as_str(), format!("observation.{index}"));
            assert_eq!(
                request.operation.generation(),
                Some(&id("fixture-runtime-a"))
            );
            assert_eq!(
                request_cursor(&request),
                if index == 0 { None } else { Some(Counter(41)) }
            );
            let page = if index == 0 { empty(41) } else { event(41) };
            async move { Ok(response(&request, page)) }
        })
        .await
        .unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(calls.get(), 2);
    assert_eq!(observer.cursor(), Some(Counter(41)));
    assert_eq!(observer.advance(), Some(Counter(42)));
}

#[tokio::test]
async fn unconsumed_events_block_the_next_read_and_retain_lossless_cursors() {
    let large = 9_007_199_254_740_993;
    let mut observer = observer(Some(large));
    observer
        .wait_with(id("first"), 16, 0, |request| async move {
            Ok(response(&request, event(large)))
        })
        .await
        .unwrap();
    assert_eq!(observer.cursor(), Some(Counter(large)));
    let error = observer
        .wait_with(id("blocked"), 16, 0, |_| async {
            panic!("unconsumed page caused another transport read")
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Conflict);
    assert_eq!(
        observer.pending_page().unwrap().events[0].sequence,
        Counter(large)
    );
    observer.advance();
    observer
        .wait_with(id("next"), 16, 0, |request| async move {
            assert_eq!(request_cursor(&request), Some(Counter(large + 1)));
            Ok(response(&request, empty(large + 1)))
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn live_reads_do_not_mistake_the_retained_floor_for_their_resume_cursor() {
    let mut observer = observer(None);
    observer
        .wait_with(id("live"), 16, 0, |request| async move {
            let mut page = event(100);
            page.start_cursor = Counter(7);
            Ok(response(&request, page))
        })
        .await
        .unwrap();
    assert_eq!(observer.cursor(), Some(Counter(100)));
    assert_eq!(observer.pending_page().unwrap().start_cursor, Counter(7));
    assert_eq!(observer.advance(), Some(Counter(101)));
}

#[tokio::test]
async fn retention_gap_requires_explicit_reconciliation_before_advancing() {
    let mut observer = observer(Some(4));
    observer
        .wait_with(id("gap"), 16, 0, |request| async move {
            let mut page = empty(9);
            page.retention_gap = true;
            Ok(response(&request, page))
        })
        .await
        .unwrap();
    assert_eq!(observer.cursor(), Some(Counter(4)));
    assert!(observer.pending_page().unwrap().retention_gap);
    assert_eq!(
        observer
            .wait_with(id("blocked"), 16, 0, |_| async {
                panic!("gap was silently skipped")
            })
            .await
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    assert_eq!(observer.advance(), Some(Counter(9)));
}

#[tokio::test]
async fn cancelled_wait_retains_the_first_confirmed_live_cursor() {
    let mut observer = observer(None);
    let called = Cell::new(false);
    let mut waiting = Box::pin(observer.wait_with(id("cancelled"), 16, 55_000, |request| {
        called.set(true);
        async move { Ok(response(&request, empty(77))) }
    }));
    tokio::select! {
        biased;
        result = &mut waiting => panic!("empty observation ended early: {result:?}"),
        _ = tokio::task::yield_now() => {}
    }
    assert!(called.get());
    drop(waiting);
    assert_eq!(observer.cursor(), Some(Counter(77)));
    assert!(observer.pending_page().is_none());
    observer
        .wait_with(id("resumed"), 16, 0, |request| async move {
            assert_eq!(request_cursor(&request), Some(Counter(77)));
            Ok(response(&request, event(77)))
        })
        .await
        .unwrap();
    assert_eq!(
        observer.pending_page().unwrap().events[0].sequence,
        Counter(77)
    );
}

#[tokio::test]
async fn a_silent_transport_expires_without_inventing_a_page_or_advancing() {
    let mut observer = observer(Some(5));
    let error = observer
        .wait_with(id("timeout"), 16, 5, |_| pending())
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Unavailable);
    assert_eq!(error.effect, Effect::None);
    assert_eq!(observer.cursor(), Some(Counter(5)));
    assert!(observer.pending_page().is_none());
}

#[tokio::test]
async fn early_empty_replies_are_paced_and_end_as_an_empty_observation() {
    let mut observer = observer(None);
    let calls = Cell::new(0);
    let started = Instant::now();
    let page = observer
        .wait_with(id("empty"), 16, 15, |request| {
            calls.set(calls.get() + 1);
            async move { Ok(response(&request, empty(7))) }
        })
        .await
        .unwrap();
    assert!(page.events.is_empty());
    assert!(started.elapsed() >= Duration::from_millis(15));
    assert!(calls.get() <= 2);
    assert_eq!(observer.cursor(), Some(Counter(7)));
}

#[tokio::test]
async fn denied_mismatched_or_replaced_backends_do_not_rebind_the_cursor() {
    for kind in ["denied", "request", "generation"] {
        let mut observer = observer(Some(8));
        let error = observer
            .wait_with(id("original"), 16, 0, |request| async move {
                let mut reply = response(&request, event(8));
                match kind {
                    "denied" => {
                        reply.body = ResponseBody::Error(BridgeError::new(
                            ErrorCode::Denied,
                            "lease expired",
                        ))
                    }
                    "request" => reply.id = id("unrelated"),
                    _ => reply.generation = Some(id("replacement-runtime")),
                }
                Ok(reply)
            })
            .await
            .unwrap_err();
        if kind == "denied" {
            assert_eq!(error.code, ErrorCode::Denied);
        }
        if kind == "generation" {
            assert_eq!(error.code, ErrorCode::StaleGeneration);
        }
        assert_eq!(observer.generation(), &id("fixture-runtime-a"));
        assert_eq!(observer.cursor(), Some(Counter(8)));
        assert!(observer.pending_page().is_none());
    }
}

#[tokio::test]
async fn request_bounds_are_rejected_before_any_transport_call() {
    for (request, limit, wait) in [
        (id("bound"), 0, 0),
        (id("bound"), 129, 0),
        (id("bound"), 16, 55_001),
        (id(&"x".repeat(125)), 16, 0),
    ] {
        let mut observer = observer(None);
        assert!(
            observer
                .wait_with(request, limit, wait, |_| async {
                    panic!("invalid observation reached transport")
                })
                .await
                .is_err()
        );
    }
}

#[test]
fn language_neutral_fixtures_pass_the_actual_zbridge_codec_and_association_checks() {
    let fixture: Value = serde_json::from_str(include_str!("contract-v1.json")).unwrap();
    assert_eq!(fixture["schema"], "ssmcp-client-contract-v1");
    for case in fixture["cases"].as_array().unwrap() {
        let request =
            zbridge::decode_request(&serde_json::to_vec(&case["request"]).unwrap()).unwrap();
        let response =
            zbridge::decode_response(&serde_json::to_vec(&case["response"]).unwrap()).unwrap();
        response.validate_for(&request).unwrap();
        if case["name"] == "command_unknown" {
            let ResponseBody::Error(error) = &response.body else {
                panic!("unknown fixture lost its error")
            };
            assert_eq!(error.effect, Effect::MayHaveOccurred);
        }
        if case["name"] == "large_cursor" {
            let ResponseBody::Events(page) = &response.body else {
                panic!("cursor fixture is not an event page")
            };
            assert_eq!(page.next_cursor, Counter(9_007_199_254_740_994));
        }
    }
}
