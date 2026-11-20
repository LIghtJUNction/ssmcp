//! Small private order ledger. Inventory reservation and payment transitions share one writer lock.
use super::pancake::WebhookEvent;
use super::{CHECKOUT_SECONDS, CommerceConfig, Error, GB, month, parse_amount, valid_subject};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

const MAX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ORDERS: usize = 10_000;
fn default_offset() -> i32 {
    480
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OrderState {
    Reserved,
    Paid,
    Granted,
    PaidExpired,
    Cancelled,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Order {
    pub id: String,
    pub subject: String,
    pub request_nonce: String,
    pub period: String,
    #[serde(default = "default_offset")]
    pub utc_offset_minutes: i32,
    pub quantity_gb: u64,
    pub bytes: u64,
    pub base_minor: u64,
    pub currency: String,
    pub currency_decimals: u8,
    pub created_at: u64,
    pub month_end: u64,
    pub checkout_deadline: u64,
    pub state: OrderState,
    pub session_id: Option<String>,
    #[serde(default)]
    pub session_expires_at: Option<u64>,
    pub gateway_order_id: Option<String>,
    pub payment_id: Option<String>,
    pub event_id: Option<String>,
    #[serde(default)]
    pub receipt: Option<PaymentReceipt>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PaymentReceipt {
    pub tax_minor: u64,
    pub charged_minor: u64,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Database {
    version: u32,
    #[serde(default)]
    scope: Option<GatewayScope>,
    orders: BTreeMap<String, Order>,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GatewayScope {
    environment: String,
    merchant_id: String,
    store_id: String,
    product_id: String,
}
impl From<&CommerceConfig> for GatewayScope {
    fn from(config: &CommerceConfig) -> Self {
        Self {
            environment: config.environment.clone(),
            merchant_id: config.merchant_id.clone(),
            store_id: config.store_id.clone(),
            product_id: config.product_id.clone(),
        }
    }
}

pub(crate) enum Settlement {
    Ignored,
    AlreadyGranted,
    Grant(Box<Order>),
    Review,
}
pub(crate) struct OrderStore {
    directory: File,
    lock: File,
    database: Database,
    failed: bool,
}
impl Drop for OrderStore {
    fn drop(&mut self) {
        // SAFETY: the descriptor is owned by this store and still open.
        unsafe {
            libc::flock(self.lock.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
fn check_file(file: &File) -> crate::Result<()> {
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no preconditions.
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(
            "commerce files must be owned by the service user, regular, single-link and mode 0600"
                .into(),
        );
    }
    Ok(())
}
impl OrderStore {
    pub(crate) fn open(path: &Path) -> crate::Result<Self> {
        if !path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err("commerce state directory must be an absolute path without '..'".into());
        }
        for ancestor in path.ancestors() {
            match fs::symlink_metadata(ancestor) {
                Ok(m) if m.file_type().is_symlink() => {
                    return Err("commerce state path must not contain symlinks".into());
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && ancestor == path => {}
                Err(e) => return Err(e.into()),
            }
        }
        if !path.exists() {
            fs::DirBuilder::new().mode(0o700).create(path)?;
        }
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let m = directory.metadata()?;
        // SAFETY: geteuid has no preconditions.
        if m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o777 != 0o700 {
            return Err("commerce state directory must be service-owned and mode 0700".into());
        }
        let anchored = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(anchored.join("writer.lock"))?;
        check_file(&lock)?;
        // SAFETY: flock operates on this owned descriptor; no waiting behind a second writer.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("commerce state already has a writer".into());
        }
        let database = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(anchored.join("orders.json"))
        {
            Ok(mut file) => {
                check_file(&file)?;
                if file.metadata()?.len() > MAX_BYTES {
                    return Err("commerce ledger is too large".into());
                }
                let mut bytes = Vec::new();
                Read::by_ref(&mut file)
                    .take(MAX_BYTES + 1)
                    .read_to_end(&mut bytes)?;
                if bytes.len() as u64 > MAX_BYTES {
                    return Err("commerce ledger is too large".into());
                }
                let database: Database =
                    serde_json::from_slice(&bytes).map_err(|_| "commerce ledger is malformed")?;
                if database.version != 1 || database.orders.len() > MAX_ORDERS {
                    return Err("commerce ledger version or size is unsupported".into());
                }
                for (id, order) in &database.orders {
                    if id != &order.id
                        || !valid_subject(&order.subject)
                        || order.quantity_gb == 0
                        || order.bytes
                            != order
                                .quantity_gb
                                .checked_mul(GB)
                                .ok_or("commerce ledger has invalid byte count")?
                        || order.base_minor == 0
                        || order.currency_decimals > 3
                        || month(order.created_at, order.utc_offset_minutes)
                            .map(|(period, end)| period != order.period || end != order.month_end)
                            .unwrap_or(true)
                        || order.created_at >= order.month_end
                        || order.checkout_deadline > order.month_end
                        || order.period.len() != 7
                    {
                        return Err("commerce ledger contains an invalid order".into());
                    }
                }
                database
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Database {
                version: 1,
                ..Database::default()
            },
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            directory,
            lock,
            database,
            failed: false,
        })
    }
    /// Prevent test orders or a different store's recovery intents from entering this ledger.
    pub(crate) fn configure_scope(&mut self, config: &CommerceConfig) -> Result<(), Error> {
        self.ensure()?;
        let scope = GatewayScope::from(config);
        if let Some(existing) = &self.database.scope {
            return if existing == &scope {
                Ok(())
            } else {
                Err(Error::Configuration)
            };
        }
        if !self.database.orders.is_empty() {
            return Err(Error::Configuration);
        }
        let mut next = self.database.clone();
        next.scope = Some(scope);
        self.persist(next)
    }
    fn ensure(&self) -> Result<(), Error> {
        if self.failed {
            Err(Error::Storage)
        } else {
            Ok(())
        }
    }
    fn persist(&mut self, next: Database) -> Result<(), Error> {
        self.ensure()?;
        let bytes = serde_json::to_vec(&next).map_err(|_| Error::Storage)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(Error::TooManyOrders);
        }
        let anchored = PathBuf::from(format!("/proc/self/fd/{}", self.directory.as_raw_fd()));
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(|_| Error::Storage)?;
        let name = random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let temporary = anchored.join(format!(".orders-{name}.tmp"));
        let result = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, anchored.join("orders.json"))?;
            self.directory.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
            // An fsync failure after rename leaves durability ambiguous. Never continue on stale state.
            self.failed = true;
            return Err(Error::Storage);
        }
        self.database = next;
        Ok(())
    }
    pub(crate) fn available_gb(&self, period: &str, capacity: u64) -> Result<u64, Error> {
        self.ensure()?;
        let reserved = self
            .database
            .orders
            .values()
            .filter(|o| o.period == period && o.state != OrderState::Cancelled)
            .try_fold(0_u64, |n, o| {
                n.checked_add(o.quantity_gb).ok_or(Error::Storage)
            })?;
        Ok(capacity.saturating_sub(reserved))
    }
    pub(crate) fn list(&self, subject: &str) -> Result<Vec<Order>, Error> {
        self.ensure()?;
        let mut orders: Vec<_> = self
            .database
            .orders
            .values()
            .filter(|o| o.subject == subject)
            .cloned()
            .collect();
        orders.sort_by_key(|o| std::cmp::Reverse(o.created_at));
        orders.truncate(100);
        Ok(orders)
    }
    pub(crate) fn owned(&self, subject: &str, id: &str) -> Result<Order, Error> {
        self.ensure()?;
        self.database
            .orders
            .get(id)
            .filter(|o| o.subject == subject)
            .cloned()
            .ok_or(Error::NotFound)
    }
    pub(crate) fn reserve(
        &mut self,
        config: &CommerceConfig,
        subject: &str,
        nonce: &str,
        quantity: u64,
        timestamp: u64,
    ) -> Result<Order, Error> {
        self.ensure()?;
        if !config.enabled {
            return Err(Error::Disabled);
        }
        if !valid_subject(subject)
            || nonce.len() != 32
            || !nonce.bytes().all(|b| b.is_ascii_hexdigit())
            || quantity == 0
            || quantity > config.max_gb_per_order
        {
            return Err(Error::Invalid);
        }
        let (period, end) = month(timestamp, config.utc_offset_minutes)?;
        // Do not offer a payment link across the monthly boundary.
        if end.saturating_sub(timestamp) < CHECKOUT_SECONDS + 60 {
            return Err(Error::Expired);
        }
        if let Some(order) = self
            .database
            .orders
            .values()
            .find(|o| o.subject == subject && o.request_nonce == nonce)
        {
            if order.quantity_gb != quantity || order.period != period {
                return Err(Error::Conflict);
            }
            return Ok(order.clone());
        }
        if self
            .database
            .orders
            .values()
            .any(|o| o.subject == subject && o.period == period && o.state == OrderState::Reserved)
        {
            return Err(Error::PendingOrder);
        }
        if self.database.orders.len() >= MAX_ORDERS {
            return Err(Error::TooManyOrders);
        }
        // Reject impossible provider amounts before reserving any inventory.
        let base_minor = config.order_base_minor(quantity)?;
        if self.available_gb(&period, config.monthly_sellable_gb)? < quantity {
            return Err(Error::SoldOut);
        }
        let id = format!(
            "traffic_{}",
            crate::hex::encode(Sha256::digest(format!("{subject}:{nonce}").as_bytes()))
        );
        let order = Order {
            id: id.clone(),
            subject: subject.into(),
            request_nonce: nonce.into(),
            period,
            utc_offset_minutes: config.utc_offset_minutes,
            quantity_gb: quantity,
            bytes: quantity.checked_mul(GB).ok_or(Error::Invalid)?,
            base_minor,
            currency: config.currency.clone(),
            currency_decimals: config.currency_decimals,
            created_at: timestamp,
            month_end: end,
            checkout_deadline: timestamp + CHECKOUT_SECONDS,
            state: OrderState::Reserved,
            session_id: None,
            session_expires_at: None,
            gateway_order_id: None,
            payment_id: None,
            event_id: None,
            receipt: None,
        };
        let mut next = self.database.clone();
        next.orders.insert(id, order.clone());
        self.persist(next)?;
        Ok(order)
    }
    pub(crate) fn bind_checkout(
        &mut self,
        id: &str,
        session_id: &str,
        expires_at: u64,
    ) -> Result<(), Error> {
        self.ensure()?;
        if session_id.is_empty() || session_id.len() > 256 {
            return Err(Error::Gateway);
        }
        let order = self.database.orders.get(id).ok_or(Error::NotFound)?;
        if let Some(existing) = &order.session_id {
            return if existing == session_id && order.session_expires_at == Some(expires_at) {
                Ok(())
            } else {
                Err(Error::Conflict)
            };
        }
        let mut next = self.database.clone();
        let order = next.orders.get_mut(id).ok_or(Error::NotFound)?;
        if expires_at <= order.created_at || expires_at > order.month_end {
            return Err(Error::Gateway);
        }
        order.session_id = Some(session_id.into());
        order.session_expires_at = Some(expires_at);
        self.persist(next)
    }
    pub(crate) fn paid(
        &mut self,
        config: &CommerceConfig,
        event: &WebhookEvent,
        timestamp: u64,
    ) -> Result<Settlement, Error> {
        self.ensure()?;
        let data = &event.data;
        let id = data
            .order_merchant_external_id
            .as_deref()
            .ok_or(Error::PaymentMismatch)?;
        let order = self.database.orders.get(id).ok_or(Error::NotFound)?;
        let (expected_total, tax_minor) = data
            .list_price
            .as_ref()
            .and_then(|price| price.checked_total(order.base_minor, order.currency_decimals))
            .ok_or(Error::PaymentMismatch)?;
        let charged_minor = data
            .charged_amount
            .as_deref()
            .and_then(|value| parse_amount(value, order.currency_decimals))
            .ok_or(Error::PaymentMismatch)?;
        let receipt = PaymentReceipt {
            tax_minor,
            charged_minor,
        };
        if event.event_type != "order.completed"
            || event.store_id != config.store_id
            || event.mode != config.environment
            || event.id.is_empty()
            || event.id.len() > 256
            || event.event_id.is_empty()
            || data.order_id.is_empty()
            || data.order_id.len() > 128
            || data
                .payment_id
                .as_ref()
                .is_none_or(|id| id.is_empty() || id.len() > 128)
            || data.order_status.as_deref() != Some("completed")
            || data.payment_status.as_deref() != Some("succeeded")
            || data.currency != order.currency
            || data.merchant_provided_buyer_identity.as_deref()
                != Some(format!("github:{}", order.subject).as_str())
            || charged_minor != expected_total
        {
            return Err(Error::PaymentMismatch);
        }
        if self.database.orders.values().any(|other| {
            other.id != order.id
                && (other.gateway_order_id.as_deref() == Some(data.order_id.as_str())
                    || other.payment_id.as_ref() == data.payment_id.as_ref()
                    || other.event_id.as_deref() == Some(event.id.as_str()))
        }) {
            return Err(Error::Conflict);
        }
        if order
            .gateway_order_id
            .as_ref()
            .is_some_and(|v| v != &data.order_id)
            || order
                .payment_id
                .as_ref()
                .is_some_and(|v| Some(v) != data.payment_id.as_ref())
        {
            return Err(Error::Conflict);
        }
        if order.state == OrderState::Cancelled
            || order
                .receipt
                .as_ref()
                .is_some_and(|prior| prior != &receipt)
        {
            return Err(Error::Conflict);
        }
        if order.state == OrderState::Granted {
            return Ok(Settlement::AlreadyGranted);
        }
        if order.state == OrderState::PaidExpired {
            return Ok(Settlement::Review);
        }
        if order.state == OrderState::Paid {
            return Ok(Settlement::Grant(Box::new(order.clone())));
        }
        let (period, _) = month(timestamp, config.utc_offset_minutes)?;
        let mut next = self.database.clone();
        let updated = next.orders.get_mut(id).ok_or(Error::NotFound)?;
        updated.gateway_order_id = Some(data.order_id.clone());
        updated.payment_id = data.payment_id.clone();
        updated.event_id = Some(event.id.clone());
        updated.receipt = Some(receipt);
        updated.state = if period == order.period {
            OrderState::Paid
        } else {
            OrderState::PaidExpired
        };
        let order = updated.clone();
        self.persist(next)?;
        if order.state == OrderState::Paid {
            Ok(Settlement::Grant(Box::new(order)))
        } else {
            Ok(Settlement::Review)
        }
    }
    pub(crate) fn release_cancelled(
        &mut self,
        proof: super::pancake::CancellationProof,
    ) -> Result<(), Error> {
        self.ensure()?;
        let order = self
            .database
            .orders
            .get(&proof.local_id)
            .ok_or(Error::NotFound)?;
        if order.state == OrderState::Cancelled
            && order.gateway_order_id.as_ref() == Some(&proof.remote_id)
        {
            return Ok(());
        }
        if order.state != OrderState::Reserved
            || order.session_id.as_deref() != Some(&proof.session_id)
            || order
                .session_expires_at
                .is_none_or(|end| proof.checked_at < end.saturating_add(60))
        {
            return Err(Error::Conflict);
        }
        let mut next = self.database.clone();
        let order = next
            .orders
            .get_mut(&proof.local_id)
            .ok_or(Error::NotFound)?;
        order.state = OrderState::Cancelled;
        order.gateway_order_id = Some(proof.remote_id);
        self.persist(next)
    }
    pub(crate) fn recharged_subjects(&self) -> Result<std::collections::BTreeSet<String>, Error> {
        self.ensure()?;
        Ok(self
            .database
            .orders
            .values()
            .filter(|order| order.state == OrderState::Granted)
            .map(|order| order.subject.clone())
            .collect())
    }
    pub(crate) fn pending_grants(&self) -> Result<Vec<Order>, Error> {
        self.ensure()?;
        Ok(self
            .database
            .orders
            .values()
            .filter(|o| o.state == OrderState::Paid)
            .cloned()
            .collect())
    }
    pub(crate) fn mark_granted(&mut self, id: &str) -> Result<(), Error> {
        self.ensure()?;
        let order = self.database.orders.get(id).ok_or(Error::NotFound)?;
        if order.state == OrderState::Granted {
            return Ok(());
        }
        if order.state != OrderState::Paid {
            return Err(Error::Conflict);
        }
        let mut next = self.database.clone();
        next.orders.get_mut(id).ok_or(Error::NotFound)?.state = OrderState::Granted;
        self.persist(next)
    }
    pub(crate) fn mark_expired(&mut self, id: &str) -> Result<(), Error> {
        self.ensure()?;
        let mut next = self.database.clone();
        let order = next.orders.get_mut(id).ok_or(Error::NotFound)?;
        if order.state != OrderState::Paid {
            return Err(Error::Conflict);
        }
        order.state = OrderState::PaidExpired;
        self.persist(next)
    }
}
#[cfg(test)]
mod tests;
