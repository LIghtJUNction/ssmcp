//! Waffo Pancake merchant protocol, verified against its official Go SDK.
//! See docs/commerce.md for upstream source paths and verified field semantics.
mod reconcile;

use super::store::Order;
use super::{CommerceConfig, Error, amount, https_origin};
use base64::{Engine, engine::general_purpose::STANDARD};
use rustls::{
    SignatureScheme,
    pki_types::{PrivateKeyDer, SubjectPublicKeyInfoDer, pem::PemObject},
    sign::SigningKey,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    sync::Arc,
    time::Duration,
};

const MAX_RESPONSE: usize = 64 * 1024;
pub(crate) const MAX_WEBHOOK: usize = 64 * 1024;
const CREATE_SESSION: &str = "/v1/actions/checkout/create-session";
const ISSUE_TOKEN: &str = "/v1/actions/auth/issue-session-token";

pub(crate) struct Pancake {
    client: reqwest::Client,
    config: CommerceConfig,
    signing_key: Arc<dyn SigningKey>,
    webhook_key: SubjectPublicKeyInfoDer<'static>,
}
pub(crate) struct Checkout {
    pub session_id: String,
    pub url: String,
    pub expires_at: u64,
}

pub(crate) struct CancellationProof {
    pub(super) local_id: String,
    pub(super) remote_id: String,
    pub(super) session_id: String,
    pub(super) checked_at: u64,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WebhookEvent {
    pub id: String,
    pub event_type: String,
    pub event_id: String,
    pub store_id: String,
    pub mode: String,
    pub data: WebhookData,
}
#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WebhookData {
    #[serde(default)]
    pub order_id: String,
    pub order_status: Option<String>,
    pub order_merchant_external_id: Option<String>,
    pub merchant_provided_buyer_identity: Option<String>,
    #[serde(default)]
    pub currency: String,
    pub charged_amount: Option<String>,
    pub list_price: Option<Price>,
    pub payment_id: Option<String>,
    pub payment_status: Option<String>,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Price {
    pub total: String,
    pub subtotal: Option<String>,
    pub tax_amount: String,
    pub list_total: Option<String>,
    pub discount_amount: Option<String>,
}
impl Price {
    /// This integration creates tax-exclusive, one-time orders without discounts.
    /// Missing actual-charge/subtotal evidence must never be inferred from a list price.
    pub(crate) fn checked_total(&self, base_minor: u64, decimals: u8) -> Option<(u64, u64)> {
        let subtotal = super::parse_amount(self.subtotal.as_deref()?, decimals)?;
        let tax = super::parse_amount(&self.tax_amount, decimals)?;
        let total = super::parse_amount(&self.total, decimals)?;
        if subtotal != base_minor || subtotal.checked_add(tax)? != total {
            return None;
        }
        match (&self.list_total, &self.discount_amount) {
            (Some(list), Some(discount))
                if super::parse_amount(list, decimals)? == total
                    && super::parse_amount(discount, decimals)? == 0 => {}
            // Older payment events omit both fields. The exact base+tax equation still applies.
            (None, None) => {}
            _ => return None,
        }
        Some((total, tax))
    }
}

/// Official one-time product limits; see docs.waffo.ai/features/products.
pub(crate) fn price_limits(currency: &str) -> Option<(u8, u64, u64)> {
    Some(match currency {
        "CNY" => (2, 100, 100_000),
        "USD" => (2, 100, 1_000_000),
        "EUR" => (2, 100, 940_000),
        "GBP" => (2, 100, 815_000),
        "JPY" => (0, 100, 1_600_000),
        "HKD" => (2, 800, 7_760_000),
        _ => return None,
    })
}

fn read_key(path: &Path, private: bool) -> crate::Result<Vec<u8>> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("commerce key paths must be absolute and contain no '..'".into());
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let m = file.metadata()?;
    // SAFETY: geteuid has no preconditions.
    if !m.is_file()
        || m.len() > 16 * 1024
        || m.nlink() != 1
        || (private && (m.mode() & 0o777 != 0o600 || m.uid() != unsafe { libc::geteuid() }))
        || (!private && m.mode() & 0o022 != 0)
    {
        return Err("commerce key file has unsafe ownership, mode, type or size".into());
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(16 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 16 * 1024 {
        return Err("commerce key file is too large".into());
    }
    Ok(bytes)
}
impl Pancake {
    pub(crate) fn new(config: &CommerceConfig) -> crate::Result<Self> {
        let private = read_key(
            config
                .private_key_file
                .as_deref()
                .ok_or("missing commerce private key")?,
            true,
        )?;
        let key = PrivateKeyDer::from_pem_slice(&private)
            .map_err(|_| "invalid commerce private key PEM")?;
        let signing_key = rustls::crypto::ring::sign::any_supported_type(&key)
            .map_err(|_| "commerce private key must be a supported RSA key")?;
        if signing_key
            .choose_scheme(&[SignatureScheme::RSA_PKCS1_SHA256])
            .is_none()
        {
            return Err("commerce requires an RSA key for PKCS#1 SHA-256 signing".into());
        }
        let public = read_key(
            config
                .webhook_public_key_file
                .as_deref()
                .ok_or("missing commerce webhook key")?,
            false,
        )?;
        let webhook_key = SubjectPublicKeyInfoDer::from_pem_slice(&public)
            .map_err(|_| "commerce webhook key must be a PUBLIC KEY PEM")?;
        webpki::RawPublicKeyEntity::try_from(&webhook_key)
            .map_err(|_| "invalid commerce webhook SPKI")?;
        let client = crate::http_client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "could not initialize commerce HTTP client")?;
        Ok(Self {
            client,
            config: config.clone(),
            signing_key,
            webhook_key,
        })
    }
    fn signature(&self, path: &str, timestamp: u64, body: &[u8]) -> Result<String, Error> {
        let canonical = format!(
            "POST\n{path}\n{timestamp}\n{}",
            STANDARD.encode(Sha256::digest(body))
        );
        let signer = self
            .signing_key
            .choose_scheme(&[SignatureScheme::RSA_PKCS1_SHA256])
            .ok_or(Error::Configuration)?;
        Ok(STANDARD.encode(
            signer
                .sign(canonical.as_bytes())
                .map_err(|_| Error::Gateway)?,
        ))
    }
    async fn post(
        &self,
        path: &str,
        body: Value,
        idempotency: Option<&str>,
    ) -> Result<Value, Error> {
        let bytes = serde_json::to_vec(&body).map_err(|_| Error::Gateway)?;
        let timestamp = super::now();
        let signature = self.signature(path, timestamp, &bytes)?;
        let mut request = self
            .client
            .post(format!(
                "{}{path}",
                self.config.api_base_url.trim_end_matches('/')
            ))
            .header("Content-Type", "application/json")
            .header("X-Merchant-Id", &self.config.merchant_id)
            .header("X-Timestamp", timestamp.to_string())
            .header("X-Signature", signature)
            .body(bytes);
        // Merchant signing keys determine the environment. Never send a browser-supplied environment.
        if let Some(key) = idempotency {
            request = request.header("X-Idempotency-Key", key);
        }
        let mut response = request.send().await.map_err(|_| Error::Gateway)?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|n| n > MAX_RESPONSE as u64)
        {
            return Err(Error::Gateway);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error::Gateway)? {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE {
                return Err(Error::Gateway);
            }
            bytes.extend_from_slice(&chunk);
        }
        let envelope: Value = serde_json::from_slice(&bytes).map_err(|_| Error::Gateway)?;
        if envelope
            .get("errors")
            .is_some_and(|e| !e.is_null() && e.as_array().is_none_or(|e| !e.is_empty()))
        {
            return Err(Error::Gateway);
        }
        envelope
            .get("data")
            .filter(|d| d.is_object())
            .cloned()
            .ok_or(Error::Gateway)
    }
    pub(crate) async fn checkout(
        &self,
        order: &Order,
        public_url: &str,
        timestamp: u64,
    ) -> Result<Checkout, Error> {
        if timestamp >= order.checkout_deadline {
            return Err(Error::Expired);
        }
        let token = self
            .post(
                ISSUE_TOKEN,
                json!({
                    "productId": self.config.product_id,
                    "buyerIdentity": format!("github:{}",order.subject)
                }),
                None,
            )
            .await?;
        let token = token
            .get("token")
            .and_then(Value::as_str)
            .filter(|t| {
                !t.is_empty()
                    && t.len() <= 8192
                    && t.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
            })
            .ok_or(Error::Gateway)?;
        let response = self.post(CREATE_SESSION, json!({
            "productId": self.config.product_id,
            "currency": order.currency,
            "priceSnapshot": { "amount": amount(order.base_minor,order.currency_decimals), "taxIncluded": false, "taxCategory": "saas" },
            "successUrl": format!("{}/account/traffic/orders/{}",public_url.trim_end_matches('/'),order.id),
            "expiresInSeconds": order.checkout_deadline.saturating_sub(timestamp),
            "orderMerchantExternalId": order.id,
            "allowPromotionCodes": false,
            "metadata": { "ssmcpPeriod": order.period, "ssmcpTrafficGb": order.quantity_gb.to_string() },
            "language": "zh-Hans"
        }), Some(&order.id)).await?;
        let session_id = response
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= 256)
            .ok_or(Error::Gateway)?;
        let expires_at = response
            .get("expiresAt")
            .and_then(Value::as_str)
            .and_then(super::utc_timestamp)
            .ok_or(Error::Gateway)?;
        if expires_at <= timestamp || expires_at > order.month_end {
            return Err(Error::Gateway);
        }
        let checkout = response
            .get("checkoutUrl")
            .and_then(Value::as_str)
            .ok_or(Error::Gateway)?;
        let mut url = reqwest::Url::parse(checkout).map_err(|_| Error::Gateway)?;
        let expected = https_origin(&self.config.checkout_origin)?;
        if url.scheme() != "https"
            || url.origin() != expected.origin()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(Error::Gateway);
        }
        url.set_fragment(Some(&format!("token={token}")));
        // The authenticated portal renders a no-store checkout link with no-referrer.
        // The bearer token is never persisted or logged.
        Ok(Checkout {
            session_id: session_id.into(),
            url: url.to_string(),
            expires_at,
        })
    }
    pub(crate) fn verify(
        &self,
        raw: &[u8],
        header: &str,
        timestamp_ms: u64,
    ) -> Result<WebhookEvent, Error> {
        verify(
            &self.webhook_key,
            raw,
            header,
            timestamp_ms,
            &self.config.store_id,
            &self.config.environment,
        )
    }
}
fn verify(
    key: &SubjectPublicKeyInfoDer<'_>,
    raw: &[u8],
    header: &str,
    now_ms: u64,
    store_id: &str,
    environment: &str,
) -> Result<WebhookEvent, Error> {
    if raw.len() > MAX_WEBHOOK || header.len() > 8192 {
        return Err(Error::Signature);
    }
    let mut timestamp = None;
    let mut signature = None;
    for component in header.split(',') {
        let (name, value) = component.trim().split_once('=').ok_or(Error::Signature)?;
        match name.trim() {
            "t" if timestamp.is_none() => timestamp = Some(value.trim()),
            "v1" if signature.is_none() => signature = Some(value.trim()),
            _ => return Err(Error::Signature),
        }
    }
    let timestamp_raw = timestamp.ok_or(Error::Signature)?;
    if !timestamp_raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::Signature);
    }
    let timestamp = timestamp_raw.parse::<u64>().map_err(|_| Error::Signature)?;
    if now_ms.saturating_sub(timestamp) > 45 * 60 * 1000
        || timestamp.saturating_sub(now_ms) > 60 * 1000
    {
        return Err(Error::Signature);
    }
    let signature = STANDARD
        .decode(signature.ok_or(Error::Signature)?)
        .map_err(|_| Error::Signature)?;
    let mut signed = Vec::with_capacity(raw.len() + timestamp_raw.len() + 1);
    signed.extend_from_slice(timestamp_raw.as_bytes());
    signed.push(b'.');
    signed.extend_from_slice(raw);
    webpki::RawPublicKeyEntity::try_from(key)
        .map_err(|_| Error::Signature)?
        .verify_signature(
            webpki::ring::RSA_PKCS1_2048_8192_SHA256,
            &signed,
            &signature,
        )
        .map_err(|_| Error::Signature)?;
    let event: WebhookEvent = serde_json::from_slice(raw).map_err(|_| Error::Signature)?;
    if event.store_id != store_id || event.mode != environment {
        return Err(Error::PaymentMismatch);
    }
    Ok(event)
}

#[cfg(test)]
mod tests;
