use super::*;
use std::sync::Arc;

fn source(run: char) -> Source {
    Source {
        principal: Principal::Account {
            account: ControlAccount {
                provider: "github".into(),
                subject: "42".into(),
                login: "fixture".into(),
            },
        },
        run_id: Some(run.to_string().repeat(32)),
        participant: None,
        controller: None,
    }
}
fn key(router: &Router) -> String {
    router.key().unwrap()["idempotency_key"]
        .as_str()
        .unwrap()
        .into()
}

#[test]
fn keys_expire_across_clock_window_and_relay_generation_without_replay() {
    let router = Router::default();
    let other = Router::default();
    let current = key(&router);
    assert!(
        other
            .reserve("owner".into(), current.clone(), [1; 32], source('a'))
            .is_err()
    );
    let mut parts = current.split('.').map(str::to_string).collect::<Vec<_>>();
    parts[1] = messaging::now_ms()
        .saturating_sub(cache::KEY_WINDOW_MS + 1000)
        .to_string();
    assert!(
        router
            .reserve("owner".into(), parts.join("."), [1; 32], source('a'))
            .unwrap_err_string()
            .contains("expired")
    );
    assert!(
        router
            .reserve("owner".into(), "arbitrary-key".into(), [1; 32], source('a'))
            .is_err()
    );
}

// Result::unwrap_err would require the successful receipt's internal synchronization fields to implement Debug.
trait ErrorText {
    fn unwrap_err_string(self) -> String;
}
impl<T> ErrorText for Result<T, String> {
    fn unwrap_err_string(self) -> String {
        match self {
            Ok(_) => panic!("expected rejection"),
            Err(e) => e,
        }
    }
}

#[test]
fn duplicate_batch_preserves_first_run_content_and_recipient_snapshot() {
    let router = Router::default();
    let key = key(&router);
    let (first, duplicate) = router
        .reserve("owner".into(), key.clone(), [1; 32], source('a'))
        .unwrap();
    assert!(!duplicate);
    first
        .state
        .lock()
        .unwrap()
        .recipients
        .push(Delivery::pending("a".repeat(64), None));
    Guard(first.clone()).finish();
    let (again, duplicate) = router
        .reserve("owner".into(), key.clone(), [1; 32], source('b'))
        .unwrap();
    assert!(duplicate);
    assert!(Arc::ptr_eq(&first, &again));
    assert_eq!(again.source.run_id, Some("a".repeat(32)));
    assert_eq!(
        again.snapshot(true)["recipients"].as_array().unwrap().len(),
        1
    );
    assert!(
        router
            .reserve("owner".into(), key, [2; 32], source('a'))
            .is_err()
    );
    assert!(router.get("other-owner", &first.message_id).is_err());
}

#[test]
fn cancellation_records_unknown_without_changing_confirmed_deliveries() {
    let router = Router::default();
    let (batch, _) = router
        .reserve("owner".into(), key(&router), [1; 32], source('a'))
        .unwrap();
    let guard = Guard(batch.clone());
    let mut accepted = Delivery::pending("a".repeat(64), None);
    accepted.state = "accepted".into();
    batch.state.lock().unwrap().recipients =
        vec![accepted, Delivery::pending("b".repeat(64), None)];
    drop(guard);
    let snapshot = batch.snapshot(false);
    assert_eq!(snapshot["in_progress"], false);
    assert_eq!(snapshot["recipients"][0]["state"], "accepted");
    assert_eq!(snapshot["recipients"][1]["state"], "unknown");
    assert_eq!(snapshot["replay"], false);
}

#[test]
fn valid_keys_are_not_evicted_to_admit_another_owner_batch() {
    let router = Router::default();
    let first = key(&router);
    for i in 0..64 {
        let key = if i == 0 { first.clone() } else { key(&router) };
        let (batch, _) = router
            .reserve("owner".into(), key, [1; 32], source('a'))
            .unwrap();
        Guard(batch).finish();
    }
    assert!(
        router
            .reserve("owner".into(), key(&router), [1; 32], source('a'))
            .is_err()
    );
    assert!(
        router
            .reserve("other".into(), key(&router), [1; 32], source('a'))
            .is_ok()
    );
    assert!(
        router
            .reserve("owner".into(), first, [1; 32], source('a'))
            .unwrap()
            .1
    );
}

#[test]
fn public_request_types_reject_forged_sources_and_private_delivery_operations() {
    let send = json!({"to":{"scope":"global"},"text":"candidate","idempotency_key":"key"});
    assert!(serde_json::from_value::<Send>(send.clone()).is_ok());
    for (name, value) in [
        (
            "source",
            json!({"principal":{"kind":"account","account":{"subject":"1"}}}),
        ),
        ("role", json!("user")),
        ("operation", json!({"op":"peer_messages"})),
    ] {
        let mut forged = send.clone();
        forged[name] = value;
        assert!(serde_json::from_value::<Send>(forged).is_err());
    }
    assert_eq!(tool()["name"], "messages");
    assert_eq!(tool()["inputSchema"]["oneOf"].as_array().unwrap().len(), 8);
}

#[test]
fn receipt_bytes_are_reserved_before_fanout_and_large_errors_are_bounded() {
    let router = Router::default();
    let mut full = false;
    for _ in 0..64 {
        let (batch, _) = router
            .reserve("owner".into(), key(&router), [1; 32], source('a'))
            .unwrap();
        if batch.reserve_recipients(messaging::MAX_BATCH).is_err() {
            Guard(batch).finish();
            full = true;
            break;
        }
        Guard(batch).finish();
    }
    assert!(
        full,
        "byte budget must reject before the per-owner batch-count limit"
    );
    let mut delivery = Delivery::pending("a".repeat(64), None);
    delivery.fail("unknown", "中".repeat(4096));
    assert!(delivery.detail.unwrap().len() <= 512);
}
