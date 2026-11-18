use super::*;
use crate::commerce::{amount, pancake::WebhookEvent};
use std::sync::{Arc, Barrier, Mutex};

struct Fixture {
    path: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce).unwrap();
        let name = nonce.iter().map(|n| format!("{n:02x}")).collect::<String>();
        let path = std::env::temp_dir().join(format!("ssmcp-commerce-{name}"));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self { path }
    }
    fn store(&self) -> OrderStore {
        OrderStore::open(&self.path.join("ledger")).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
fn config() -> CommerceConfig {
    CommerceConfig {
        enabled: true,
        monthly_sellable_gb: 3,
        max_gb_per_order: 3,
        store_id: "STO_test".into(),
        ..CommerceConfig::default()
    }
}
fn nonce(n: u64) -> String {
    format!("{n:032x}")
}
fn event(order: &Order) -> WebhookEvent {
    serde_json::from_value(serde_json::json!({
        "id":format!("event-{}",order.id),"eventType":"order.completed",
        "eventId":format!("payment-{}",order.subject),"storeId":"STO_test","mode":"test",
        "data":{
            "orderId":format!("ORD_{}",order.subject),"orderStatus":"completed",
            "orderMerchantExternalId":order.id,
            "merchantProvidedBuyerIdentity":format!("github:{}",order.subject),
            "currency":order.currency,"chargedAmount":amount(order.base_minor,order.currency_decimals),
            "listPrice":{"total":amount(order.base_minor,order.currency_decimals),
                "subtotal":amount(order.base_minor,order.currency_decimals),"taxAmount":"0",
                "listTotal":amount(order.base_minor,order.currency_decimals),"discountAmount":"0"},
            "paymentId":format!("PAY_{}",order.subject),"paymentStatus":"succeeded"
        }
    })).unwrap()
}
#[test]
fn concurrent_buyers_cannot_oversell_and_restart_preserves_reservations() {
    let fixture = Fixture::new();
    let store = Arc::new(Mutex::new(fixture.store()));
    let barrier = Arc::new(Barrier::new(16));
    let handles: Vec<_> = (1..=16)
        .map(|number| {
            let store = store.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store
                    .lock()
                    .unwrap()
                    .reserve(&config(), &number.to_string(), &nonce(number), 1, 100)
                    .is_ok()
            })
        })
        .collect();
    let success = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(|ok| *ok)
        .count();
    assert_eq!(success, 3);
    assert_eq!(store.lock().unwrap().available_gb("1970-01", 3).unwrap(), 0);
    drop(store);
    let store = fixture.store();
    assert_eq!(store.available_gb("1970-01", 3).unwrap(), 0);
    assert_eq!(store.available_gb("1970-02", 3).unwrap(), 3);
}
#[test]
fn duplicate_form_is_idempotent_and_subject_is_server_bound() {
    let fixture = Fixture::new();
    let mut store = fixture.store();
    let c = config();
    let first = store.reserve(&c, "123", &nonce(1), 1, 100).unwrap();
    assert_eq!(
        store.reserve(&c, "123", &nonce(1), 1, 110).unwrap().id,
        first.id
    );
    assert_eq!(store.available_gb("1970-01", 3).unwrap(), 2);
    assert!(matches!(
        store.reserve(&c, "123", &nonce(1), 2, 110),
        Err(Error::Conflict)
    ));
    assert!(matches!(
        store.reserve(&c, "123", &nonce(2), 1, 110),
        Err(Error::PendingOrder)
    ));
    assert!(matches!(
        store.owned("999", &first.id),
        Err(Error::NotFound)
    ));
    assert!(matches!(
        store.reserve(&c, "00123", &nonce(3), 1, 110),
        Err(Error::Invalid)
    ));
}
#[test]
fn payment_must_match_all_financial_and_identity_fields() {
    let fixture = Fixture::new();
    let mut store = fixture.store();
    let c = config();
    let order = store.reserve(&c, "123", &nonce(1), 1, 100).unwrap();
    let valid = event(&order);
    let mut invalid = Vec::new();
    let mut e = valid.clone();
    e.data.currency = "USD".into();
    invalid.push(e);
    let mut e = valid.clone();
    e.data.charged_amount = Some("0.99".into());
    invalid.push(e);
    let mut e = valid.clone();
    e.data.charged_amount = None;
    invalid.push(e);
    let mut e = valid.clone();
    e.data.list_price.as_mut().unwrap().total = "2.00".into();
    invalid.push(e);
    let mut e = valid.clone();
    e.data.merchant_provided_buyer_identity = Some("github:999".into());
    invalid.push(e);
    let mut e = valid.clone();
    e.data.payment_status = Some("pending".into());
    invalid.push(e);
    let mut e = valid.clone();
    e.data.order_status = Some("canceled".into());
    invalid.push(e);
    let mut e = valid.clone();
    e.data.payment_id = None;
    invalid.push(e);
    let mut e = valid.clone();
    e.store_id = "STO_other".into();
    invalid.push(e);
    let mut e = valid.clone();
    e.mode = "prod".into();
    invalid.push(e);
    for e in invalid {
        assert!(matches!(
            store.paid(&c, &e, 200),
            Err(Error::PaymentMismatch)
        ));
    }
    assert_eq!(
        store.owned("123", &order.id).unwrap().state,
        OrderState::Reserved
    );
    assert!(matches!(
        store.paid(&c, &valid, 200),
        Ok(Settlement::Grant(_))
    ));
}
#[test]
fn paid_intent_survives_crash_and_grant_ack_is_idempotent() {
    let fixture = Fixture::new();
    let mut store = fixture.store();
    let c = config();
    let order = store.reserve(&c, "123", &nonce(1), 1, 100).unwrap();
    let valid = event(&order);
    assert!(matches!(
        store.paid(&c, &valid, 200),
        Ok(Settlement::Grant(_))
    ));
    drop(store);
    let mut store = fixture.store();
    assert_eq!(store.pending_grants().unwrap().len(), 1);
    assert!(matches!(
        store.paid(&c, &valid, 210),
        Ok(Settlement::Grant(_))
    ));
    store.mark_granted(&order.id).unwrap();
    assert!(matches!(
        store.paid(&c, &valid, 220),
        Ok(Settlement::AlreadyGranted)
    ));
    assert!(store.pending_grants().unwrap().is_empty());
    let mut conflicting = valid.clone();
    conflicting.data.payment_id = Some("PAY_different".into());
    assert!(matches!(
        store.paid(&c, &conflicting, 220),
        Err(Error::Conflict)
    ));
    assert_eq!(store.available_gb("1970-01", 3).unwrap(), 2);
}
#[test]
fn month_rollover_does_not_move_an_old_payment_into_new_inventory() {
    let fixture = Fixture::new();
    let mut store = fixture.store();
    let c = config();
    let order = store.reserve(&c, "123", &nonce(1), 3, 100).unwrap();
    assert!(matches!(
        store.paid(&c, &event(&order), order.month_end),
        Ok(Settlement::Review)
    ));
    assert_eq!(
        store.owned("123", &order.id).unwrap().state,
        OrderState::PaidExpired
    );
    assert_eq!(store.available_gb("1970-02", 3).unwrap(), 3);
    let fresh = store
        .reserve(&c, "123", &nonce(2), 1, order.month_end + 100)
        .unwrap();
    assert_eq!(fresh.period, "1970-02");
    assert!(matches!(
        store.reserve(&c, "999", &nonce(3), 1, fresh.month_end - 1),
        Err(Error::Expired)
    ));
}
#[test]
fn timeout_does_not_release_potentially_payable_inventory() {
    let fixture = Fixture::new();
    let mut store = fixture.store();
    let c = config();
    let order = store.reserve(&c, "123", &nonce(1), 3, 100).unwrap();
    assert!(matches!(
        store.reserve(&c, "999", &nonce(2), 1, order.checkout_deadline + 100),
        Err(Error::SoldOut)
    ));
    assert!(matches!(
        store.paid(&c, &event(&order), order.checkout_deadline + 100),
        Ok(Settlement::Grant(_))
    ));
}
#[test]
fn single_writer_and_storage_failure_are_fail_closed() {
    let fixture = Fixture::new();
    let mut store = fixture.store();
    assert!(OrderStore::open(&fixture.path.join("ledger")).is_err());
    fs::create_dir(fixture.path.join("ledger/orders.json")).unwrap();
    assert!(matches!(
        store.reserve(&config(), "123", &nonce(1), 1, 100),
        Err(Error::Storage)
    ));
    fs::remove_dir(fixture.path.join("ledger/orders.json")).unwrap();
    assert!(matches!(
        store.reserve(&config(), "999", &nonce(2), 1, 100),
        Err(Error::Storage)
    ));
}

#[test]
fn added_tax_requires_exact_base_tax_due_and_actual_charge() {
    let fixture = Fixture::new();
    let mut store = fixture.store();
    let c = config();
    let order = store.reserve(&c, "123", &nonce(1), 1, 100).unwrap();
    let mut paid = event(&order);
    paid.data.charged_amount = Some("1.10".into());
    let price = paid.data.list_price.as_mut().unwrap();
    price.total = "1.10".into();
    price.tax_amount = "0.10".into();
    price.list_total = Some("1.10".into());
    let mut invalid = Vec::new();
    let mut e = paid.clone();
    e.data.list_price.as_mut().unwrap().subtotal = Some("0.90".into());
    invalid.push(e);
    let mut e = paid.clone();
    e.data.list_price.as_mut().unwrap().tax_amount = "0.20".into();
    invalid.push(e);
    let mut e = paid.clone();
    e.data.charged_amount = Some("1.20".into());
    invalid.push(e);
    let mut e = paid.clone();
    e.data.list_price.as_mut().unwrap().discount_amount = Some("0.10".into());
    invalid.push(e);
    let mut e = paid.clone();
    e.data.list_price.as_mut().unwrap().subtotal = None;
    invalid.push(e);
    for e in invalid {
        assert!(matches!(
            store.paid(&c, &e, 200),
            Err(Error::PaymentMismatch)
        ));
    }
    assert!(matches!(
        store.paid(&c, &paid, 200),
        Ok(Settlement::Grant(_))
    ));
    let receipt = store.owned("123", &order.id).unwrap().receipt.unwrap();
    assert_eq!(
        receipt,
        PaymentReceipt {
            tax_minor: 10,
            charged_minor: 110
        }
    );
    store.mark_granted(&order.id).unwrap();
    assert!(matches!(
        store.paid(&c, &paid, 210),
        Ok(Settlement::AlreadyGranted)
    ));
    let price = paid.data.list_price.as_mut().unwrap();
    price.total = "1.20".into();
    price.tax_amount = "0.20".into();
    price.list_total = Some("1.20".into());
    paid.data.charged_amount = Some("1.20".into());
    assert!(matches!(store.paid(&c, &paid, 210), Err(Error::Conflict)));
}
#[test]
fn provider_price_bounds_reject_before_inventory_reservation() {
    let fixture = Fixture::new();
    let mut store = fixture.store();
    let mut c = config();
    c.monthly_sellable_gb = 2000;
    c.max_gb_per_order = 2000;
    assert!(matches!(
        store.reserve(&c, "123", &nonce(1), 1001, 100),
        Err(Error::PriceOutOfRange)
    ));
    assert_eq!(store.available_gb("1970-01", 2000).unwrap(), 2000);
    c.price_per_gb_minor = Some(50);
    assert_eq!(c.quantity_range(1), None);
    assert_eq!(c.quantity_range(3), Some((2, 3)));
    assert!(matches!(
        store.reserve(&c, "123", &nonce(1), 1, 100),
        Err(Error::PriceOutOfRange)
    ));
    assert!(store.reserve(&c, "123", &nonce(1), 2, 100).is_ok());
    assert_eq!(store.available_gb("1970-01", 2000).unwrap(), 1998);
}
#[test]
fn cancellation_releases_once_and_cannot_reverse_a_paid_order() {
    fn proof(order: &Order) -> super::super::pancake::CancellationProof {
        super::super::pancake::CancellationProof {
            local_id: order.id.clone(),
            remote_id: format!("ORD_{}", order.subject),
            session_id: "SESSION_test".into(),
            checked_at: order.checkout_deadline + 60,
        }
    }
    let fixture = Fixture::new();
    let mut store = fixture.store();
    let c = config();
    let order = store.reserve(&c, "123", &nonce(1), 1, 100).unwrap();
    store
        .bind_checkout(&order.id, "SESSION_test", order.checkout_deadline)
        .unwrap();
    store.release_cancelled(proof(&order)).unwrap();
    store.release_cancelled(proof(&order)).unwrap();
    assert_eq!(store.available_gb("1970-01", 3).unwrap(), 3);
    assert!(matches!(
        store.paid(&c, &event(&order), 1100),
        Err(Error::Conflict)
    ));
    assert!(store.reserve(&c, "123", &nonce(2), 1, 1100).is_ok());
    let paid = store.reserve(&c, "456", &nonce(3), 1, 100).unwrap();
    store
        .bind_checkout(&paid.id, "SESSION_test", paid.checkout_deadline)
        .unwrap();
    assert!(matches!(
        store.paid(&c, &event(&paid), 200),
        Ok(Settlement::Grant(_))
    ));
    assert!(matches!(
        store.release_cancelled(proof(&paid)),
        Err(Error::Conflict)
    ));
    assert_eq!(store.available_gb("1970-01", 3).unwrap(), 1);
}
#[test]
fn ledger_cannot_switch_environment_or_store() {
    let fixture = Fixture::new();
    let mut store = fixture.store();
    let c = config();
    store.configure_scope(&c).unwrap();
    drop(store);
    let mut store = fixture.store();
    store.configure_scope(&c).unwrap();
    let mut other = c.clone();
    other.environment = "prod".into();
    assert_eq!(store.configure_scope(&other), Err(Error::Configuration));
    other = c.clone();
    other.store_id = "STO_other".into();
    assert_eq!(store.configure_scope(&other), Err(Error::Configuration));
}
