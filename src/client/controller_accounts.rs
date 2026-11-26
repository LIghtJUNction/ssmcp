//! Local approval of relay-authenticated controllers. Names are display data;
//! a grant belongs to the relay origin, identity provider and stable subject.
use super::account;
use crate::{Result, wire::ControlAccount};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_ENTRIES: usize = 256;
const MAX_BYTES: usize = 512 * 1024;
const MAX_EVENTS: usize = 256;
// Coalesce repeated tool calls by one already-seen account into a minute.
const VISIT_INTERVAL: u64 = 60;
const INITIALIZED: &[u8] = b"ssmcp-controller-accounts-v1\n";

#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Decision {
    Candidate,
    Default,
    Approved,
    Pending,
    Revoked,
}

impl Decision {
    /// A reservation permits publishing the selected device to that account.
    pub(super) fn permits_target(self) -> bool {
        self == Self::Candidate || self.is_allowed()
    }

    fn allowed_here(self, session_grant: bool) -> bool {
        self.is_allowed() || (session_grant && matches!(self, Self::Pending | Self::Candidate))
    }
    pub(super) fn is_allowed(self) -> bool {
        matches!(self, Self::Default | Self::Approved)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Entry {
    pub relay: String,
    pub provider: String,
    pub subject: String,
    pub target: Option<String>,
    pub login: Option<String>,
    pub status: Decision,
    /// Unix seconds. Repeated observations within a minute share the previous visit.
    pub first_seen: u64,
    pub last_seen: u64,
    /// Set only when the relay attributes an authenticated control operation.
    pub first_verified: Option<u64>,
    pub last_verified: Option<u64>,
    pub last_denied: Option<u64>,
    pub decided_at: Option<u64>,
}

impl Entry {
    pub(super) fn display_name(&self) -> &str {
        self.login
            .as_deref()
            .or(self.target.as_deref())
            .unwrap_or(&self.subject)
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum EventKind {
    TargetResolved,
    ControlAllowed,
    ControlDenied,
    Approved,
    Revoked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    sequence: u64,
    at: u64,
    kind: EventKind,
    controller: usize,
    connection_id: Option<String>,
    label: String,
}

#[derive(Debug, Serialize)]
pub(super) struct Event {
    pub sequence: u64,
    pub at: u64,
    pub kind: EventKind,
    pub relay: String,
    pub provider: String,
    pub subject: String,
    pub label: String,
    pub connection_id: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct History {
    version: u32,
    entries: Vec<Entry>,
    events: VecDeque<Record>,
}

impl History {
    fn record(
        &mut self,
        index: usize,
        kind: EventKind,
        at: u64,
        connection_id: Option<&str>,
    ) -> Result<()> {
        let previous = self.events.back().map_or(0, |last| last.sequence);
        let sequence = previous
            .checked_add(1)
            .ok_or("controller history sequence exhausted")?;
        if self.events.len() == MAX_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(Record {
            sequence,
            at,
            kind,
            controller: index,
            connection_id: connection_id.map(str::to_owned),
            label: self.entries[index].display_name().to_owned(),
        });
        Ok(())
    }

    fn journal(&self) -> Vec<Event> {
        self.events
            .iter()
            .map(|event| {
                let entry = &self.entries[event.controller];
                Event {
                    sequence: event.sequence,
                    at: event.at,
                    kind: event.kind,
                    label: event.label.clone(),
                    relay: entry.relay.clone(),
                    provider: entry.provider.clone(),
                    subject: entry.subject.clone(),
                    connection_id: event.connection_id.clone(),
                }
            })
            .collect()
    }
}

fn valid_connection(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

fn decode_history(bytes: &[u8]) -> Result<History> {
    match serde_json::from_slice(bytes) {
        Ok(history) => Ok(history),
        Err(current_error) => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct SummaryOnly {
                version: u32,
                entries: Vec<Entry>,
            }
            match serde_json::from_slice(bytes) {
                Ok(SummaryOnly {
                    version: 1,
                    entries,
                }) => Ok(History {
                    version: 1,
                    entries,
                    events: VecDeque::new(),
                }),
                _ => Err(current_error.into()),
            }
        }
    }
}

fn canonical_subject(provider: &str, subject: &str) -> Result<String> {
    if provider != "github" || subject.len() > 20 || !subject.bytes().all(|b| b.is_ascii_digit()) {
        return Err("expected a GitHub account with a numeric subject".into());
    }
    let id: u64 = subject.parse()?;
    if id == 0 {
        return Err("invalid GitHub account subject".into());
    }
    Ok(id.to_string())
}

fn validate(history: &History) -> Result<()> {
    if !matches!(history.version, 1 | 2)
        || (history.version == 1 && !history.events.is_empty())
        || history.entries.is_empty()
        || history.entries.len() > MAX_ENTRIES
        || history.events.len() > MAX_EVENTS
    {
        return Err("invalid local controller history; inspect the private record".into());
    }
    let mut seen = HashSet::new();
    for (index, entry) in history.entries.iter().enumerate() {
        if entry.relay.len() > 512
            || crate::server_endpoint::normalize_server(&entry.relay)? != entry.relay
            || canonical_subject(&entry.provider, &entry.subject)? != entry.subject
            || entry.login.as_ref().is_some_and(|login| {
                crate::wire::canonical_github_principal(&format!("{login}@github")).is_err()
            })
            || entry
                .target
                .as_deref()
                .is_some_and(|target| crate::username::normalize(target).as_deref() != Ok(target))
            || (entry.target.is_none() && entry.login.is_none())
            || entry.login.is_some() != entry.first_verified.is_some()
            || entry.last_seen < entry.first_seen
            || entry
                .last_denied
                .is_some_and(|time| time < entry.first_seen || time > entry.last_seen)
            || entry.decided_at.is_some_and(|time| time < entry.first_seen)
            || entry.first_verified.is_some() != entry.last_verified.is_some()
            || entry
                .first_verified
                .zip(entry.last_verified)
                .is_some_and(|(first, last)| {
                    first < entry.first_seen || last < first || last > entry.last_seen
                })
            || (index == 0
                && !matches!(
                    entry.status,
                    Decision::Candidate | Decision::Default | Decision::Revoked
                ))
            || (index != 0 && matches!(entry.status, Decision::Candidate | Decision::Default))
            || (entry.status == Decision::Default && entry.first_verified.is_none())
            || (entry.status == Decision::Candidate && entry.first_verified.is_some())
            || !seen.insert((&entry.relay, &entry.provider, &entry.subject))
        {
            return Err("invalid local controller history; inspect the private record".into());
        }
    }
    let mut last_sequence = None;
    for event in &history.events {
        let is_call = matches!(
            event.kind,
            EventKind::ControlAllowed | EventKind::ControlDenied
        );
        if event.controller >= history.entries.len()
            || event.sequence == 0
            || last_sequence.is_some_and(|last: u64| last.checked_add(1) != Some(event.sequence))
            || event.connection_id.is_some() != is_call
            || event
                .connection_id
                .as_deref()
                .is_some_and(|id| !valid_connection(id))
            || event.label.is_empty()
            || event.label.len() > 64
            || event.label.chars().any(char::is_control)
        {
            return Err("invalid local controller journal".into());
        }
        last_sequence = Some(event.sequence);
    }
    Ok(())
}

fn transaction<T>(
    root: &Path,
    operation: impl FnOnce(&mut History) -> Result<(T, bool)>,
) -> Result<T> {
    account::create_tree(root)?;
    account::verify_directory(root)?;
    let mut lock = crate::session::lock(&root.join("controller-accounts.lock"), false)?;
    let mut marker = Vec::new();
    (&lock)
        .take((INITIALIZED.len() + 1) as u64)
        .read_to_end(&mut marker)?;
    if !marker.is_empty() && marker != INITIALIZED {
        return Err("invalid controller history initialization record".into());
    }
    let path = root.join("controller-accounts.json");
    let mut history = match account::read_private_limit(&path, MAX_BYTES)? {
        Some(bytes) => {
            let history = decode_history(&bytes)?;
            validate(&history)?;
            history
        }
        None if marker.is_empty() => History {
            version: 2,
            entries: vec![],
            events: VecDeque::new(),
        },
        None => return Err("previously initialized controller history is missing".into()),
    };
    // Version 1 has summaries only. Keep them, without inventing dated events.
    let migrate = history.version == 1;
    history.version = 2;
    let (result, changed) = operation(&mut history)?;
    if changed || migrate {
        account::write_private_limit(&path, &serde_json::to_vec(&history)?, MAX_BYTES)?;
        #[cfg(unix)]
        std::fs::File::open(root)?.sync_all()?;
    }
    // Written only after the first complete policy is durable. Empty state is
    // valid solely before first use; a lost ledger must never grant a new default.
    if marker.is_empty() && !history.entries.is_empty() {
        lock.write_all(INITIALIZED)?;
        lock.sync_all()?;
        #[cfg(unix)]
        std::fs::File::open(root)?.sync_all()?;
    }
    Ok(result)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Observation<'a> {
    // Identity resolved by the relay for a locally selected connection target.
    Target(&'a str),
    // Origin attached by the relay to an authenticated controller operation.
    Verified {
        login: &'a str,
        connection: &'a str,
        previous_outcome: Option<bool>,
        session_grant: bool,
    },
}

fn observe_at(
    root: &Path,
    relay: &str,
    provider: &str,
    subject: &str,
    when: u64,
    observation: Observation<'_>,
) -> Result<Decision> {
    let relay = crate::server_endpoint::normalize_server(relay)?;
    if relay.len() > 512 {
        return Err("controller relay origin exceeds its size limit".into());
    }
    let subject = canonical_subject(provider, subject)?;
    let verified = matches!(observation, Observation::Verified { .. });
    let session_grant = matches!(
        observation,
        Observation::Verified {
            session_grant: true,
            ..
        }
    );
    transaction(root, |history| {
        let entries = &mut history.entries;
        if session_grant && entries.is_empty() {
            return Err("verify the selected device owner before sharing this session".into());
        }
        if let Some((index, entry)) = entries.iter_mut().enumerate().find(|(_, entry)| {
            entry.relay == relay && entry.provider == provider && entry.subject == subject
        }) {
            let now = when.max(entry.last_seen);
            let mut changed = now.saturating_sub(entry.last_seen) >= VISIT_INTERVAL;
            if verified {
                if entry.first_verified.is_none() {
                    entry.first_verified = Some(now);
                    changed = true;
                }
                if entry
                    .last_verified
                    .is_none_or(|previous| now.saturating_sub(previous) >= VISIT_INTERVAL)
                {
                    entry.last_verified = Some(now);
                    changed = true;
                }
                if entry.status == Decision::Candidate {
                    entry.status = Decision::Default;
                    changed = true;
                }
            }
            if !entry.status.permits_target()
                && !entry.status.allowed_here(session_grant)
                && entry
                    .last_denied
                    .is_none_or(|previous| now.saturating_sub(previous) >= VISIT_INTERVAL)
            {
                entry.last_denied = Some(now);
                changed = true;
            }
            let (stored, observed) = match observation {
                Observation::Verified { login, .. } => (&mut entry.login, login),
                Observation::Target(handle) => (&mut entry.target, handle),
            };
            if stored.as_deref() != Some(observed) {
                *stored = Some(observed.to_owned());
                changed = true;
            }
            let status = entry.status;
            let event = match observation {
                Observation::Verified {
                    connection,
                    previous_outcome,
                    ..
                } => {
                    let previous = previous_outcome.or_else(|| {
                        history
                            .events
                            .iter()
                            .rev()
                            .find(|e| {
                                e.controller == index
                                    && e.connection_id.as_deref() == Some(connection)
                            })
                            .map(|e| e.kind == EventKind::ControlAllowed)
                    });
                    if previous == Some(status.allowed_here(session_grant)) {
                        None
                    } else {
                        Some((
                            if status.allowed_here(session_grant) {
                                EventKind::ControlAllowed
                            } else {
                                EventKind::ControlDenied
                            },
                            Some(connection),
                        ))
                    }
                }
                Observation::Target(_) => None,
            };
            if changed || event.is_some() {
                entry.last_seen = now;
            }
            if let Some((kind, connection)) = event {
                entry.last_verified = Some(now);
                if kind == EventKind::ControlDenied {
                    entry.last_denied = Some(now);
                }
                history.record(index, kind, now, connection)?;
                changed = true;
            }
            return Ok((status, changed));
        }
        if entries.len() >= MAX_ENTRIES {
            return Err("controller history is full; new accounts are denied".into());
        }
        let status = match (entries.is_empty(), observation) {
            (true, Observation::Target(_)) => Decision::Candidate,
            (true, Observation::Verified { .. }) => Decision::Default,
            (false, _) => Decision::Pending,
        };
        let (target, login) = match observation {
            Observation::Target(handle) => (Some(handle.to_owned()), None),
            Observation::Verified { login, .. } => (None, Some(login.to_owned())),
        };
        entries.push(Entry {
            relay,
            provider: provider.to_owned(),
            subject,
            target,
            login,
            status,
            first_seen: when,
            last_seen: when,
            first_verified: verified.then_some(when),
            last_verified: verified.then_some(when),
            last_denied: (!status.permits_target() && !status.allowed_here(session_grant))
                .then_some(when),
            decided_at: None,
        });
        let index = entries.len() - 1;
        match observation {
            Observation::Verified { connection, .. } => {
                let kind = if status.allowed_here(session_grant) {
                    EventKind::ControlAllowed
                } else {
                    EventKind::ControlDenied
                };
                history.record(index, kind, when, Some(connection))?;
            }
            Observation::Target(_) => {
                history.record(index, EventKind::TargetResolved, when, None)?
            }
        }
        Ok((status, true))
    })
}

fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

// The foreground process handles one relay connection at a time. Keep only that
// connection's last logged outcomes so journal eviction never creates duplicate
// visits for a still-live controller. The disk policy always authorizes the call.
#[derive(Default)]
struct LiveVisits {
    root: PathBuf,
    relay: String,
    connection: String,
    outcomes: HashMap<(String, String), bool>,
}

fn verified_at(
    root: &Path,
    relay: &str,
    account: &ControlAccount,
    connection: &str,
    at: u64,
    session_grant: bool,
) -> Result<Decision> {
    static VISITS: Mutex<Option<LiveVisits>> = Mutex::new(None);
    account.validate()?;
    if !valid_connection(connection) {
        return Err("invalid authenticated controller connection ID".into());
    }
    let mut cache = VISITS
        .lock()
        .map_err(|_| "controller journal cache unavailable")?;
    let visits = cache.get_or_insert_with(Default::default);
    let relay = crate::server_endpoint::normalize_server(relay)?;
    if visits.root != root || visits.relay != relay || visits.connection != connection {
        *visits = LiveVisits {
            root: root.to_owned(),
            relay: relay.clone(),
            connection: connection.to_owned(),
            ..Default::default()
        };
    }
    let key = (
        account.provider.clone(),
        canonical_subject(&account.provider, &account.subject)?,
    );
    let decision = observe_at(
        root,
        &relay,
        &key.0,
        &key.1,
        at,
        Observation::Verified {
            login: &account.login,
            connection,
            previous_outcome: visits.outcomes.get(&key).copied(),
            session_grant,
        },
    )?;
    visits
        .outcomes
        .insert(key, decision.allowed_here(session_grant));
    Ok(decision)
}

pub(super) fn observe_verified(
    relay: &str,
    account: &ControlAccount,
    connection_id: &str,
) -> Result<bool> {
    let session_grant = super::access::permits(relay, connection_id, account);
    let decision = verified_at(
        &account::state_root()?,
        relay,
        account,
        connection_id,
        now()?,
        session_grant,
    )?;
    Ok(decision.allowed_here(session_grant))
}

pub(super) fn check_session_target(relay: &str, provider: &str, subject: &str) -> Result<()> {
    let relay = crate::server_endpoint::normalize_server(relay)?;
    let subject = canonical_subject(provider, subject)?;
    transaction(&account::state_root()?, |history| {
        if history.entries.is_empty() {
            return Err("verify the selected device owner before sharing this session".into());
        }
        if history.entries.iter().any(|entry| {
            entry.relay == relay
                && entry.provider == provider
                && entry.subject == subject
                && entry.status == Decision::Revoked
        }) {
            return Err("this controller was revoked on the device; review ssmcp controllers before sharing this session".into());
        }
        Ok(((), false))
    })
}

pub(super) fn admit_resolved_target(
    relay: &str,
    provider: &str,
    subject: &str,
    relay_handle: &str,
) -> Result<Decision> {
    let handle = crate::username::normalize(relay_handle)?;
    observe_at(
        &account::state_root()?,
        relay,
        provider,
        subject,
        now()?,
        Observation::Target(&handle),
    )
}

pub(super) fn history() -> Result<Vec<Entry>> {
    transaction(&account::state_root()?, |history| {
        Ok((history.entries.clone(), false))
    })
}

pub(super) fn events() -> Result<Vec<Event>> {
    transaction(&account::state_root()?, |history| {
        Ok((history.journal(), false))
    })
}

fn decide_at(
    root: &Path,
    relay: &str,
    provider: &str,
    subject: &str,
    approve: bool,
) -> Result<Entry> {
    let relay = crate::server_endpoint::normalize_server(relay)?;
    let subject = canonical_subject(provider, subject)?;
    transaction(root, |history| {
        let (index, entry) = history
            .entries
            .iter_mut()
            .enumerate()
            .find(|(_, entry)| {
                entry.relay == relay && entry.provider == provider && entry.subject == subject
            })
            .ok_or("no verified controller observation matches this relay, provider and subject")?;
        let next = match (approve, index) {
            (true, 0) if entry.first_verified.is_none() => Decision::Candidate,
            (true, 0) => Decision::Default,
            (true, _) => Decision::Approved,
            (false, _) => Decision::Revoked,
        };
        let changed = entry.status != next;
        if changed {
            entry.decided_at = Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)?
                    .as_secs()
                    .max(entry.first_seen),
            );
            entry.status = next;
        }
        let result = entry.clone();
        if changed {
            let kind = if approve {
                EventKind::Approved
            } else {
                EventKind::Revoked
            };
            history.record(
                index,
                kind,
                result.decided_at.expect("decision timestamp"),
                None,
            )?;
        }
        Ok((result, changed))
    })
}

pub(super) fn decide(relay: &str, provider: &str, subject: &str, approve: bool) -> Result<Entry> {
    decide_at(&account::state_root()?, relay, provider, subject, approve)
}

#[cfg(test)]
#[path = "controller_accounts/tests.rs"]
mod tests;
