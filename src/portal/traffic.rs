//! Durable monthly quota journal. The account Store's writer lease and mutex
//! serialize reservations and paid grants across every connection.
//!
//! Each reservation appends one small record and syncs it before forwarding.
//! Periodic compaction replaces only this journal, never the account database.
//! A reservation may remain charged if forwarding fails after it is committed.
use super::{Problem, store::private_file};
use crate::user_policy::{FREE_MONTHLY_TRAFFIC_BYTES, TrafficQuota, valid_traffic_period};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::PathBuf,
};

const COMPACT_AFTER: u64 = 4 * 1024 * 1024;
const MAX_SNAPSHOT: usize = 64 * 1024 * 1024;
const MAX_JOURNAL: u64 = MAX_SNAPSHOT as u64 + COMPACT_AFTER + 4096;
const MAX_ORDERS: usize = 200_000;
const MAX_ACCOUNT_PERIODS: usize = 500_000;
const FILE_NAME: &str = "traffic.jsonl";

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Usage {
    used_bytes: u64,
    paid_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    adjusted_limit_bytes: Option<u64>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Grant {
    subject: String,
    period: String,
    bytes: u64,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LedgerState {
    periods: BTreeMap<String, BTreeMap<String, Usage>>,
    orders: BTreeMap<String, Grant>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Record {
    Snapshot {
        version: u8,
        state: LedgerState,
    },
    Used {
        subject: String,
        period: String,
        used_bytes: u64,
    },
    Adjusted {
        subject: String,
        period: String,
        limit_bytes: u64,
    },
    Paid {
        order_id: String,
        grant: Grant,
    },
}

fn valid_order_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && id.bytes().all(|b| b.is_ascii_graphic())
}

impl LedgerState {
    fn usage(&self, subject: &str, period: &str) -> Usage {
        self.periods
            .get(period)
            .and_then(|p| p.get(subject))
            .cloned()
            .unwrap_or_default()
    }

    fn validate(&self) -> Result<(), Problem> {
        if self.orders.len() > MAX_ORDERS
            || self.periods.values().map(BTreeMap::len).sum::<usize>() > MAX_ACCOUNT_PERIODS
        {
            return Err(Problem::Storage);
        }
        let mut paid: BTreeMap<(String, String), u64> = BTreeMap::new();
        for (id, grant) in &self.orders {
            if !valid_order_id(id)
                || !crate::management::valid_subject(&grant.subject)
                || !valid_traffic_period(&grant.period)
                || grant.bytes == 0
            {
                return Err(Problem::Storage);
            }
            let total = paid
                .entry((grant.period.clone(), grant.subject.clone()))
                .or_default();
            *total = total.checked_add(grant.bytes).ok_or(Problem::Storage)?;
        }
        for (period, accounts) in &self.periods {
            if !valid_traffic_period(period) {
                return Err(Problem::Storage);
            }
            for (subject, usage) in accounts {
                if !crate::management::valid_subject(subject)
                    || usage
                        .paid_bytes
                        .checked_add(FREE_MONTHLY_TRAFFIC_BYTES)
                        .is_none()
                    || usage.paid_bytes
                        != paid
                            .remove(&(period.clone(), subject.clone()))
                            .unwrap_or_default()
                {
                    return Err(Problem::Storage);
                }
            }
        }
        if !paid.is_empty() {
            return Err(Problem::Storage);
        }
        Ok(())
    }

    fn apply(&mut self, record: Record) -> Result<(), Problem> {
        match record {
            Record::Snapshot { .. } => return Err(Problem::Storage),
            Record::Used {
                subject,
                period,
                used_bytes,
            } => {
                if !crate::management::valid_subject(&subject) || !valid_traffic_period(&period) {
                    return Err(Problem::Storage);
                }
                let usage = self
                    .periods
                    .entry(period)
                    .or_default()
                    .entry(subject)
                    .or_default();
                if used_bytes < usage.used_bytes {
                    return Err(Problem::Storage);
                }
                usage.used_bytes = used_bytes;
            }
            Record::Adjusted {
                subject,
                period,
                limit_bytes,
            } => {
                if !crate::management::valid_subject(&subject) || !valid_traffic_period(&period) {
                    return Err(Problem::Storage);
                }
                self.periods
                    .entry(period)
                    .or_default()
                    .entry(subject)
                    .or_default()
                    .adjusted_limit_bytes = Some(limit_bytes);
            }
            Record::Paid { order_id, grant } => {
                if !valid_order_id(&order_id)
                    || !crate::management::valid_subject(&grant.subject)
                    || !valid_traffic_period(&grant.period)
                    || grant.bytes == 0
                    || self.orders.contains_key(&order_id)
                {
                    return Err(Problem::Storage);
                }
                let usage = self
                    .periods
                    .entry(grant.period.clone())
                    .or_default()
                    .entry(grant.subject.clone())
                    .or_default();
                let total = usage
                    .paid_bytes
                    .checked_add(grant.bytes)
                    .ok_or(Problem::Storage)?;
                total
                    .checked_add(FREE_MONTHLY_TRAFFIC_BYTES)
                    .ok_or(Problem::Storage)?;
                if let Some(limit) = &mut usage.adjusted_limit_bytes {
                    *limit = limit.checked_add(grant.bytes).ok_or(Problem::Storage)?;
                }
                usage.paid_bytes = total;
                self.orders.insert(order_id, grant);
            }
        }
        Ok(())
    }
}

pub(super) struct TrafficLedger {
    file: File,
    state: LedgerState,
    appended: u64,
    failed: bool,
}

impl TrafficLedger {
    pub(super) fn open(directory: &File) -> crate::Result<Self> {
        let dir = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(dir.join(FILE_NAME))?;
        private_file(&file)?;
        if file.metadata()?.len() > MAX_JOURNAL {
            return Err("traffic journal exceeds its bounded storage limit".into());
        }
        let mut bytes = Vec::new();
        (&mut file).take(MAX_JOURNAL + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_JOURNAL {
            return Err("traffic journal exceeds its bounded storage limit".into());
        }
        let complete = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        if !bytes.is_empty() && complete == 0 {
            return Err("incomplete traffic snapshot; refusing to reset quota".into());
        }
        if complete != bytes.len() {
            // An incomplete final append was never acknowledged for forwarding.
            // Complete but malformed records fail closed instead of resetting quota.
            file.set_len(complete as u64)?;
            file.sync_all()?;
            bytes.truncate(complete);
        }
        let mut state = LedgerState::default();
        let mut appended = 0;
        for (index, line) in bytes.split_inclusive(|b| *b == b'\n').enumerate() {
            let line = &line[..line.len() - 1];
            if line.is_empty()
                || (index == 0 && line.len() > MAX_SNAPSHOT)
                || (index > 0 && line.len() > 4096)
            {
                return Err("invalid traffic journal record size".into());
            }
            let record = serde_json::from_slice::<Record>(line)
                .map_err(|_| "invalid traffic journal; refusing to reset quota")?;
            if index == 0 {
                match record {
                    Record::Snapshot {
                        version: 1,
                        state: snapshot,
                    } => state = snapshot,
                    _ => return Err("traffic journal is missing its initial snapshot".into()),
                }
                state
                    .validate()
                    .map_err(|_| "invalid traffic quota snapshot")?;
            } else {
                appended += line.len() as u64 + 1;
                state
                    .apply(record)
                    .map_err(|_| "invalid traffic quota reservation")?;
            }
        }
        state
            .validate()
            .map_err(|_| "invalid traffic quota ledger")?;
        if bytes.is_empty() {
            let mut initial = serde_json::to_vec(&Record::Snapshot {
                version: 1,
                state: LedgerState::default(),
            })?;
            initial.push(b'\n');
            file.write_all(&initial)?;
            file.sync_all()?;
            directory.sync_all()?;
        }
        Ok(Self {
            file,
            state,
            appended,
            failed: false,
        })
    }

    pub(super) fn snapshot(
        &self,
        subject: &str,
        period: &str,
        utc_offset_minutes: i32,
        unlimited: bool,
    ) -> Result<TrafficQuota, Problem> {
        if self.failed {
            return Err(Problem::Storage);
        }
        if !crate::management::valid_subject(subject) || !valid_traffic_period(period) {
            return Err(Problem::Invalid);
        }
        let usage = self.state.usage(subject, period);
        let mut quota = TrafficQuota::new(
            period.to_owned(),
            utc_offset_minutes,
            usage.used_bytes,
            usage.paid_bytes,
            unlimited,
        );
        if let Some(limit) = usage.adjusted_limit_bytes.filter(|_| !unlimited) {
            quota.limit_bytes = Some(limit);
            quota.remaining_bytes = Some(limit.saturating_sub(usage.used_bytes));
        }
        Ok(quota)
    }

    pub(super) fn reserve(
        &mut self,
        directory: &File,
        subject: &str,
        period: &str,
        utc_offset_minutes: i32,
        bytes: u64,
        unlimited: bool,
    ) -> Result<(), Problem> {
        let quota = self.snapshot(subject, period, utc_offset_minutes, unlimited)?;
        if quota
            .remaining_bytes
            .is_some_and(|remaining| bytes > remaining)
        {
            return Err(Problem::TrafficQuotaExceeded);
        }
        if bytes == 0 {
            return Ok(());
        }
        let used_bytes = quota.used_bytes.checked_add(bytes).ok_or(Problem::Limit)?;
        self.require_period_capacity(subject, period)?;
        self.append(
            directory,
            Record::Used {
                subject: subject.into(),
                period: period.into(),
                used_bytes,
            },
        )
    }

    pub(super) fn set_remaining(
        &mut self,
        directory: &File,
        subject: &str,
        period: &str,
        utc_offset_minutes: i32,
        remaining_bytes: u64,
    ) -> Result<(), Problem> {
        let quota = self.snapshot(subject, period, utc_offset_minutes, false)?;
        let limit_bytes = quota
            .used_bytes
            .checked_add(remaining_bytes)
            .ok_or(Problem::Invalid)?;
        if quota.limit_bytes == Some(limit_bytes) {
            return Ok(());
        }
        self.require_period_capacity(subject, period)?;
        self.append(
            directory,
            Record::Adjusted {
                subject: subject.into(),
                period: period.into(),
                limit_bytes,
            },
        )
    }

    fn require_period_capacity(&self, subject: &str, period: &str) -> Result<(), Problem> {
        if !self
            .state
            .periods
            .get(period)
            .is_some_and(|p| p.contains_key(subject))
            && self
                .state
                .periods
                .values()
                .map(BTreeMap::len)
                .sum::<usize>()
                >= MAX_ACCOUNT_PERIODS
        {
            return Err(Problem::Limit);
        }
        Ok(())
    }

    /// Returns false only for an identical, already durable grant. Late first-time
    /// payments are rejected; they must be resolved by commerce, never shifted to
    /// another month. Deduplication survives monthly rollover and compaction.
    pub(super) fn grant(
        &mut self,
        directory: &File,
        subject: &str,
        period: &str,
        current_period: &str,
        order_id: &str,
        bytes: u64,
    ) -> Result<bool, Problem> {
        if self.failed {
            return Err(Problem::Storage);
        }
        if !crate::management::valid_subject(subject)
            || !valid_traffic_period(period)
            || !valid_order_id(order_id)
            || bytes == 0
        {
            return Err(Problem::Invalid);
        }
        let grant = Grant {
            subject: subject.into(),
            period: period.into(),
            bytes,
        };
        if let Some(existing) = self.state.orders.get(order_id) {
            return if existing == &grant {
                Ok(false)
            } else {
                Err(Problem::TrafficGrantConflict)
            };
        }
        if period != current_period {
            return Err(Problem::TrafficPeriodMismatch);
        }
        if self.state.orders.len() >= MAX_ORDERS {
            return Err(Problem::Limit);
        }
        self.require_period_capacity(subject, period)?;
        let usage = self.state.usage(subject, period);
        usage
            .paid_bytes
            .checked_add(bytes)
            .and_then(|total| total.checked_add(FREE_MONTHLY_TRAFFIC_BYTES))
            .ok_or(Problem::Limit)?;
        if let Some(limit) = usage.adjusted_limit_bytes {
            limit.checked_add(bytes).ok_or(Problem::Limit)?;
        }
        self.append(
            directory,
            Record::Paid {
                order_id: order_id.into(),
                grant,
            },
        )?;
        Ok(true)
    }

    fn append(&mut self, directory: &File, record: Record) -> Result<(), Problem> {
        if self.failed {
            return Err(Problem::Storage);
        }
        if self.appended >= COMPACT_AFTER {
            self.compact(directory)?;
        }
        let mut bytes = serde_json::to_vec(&record).map_err(|_| Problem::Storage)?;
        bytes.push(b'\n');
        if self
            .file
            .write_all(&bytes)
            .and_then(|_| self.file.sync_data())
            .is_err()
        {
            self.failed = true;
            return Err(Problem::Storage);
        }
        self.appended += bytes.len() as u64;
        if self.state.apply(record).is_err() {
            self.failed = true;
            return Err(Problem::Storage);
        }
        Ok(())
    }

    fn compact(&mut self, directory: &File) -> Result<(), Problem> {
        #[derive(Serialize)]
        struct Snapshot<'a> {
            r#type: &'static str,
            version: u8,
            state: &'a LedgerState,
        }
        let mut bytes = serde_json::to_vec(&Snapshot {
            r#type: "snapshot",
            version: 1,
            state: &self.state,
        })
        .map_err(|_| Problem::Storage)?;
        if bytes.len() > MAX_SNAPSHOT {
            return Err(Problem::Limit);
        }
        bytes.push(b'\n');
        let dir = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        let temporary = dir.join(format!(".traffic-{}.tmp", super::random()?));
        let result = (|| -> std::io::Result<File> {
            let mut file = OpenOptions::new()
                .read(true)
                .append(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, dir.join(FILE_NAME))?;
            directory.sync_all()?;
            Ok(file)
        })();
        match result {
            Ok(file) => {
                self.file = file;
                self.appended = 0;
                Ok(())
            }
            Err(_) => {
                let _ = fs::remove_file(&temporary);
                self.failed = true;
                Err(Problem::Storage)
            }
        }
    }
}

#[cfg(test)]
mod tests;
