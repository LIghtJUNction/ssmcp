//! Read-only reconciliation using documented merchant GraphQL queries.
use super::{CancellationProof, Pancake, Price};
use crate::commerce::{
    Error, now,
    store::{Order, OrderState},
};
use serde_json::{Value, json};

impl Pancake {
    pub(crate) async fn cancelled_order(&self, order: &Order) -> Result<CancellationProof, Error> {
        eligible(order, now())?;
        // Two matches are enough to reject an ambiguous business reference. Never release
        // on an empty query result: an absent order does not prove no payment can arrive.
        let detail = self.post("/v1/graphql", json!({
            "query":"query ($store: String!, $ref: String!) { onetimeOrders(storeId: $store, filter: { orderMerchantExternalId: { eq: $ref } }, limit: 2) { id currency status testMode orderMerchantExternalId priceSnapshot { currency subtotal taxAmount total } onetimeProduct { id } } }",
            "variables":{"store":self.config.store_id,"ref":order.id}
        }), None).await?;
        let payments = self.post("/v1/graphql", json!({
            "query":"query ($ref: String!) { paymentsCount(filter: { orderMerchantExternalId: { eq: $ref } }) payments(filter: { orderMerchantExternalId: { eq: $ref } }) { id orderId status orderMerchantExternalId } }",
            "variables":{"ref":order.id}
        }), None).await?;
        verify_cancellation(&self.config, order, &detail, &payments, now())
    }
}
fn eligible(order: &Order, timestamp: u64) -> Result<(), Error> {
    if order.state != OrderState::Reserved
        || order.session_id.is_none()
        || order
            .session_expires_at
            .is_none_or(|end| timestamp < end.saturating_add(60))
    {
        return Err(Error::CancellationUnconfirmed);
    }
    Ok(())
}

fn verify_cancellation(
    config: &crate::commerce::CommerceConfig,
    order: &Order,
    detail: &Value,
    payments: &Value,
    timestamp: u64,
) -> Result<CancellationProof, Error> {
    eligible(order, timestamp)?;
    let orders = detail
        .get("onetimeOrders")
        .and_then(Value::as_array)
        .filter(|items| items.len() == 1)
        .ok_or(Error::CancellationUnconfirmed)?;
    let canonical = &orders[0];
    let remote_id = canonical
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| id.starts_with("ORD_") && id.len() <= 128)
        .ok_or(Error::CancellationUnconfirmed)?;
    let price: Price = serde_json::from_value(canonical["priceSnapshot"].clone())
        .map_err(|_| Error::CancellationUnconfirmed)?;
    let list = payments
        .get("payments")
        .and_then(Value::as_array)
        .ok_or(Error::CancellationUnconfirmed)?;
    // The count prevents an incomplete/default-paginated list from hiding a live payment.
    // Zero is allowed only alongside the canonical canceled order above.
    if list.len() > 1000
        || payments.get("paymentsCount").and_then(Value::as_u64) != Some(list.len() as u64)
        || canonical.get("status").and_then(Value::as_str) != Some("canceled")
        || canonical.get("testMode").and_then(Value::as_bool) != Some(config.environment == "test")
        || canonical.get("currency").and_then(Value::as_str) != Some(order.currency.as_str())
        || canonical
            .get("orderMerchantExternalId")
            .and_then(Value::as_str)
            != Some(order.id.as_str())
        || canonical["onetimeProduct"]["id"].as_str() != Some(config.product_id.as_str())
        || canonical["priceSnapshot"]["currency"].as_str() != Some(order.currency.as_str())
        || price
            .checked_total(order.base_minor, order.currency_decimals)
            .is_none()
        || list.iter().any(|payment| {
            payment["orderId"].as_str() != Some(remote_id)
                || payment["orderMerchantExternalId"].as_str() != Some(order.id.as_str())
                || payment["id"]
                    .as_str()
                    .is_none_or(|id| id.is_empty() || id.len() > 128)
                || !matches!(payment["status"].as_str(), Some("canceled" | "failed"))
        })
    {
        return Err(Error::CancellationUnconfirmed);
    }
    Ok(CancellationProof {
        local_id: order.id.clone(),
        remote_id: remote_id.into(),
        session_id: order
            .session_id
            .clone()
            .ok_or(Error::CancellationUnconfirmed)?,
        checked_at: timestamp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commerce::{CommerceConfig, GB};
    fn fixture() -> (CommerceConfig, Order, Value, Value) {
        let c = CommerceConfig {
            enabled: true,
            product_id: "PROD_test".into(),
            ..CommerceConfig::default()
        };
        let o = Order {
            id: "traffic_test".into(),
            subject: "123".into(),
            request_nonce: "0".repeat(32),
            period: "1970-01".into(),
            utc_offset_minutes: 480,
            quantity_gb: 1,
            bytes: GB,
            base_minor: 100,
            currency: "CNY".into(),
            currency_decimals: 2,
            created_at: 100,
            month_end: 2_649_600,
            checkout_deadline: 1000,
            state: OrderState::Reserved,
            session_id: Some("SESSION_test".into()),
            session_expires_at: Some(1000),
            gateway_order_id: None,
            payment_id: None,
            event_id: None,
            receipt: None,
        };
        let p = json!({"paymentsCount":1,"payments":[{"id":"PAY_test","orderId":"ORD_test","status":"canceled","orderMerchantExternalId":o.id}]});
        let d = json!({"onetimeOrders":[{"id":"ORD_test","currency":"CNY","status":"canceled","testMode":true,
            "orderMerchantExternalId":o.id,"priceSnapshot":{"currency":"CNY","subtotal":"1.00","taxAmount":"0.10","total":"1.10"},
            "onetimeProduct":{"id":"PROD_test"}}]});
        (c, o, d, p)
    }
    #[test]
    fn cancellation_requires_canonical_terminal_state_and_expired_session() {
        let (c, o, d, p) = fixture();
        assert!(verify_cancellation(&c, &o, &d, &p, 1060).is_ok());
        assert!(verify_cancellation(&c, &o, &d, &p, 1059).is_err());
        for field in ["status", "testMode", "orderMerchantExternalId"] {
            let mut invalid = d.clone();
            invalid["onetimeOrders"][0][field] = match field {
                "status" => json!("pending"),
                "testMode" => json!(false),
                _ => json!("other"),
            };
            assert!(verify_cancellation(&c, &o, &invalid, &p, 1060).is_err());
        }
        for status in ["pending", "succeeded"] {
            let mut invalid = p.clone();
            invalid["payments"][0]["status"] = json!(status);
            assert!(verify_cancellation(&c, &o, &d, &invalid, 1060).is_err());
        }
        let mut partial = p.clone();
        partial["paymentsCount"] = json!(2);
        assert!(verify_cancellation(&c, &o, &d, &partial, 1060).is_err());
    }
    #[test]
    fn empty_payment_history_requires_one_proven_canceled_order() {
        let (c, o, d, _) = fixture();
        let empty = json!({"paymentsCount":0,"payments":[]});
        assert!(verify_cancellation(&c, &o, &d, &empty, 1060).is_ok());
        let missing = json!({"onetimeOrders":[]});
        assert!(verify_cancellation(&c, &o, &missing, &empty, 1060).is_err());
        let duplicated = json!({"onetimeOrders":[d["onetimeOrders"][0],d["onetimeOrders"][0]]});
        assert!(verify_cancellation(&c, &o, &duplicated, &empty, 1060).is_err());
    }
}
