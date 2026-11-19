use super::*;
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt};

const SUBJECT: &str = "123";
const PERIOD: &str = "2026-11";
const NEXT_PERIOD: &str = "2026-12";
const OFFSET: i32 = 480;

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "ssmcp-traffic-test-{}",
            super::super::random().unwrap()
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }

    fn directory(&self) -> File {
        File::open(&self.0).unwrap()
    }

    fn journal(&self) -> PathBuf {
        self.0.join(FILE_NAME)
    }

    fn append(&self, bytes: &[u8]) {
        let mut file = OpenOptions::new()
            .append(true)
            .open(self.journal())
            .unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }

    fn create_journal(&self, bytes: &[u8]) {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.journal())
            .unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn line(record: &Record) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(record).unwrap();
    bytes.push(b'\n');
    bytes
}

fn quota(ledger: &TrafficLedger, period: &str) -> TrafficQuota {
    ledger.snapshot(SUBJECT, period, OFFSET, false).unwrap()
}

#[test]
fn setting_remaining_preserves_usage_and_purchases_through_restart_and_rollover() {
    let temp = Temp::new();
    let directory = temp.directory();
    let mut ledger = TrafficLedger::open(&directory).unwrap();
    ledger
        .reserve(&directory, SUBJECT, PERIOD, OFFSET, 50, false)
        .unwrap();
    assert!(
        ledger
            .grant(&directory, SUBJECT, PERIOD, PERIOD, "first", 100)
            .unwrap()
    );
    ledger
        .set_remaining(&directory, SUBJECT, PERIOD, OFFSET, 0)
        .unwrap();
    assert_eq!(
        ledger.reserve(&directory, SUBJECT, PERIOD, OFFSET, 1, false),
        Err(Problem::TrafficQuotaExceeded)
    );
    drop(ledger);
    let mut ledger = TrafficLedger::open(&directory).unwrap();
    assert_eq!(quota(&ledger, PERIOD).remaining_bytes, Some(0));
    ledger
        .set_remaining(&directory, SUBJECT, PERIOD, OFFSET, 500)
        .unwrap();
    assert!(
        !ledger
            .grant(&directory, SUBJECT, PERIOD, PERIOD, "first", 100)
            .unwrap()
    );
    assert!(
        ledger
            .grant(&directory, SUBJECT, PERIOD, PERIOD, "second", 200)
            .unwrap()
    );
    ledger.compact(&directory).unwrap();
    drop(ledger);
    let ledger = TrafficLedger::open(&directory).unwrap();
    let adjusted = quota(&ledger, PERIOD);
    assert_eq!(adjusted.remaining_bytes, Some(700));
    assert_eq!(adjusted.used_bytes, 50);
    assert_eq!(adjusted.paid_bytes, 300);
    assert_eq!(
        quota(&ledger, NEXT_PERIOD).remaining_bytes,
        Some(FREE_MONTHLY_TRAFFIC_BYTES)
    );
}

#[test]
fn exact_free_quota_boundary_rejects_without_charging_and_survives_reopen() {
    let temp = Temp::new();
    let directory = temp.directory();
    let mut ledger = TrafficLedger::open(&directory).unwrap();
    assert_eq!(quota(&ledger, PERIOD).remaining_bytes, Some(1_000_000_000));
    ledger
        .reserve(&directory, SUBJECT, PERIOD, OFFSET, 999_999_999, false)
        .unwrap();
    ledger
        .reserve(&directory, SUBJECT, PERIOD, OFFSET, 1, false)
        .unwrap();
    let exhausted = quota(&ledger, PERIOD);
    assert_eq!(exhausted.used_bytes, 1_000_000_000);
    assert_eq!(exhausted.remaining_bytes, Some(0));
    let committed_bytes = fs::read(temp.journal()).unwrap();
    assert_eq!(
        ledger.reserve(&directory, SUBJECT, PERIOD, OFFSET, 1, false),
        Err(Problem::TrafficQuotaExceeded)
    );
    ledger
        .reserve(&directory, SUBJECT, PERIOD, OFFSET, 0, false)
        .unwrap();
    assert_eq!(quota(&ledger, PERIOD), exhausted);
    assert_eq!(fs::read(temp.journal()).unwrap(), committed_bytes);
    drop(ledger);
    let reopened = TrafficLedger::open(&directory).unwrap();
    assert_eq!(quota(&reopened, PERIOD), exhausted);
    assert_eq!(
        reopened
            .snapshot("456", PERIOD, OFFSET, false)
            .unwrap()
            .remaining_bytes,
        Some(1_000_000_000)
    );
}

#[test]
fn monthly_rollover_resets_free_quota_and_does_not_roll_over_paid_traffic() {
    let temp = Temp::new();
    let directory = temp.directory();
    let mut ledger = TrafficLedger::open(&directory).unwrap();
    assert_eq!(
        ledger.grant(
            &directory,
            SUBJECT,
            PERIOD,
            PERIOD,
            "paid-november",
            500_000_000
        ),
        Ok(true)
    );
    ledger
        .reserve(&directory, SUBJECT, PERIOD, OFFSET, 1_250_000_000, false)
        .unwrap();
    let november = quota(&ledger, PERIOD);
    assert_eq!(november.paid_bytes, 500_000_000);
    assert_eq!(november.remaining_bytes, Some(250_000_000));
    let december = quota(&ledger, NEXT_PERIOD);
    assert_eq!(december.used_bytes, 0);
    assert_eq!(december.free_bytes, 1_000_000_000);
    assert_eq!(december.paid_bytes, 0);
    assert_eq!(december.limit_bytes, Some(1_000_000_000));
    ledger
        .reserve(&directory, SUBJECT, NEXT_PERIOD, OFFSET, 1, false)
        .unwrap();
    drop(ledger);
    let reopened = TrafficLedger::open(&directory).unwrap();
    assert_eq!(quota(&reopened, PERIOD), november);
    assert_eq!(
        quota(&reopened, NEXT_PERIOD).remaining_bytes,
        Some(999_999_999)
    );
}

#[test]
fn paid_grants_remain_idempotent_across_compaction_reopen_and_month_boundary() {
    let temp = Temp::new();
    let directory = temp.directory();
    let mut ledger = TrafficLedger::open(&directory).unwrap();
    assert_eq!(
        ledger.grant(&directory, SUBJECT, PERIOD, PERIOD, "order-1", 42),
        Ok(true)
    );
    assert_eq!(
        ledger.grant(&directory, SUBJECT, PERIOD, PERIOD, "order-1", 42),
        Ok(false)
    );
    ledger
        .reserve(&directory, SUBJECT, PERIOD, OFFSET, 50, false)
        .unwrap();
    ledger.compact(&directory).unwrap();
    drop(ledger);
    let mut ledger = TrafficLedger::open(&directory).unwrap();
    let committed = fs::read(temp.journal()).unwrap();
    // Re-delivery of an already credited order is harmless even after expiry.
    assert_eq!(
        ledger.grant(&directory, SUBJECT, PERIOD, NEXT_PERIOD, "order-1", 42),
        Ok(false)
    );
    for (subject, period, bytes) in [
        ("456", PERIOD, 42),
        (SUBJECT, NEXT_PERIOD, 42),
        (SUBJECT, PERIOD, 43),
    ] {
        assert_eq!(
            ledger.grant(&directory, subject, period, NEXT_PERIOD, "order-1", bytes),
            Err(Problem::TrafficGrantConflict)
        );
    }
    assert_eq!(fs::read(temp.journal()).unwrap(), committed);
    assert_eq!(quota(&ledger, PERIOD).paid_bytes, 42);
    assert_eq!(quota(&ledger, PERIOD).used_bytes, 50);
    assert_eq!(quota(&ledger, NEXT_PERIOD).paid_bytes, 0);
    assert_eq!(ledger.state.orders.len(), 1);
}

#[test]
fn uncredited_orders_for_expired_or_future_months_are_rejected_without_mutation() {
    let temp = Temp::new();
    let directory = temp.directory();
    let mut ledger = TrafficLedger::open(&directory).unwrap();
    let committed = fs::read(temp.journal()).unwrap();
    for period in ["2026-10", NEXT_PERIOD] {
        assert_eq!(
            ledger.grant(&directory, SUBJECT, period, PERIOD, "uncredited", 500),
            Err(Problem::TrafficPeriodMismatch)
        );
        assert_eq!(quota(&ledger, period).paid_bytes, 0);
    }
    assert!(ledger.state.orders.is_empty());
    assert_eq!(fs::read(temp.journal()).unwrap(), committed);
}

#[test]
fn incomplete_unacknowledged_tail_is_discarded_without_losing_synced_quota() {
    for tail in [
        br#"{"type":"used","subject":"123","period":"2026-11","used_bytes":"#.as_slice(),
        br#"{"type":"used","subject":"123","period":"2026-11","used_bytes":999}"#.as_slice(),
    ] {
        let temp = Temp::new();
        let directory = temp.directory();
        let mut ledger = TrafficLedger::open(&directory).unwrap();
        ledger
            .grant(&directory, SUBJECT, PERIOD, PERIOD, "credited", 42)
            .unwrap();
        ledger
            .reserve(&directory, SUBJECT, PERIOD, OFFSET, 123, false)
            .unwrap();
        let expected = quota(&ledger, PERIOD);
        let committed = fs::read(temp.journal()).unwrap();
        drop(ledger);
        temp.append(tail);
        let reopened = TrafficLedger::open(&directory).unwrap();
        assert_eq!(quota(&reopened, PERIOD), expected);
        assert_eq!(fs::read(temp.journal()).unwrap(), committed);
        assert!(reopened.state.orders.contains_key("credited"));
    }
}

#[test]
fn genuinely_empty_file_initializes_but_partial_initial_snapshot_fails_closed() {
    let empty = Temp::new();
    empty.create_journal(b"");
    let ledger = TrafficLedger::open(&empty.directory()).unwrap();
    assert_eq!(quota(&ledger, PERIOD).remaining_bytes, Some(1_000_000_000));
    assert!(fs::read(empty.journal()).unwrap().ends_with(b"\n"));
    for bytes in [
        br#"{"type":"snapshot","version":1,"state":"#.as_slice(),
        br#"{"type":"snapshot","version":1,"state":{"periods":{},"orders":{}}}"#.as_slice(),
        b"\n".as_slice(),
        b"\n\n".as_slice(),
    ] {
        let temp = Temp::new();
        temp.create_journal(bytes);
        assert!(TrafficLedger::open(&temp.directory()).is_err());
    }
}

#[test]
fn complete_malformed_or_regressive_records_fail_closed() {
    let invalid_records = [
        b"{not json}\n".to_vec(),
        b"\n".to_vec(),
        line(&Record::Used {
            subject: SUBJECT.into(),
            period: PERIOD.into(),
            used_bytes: 6,
        }),
        line(&Record::Used {
            subject: "0123".into(),
            period: PERIOD.into(),
            used_bytes: 8,
        }),
        line(&Record::Used {
            subject: SUBJECT.into(),
            period: "2026-13".into(),
            used_bytes: 8,
        }),
        line(&Record::Paid {
            order_id: "credited".into(),
            grant: Grant {
                subject: SUBJECT.into(),
                period: PERIOD.into(),
                bytes: 42,
            },
        }),
        line(&Record::Snapshot {
            version: 1,
            state: LedgerState::default(),
        }),
    ];
    for tail in invalid_records {
        let temp = Temp::new();
        let directory = temp.directory();
        let mut ledger = TrafficLedger::open(&directory).unwrap();
        ledger
            .grant(&directory, SUBJECT, PERIOD, PERIOD, "credited", 42)
            .unwrap();
        ledger
            .reserve(&directory, SUBJECT, PERIOD, OFFSET, 7, false)
            .unwrap();
        drop(ledger);
        temp.append(&tail);
        let damaged = fs::read(temp.journal()).unwrap();
        assert!(TrafficLedger::open(&directory).is_err());
        assert_eq!(fs::read(temp.journal()).unwrap(), damaged);
    }
}

#[test]
fn snapshots_with_unbacked_paid_balance_or_unknown_version_fail_closed() {
    let mut unbacked = LedgerState::default();
    unbacked.periods.entry(PERIOD.into()).or_default().insert(
        SUBJECT.into(),
        Usage {
            used_bytes: 0,
            paid_bytes: 100,
            adjusted_limit_bytes: None,
        },
    );
    for record in [
        Record::Snapshot {
            version: 1,
            state: unbacked,
        },
        Record::Snapshot {
            version: 2,
            state: LedgerState::default(),
        },
    ] {
        let temp = Temp::new();
        temp.create_journal(&line(&record));
        assert!(TrafficLedger::open(&temp.directory()).is_err());
    }
}

#[test]
fn compaction_replaces_the_file_and_replays_snapshot_plus_next_reservation() {
    let temp = Temp::new();
    let directory = temp.directory();
    let mut ledger = TrafficLedger::open(&directory).unwrap();
    ledger
        .grant(&directory, SUBJECT, PERIOD, PERIOD, "order-compact", 500)
        .unwrap();
    ledger
        .reserve(&directory, SUBJECT, PERIOD, OFFSET, 200, false)
        .unwrap();
    ledger
        .reserve(&directory, "456", PERIOD, OFFSET, 300, false)
        .unwrap();
    let old_file = ledger.file.try_clone().unwrap();
    let old_inode = old_file.metadata().unwrap().ino();
    let old_bytes = fs::read(temp.journal()).unwrap();
    // Exercise the automatic compaction branch without writing 4 MiB of fixtures.
    ledger.appended = COMPACT_AFTER;
    ledger
        .reserve(&directory, SUBJECT, PERIOD, OFFSET, 25, false)
        .unwrap();
    assert_ne!(fs::metadata(temp.journal()).unwrap().ino(), old_inode);
    assert_eq!(
        fs::metadata(temp.journal()).unwrap().ino(),
        ledger.file.metadata().unwrap().ino()
    );
    let mut original = vec![0; old_bytes.len()];
    old_file.read_exact_at(&mut original, 0).unwrap();
    assert_eq!(original, old_bytes);
    let compacted = fs::read(temp.journal()).unwrap();
    let records: Vec<_> = compacted
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .collect();
    assert_eq!(records.len(), 2);
    assert!(matches!(
        serde_json::from_slice::<Record>(records[0]).unwrap(),
        Record::Snapshot { version: 1, .. }
    ));
    assert!(ledger.appended < COMPACT_AFTER);
    assert_eq!(fs::read_dir(&temp.0).unwrap().count(), 1);
    let expected = serde_json::to_value(&ledger.state).unwrap();
    drop(ledger);
    let reopened = TrafficLedger::open(&directory).unwrap();
    assert_eq!(serde_json::to_value(&reopened.state).unwrap(), expected);
    assert_eq!(quota(&reopened, PERIOD).used_bytes, 225);
    assert_eq!(quota(&reopened, PERIOD).paid_bytes, 500);
    assert_eq!(
        reopened
            .snapshot("456", PERIOD, OFFSET, false)
            .unwrap()
            .used_bytes,
        300
    );
}

#[test]
fn failed_reservation_does_not_charge_and_poisons_all_further_access() {
    let temp = Temp::new();
    let directory = temp.directory();
    let mut ledger = TrafficLedger::open(&directory).unwrap();
    ledger
        .grant(&directory, SUBJECT, PERIOD, PERIOD, "credited", 42)
        .unwrap();
    ledger
        .reserve(&directory, SUBJECT, PERIOD, OFFSET, 10, false)
        .unwrap();
    let expected = quota(&ledger, PERIOD);
    let committed = fs::read(temp.journal()).unwrap();
    ledger.file = File::open(temp.journal()).unwrap();
    assert_eq!(
        ledger.reserve(&directory, SUBJECT, PERIOD, OFFSET, 1, false),
        Err(Problem::Storage)
    );
    assert!(ledger.failed);
    assert_eq!(ledger.state.usage(SUBJECT, PERIOD).used_bytes, 10);
    assert_eq!(
        ledger.snapshot(SUBJECT, PERIOD, OFFSET, false),
        Err(Problem::Storage)
    );
    assert_eq!(
        ledger.grant(&directory, SUBJECT, PERIOD, PERIOD, "new-order", 100),
        Err(Problem::Storage)
    );
    // A healthy descriptor does not silently clear an ambiguous failed writer.
    ledger.file = OpenOptions::new()
        .read(true)
        .append(true)
        .open(temp.journal())
        .unwrap();
    assert_eq!(
        ledger.reserve(&directory, SUBJECT, PERIOD, OFFSET, 0, false),
        Err(Problem::Storage)
    );
    assert_eq!(fs::read(temp.journal()).unwrap(), committed);
    drop(ledger);
    let reopened = TrafficLedger::open(&directory).unwrap();
    assert_eq!(quota(&reopened, PERIOD), expected);
    assert!(!reopened.state.orders.contains_key("new-order"));
}

#[test]
fn failed_paid_append_neither_credits_balance_nor_remembers_the_order() {
    let temp = Temp::new();
    let directory = temp.directory();
    let mut ledger = TrafficLedger::open(&directory).unwrap();
    ledger.file = File::open(temp.journal()).unwrap();
    assert_eq!(
        ledger.grant(&directory, SUBJECT, PERIOD, PERIOD, "failed-order", 500),
        Err(Problem::Storage)
    );
    assert_eq!(ledger.state.usage(SUBJECT, PERIOD).paid_bytes, 0);
    assert!(!ledger.state.orders.contains_key("failed-order"));
    assert!(ledger.failed);
    drop(ledger);
    let mut reopened = TrafficLedger::open(&directory).unwrap();
    assert_eq!(quota(&reopened, PERIOD).paid_bytes, 0);
    assert_eq!(
        reopened.grant(&directory, SUBJECT, PERIOD, PERIOD, "failed-order", 500),
        Ok(true)
    );
}

#[test]
fn unlimited_quota_tracks_more_than_one_gigabyte_durably_without_limit() {
    let temp = Temp::new();
    let directory = temp.directory();
    let mut ledger = TrafficLedger::open(&directory).unwrap();
    ledger
        .reserve(&directory, SUBJECT, PERIOD, OFFSET, 1_000_000_001, true)
        .unwrap();
    let unlimited = ledger.snapshot(SUBJECT, PERIOD, OFFSET, true).unwrap();
    assert_eq!(unlimited.used_bytes, 1_000_000_001);
    assert_eq!(unlimited.limit_bytes, None);
    assert_eq!(unlimited.remaining_bytes, None);
    drop(ledger);
    let reopened = TrafficLedger::open(&directory).unwrap();
    assert_eq!(
        reopened.snapshot(SUBJECT, PERIOD, OFFSET, true).unwrap(),
        unlimited
    );
    assert_eq!(quota(&reopened, PERIOD).remaining_bytes, Some(0));
}

#[test]
fn paid_grant_cannot_exceed_account_period_capacity() {
    let temp = Temp::new();
    let directory = temp.directory();
    let mut ledger = TrafficLedger::open(&directory).unwrap();
    ledger.state.periods.insert(
        PERIOD.into(),
        (1..=MAX_ACCOUNT_PERIODS)
            .map(|id| (id.to_string(), Usage::default()))
            .collect(),
    );
    let committed = fs::read(temp.journal()).unwrap();
    let new_subject = (MAX_ACCOUNT_PERIODS + 1).to_string();
    assert_eq!(
        ledger.grant(&directory, &new_subject, PERIOD, PERIOD, "over-capacity", 1),
        Err(Problem::Limit)
    );
    assert_eq!(fs::read(temp.journal()).unwrap(), committed);
    assert!(ledger.state.orders.is_empty());
    assert!(!ledger.state.periods[PERIOD].contains_key(&new_subject));
}

#[test]
fn invalid_or_overflowing_grants_leave_the_journal_unchanged() {
    let temp = Temp::new();
    let directory = temp.directory();
    let mut ledger = TrafficLedger::open(&directory).unwrap();
    let committed = fs::read(temp.journal()).unwrap();
    for (subject, order, bytes, error) in [
        ("0123", "invalid-subject", 1, Problem::Invalid),
        (SUBJECT, "invalid\norder", 1, Problem::Invalid),
        (SUBJECT, "zero", 0, Problem::Invalid),
        (SUBJECT, "overflow", u64::MAX, Problem::Limit),
    ] {
        assert_eq!(
            ledger.grant(&directory, subject, PERIOD, PERIOD, order, bytes),
            Err(error)
        );
    }
    assert_eq!(fs::read(temp.journal()).unwrap(), committed);
    assert_eq!(quota(&ledger, PERIOD).paid_bytes, 0);
}

#[test]
fn oversized_journal_is_rejected_before_replay() {
    let temp = Temp::new();
    temp.create_journal(b"");
    let file = OpenOptions::new().write(true).open(temp.journal()).unwrap();
    file.set_len(MAX_JOURNAL + 1).unwrap();
    file.sync_all().unwrap();
    assert!(TrafficLedger::open(&temp.directory()).is_err());
    assert_eq!(file.metadata().unwrap().len(), MAX_JOURNAL + 1);
}

#[test]
fn failed_sync_does_not_charge_or_acknowledge_a_reservation() {
    let temp = Temp::new();
    let directory = temp.directory();
    let mut ledger = TrafficLedger::open(&directory).unwrap();
    ledger
        .reserve(&directory, SUBJECT, PERIOD, OFFSET, 10, false)
        .unwrap();
    let expected = quota(&ledger, PERIOD);
    let committed = fs::read(temp.journal()).unwrap();
    // Linux /dev/null accepts writes but cannot sync durable file data. This
    // reaches the sync failure path after write_all has succeeded.
    ledger.file = OpenOptions::new().write(true).open("/dev/null").unwrap();
    assert_eq!(
        ledger.reserve(&directory, SUBJECT, PERIOD, OFFSET, 1, false),
        Err(Problem::Storage)
    );
    assert!(ledger.failed);
    assert_eq!(ledger.state.usage(SUBJECT, PERIOD).used_bytes, 10);
    assert_eq!(
        ledger.snapshot(SUBJECT, PERIOD, OFFSET, false),
        Err(Problem::Storage)
    );
    assert_eq!(fs::read(temp.journal()).unwrap(), committed);
    drop(ledger);
    let reopened = TrafficLedger::open(&directory).unwrap();
    assert_eq!(quota(&reopened, PERIOD), expected);
}
