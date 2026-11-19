//! Monthly traffic orders. Prices and inventory are explicit configuration, never inferred.
mod calendar;
pub(crate) use calendar::{expiry_label, utc_timestamp};
pub(crate) mod pancake;
pub(crate) mod store;

use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use store::{Order, OrderStore};

pub const GB: u64 = 1_000_000_000;
pub(crate) const CHECKOUT_SECONDS: u64 = 900;

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CommerceConfig {
    pub enabled: bool,
    pub gateway: String,
    pub environment: String,
    pub api_base_url: String,
    pub checkout_origin: String,
    pub merchant_id: String,
    pub store_id: String,
    pub product_id: String,
    pub private_key_file: Option<PathBuf>,
    pub webhook_public_key_file: Option<PathBuf>,
    pub currency: String,
    pub currency_decimals: u8,
    pub price_per_gb_minor: Option<u64>,
    pub monthly_sellable_gb: u64,
    pub max_gb_per_order: u64,
    pub utc_offset_minutes: i32,
}
impl Default for CommerceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            gateway: "waffo_pancake".into(),
            environment: "test".into(),
            api_base_url: "https://api.waffo.ai".into(),
            checkout_origin: String::new(),
            merchant_id: String::new(),
            store_id: String::new(),
            product_id: String::new(),
            private_key_file: None,
            webhook_public_key_file: None,
            currency: "CNY".into(),
            currency_decimals: 2,
            price_per_gb_minor: Some(100),
            monthly_sellable_gb: 0,
            max_gb_per_order: 100,
            utc_offset_minutes: 480,
        }
    }
}
impl CommerceConfig {
    pub fn validate(&self) -> crate::Result<()> {
        if !(-720..=840).contains(&self.utc_offset_minutes) {
            return Err("commerce.utc_offset_minutes must be between -720 and 840".into());
        }
        if !self.enabled {
            return Ok(());
        }
        if self.gateway != "waffo_pancake" || !matches!(self.environment.as_str(), "test" | "prod")
        {
            return Err(
                "commerce requires gateway=waffo_pancake and environment=test or prod".into(),
            );
        }
        if self.currency.len() != 3
            || !self.currency.bytes().all(|b| b.is_ascii_uppercase())
            || self.currency_decimals > 3
            || self.price_per_gb_minor.unwrap_or(0) == 0
            || self.max_gb_per_order == 0
            || self.monthly_sellable_gb.checked_mul(GB).is_none()
            || self
                .max_gb_per_order
                .checked_mul(self.price_per_gb_minor.unwrap_or(0))
                .is_none()
        {
            return Err("commerce requires an explicit currency, positive price and valid monthly inventory".into());
        }
        if pancake::price_limits(&self.currency)
            .is_none_or(|(decimals, _, _)| decimals != self.currency_decimals)
        {
            return Err(
                "commerce currency/precision is not supported by Waffo one-time products".into(),
            );
        }
        for (id, prefix) in [
            (&self.merchant_id, "MER_"),
            (&self.store_id, "STO_"),
            (&self.product_id, "PROD_"),
        ] {
            if !id.starts_with(prefix)
                || id.len() <= prefix.len()
                || id.len() > 128
                || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                return Err("commerce gateway identifiers are missing or invalid".into());
            }
        }
        https_origin(&self.api_base_url)
            .map_err(|_| "commerce.api_base_url must be an HTTPS origin")?;
        https_origin(&self.checkout_origin)
            .map_err(|_| "commerce.checkout_origin must be an explicit HTTPS origin")?;
        if self.private_key_file.is_none() || self.webhook_public_key_file.is_none() {
            return Err("commerce requires private_key_file and environment-specific webhook_public_key_file".into());
        }
        Ok(())
    }

    pub(crate) fn order_base_minor(&self, quantity: u64) -> Result<u64, Error> {
        let (decimals, min, max) =
            pancake::price_limits(&self.currency).ok_or(Error::Configuration)?;
        if decimals != self.currency_decimals {
            return Err(Error::Configuration);
        }
        let base = quantity
            .checked_mul(self.price_per_gb_minor.ok_or(Error::Configuration)?)
            .ok_or(Error::Invalid)?;
        if !(min..=max).contains(&base) {
            return Err(Error::PriceOutOfRange);
        }
        Ok(base)
    }

    pub(crate) fn quantity_range(&self, available: u64) -> Option<(u64, u64)> {
        let (decimals, min, max) = pancake::price_limits(&self.currency)?;
        let unit = self.price_per_gb_minor.filter(|n| *n > 0)?;
        if decimals != self.currency_decimals {
            return None;
        }
        let minimum = min.div_ceil(unit).max(1);
        let maximum = self.max_gb_per_order.min(available).min(max / unit);
        (minimum <= maximum).then_some((minimum, maximum))
    }
}
pub(crate) fn https_origin(raw: &str) -> Result<reqwest::Url, Error> {
    let url = reqwest::Url::parse(raw).map_err(|_| Error::Configuration)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(Error::Configuration);
    }
    Ok(url)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    Disabled,
    Configuration,
    Invalid,
    SoldOut,
    PendingOrder,
    NotFound,
    Conflict,
    Storage,
    Gateway,
    Signature,
    PaymentMismatch,
    Expired,
    TooManyOrders,
    CancellationUnconfirmed,
    PriceOutOfRange,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

pub struct Commerce {
    pub(crate) config: CommerceConfig,
    pub(crate) gateway: pancake::Pancake,
    store: Arc<Mutex<OrderStore>>,
}
impl Commerce {
    pub fn open(config: CommerceConfig, state_dir: &Path) -> crate::Result<Option<Self>> {
        config.validate()?;
        if !config.enabled {
            return Ok(None);
        }
        let gateway = pancake::Pancake::new(&config)?;
        let mut store = OrderStore::open(&state_dir.join("commerce"))?;
        store.configure_scope(&config).map_err(|_| "commerce ledger scope differs or cannot be initialized; use a separate test/store state directory")?;
        Ok(Some(Self {
            config,
            gateway,
            store: Arc::new(Mutex::new(store)),
        }))
    }

    pub(crate) async fn storage<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut OrderStore) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = store.lock().map_err(|_| Error::Storage)?;
            operation(&mut guard)
        })
        .await
        .map_err(|_| Error::Storage)?
    }

    pub(crate) async fn reserve(
        &self,
        subject: String,
        nonce: String,
        quantity: u64,
    ) -> Result<Order, Error> {
        let config = self.config.clone();
        self.storage(move |store| store.reserve(&config, &subject, &nonce, quantity, now()))
            .await
    }

    pub(crate) async fn checkout(&self, order: Order, public_url: &str) -> Result<String, Error> {
        let timestamp = now();
        let (period, _) = month(timestamp, self.config.utc_offset_minutes)?;
        if period != order.period || timestamp >= order.checkout_deadline {
            return Err(Error::Expired);
        }
        if order.state != store::OrderState::Reserved {
            return Err(Error::Conflict);
        }
        // Never extend the original reservation or idempotency window on a retry.
        let session = self.gateway.checkout(&order, public_url, timestamp).await?;
        let id = order.id.clone();
        let session_id = session.session_id.clone();
        let expires_at = session.expires_at;
        self.storage(move |store| store.bind_checkout(&id, &session_id, expires_at))
            .await?;
        Ok(session.url)
    }

    pub(crate) async fn reconcile_cancelled(&self, order: Order) -> Result<(), Error> {
        let proof = self.gateway.cancelled_order(&order).await?;
        self.storage(move |store| store.release_cancelled(proof))
            .await
    }

    pub(crate) async fn accept_webhook(
        &self,
        raw: &[u8],
        signature: &str,
    ) -> Result<store::Settlement, Error> {
        let event = self
            .gateway
            .verify(raw, signature, now().saturating_mul(1000))?;
        // The Pancake dashboard sends a signed zero-value probe with no local order.
        let dashboard_probe = event.mode == "test"
            && event.data.charged_amount.as_deref() == Some("0.00")
            && event
                .data
                .order_merchant_external_id
                .as_deref()
                .is_some_and(|id| id.starts_with("[TEST] ORDER-REF-"));
        if event.event_type != "order.completed" || dashboard_probe {
            return Ok(store::Settlement::Ignored);
        }
        let config = self.config.clone();
        self.storage(move |store| store.paid(&config, &event, now()))
            .await
    }
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Calendar boundaries use the configured fixed UTC offset, never a 30-day rolling window.
pub(crate) fn month(timestamp: u64, offset: i32) -> Result<(String, u64), Error> {
    if !(-720..=840).contains(&offset) {
        return Err(Error::Configuration);
    }
    let raw = i64::try_from(timestamp)
        .map_err(|_| Error::Invalid)?
        .checked_add(i64::from(offset) * 60)
        .ok_or(Error::Invalid)?;
    let mut date: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: gmtime_r reads one valid time_t and writes one valid tm; no shared C state.
    if unsafe { libc::gmtime_r(&raw, &mut date) }.is_null() {
        return Err(Error::Invalid);
    }
    let period = format!("{:04}-{:02}", date.tm_year + 1900, date.tm_mon + 1);
    date.tm_mon += 1;
    date.tm_mday = 1;
    date.tm_hour = 0;
    date.tm_min = 0;
    date.tm_sec = 0;
    // SAFETY: timegm accepts and normalizes a writable tm, including December + 1.
    let end = unsafe { libc::timegm(&mut date) }
        .checked_sub(i64::from(offset) * 60)
        .ok_or(Error::Invalid)?;
    Ok((period, u64::try_from(end).map_err(|_| Error::Invalid)?))
}
pub(crate) fn valid_subject(subject: &str) -> bool {
    subject
        .parse::<u64>()
        .is_ok_and(|n| n > 0 && n.to_string() == subject)
}
pub(crate) fn amount(minor: u64, decimals: u8) -> String {
    if decimals == 0 {
        return minor.to_string();
    }
    let scale = 10_u64.pow(u32::from(decimals));
    format!(
        "{}.{:0width$}",
        minor / scale,
        minor % scale,
        width = usize::from(decimals)
    )
}
pub(crate) fn parse_amount(raw: &str, decimals: u8) -> Option<u64> {
    if raw.len() > 32 || decimals > 3 {
        return None;
    }
    let (whole, fraction) = raw.split_once('.').unwrap_or((raw, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || fraction.len() > usize::from(decimals)
        || (raw.contains('.') && fraction.is_empty())
    {
        return None;
    }
    let fraction: u64 = if fraction.is_empty() {
        0
    } else {
        fraction.parse().ok()?
    };
    whole
        .parse::<u64>()
        .ok()?
        .checked_mul(10_u64.pow(u32::from(decimals)))?
        .checked_add(
            fraction.checked_mul(10_u64.pow(
                u32::from(decimals) - raw.split_once('.').map_or(0, |(_, f)| f.len() as u32),
            ))?,
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_do_not_sell() {
        let c = CommerceConfig::default();
        assert!(!c.enabled);
        assert!(c.validate().is_ok());
        let mut c = c;
        c.enabled = true;
        assert!(c.validate().is_err());
    }
    #[test]
    fn decimal_amounts_never_round() {
        assert_eq!(parse_amount("1.25", 2), Some(125));
        assert_eq!(parse_amount("1.2", 2), Some(120));
        assert_eq!(parse_amount("1", 2), Some(100));
        for value in ["1.001", "NaN", "1e2", "-1", " 1", "1.", ".1", "1.2.3"] {
            assert_eq!(parse_amount(value, 2), None);
        }
        assert_eq!(amount(125, 2), "1.25");
        assert_eq!(parse_amount("18446744073709551615", 2), None);
    }
    #[test]
    fn natural_month_uses_configured_timezone() {
        assert_eq!(month(0, 480).unwrap(), ("1970-01".into(), 2_649_600));
        assert_eq!(month(2_649_599, 480).unwrap().0, "1970-01");
        assert_eq!(month(2_649_600, 480).unwrap().0, "1970-02");
        assert!(valid_subject("123"));
        assert!(!valid_subject("00123"));
    }
}
