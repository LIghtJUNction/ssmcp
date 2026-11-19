//! Per-account relay bandwidth shared by every connection in each direction.
use serde::{Deserialize, Serialize};
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
pub(crate) const RECHARGED_RATE_LIMIT_BPS: u64 = 64 * 1024 * 1024;
pub const MAX_RATE_LIMIT_BPS: u64 = 1024 * 1024 * 1024;
const MAX_BURST: u64 = 64 * 1024;

pub const FREE_MONTHLY_TRAFFIC_BYTES: u64 = 1_000_000_000;
pub const DEFAULT_TRAFFIC_UTC_OFFSET_MINUTES: i32 = 8 * 60;

/// Server-authoritative monthly allowance. A missing limit means an administrator
/// explicitly named by ManagementPolicy, never a role supplied by a client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrafficQuota {
    pub period: String,
    pub utc_offset_minutes: i32,
    pub free_bytes: u64,
    pub paid_bytes: u64,
    pub used_bytes: u64,
    pub limit_bytes: Option<u64>,
    pub remaining_bytes: Option<u64>,
}

impl TrafficQuota {
    pub(crate) fn new(
        period: String,
        utc_offset_minutes: i32,
        used_bytes: u64,
        paid_bytes: u64,
        unlimited: bool,
    ) -> Self {
        let limit = FREE_MONTHLY_TRAFFIC_BYTES.saturating_add(paid_bytes);
        Self {
            period,
            utc_offset_minutes,
            free_bytes: FREE_MONTHLY_TRAFFIC_BYTES,
            paid_bytes,
            used_bytes,
            limit_bytes: (!unlimited).then_some(limit),
            remaining_bytes: (!unlimited).then_some(limit.saturating_sub(used_bytes)),
        }
    }
}

pub fn valid_traffic_utc_offset(minutes: i32) -> bool {
    (-12 * 60..=14 * 60).contains(&minutes)
}

pub(crate) fn valid_traffic_period(period: &str) -> bool {
    period.is_ascii()
        && period.len() == 7
        && period.as_bytes()[4] == b'-'
        && period[..4].bytes().all(|b| b.is_ascii_digit())
        && period[5..].bytes().all(|b| b.is_ascii_digit())
        && period[..4]
            .parse::<u32>()
            .is_ok_and(|year| (1..=9999).contains(&year))
        && period[5..]
            .parse::<u32>()
            .is_ok_and(|month| (1..=12).contains(&month))
}

/// Gregorian calendar month for a fixed, explicit server offset. This avoids
/// dependence on the process locale, host timezone, or a daylight-saving rule.
pub fn traffic_period_at(unix_seconds: u64, utc_offset_minutes: i32) -> String {
    let seconds = (unix_seconds.min(i64::MAX as u64) as i64)
        .saturating_add(i64::from(utc_offset_minutes) * 60);
    let days = seconds.div_euclid(86_400) + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let month = month_from_march + if month_from_march < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!("{year:04}-{month:02}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TrafficError {
    Banned,
    Exhausted,
    AccountRequired,
    Storage,
}
impl std::fmt::Display for TrafficError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Banned => "account is banned",
            Self::Exhausted => "traffic_quota_exceeded: monthly traffic allowance exhausted; purchase a traffic package or wait for the next month",
            Self::AccountRequired => "account-backed relay required: run ssmcp client login or ask the administrator to configure portal.owner_mappings for a registered GitHub account",
            Self::Storage => "traffic quota storage unavailable; message was not forwarded",
        })
    }
}
impl std::error::Error for TrafficError {}

pub(crate) fn traffic_recharge_url(public_url: &str) -> String {
    format!("{}/account/traffic", public_url.trim_end_matches('/'))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TrafficOutcome {
    NotDispatched,
    ResultNotDelivered,
    Unknown,
}

impl TrafficError {
    pub(crate) fn notice(self, public_url: &str, outcome: TrafficOutcome) -> String {
        let outcome = match outcome {
            TrafficOutcome::NotDispatched => "请求未发送到客户端，未执行。",
            TrafficOutcome::ResultNotDelivered => {
                "客户端结果未能送达；操作可能已经执行，请先检查实际状态，勿直接重复执行。"
            }
            TrafficOutcome::Unknown => {
                "连接已中断；已发送的请求可能已经执行，重试前请先检查实际状态。"
            }
        };
        if self == Self::Exhausted {
            format!(
                "traffic_quota_exceeded: 本月剩余流量不足。{outcome}充值后请重新连接客户端，或等待下月额度恢复。流量与充值：{}",
                traffic_recharge_url(public_url)
            )
        } else {
            format!("{self}; {outcome}")
        }
    }
}

/// A trusted transport failure, never a frame supplied by a remote client.
/// Call and reply IDs distinguish an undispatched request from a lost result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TrafficFailure {
    pub(crate) error: TrafficError,
    blocked_call: Option<u64>,
    blocked_reply: Option<u64>,
}

impl TrafficFailure {
    pub(crate) fn new(error: TrafficError) -> Self {
        Self {
            error,
            blocked_call: None,
            blocked_reply: None,
        }
    }

    pub(crate) fn rejected_call(error: TrafficError, id: u64) -> Self {
        Self {
            error,
            blocked_call: Some(id),
            blocked_reply: None,
        }
    }

    pub(crate) fn rejected_reply(error: TrafficError, id: u64) -> Self {
        Self {
            error,
            blocked_call: None,
            blocked_reply: Some(id),
        }
    }

    pub(crate) fn notice(self, public_url: &str) -> String {
        let outcome = if self.blocked_call.is_some() {
            TrafficOutcome::NotDispatched
        } else if self.blocked_reply.is_some() {
            TrafficOutcome::ResultNotDelivered
        } else {
            TrafficOutcome::Unknown
        };
        self.error.notice(public_url, outcome)
    }

    pub(crate) fn call_notice(self, public_url: &str, id: u64) -> String {
        // The broker assigns increasing IDs and writes calls in order. Once
        // an outbound call is refused, no later buffered call can be delivered.
        let outcome = if self
            .blocked_call
            .is_some_and(|first_unsent| id >= first_unsent)
        {
            TrafficOutcome::NotDispatched
        } else if self.blocked_reply == Some(id) {
            TrafficOutcome::ResultNotDelivered
        } else {
            TrafficOutcome::Unknown
        };
        self.error.notice(public_url, outcome)
    }
}

impl std::fmt::Display for TrafficFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}
impl std::error::Error for TrafficFailure {}

/// Set before either duplex half is dropped, so EOF cannot erase the reason.
/// This state is shared only by the server's WebSocket pumps and broker.
#[derive(Clone, Default)]
pub(crate) struct TrafficFailureState(Arc<Mutex<Option<TrafficFailure>>>);

impl TrafficFailureState {
    pub(crate) fn record(&self, failure: TrafficFailure) -> TrafficFailure {
        *self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert(failure)
    }

    pub(crate) fn get(&self) -> Option<TrafficFailure> {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

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
    pub(crate) fn apply_account(
        &self,
        subject: &str,
        banned: bool,
        rate: Option<u64>,
        has_recharged: bool,
    ) {
        let default = if has_recharged {
            self.default_rate().max(RECHARGED_RATE_LIMIT_BPS)
        } else {
            self.default_rate()
        };
        self.get(subject).configure(banned, rate.unwrap_or(default));
    }
    #[cfg(test)]
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
    fn recharge_updates_the_existing_bucket_and_preserves_explicit_rates() {
        let policies = UserPolicies::default();
        policies.set_default(16 * 1024 * 1024).unwrap();
        let active = policies.get("123");
        policies.apply_account("123", false, None, true);
        assert_eq!(
            active.settings.read().unwrap().rate,
            RECHARGED_RATE_LIMIT_BPS
        );
        policies.apply_account("123", true, Some(1024), true);
        assert_eq!(active.settings.read().unwrap().rate, 1024);
        assert!(active.banned());
        policies.apply_account("123", false, None, true);
        assert_eq!(
            active.settings.read().unwrap().rate,
            RECHARGED_RATE_LIMIT_BPS
        );
        assert!(!active.banned());
        assert_eq!(
            policies.get("456").settings.read().unwrap().rate,
            policies.default_rate()
        );
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
    #[test]
    fn traffic_months_observe_server_offset_leap_days_and_year_boundary() {
        assert_eq!(traffic_period_at(1_796_054_399, 480), "2026-11");
        assert_eq!(traffic_period_at(1_796_054_400, 480), "2026-12");
        assert_eq!(traffic_period_at(1_796_054_400, 0), "2026-11");
        assert_eq!(traffic_period_at(1_798_732_800, 480), "2027-01");
        assert_eq!(traffic_period_at(1_835_452_799, 480), "2028-02");
        assert_eq!(traffic_period_at(1_835_452_800, 480), "2028-03");
        assert_eq!(traffic_period_at(0, -720), "1969-12");
        assert_eq!(traffic_period_at(0, 840), "1970-01");
    }

    #[test]
    fn traffic_period_validation_is_bounded_and_handles_non_ascii() {
        for period in ["2026-11", "2000-02", "9999-12"] {
            assert!(valid_traffic_period(period));
        }
        for period in [
            "",
            "2026-00",
            "2026-13",
            "0000-01",
            "２０26-1",
            "abcd-11",
            "2026-1",
            "2026-111",
        ] {
            assert!(!valid_traffic_period(period), "{period}");
        }
    }
}
