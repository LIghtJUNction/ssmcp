use super::*;
use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Barrier},
};

const RELAY: &str = "https://relay.example";

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let mut suffix = [0; 16];
        crate::platform::random_bytes(&mut suffix).unwrap();
        Self(std::env::temp_dir().join(format!("ssmcp-controller-{}", crate::hex::encode(suffix))))
    }
    fn entries(&self) -> Vec<Entry> {
        transaction(&self.0, |history| Ok((history.entries.clone(), false))).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn account(subject: &str, login: &str) -> ControlAccount {
    ControlAccount {
        provider: "github".into(),
        subject: subject.into(),
        login: login.into(),
    }
}

fn verified(root: &Path, relay: &str, account: &ControlAccount, when: u64) -> Result<Decision> {
    verified_at(root, relay, account, &"b".repeat(64), when, false)
}

#[test]
fn default_is_device_wide_and_approval_uses_the_verified_stable_subject() {
    let f = Fixture::new();
    let a = account("123", "alice");
    let b = account("456", "alice");
    let foreign = "https://other.example";
    assert!(f.entries().is_empty());
    assert!(decide_at(&f.0, RELAY, "github", "123", true).is_err());
    assert_eq!(verified(&f.0, RELAY, &a, 10).unwrap(), Decision::Default);
    assert_eq!(verified(&f.0, RELAY, &b, 11).unwrap(), Decision::Pending);
    assert_eq!(verified(&f.0, foreign, &a, 12).unwrap(), Decision::Pending);
    assert_eq!(
        verified(&f.0, RELAY, &account("0123", "renamed"), 13).unwrap(),
        Decision::Default
    );
    assert_eq!(
        decide_at(&f.0, RELAY, "github", "456", true)
            .unwrap()
            .status,
        Decision::Approved
    );
    assert_eq!(verified(&f.0, RELAY, &b, 160).unwrap(), Decision::Approved);
    assert!(
        decide_at(&f.0, RELAY, "github", "123", false)
            .unwrap()
            .decided_at
            .is_some()
    );
    assert_eq!(verified(&f.0, RELAY, &a, 190).unwrap(), Decision::Revoked);
    assert_eq!(verified(&f.0, foreign, &a, 200).unwrap(), Decision::Pending);
    assert_eq!(
        verified(&f.0, RELAY, &account("789", "another"), 200).unwrap(),
        Decision::Pending
    );
    assert_eq!(
        decide_at(&f.0, RELAY, "github", "123", true)
            .unwrap()
            .status,
        Decision::Default
    );
    let entries = f.entries();
    assert_eq!(entries.len(), 4);
    assert_eq!(
        (entries[0].first_seen, entries[0].last_denied),
        (10, Some(190))
    );
    assert_eq!(entries[1].last_denied, Some(11));
    assert_eq!(entries[2].last_denied, Some(200));
}

#[test]
fn authenticated_relay_preflight_reserves_only_one_target_without_a_false_sign_in() {
    let f = Fixture::new();
    let first = account("1", "owner");
    let second = account("2", "second");
    let reserve = |account: &ControlAccount, now| {
        observe_at(
            &f.0,
            RELAY,
            &account.provider,
            &account.subject,
            now,
            Observation::Target("work-owner"),
        )
    };
    let candidate = reserve(&first, 100).unwrap();
    assert!(candidate.permits_target());
    assert_eq!(f.entries()[0].target.as_deref(), Some("work-owner"));
    assert!(f.entries()[0].login.is_none());
    assert!(!candidate.is_allowed());
    assert_eq!(reserve(&second, 110).unwrap(), Decision::Pending);
    assert!(
        f.entries()
            .iter()
            .all(|entry| entry.first_verified.is_none())
    );
    assert_eq!(
        verified(&f.0, RELAY, &second, 111).unwrap(),
        Decision::Pending
    );
    assert_eq!(
        decide_at(&f.0, RELAY, "github", "2", true).unwrap().status,
        Decision::Approved
    );
    assert_eq!(reserve(&second, 113).unwrap(), Decision::Approved);
    assert_eq!(
        verified(&f.0, RELAY, &first, 114).unwrap(),
        Decision::Default
    );
    let entries = f.entries();
    assert_eq!(
        (entries[0].first_seen, entries[0].first_verified),
        (100, Some(114))
    );
    assert_eq!(
        (entries[1].first_seen, entries[1].first_verified),
        (110, Some(111))
    );
}

#[test]
fn controller_target_matches_the_recorded_relay_handle_and_fails_on_ambiguity() {
    use crate::client::controller_command::resolve_controller;

    let fixture = Fixture::new();
    let record = |relay, subject, target| {
        observe_at(
            &fixture.0,
            relay,
            "github",
            subject,
            10,
            Observation::Target(target),
        )
        .unwrap()
    };
    record(RELAY, "42", "team-owner");
    assert_eq!(record(RELAY, "76", "teammate"), Decision::Pending);
    record("https://other.example", "52", "teammate");
    verified(&fixture.0, RELAY, &account("76", "github-login"), 11).unwrap();
    let select = |target: &[&str]| resolve_controller(fixture.entries(), target);
    let entry = select(&["TeamMate@RELAY.EXAMPLE:443"]).unwrap();
    assert_eq!(entry.subject, "76");
    assert!(select(&["github-login@relay.example"]).is_err());
    assert_eq!(select(&["teammate@other.example"]).unwrap().subject, "52");
    for (approve, status) in [(true, Decision::Approved), (false, Decision::Revoked)] {
        assert_eq!(
            decide_at(
                &fixture.0,
                &entry.relay,
                &entry.provider,
                &entry.subject,
                approve
            )
            .unwrap()
            .status,
            status
        );
    }
    record(RELAY, "102", "teammate");
    assert!(select(&["teammate@relay.example"]).is_err());
    assert_eq!(select(&[RELAY, "github", "102"]).unwrap().subject, "102");
}

#[test]
fn journal_distinguishes_connections_and_decisions_and_coalesces_tool_calls() {
    let f = Fixture::new();
    let a = account("42", "alice");
    let b = account("76", "bob");
    let first = "1".repeat(64);
    let second = "2".repeat(64);
    let record = |identity: &ControlAccount, id: &str, when| {
        verified_at(&f.0, RELAY, identity, id, when, false).unwrap()
    };
    assert_eq!(record(&a, &first, 10), Decision::Default);
    assert_eq!(record(&a, &first, 80), Decision::Default);
    assert_eq!(record(&b, &first, 90), Decision::Pending);
    assert_eq!(record(&b, &first, 150), Decision::Pending);
    decide_at(&f.0, RELAY, "github", "76", true).unwrap();
    assert_eq!(record(&b, &first, 160), Decision::Approved);
    assert_eq!(record(&a, &second, 170), Decision::Default);
    decide_at(&f.0, RELAY, "github", "42", false).unwrap();
    assert_eq!(record(&a, &second, 180), Decision::Revoked);
    let journal = transaction(&f.0, |history| Ok((history.journal(), false))).unwrap();
    let kinds: Vec<_> = journal.iter().map(|entry| entry.kind).collect();
    assert_eq!(
        kinds,
        [
            EventKind::ControlAllowed,
            EventKind::ControlDenied,
            EventKind::Approved,
            EventKind::ControlAllowed,
            EventKind::ControlAllowed,
            EventKind::Revoked,
            EventKind::ControlDenied
        ]
    );
    for (index, event) in journal.iter().enumerate() {
        assert_eq!(event.sequence, index as u64 + 1);
    }
    assert_eq!(journal[0].connection_id.as_deref(), Some(first.as_str()));
    assert_eq!(journal[4].connection_id.as_deref(), Some(second.as_str()));
    assert!(journal[5].connection_id.is_none());
}

#[test]
fn migration_preserves_approvals_and_bounded_history_keeps_real_sequence_numbers() {
    let f = Fixture::new();
    let a = account("42", "alice");
    verified(&f.0, RELAY, &a, 1).unwrap();
    let path = f.0.join("controller-accounts.json");
    let summaries = f.entries();
    let legacy = serde_json::json!({"version":1,"entries":summaries});
    fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
    let migrated = transaction(&f.0, |history| Ok((history.journal(), false))).unwrap();
    assert!(migrated.is_empty());
    assert_eq!(f.entries()[0].status, Decision::Default);
    let on_disk: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(on_disk["version"], 2);
    assert_eq!(on_disk["events"], serde_json::json!([]));
    transaction(&f.0, |history| {
        for _ in 0..=MAX_EVENTS {
            history.record(0, EventKind::TargetResolved, 10, None)?;
        }
        Ok(((), true))
    })
    .unwrap();
    let journal = transaction(&f.0, |history| Ok((history.journal(), false))).unwrap();
    assert_eq!(
        (
            journal.len(),
            journal[0].sequence,
            journal.last().unwrap().sequence
        ),
        (MAX_EVENTS, 2, MAX_EVENTS as u64 + 1)
    );
    let broken = serde_json::json!({"version":2, "entries":summaries});
    fs::write(&path, serde_json::to_vec(&broken).unwrap()).unwrap();
    assert!(verified(&f.0, RELAY, &a, 20).is_err());
}

#[test]
fn simultaneous_first_contacts_create_exactly_one_default() {
    let f = Fixture::new();
    let gate = Arc::new(Barrier::new(8));
    std::thread::scope(|scope| {
        let jobs: Vec<_> = (1..=8)
            .map(|number| {
                let gate = Arc::clone(&gate);
                let root = &f.0;
                scope.spawn(move || {
                    gate.wait();
                    observe_at(
                        root,
                        RELAY,
                        "github",
                        &number.to_string(),
                        1,
                        Observation::Verified {
                            login: "same-name",
                            connection: &"a".repeat(64),
                            previous_outcome: None,
                            session_grant: false,
                        },
                    )
                    .unwrap()
                })
            })
            .collect();
        let defaults = jobs
            .into_iter()
            .map(|job| job.join().unwrap())
            .filter(|decision| decision.is_allowed())
            .count();
        assert_eq!(defaults, 1);
    });
    assert_eq!(f.entries().len(), 8);
}

#[test]
fn unreadable_invalid_or_missing_initialized_policy_never_bootstraps_again() {
    let f = Fixture::new();
    let a = account("12", "alice");
    assert!(verified(&f.0, RELAY, &account("bad", "alice"), 1).is_err());
    verified(&f.0, RELAY, &a, 1).unwrap();
    let path = f.0.join("controller-accounts.json");
    let saved = fs::read(&path).unwrap();
    // High frequency activity is checked each call but doesn't rewrite the ledger.
    verified(&f.0, RELAY, &a, 59).unwrap();
    assert_eq!(saved, fs::read(&path).unwrap());
    for bad in [b"not json".as_slice(), br#"{"version":1,"entries":[]}"#] {
        fs::write(&path, bad).unwrap();
        assert!(verified(&f.0, RELAY, &a, 60).is_err());
        assert_eq!(fs::read(&path).unwrap(), bad);
    }
    fs::remove_file(&path).unwrap();
    assert!(verified(&f.0, RELAY, &a, 60).is_err());
    assert!(!path.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::write(&path, saved).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(verified(&f.0, RELAY, &a, 60).is_err());
    }
}

#[test]
fn a_session_grant_cannot_bootstrap_or_override_device_wide_access() {
    let fixture = Fixture::new();
    let guest = account("2", "guest");
    let verify_guest = |when, session_grant| {
        verified_at(
            &fixture.0,
            RELAY,
            &guest,
            &"c".repeat(64),
            when,
            session_grant,
        )
    };
    assert!(verify_guest(1, true).is_err());
    assert!(fixture.entries().is_empty());
    verified(&fixture.0, RELAY, &account("1", "owner"), 2).unwrap();
    assert!(verify_guest(3, true).unwrap().allowed_here(true));
    assert_eq!(fixture.entries()[1].status, Decision::Pending);
    assert!(!verify_guest(4, false).unwrap().allowed_here(false));
    decide_at(&fixture.0, RELAY, "github", "2", false).unwrap();
    assert!(!verify_guest(5, true).unwrap().allowed_here(true));
}
