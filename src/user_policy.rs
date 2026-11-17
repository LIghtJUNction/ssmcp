//! Per-account relay bandwidth shared by every connection in each direction.
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Notify, time::Instant};

pub const DEFAULT_RATE_LIMIT_BPS: u64 = 8 * 1024 * 1024;
pub const MAX_RATE_LIMIT_BPS: u64 = 1024 * 1024 * 1024;
const MAX_BURST: u64 = 64 * 1024;

pub fn default_rate_limit() -> u64 {
    DEFAULT_RATE_LIMIT_BPS
}

pub fn valid_rate(value: u64) -> bool {
    (1..=MAX_RATE_LIMIT_BPS).contains(&value)
}

#[derive(Clone, Copy)]
struct Settings {
    banned: bool,
    rate: u64,
}

struct Bucket {
    tokens: f64,
    updated: Instant,
}
impl Bucket {
    fn new(rate: u64) -> Self {
        Self {
            tokens: rate.min(MAX_BURST) as f64,
            updated: Instant::now(),
        }
    }
    fn take(&mut self, now: Instant, rate: u64, bytes: usize) -> Option<Duration> {
        let capacity = rate.min(MAX_BURST) as f64;
        self.tokens = (self.tokens + now.duration_since(self.updated).as_secs_f64() * rate as f64)
            .min(capacity);
        self.updated = now;
        if self.tokens >= bytes as f64 {
            self.tokens -= bytes as f64;
            None
        } else {
            Some(Duration::from_secs_f64(
                (bytes as f64 - self.tokens) / rate as f64,
            ))
        }
    }
}

pub(crate) struct Bandwidth {
    settings: RwLock<Settings>,
    buckets: [Mutex<Bucket>; 2],
    changed: Notify,
}
impl Bandwidth {
    fn new(rate: u64) -> Self {
        Self {
            settings: RwLock::new(Settings {
                banned: false,
                rate,
            }),
            buckets: [Mutex::new(Bucket::new(rate)), Mutex::new(Bucket::new(rate))],
            changed: Notify::new(),
        }
    }
    fn configure(&self, banned: bool, rate: u64) {
        let mut settings = self.settings.write().unwrap();
        *settings = Settings { banned, rate };
        for bucket in &self.buckets {
            let mut bucket = bucket.lock().unwrap();
            // Changing policy must not mint a fresh burst for every save.
            bucket.tokens = bucket.tokens.min(rate.min(MAX_BURST) as f64);
            bucket.updated = Instant::now();
        }
        drop(settings);
        self.changed.notify_waiters();
    }
    pub(crate) fn banned(&self) -> bool {
        self.settings.read().unwrap().banned
    }
    pub(crate) async fn wait_banned(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.banned() {
                return;
            }
            changed.await;
        }
    }
    /// Charge the serialized relay payload before forwarding it. A large
    /// WebSocket message consumes multiple bounded chunks from the same bucket.
    pub(crate) async fn charge(&self, outbound: bool, mut bytes: usize) -> Result<(), ()> {
        while bytes > 0 {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let (chunk, wait) = {
                let settings = *self.settings.read().unwrap();
                if settings.banned {
                    return Err(());
                }
                let chunk = bytes.min(settings.rate.min(MAX_BURST) as usize);
                let wait = self.buckets[usize::from(outbound)].lock().unwrap().take(
                    Instant::now(),
                    settings.rate,
                    chunk,
                );
                (chunk, wait)
            };
            match wait {
                Some(delay) => tokio::select! {
                    _ = tokio::time::sleep(delay) => {},
                    _ = &mut changed => {},
                },
                None => bytes -= chunk,
            }
        }
        if self.banned() { Err(()) } else { Ok(()) }
    }
}

/// Entries are created only for authenticated, registered accounts. Keeping the
/// bucket after disconnect prevents a reconnect from resetting its byte budget.
pub(crate) struct UserPolicies {
    default_rate: AtomicU64,
    entries: Mutex<HashMap<String, Arc<Bandwidth>>>,
}
impl Default for UserPolicies {
    fn default() -> Self {
        Self {
            default_rate: AtomicU64::new(DEFAULT_RATE_LIMIT_BPS),
            entries: Mutex::default(),
        }
    }
}
impl UserPolicies {
    pub(crate) fn default_rate(&self) -> u64 {
        self.default_rate.load(Ordering::Relaxed)
    }
    pub(crate) fn set_default(&self, rate: u64) -> crate::Result<()> {
        if !valid_rate(rate) {
            return Err("default_rate_limit_bps must be in 1..=1073741824 bytes per second".into());
        }
        self.default_rate.store(rate, Ordering::Relaxed);
        Ok(())
    }
    pub(crate) fn get(&self, subject: &str) -> Arc<Bandwidth> {
        self.entries
            .lock()
            .unwrap()
            .entry(subject.to_owned())
            .or_insert_with(|| Arc::new(Bandwidth::new(self.default_rate())))
            .clone()
    }
    pub(crate) fn is_banned(&self, subject: &str) -> bool {
        self.entries
            .lock()
            .unwrap()
            .get(subject)
            .is_some_and(|entry| entry.banned())
    }
    pub(crate) fn set(&self, subject: &str, banned: bool, rate: Option<u64>) {
        self.get(subject)
            .configure(banned, rate.unwrap_or_else(|| self.default_rate()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_connections_share_budget_but_directions_and_users_are_independent() {
        let policies = UserPolicies::default();
        policies.set_default(1024).unwrap();
        let first = policies.get("123");
        let second = policies.get("123");
        let other = policies.get("456");
        assert!(Arc::ptr_eq(&first, &second));
        let now = Instant::now();
        assert_eq!(first.buckets[0].lock().unwrap().take(now, 1024, 1024), None);
        assert_eq!(
            second.buckets[0].lock().unwrap().take(now, 1024, 512),
            Some(Duration::from_millis(500))
        );
        assert_eq!(first.buckets[1].lock().unwrap().take(now, 1024, 1024), None);
        assert_eq!(other.buckets[0].lock().unwrap().take(now, 1024, 1024), None);
        assert_eq!(
            second.buckets[0]
                .lock()
                .unwrap()
                .take(now + Duration::from_millis(500), 1024, 512),
            None
        );
    }

    #[tokio::test]
    async fn live_rate_change_and_ban_wake_waiters() {
        let policies = UserPolicies::default();
        policies.set_default(1).unwrap();
        let bandwidth = policies.get("123");
        bandwidth.charge(false, 1).await.unwrap();
        let waiter = tokio::spawn({
            let bandwidth = bandwidth.clone();
            async move { bandwidth.charge(false, 1000).await }
        });
        tokio::task::yield_now().await;
        policies.set("123", false, Some(1_000_000));
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let waiter = tokio::spawn({
            let bandwidth = bandwidth.clone();
            async move { bandwidth.wait_banned().await }
        });
        policies.set("123", true, None);
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
        assert!(bandwidth.charge(true, 1).await.is_err());
        policies.set("123", false, None);
        assert!(!bandwidth.banned());
    }

    #[test]
    fn rate_bounds_and_burst_are_bounded() {
        assert!(!valid_rate(0));
        assert!(!valid_rate(MAX_RATE_LIMIT_BPS + 1));
        assert!(valid_rate(DEFAULT_RATE_LIMIT_BPS));
        let mut bucket = Bucket::new(DEFAULT_RATE_LIMIT_BPS);
        let now = Instant::now() + Duration::from_secs(3600);
        assert_eq!(
            bucket.take(now, DEFAULT_RATE_LIMIT_BPS, MAX_BURST as usize),
            None
        );
        assert!(bucket.take(now, DEFAULT_RATE_LIMIT_BPS, 1).is_some());
    }
}
